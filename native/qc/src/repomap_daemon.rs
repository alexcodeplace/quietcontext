use crate::config::MapConfig;
use crate::repomap;
use crate::repomap_index::{
    self, build_generation, refresh_replacement, scan_inputs, PathChange, RefreshOutcome,
    ScanConfig, ScanInputs, SourceIndex,
};
use crate::repomap_protocol::{
    self, CacheState, EffectiveMapConfig, LookupOperation, LookupRequest, LookupResponse,
    LookupTimings, PROTOCOL_VERSION,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind, RenameMode};
#[cfg(unix)]
use notify::Watcher as NotifyWatcher;
#[cfg(unix)]
use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc::{self, Receiver, TryRecvError};

const SOCKET_REVISION: &str = "v2";
const MAX_ROOTS: usize = 8;
const MAX_CLIENT_QUEUE: usize = 64;
const MAX_EVENT_QUEUE: usize = 4096;
const MAX_WATCHES_PER_ROOT: usize = 8192;
const MAX_WATCHES: usize = 16384;
const MAX_MEMO_PER_ROOT: usize = 256;
const MAX_LOGICAL_BYTES: usize = 384 * 1024 * 1024;
const MAX_RSS_BYTES: usize = 512 * 1024 * 1024;
const RSS_BUILD_MULTIPLIER: usize = 2;
const RSS_BUILD_HEADROOM_BYTES: usize = 16 * 1024 * 1024;
const STARTUP_LOCK_WAIT: Duration = Duration::from_millis(5);
const REQUEST_WAIT: Duration = Duration::from_millis(25);
const ROOT_IDLE: Duration = Duration::from_secs(30 * 60);
const DAEMON_IDLE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug)]
enum DaemonError {
    Io(io::Error),
    Scan(repomap_index::ScanError),
    #[cfg(not(unix))]
    Unsupported,
    Busy,
    Unstable,
    WatchUnavailable,
    Memory,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "daemon I/O error: {error}"),
            Self::Scan(error) => write!(f, "daemon scan error: {error}"),
            #[cfg(not(unix))]
            Self::Unsupported => write!(f, "daemon unsupported on this platform"),
            Self::Busy => write!(f, "daemon startup or capacity is busy"),
            Self::Unstable => write!(f, "repository remained unstable"),
            Self::WatchUnavailable => write!(f, "filesystem watcher unavailable"),
            Self::Memory => write!(f, "lookup index memory bound exceeded"),
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<io::Error> for DaemonError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<repomap_index::ScanError> for DaemonError {
    fn from(error: repomap_index::ScanError) -> Self {
        Self::Scan(error)
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct RootId {
    root: PathBuf,
    policy: repomap_index::ScanPolicyKey,
}

#[derive(Clone, Eq, PartialEq)]
struct MemoKey {
    generation: u64,
    operation: LookupOperation,
    query: Option<String>,
    config: EffectiveMapConfig,
}

impl Hash for MemoKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.generation.hash(state);
        match self.operation {
            LookupOperation::Map => 0_u8.hash(state),
            LookupOperation::Sym => 1_u8.hash(state),
            LookupOperation::Refs => 2_u8.hash(state),
        }
        self.query.hash(state);
        self.config.max_bytes.hash(state);
        self.config.max_files.hash(state);
        self.config.max_file_bytes.hash(state);
        self.config.refs_max_per_file.hash(state);
        self.config.refs_max_total.hash(state);
    }
}

impl MemoKey {
    fn bytes(&self) -> usize {
        self.query
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(std::mem::size_of::<Self>())
    }
}

struct MemoValue {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

struct Memo {
    limit: usize,
    values: VecDeque<(MemoKey, MemoValue)>,
    generation: u64,
}

impl Memo {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            values: VecDeque::new(),
            generation: 0,
        }
    }

    fn clear(&mut self, generation: u64) {
        self.values.clear();
        self.generation = generation;
    }

    fn get(&mut self, key: &MemoKey) -> Option<MemoValue> {
        if key.generation != self.generation {
            return None;
        }
        let position = self
            .values
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        let (candidate, value) = self.values.remove(position)?;
        self.values.push_back((candidate, value));
        self.values.back_mut().map(|(_, value)| MemoValue {
            stdout: value.stdout.clone(),
            stderr: value.stderr.clone(),
            exit_code: value.exit_code,
        })
    }

    fn insert(&mut self, key: MemoKey, value: MemoValue) {
        if key.generation != self.generation || self.limit == 0 {
            return;
        }
        self.values.retain(|(candidate, _)| candidate != &key);
        self.values.push_back((key, value));
        while self.values.len() > self.limit {
            self.values.pop_front();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.values.len()
    }

    fn bytes(&self) -> usize {
        self.values
            .iter()
            .map(|(key, value)| {
                key.bytes()
                    .saturating_add(value.stdout.len())
                    .saturating_add(value.stderr.len())
            })
            .sum()
    }
}

struct RootState {
    index: Option<Arc<SourceIndex>>,
    stale: bool,
    events: VecDeque<PathChange>,
    memo: Memo,
    watches: Vec<PathBuf>,
    last_used: Instant,
    active: usize,
    event_limit: usize,
}

impl RootState {
    fn new(now: Instant, memo_limit: usize, event_limit: usize) -> Self {
        Self {
            index: None,
            stale: true,
            events: VecDeque::new(),
            memo: Memo::new(memo_limit),
            watches: Vec::new(),
            last_used: now,
            active: 0,
            event_limit,
        }
    }

    fn mark_stale(&mut self) {
        self.stale = true;
        self.events.clear();
    }

    fn queue_change(&mut self, change: PathChange) {
        if self.events.len() >= self.event_limit {
            self.mark_stale();
            return;
        }
        self.events.push_back(change);
    }

    fn generation(&self) -> u64 {
        self.index.as_ref().map_or(0, |index| index.generation())
    }

    fn bytes(&self) -> usize {
        self.index
            .as_ref()
            .map_or(0, |index| index.logical_bytes())
            .saturating_add(self.memo.bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WatchEventKind {
    Created,
    Modified,
    Removed,
    DirectoryChanged,
    RenameFrom,
    RenameTo,
    Renamed {
        from: PathBuf,
        to: PathBuf,
    },
    Overflow,
    Error,
    #[allow(dead_code)]
    RootReplaced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WatchEvent {
    path: PathBuf,
    kind: WatchEventKind,
    cookie: u32,
}

impl WatchEvent {
    fn created(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: WatchEventKind::Created,
            cookie: 0,
        }
    }

    fn modified(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: WatchEventKind::Modified,
            cookie: 0,
        }
    }

    fn removed(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: WatchEventKind::Removed,
            cookie: 0,
        }
    }

    fn directory_changed(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            kind: WatchEventKind::DirectoryChanged,
            cookie: 0,
        }
    }

    fn renamed(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        let from = from.into();
        let to = to.into();
        Self {
            path: to.clone(),
            kind: WatchEventKind::Renamed { from, to },
            cookie: 0,
        }
    }

    fn overflow() -> Self {
        Self {
            path: PathBuf::new(),
            kind: WatchEventKind::Overflow,
            cookie: 0,
        }
    }

    fn error() -> Self {
        Self {
            path: PathBuf::new(),
            kind: WatchEventKind::Error,
            cookie: 0,
        }
    }
}

#[derive(Debug)]
struct BoundedQueue<T> {
    capacity: usize,
    items: VecDeque<T>,
}

impl<T> BoundedQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity.min(1024)),
        }
    }

    fn push(&mut self, item: T) -> Result<(), T> {
        if self.capacity == 0 || self.items.len() >= self.capacity {
            return Err(item);
        }
        self.items.push_back(item);
        Ok(())
    }

    fn pop(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    fn clear(&mut self) {
        self.items.clear();
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.capacity
    }
}

#[derive(Clone, Debug)]
struct DaemonConfig {
    max_roots: usize,
    max_watches_per_root: usize,
    max_watches: usize,
    max_event_queue: usize,
    max_client_queue: usize,
    max_memo_per_root: usize,
    max_logical_bytes: usize,
    max_rss_bytes: usize,
    root_idle: Duration,
    daemon_idle: Duration,
    request_timeout: Duration,
    stabilization_attempts: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            max_roots: MAX_ROOTS,
            max_watches_per_root: MAX_WATCHES_PER_ROOT,
            max_watches: MAX_WATCHES,
            max_event_queue: MAX_EVENT_QUEUE,
            max_client_queue: MAX_CLIENT_QUEUE,
            max_memo_per_root: MAX_MEMO_PER_ROOT,
            max_logical_bytes: MAX_LOGICAL_BYTES,
            max_rss_bytes: MAX_RSS_BYTES,
            root_idle: ROOT_IDLE,
            daemon_idle: DAEMON_IDLE,
            request_timeout: REQUEST_WAIT,
            stabilization_attempts: repomap_index::MAX_STABILIZATION_ATTEMPTS,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WatchError {
    Unavailable,
    BudgetExceeded,
    InvalidPath(PathBuf),
    Io(String),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "filesystem watcher unavailable"),
            Self::BudgetExceeded => write!(f, "filesystem watcher budget exceeded"),
            Self::InvalidPath(path) => write!(f, "invalid watch path: {}", path.display()),
            Self::Io(error) => write!(f, "filesystem watcher error: {error}"),
        }
    }
}

#[cfg(unix)]
struct WatchRegistration {
    roots: HashSet<RootId>,
}

#[cfg(unix)]
struct Watcher {
    watcher: Option<RecommendedWatcher>,
    receiver: Receiver<notify::Result<NotifyEvent>>,
    overflow: Arc<AtomicBool>,
    paths: HashMap<PathBuf, WatchRegistration>,
    roots: HashMap<RootId, Vec<PathBuf>>,
    max_watches: usize,
}

#[cfg(unix)]
fn should_queue_notify_event(event: &notify::Result<NotifyEvent>) -> bool {
    match event {
        Ok(event) if !event.need_rescan() => {
            !matches!(&event.kind, EventKind::Access(AccessKind::Open(_)))
        }
        Ok(_) | Err(_) => true,
    }
}

#[cfg(unix)]
impl Watcher {
    fn with_limit(max_watches: usize, queue_limit: usize) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(queue_limit.max(1));
        let overflow = Arc::new(AtomicBool::new(false));
        let callback_overflow = Arc::clone(&overflow);
        let watcher = notify::recommended_watcher(move |event| {
            if should_queue_notify_event(&event) && sender.try_send(event).is_err() {
                callback_overflow.store(true, Ordering::Release);
            }
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            watcher: Some(watcher),
            receiver,
            overflow,
            paths: HashMap::new(),
            roots: HashMap::new(),
            max_watches,
        })
    }

    fn disabled(max_watches: usize) -> Self {
        let (_sender, receiver) = mpsc::sync_channel(1);
        Self {
            watcher: None,
            receiver,
            overflow: Arc::new(AtomicBool::new(false)),
            paths: HashMap::new(),
            roots: HashMap::new(),
            max_watches,
        }
    }

    fn paths_for_root(&self, id: &RootId) -> Vec<PathBuf> {
        self.roots.get(id).cloned().unwrap_or_default()
    }

    fn replace_root(
        &mut self,
        id: &RootId,
        requested: &[PathBuf],
        per_root_limit: usize,
    ) -> Result<Vec<PathBuf>, WatchError> {
        let Some(watcher) = self.watcher.as_mut() else {
            return Err(WatchError::Unavailable);
        };
        let mut paths = Vec::with_capacity(requested.len());
        for path in requested {
            paths.push(watch_target(path).ok_or_else(|| WatchError::InvalidPath(path.clone()))?);
        }
        paths.sort();
        paths.dedup();
        if paths.len() > per_root_limit {
            return Err(WatchError::BudgetExceeded);
        }
        let old = self.roots.get(id).cloned().unwrap_or_default();
        let new_set: HashSet<_> = paths.iter().cloned().collect();
        let removed_unique = old
            .iter()
            .filter(|path| {
                !new_set.contains(*path)
                    && self
                        .paths
                        .get(*path)
                        .is_some_and(|registration| registration.roots.len() == 1)
            })
            .count();
        let added_unique = paths
            .iter()
            .filter(|path| !self.paths.contains_key(*path))
            .count();
        if self
            .paths
            .len()
            .saturating_sub(removed_unique)
            .saturating_add(added_unique)
            > self.max_watches
        {
            return Err(WatchError::BudgetExceeded);
        }

        let mut added: Vec<PathBuf> = Vec::new();
        for path in &paths {
            if let Some(registration) = self.paths.get_mut(path) {
                registration.roots.insert(id.clone());
                continue;
            }
            if let Err(error) = watcher.watch(path, RecursiveMode::NonRecursive) {
                for added_path in added.iter().rev() {
                    let _ = watcher.unwatch(added_path);
                    self.paths.remove(added_path);
                }
                return Err(WatchError::Io(error.to_string()));
            }
            let mut roots = HashSet::new();
            roots.insert(id.clone());
            self.paths.insert(path.clone(), WatchRegistration { roots });
            added.push(path.clone());
        }
        for path in old {
            if !new_set.contains(&path) {
                self.remove_root_path(id, &path);
            }
        }
        self.roots.insert(id.clone(), paths.clone());
        Ok(paths)
    }

    fn remove_root_path(&mut self, id: &RootId, path: &Path) {
        let Some(registration) = self.paths.get_mut(path) else {
            return;
        };
        registration.roots.remove(id);
        if !registration.roots.is_empty() {
            return;
        }
        self.paths.remove(path);
        if let Some(watcher) = self.watcher.as_mut() {
            let _ = watcher.unwatch(path);
        }
    }

    fn remove_root(&mut self, id: &RootId) {
        if let Some(paths) = self.roots.remove(id) {
            for path in paths {
                self.remove_root_path(id, &path);
            }
        }
    }

    fn roots_for_path(&self, path: &Path) -> HashSet<RootId> {
        let mut roots = HashSet::new();
        let mut current = Some(path);
        while let Some(candidate) = current {
            if let Some(registration) = self.paths.get(candidate) {
                roots.extend(registration.roots.iter().cloned());
            }
            current = candidate.parent();
        }
        roots
    }

    fn push_translated(
        output: &mut Vec<(Option<RootId>, WatchEvent)>,
        roots: &HashSet<RootId>,
        event: WatchEvent,
        limit: usize,
    ) -> bool {
        if roots.is_empty() {
            if output.len() >= limit {
                return true;
            }
            output.push((None, event));
            return false;
        }
        for root in roots {
            if output.len() >= limit {
                return true;
            }
            output.push((Some(root.clone()), event.clone()));
        }
        false
    }

    fn translate(&self, event: NotifyEvent, limit: usize) -> Vec<(Option<RootId>, WatchEvent)> {
        let limit = limit.max(1);
        if event.need_rescan() {
            return vec![(None, WatchEvent::overflow())];
        }
        let roots = event.paths.iter().fold(HashSet::new(), |mut roots, path| {
            roots.extend(self.roots_for_path(path));
            roots
        });
        let cookie = event
            .tracker()
            .map_or(0, |tracker| tracker.min(u32::MAX as usize) as u32);
        let paths = event.paths;
        if paths.is_empty() {
            return vec![(None, WatchEvent::error())];
        }

        let mut output = Vec::new();
        let mut overflow = false;

        match event.kind {
            EventKind::Create(CreateKind::Folder) => {
                for path in paths {
                    if Self::push_translated(
                        &mut output,
                        &roots,
                        WatchEvent::directory_changed(path),
                        limit,
                    ) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Create(CreateKind::File) => {
                for path in paths {
                    if Self::push_translated(&mut output, &roots, WatchEvent::created(path), limit)
                    {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Create(CreateKind::Any | CreateKind::Other) => {
                for path in paths {
                    if Self::push_translated(
                        &mut output,
                        &roots,
                        WatchEvent::directory_changed(path),
                        limit,
                    ) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Remove(RemoveKind::Folder) => {
                for path in paths {
                    if Self::push_translated(
                        &mut output,
                        &roots,
                        WatchEvent::directory_changed(path),
                        limit,
                    ) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Remove(RemoveKind::File) => {
                for path in paths {
                    if Self::push_translated(&mut output, &roots, WatchEvent::removed(path), limit)
                    {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Remove(RemoveKind::Any | RemoveKind::Other) => {
                for path in paths {
                    if Self::push_translated(
                        &mut output,
                        &roots,
                        WatchEvent::directory_changed(path),
                        limit,
                    ) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                let mut paths = paths.into_iter();
                let from = paths.next().expect("rename event has source path");
                let to = paths.next().expect("rename event has target path");
                overflow = Self::push_translated(
                    &mut output,
                    &roots,
                    WatchEvent::renamed(from, to),
                    limit,
                );
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                overflow = Self::push_translated(&mut output, &roots, WatchEvent::error(), limit);
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for path in paths {
                    let mut change = WatchEvent::removed(path);
                    change.kind = WatchEventKind::RenameFrom;
                    change.cookie = cookie;
                    if Self::push_translated(&mut output, &roots, change, limit) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                for path in paths {
                    let mut change = WatchEvent::created(path);
                    change.kind = WatchEventKind::RenameTo;
                    change.cookie = cookie;
                    if Self::push_translated(&mut output, &roots, change, limit) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                overflow = Self::push_translated(&mut output, &roots, WatchEvent::error(), limit);
            }
            EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Metadata(_)) => {
                for path in paths {
                    if Self::push_translated(&mut output, &roots, WatchEvent::modified(path), limit)
                    {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Modify(ModifyKind::Any | ModifyKind::Other) => {
                for path in paths {
                    if Self::push_translated(
                        &mut output,
                        &roots,
                        WatchEvent::directory_changed(path),
                        limit,
                    ) {
                        overflow = true;
                        break;
                    }
                }
            }
            EventKind::Access(_) => {}
            EventKind::Other | EventKind::Any => {
                overflow = Self::push_translated(&mut output, &roots, WatchEvent::error(), limit);
            }
        }

        if overflow {
            output.truncate(limit.saturating_sub(1));
            output.push((None, WatchEvent::overflow()));
        }
        output
    }

    fn drain(&mut self, limit: usize) -> Vec<(Option<RootId>, WatchEvent)> {
        let limit = limit.max(1);
        let mut output = Vec::with_capacity(limit.min(1024));
        let mut overflow = self.overflow.swap(false, Ordering::AcqRel);
        let mut terminal_error = false;
        let mut received_limit = true;
        if overflow {
            output.push((None, WatchEvent::overflow()));
        }
        for _ in 0..limit {
            match self.receiver.try_recv() {
                Ok(Ok(event)) => {
                    if output.len() >= limit {
                        overflow = true;
                        continue;
                    }
                    let translated = self.translate(event, limit - output.len());
                    if translated
                        .iter()
                        .any(|(_, event)| matches!(event.kind, WatchEventKind::Overflow))
                    {
                        overflow = true;
                    }
                    output.extend(translated);
                }
                Ok(Err(_)) => {
                    terminal_error = true;
                    received_limit = false;
                    break;
                }
                Err(TryRecvError::Empty) => {
                    received_limit = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    terminal_error = true;
                    received_limit = false;
                    break;
                }
            }
        }
        if received_limit {
            match self.receiver.try_recv() {
                Ok(_) => overflow = true,
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => terminal_error = true,
            }
        }
        if terminal_error {
            if output.len() >= limit {
                output.truncate(limit.saturating_sub(1));
            }
            output.push((None, WatchEvent::error()));
        }
        if overflow {
            output.truncate(limit.saturating_sub(1));
            output.push((None, WatchEvent::overflow()));
        }
        output
    }
}

#[cfg(unix)]
impl Drop for Watcher {
    fn drop(&mut self) {
        self.paths.clear();
        self.roots.clear();
        self.watcher.take();
    }
}

#[cfg(not(unix))]
struct Watcher;

fn watch_target(path: &Path) -> Option<PathBuf> {
    let mut candidate = path.to_path_buf();
    loop {
        if let Ok(metadata) = fs::symlink_metadata(&candidate) {
            if metadata.file_type().is_dir() {
                return Some(candidate);
            }
        }
        let parent = candidate.parent()?.to_path_buf();
        if parent == candidate {
            return None;
        }
        candidate = parent;
    }
}

#[cfg(all(unix, test))]
fn complete_scan_inputs(root: &Path, config: &ScanConfig) -> Result<ScanInputs, DaemonError> {
    complete_scan_inputs_bounded(root, config, MAX_WATCHES_PER_ROOT)
}

#[cfg(unix)]
fn complete_scan_inputs_bounded(
    root: &Path,
    config: &ScanConfig,
    watch_limit: usize,
) -> Result<ScanInputs, DaemonError> {
    let watch_limit = watch_limit.clamp(1, MAX_WATCHES_PER_ROOT);
    let mut inputs = scan_inputs(root, config).map_err(DaemonError::Scan)?;
    if inputs.dependency_parents.len() > watch_limit {
        return Err(DaemonError::WatchUnavailable);
    }

    let mut directories = BTreeSet::new();
    directories.insert(inputs.canonical_root.clone());
    if let Some(parent) = inputs.canonical_root.parent() {
        directories.insert(parent.to_path_buf());
    }
    let mut walker = ignore::WalkBuilder::new(&inputs.canonical_root);
    walker
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false)
        .sort_by_file_name(|left, right| left.cmp(right))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|file_type| {
                    file_type.is_dir()
                        && repomap_index::STATIC_SKIP_DIRECTORIES
                            .contains(&entry.file_name().to_string_lossy().as_ref())
                })
        });
    for entry in walker.build() {
        let entry = entry.map_err(|_| DaemonError::WatchUnavailable)?;
        let Some(file_type) = entry.file_type() else {
            return Err(DaemonError::WatchUnavailable);
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            directories.insert(entry.path().to_path_buf());
            if directories.len() > watch_limit {
                return Err(DaemonError::WatchUnavailable);
            }
        }
    }

    let mut all_watch_paths = directories.clone();
    all_watch_paths.extend(inputs.dependency_parents.iter().cloned());
    if all_watch_paths.len() > watch_limit {
        return Err(DaemonError::WatchUnavailable);
    }
    inputs.watched_directories = directories.into_iter().collect();
    inputs.dependency_parents.sort_unstable();
    inputs.dependency_parents.dedup();
    Ok(inputs)
}

#[cfg(all(not(unix), test))]
fn complete_scan_inputs(_root: &Path, _config: &ScanConfig) -> Result<ScanInputs, DaemonError> {
    Err(DaemonError::Unsupported)
}

#[cfg(not(unix))]
fn complete_scan_inputs_bounded(
    _root: &Path,
    _config: &ScanConfig,
    _watch_limit: usize,
) -> Result<ScanInputs, DaemonError> {
    Err(DaemonError::Unsupported)
}

fn watch_paths(inputs: &ScanInputs) -> Vec<PathBuf> {
    let mut paths: BTreeSet<_> = inputs.watched_directories.iter().cloned().collect();
    paths.extend(inputs.dependency_parents.iter().cloned());
    paths.into_iter().collect()
}

#[derive(Clone, Debug)]
struct DaemonPaths {
    state_dir: PathBuf,
    socket: PathBuf,
    owner_lock: PathBuf,
    pid: PathBuf,
}

impl DaemonPaths {
    fn from_state_dir(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            socket: state_dir.join(format!("repomap-{SOCKET_REVISION}.sock")),
            owner_lock: state_dir.join(format!("repomap-{SOCKET_REVISION}.owner.lock")),
            pid: state_dir.join(format!("repomap-{SOCKET_REVISION}.pid")),
            state_dir,
        }
    }

    fn discover() -> io::Result<Self> {
        let state_dir = crate::util::state_dir().ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                "QuietContext state directory unavailable",
            )
        })?;
        Ok(Self::from_state_dir(state_dir.join("repomap")))
    }

    fn ensure_private(&self) -> io::Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(&self.state_dir)?;
            if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "daemon state directory is not user-owned",
                ));
            }
            fs::set_permissions(&self.state_dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum StartupError {
    Io(io::Error),
    Busy,
    UnsafeSocket,
    #[cfg(not(unix))]
    Unsupported,
}

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "daemon startup I/O error: {error}"),
            Self::Busy => write!(f, "another daemon owns the socket"),
            Self::UnsafeSocket => write!(f, "daemon socket is not user-owned"),
            #[cfg(not(unix))]
            Self::Unsupported => write!(f, "daemon unsupported on this platform"),
        }
    }
}

impl std::error::Error for StartupError {}

#[cfg(unix)]
struct StartupLock {
    file: File,
}

#[cfg(unix)]
impl StartupLock {
    fn acquire(path: &Path) -> Result<Self, StartupError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(StartupError::Io)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(StartupError::Io)?;
        let start = Instant::now();
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self { file });
            }
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != ErrorKind::WouldBlock && error.raw_os_error() != Some(libc::EAGAIN) {
                return Err(StartupError::Io(error));
            }
            if start.elapsed() >= STARTUP_LOCK_WAIT {
                return Err(StartupError::Busy);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(unix)]
impl Drop for StartupLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(unix)]
struct DaemonOwner {
    listener: UnixListener,
    paths: DaemonPaths,
    socket_identity: Option<(u64, u64)>,
    pid: u32,
    _lock: StartupLock,
}

#[cfg(unix)]
impl DaemonOwner {
    fn acquire(paths: DaemonPaths) -> Result<Self, StartupError> {
        paths.ensure_private().map_err(StartupError::Io)?;
        let lock = StartupLock::acquire(&paths.owner_lock)?;
        let listener = bind_socket(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .map_err(StartupError::Io)?;
        write_pid(&paths.pid).map_err(StartupError::Io)?;
        Ok(Self {
            listener,
            socket_identity: socket_identity(&paths.socket),
            pid: std::process::id(),
            paths,
            _lock: lock,
        })
    }
}

#[cfg(unix)]
impl Drop for DaemonOwner {
    fn drop(&mut self) {
        if socket_identity(&self.paths.socket) == self.socket_identity {
            let _ = fs::remove_file(&self.paths.socket);
        }
        let owns_pid = fs::read_to_string(&self.paths.pid)
            .ok()
            .and_then(|contents| contents.trim().parse::<u32>().ok())
            == Some(self.pid);
        if owns_pid {
            let _ = fs::remove_file(&self.paths.pid);
        }
    }
}

#[cfg(not(unix))]
struct DaemonOwner;

#[cfg(not(unix))]
impl DaemonOwner {
    fn acquire(_paths: DaemonPaths) -> Result<Self, StartupError> {
        Err(StartupError::Unsupported)
    }
}

#[cfg(unix)]
fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(unix)]
fn bind_socket(path: &Path) -> Result<UnixListener, StartupError> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == ErrorKind::AddrInUse => {
            let metadata = fs::symlink_metadata(path).map_err(StartupError::Io)?;
            if metadata.uid() != unsafe { libc::geteuid() }
                || !metadata.file_type().is_socket()
                || metadata.mode() & 0o077 != 0
            {
                return Err(StartupError::UnsafeSocket);
            }
            match UnixStream::connect(path) {
                Ok(_) => Err(StartupError::Busy),
                Err(connect_error)
                    if matches!(
                        connect_error.kind(),
                        ErrorKind::ConnectionRefused
                            | ErrorKind::NotFound
                            | ErrorKind::ConnectionReset
                    ) =>
                {
                    fs::remove_file(path).map_err(StartupError::Io)?;
                    UnixListener::bind(path).map_err(StartupError::Io)
                }
                Err(connect_error) => Err(StartupError::Io(connect_error)),
            }
        }
        Err(error) => Err(StartupError::Io(error)),
    }
}

#[cfg(unix)]
fn write_pid(path: &Path) -> io::Result<()> {
    let temporary = path.with_extension("pid.tmp");
    fs::write(&temporary, format!("{}\n", std::process::id()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(temporary, path)
}

#[cfg(unix)]
fn same_uid(stream: &UnixStream) -> io::Result<bool> {
    let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(credential.uid == unsafe { libc::geteuid() })
}

#[cfg(target_os = "linux")]
fn resident_set_bytes() -> Option<usize> {
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<usize>().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    resident_pages.checked_mul(page_size as usize)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn resident_set_bytes() -> Option<usize> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let maximum = unsafe { usage.assume_init().ru_maxrss };
    if maximum <= 0 {
        return None;
    }
    let raw = maximum as u128;
    #[cfg(target_os = "macos")]
    let bytes = raw;
    #[cfg(not(target_os = "macos"))]
    let bytes = raw.saturating_mul(1024);
    usize::try_from(bytes).ok()
}

#[cfg(not(unix))]
fn resident_set_bytes() -> Option<usize> {
    None
}

struct Daemon {
    roots: HashMap<RootId, RootState>,
    events: BoundedQueue<(Option<RootId>, WatchEvent)>,
    client_queue: BoundedQueue<LookupRequest>,
    last_activity: Instant,
    watcher: Watcher,
    config: DaemonConfig,
    generation: u64,
    shutdown: bool,
}

fn ignore_environment_changed(index: &SourceIndex) -> bool {
    let current = repomap_index::IgnoreContextKey::default();
    let cached = index.ignore_context();
    current.home != cached.home
        || current.xdg_config_home != cached.xdg_config_home
        || current.git_config != cached.git_config
}

fn ignore_event_path(index: &SourceIndex, path: &Path) -> bool {
    let ignore_name = path.file_name().is_some_and(|name| {
        name == std::ffi::OsStr::new(".gitignore") || name == std::ffi::OsStr::new(".ignore")
    });
    ignore_name
        || index.ignore_context().inputs.iter().any(|input| {
            input.path == path || input.path.parent().is_some_and(|parent| parent == path)
        })
}

fn retry_scan_error(error: &repomap_index::ScanError) -> bool {
    matches!(error, repomap_index::ScanError::IgnoreContextMismatch)
}

impl Daemon {
    fn new() -> Self {
        Self::with_config(DaemonConfig::default())
    }

    fn with_config(mut config: DaemonConfig) -> Self {
        config.max_roots = config.max_roots.clamp(1, MAX_ROOTS);
        config.max_watches_per_root = config.max_watches_per_root.clamp(1, MAX_WATCHES_PER_ROOT);
        config.max_watches = config.max_watches.clamp(1, MAX_WATCHES);
        config.max_event_queue = config.max_event_queue.clamp(1, MAX_EVENT_QUEUE);
        config.max_client_queue = config.max_client_queue.clamp(1, MAX_CLIENT_QUEUE);
        config.max_memo_per_root = config.max_memo_per_root.min(MAX_MEMO_PER_ROOT);
        config.max_rss_bytes = config.max_rss_bytes.clamp(1, MAX_RSS_BYTES);
        config.max_logical_bytes = config
            .max_logical_bytes
            .min(MAX_LOGICAL_BYTES)
            .min(config.max_rss_bytes);
        config.stabilization_attempts = config
            .stabilization_attempts
            .clamp(1, repomap_index::MAX_STABILIZATION_ATTEMPTS);
        let watcher = {
            #[cfg(unix)]
            {
                Watcher::with_limit(config.max_watches, config.max_event_queue)
                    .unwrap_or_else(|_| Watcher::disabled(config.max_watches))
            }
            #[cfg(not(unix))]
            {
                Watcher
            }
        };
        Self {
            roots: HashMap::new(),
            events: BoundedQueue::new(config.max_event_queue),
            client_queue: BoundedQueue::new(config.max_client_queue),
            last_activity: Instant::now(),
            watcher,
            config,
            generation: 0,
            shutdown: false,
        }
    }

    fn client_queue_len(&self) -> usize {
        self.client_queue.len()
    }

    #[cfg(test)]
    fn memo_len(&self, root: &Path, policy: &repomap_index::ScanPolicyKey) -> usize {
        self.roots
            .get(&RootId {
                root: root.to_path_buf(),
                policy: policy.clone(),
            })
            .map_or(0, |state| state.memo.len())
    }

    #[cfg(test)]
    fn enqueue_watch_event(&mut self, event: WatchEvent) -> bool {
        self.last_activity = Instant::now();
        if self.events.push((None, event)).is_err() {
            self.events.clear();
            self.mark_all_stale();
            return false;
        }
        true
    }

    fn mark_needs_reconcile(&mut self, root: Option<&Path>) {
        self.last_activity = Instant::now();
        match root {
            Some(root) => {
                for (id, state) in &mut self.roots {
                    if id.root == root {
                        state.mark_stale();
                    }
                }
            }
            None => self.mark_all_stale(),
        }
    }

    fn mark_all_stale(&mut self) {
        for state in self.roots.values_mut() {
            state.mark_stale();
        }
    }

    fn request_shutdown(&mut self) {
        self.shutdown = true;
        self.mark_all_stale();
    }

    fn check_rss(&mut self) -> Result<usize, DaemonError> {
        match resident_set_bytes() {
            Some(rss) if rss <= self.config.max_rss_bytes => Ok(rss),
            _ => {
                self.request_shutdown();
                Err(DaemonError::Memory)
            }
        }
    }

    fn bounded_scan_config(&mut self, scan: &ScanConfig) -> Result<ScanConfig, DaemonError> {
        let rss = self.check_rss()?;
        let available = self.config.max_rss_bytes.saturating_sub(rss);
        let budget = available
            .saturating_sub(RSS_BUILD_HEADROOM_BYTES)
            .checked_div(RSS_BUILD_MULTIPLIER)
            .unwrap_or(0);
        if budget == 0 {
            self.request_shutdown();
            return Err(DaemonError::Memory);
        }
        let mut bounded = scan.clone();
        bounded.max_index_bytes = bounded
            .max_index_bytes
            .min(self.config.max_logical_bytes)
            .min(budget);
        if bounded.max_index_bytes == 0 {
            self.request_shutdown();
            return Err(DaemonError::Memory);
        }
        Ok(bounded)
    }

    fn submit_client_request(&mut self, request: LookupRequest) -> Result<(), LookupRequest> {
        self.client_queue.push(request)
    }

    fn pop_client_request(&mut self) -> Option<LookupRequest> {
        self.client_queue.pop()
    }

    fn drain_events(&mut self) {
        #[cfg(unix)]
        {
            let delivered = self.watcher.drain(self.config.max_event_queue);
            if !delivered.is_empty() {
                self.last_activity = Instant::now();
            }
            for event in delivered {
                if self.events.push(event).is_err() {
                    self.events.clear();
                    self.mark_all_stale();
                    break;
                }
            }
        }
        self.process_events();
    }

    fn process_events(&mut self) {
        let mut renames: HashMap<(Option<RootId>, u32), PathBuf> = HashMap::new();
        while let Some((root, event)) = self.events.pop() {
            match event.kind {
                WatchEventKind::RenameFrom => {
                    if event.cookie == 0 {
                        self.apply_event(root, WatchEvent::error());
                    } else {
                        renames.insert((root, event.cookie), event.path);
                    }
                }
                WatchEventKind::RenameTo => {
                    if event.cookie == 0 {
                        self.apply_event(root, WatchEvent::error());
                    } else if let Some(from) = renames.remove(&(root.clone(), event.cookie)) {
                        self.apply_event(root, WatchEvent::renamed(from, event.path));
                    } else {
                        self.apply_event(root, WatchEvent::error());
                    }
                }
                _ => self.apply_event(root, event),
            }
        }
        for ((root, _), _) in renames {
            self.apply_event(root, WatchEvent::error());
        }
    }

    fn apply_event(&mut self, tagged_root: Option<RootId>, event: WatchEvent) {
        if matches!(event.kind, WatchEventKind::Overflow | WatchEventKind::Error) {
            self.mark_needs_reconcile(None);
            return;
        }
        let roots: Vec<_> = match tagged_root {
            Some(root) => vec![root],
            None => self.roots.keys().cloned().collect(),
        };
        for id in roots {
            let Some(change) = self.classify_event(&id, &event) else {
                continue;
            };
            if let Some(state) = self.roots.get_mut(&id) {
                state.queue_change(change);
            }
        }
    }

    fn classify_event(&self, id: &RootId, event: &WatchEvent) -> Option<PathChange> {
        let state = self.roots.get(id)?;
        let root = &id.root;
        let rename_paths = match &event.kind {
            WatchEventKind::Renamed { from, to } => Some((from.as_path(), to.as_path())),
            _ => None,
        };
        let is_root_boundary =
            |path: &Path| path == root || root.parent().is_some_and(|parent| path == parent);
        if is_root_boundary(&event.path)
            || rename_paths.is_some_and(|(from, to)| is_root_boundary(from) || is_root_boundary(to))
            || matches!(event.kind, WatchEventKind::RootReplaced)
        {
            return Some(PathChange::RootReplaced);
        }
        if let Some(index) = &state.index {
            if ignore_event_path(index, &event.path) {
                return Some(PathChange::IgnoreChanged(event.path.clone()));
            }
        }
        let affects_root = event.path.starts_with(root)
            || rename_paths
                .is_some_and(|(from, to)| from.starts_with(root) || to.starts_with(root));
        if !affects_root {
            return None;
        }
        match &event.kind {
            WatchEventKind::Created => Some(PathChange::Created(event.path.clone())),
            WatchEventKind::Modified => Some(PathChange::Modified(event.path.clone())),
            WatchEventKind::Removed => Some(PathChange::Removed(event.path.clone())),
            WatchEventKind::DirectoryChanged => {
                Some(PathChange::DirectoryChanged(event.path.clone()))
            }
            WatchEventKind::Renamed { from, to } => Some(PathChange::Renamed {
                from: from.clone(),
                to: to.clone(),
            }),
            WatchEventKind::RootReplaced => Some(PathChange::RootReplaced),
            WatchEventKind::RenameFrom | WatchEventKind::RenameTo => Some(PathChange::Error),
            WatchEventKind::Overflow | WatchEventKind::Error => Some(PathChange::Error),
        }
    }

    fn evict(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .roots
            .iter()
            .filter(|(_, state)| {
                state.active == 0
                    && now.saturating_duration_since(state.last_used) >= self.config.root_idle
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove_root(&id);
        }
    }

    fn remove_root(&mut self, id: &RootId) {
        #[cfg(unix)]
        self.watcher.remove_root(id);
        self.roots.remove(id);
    }

    fn ensure_slot(&mut self, id: &RootId, now: Instant) -> Result<(), DaemonError> {
        if self.roots.contains_key(id) {
            return Ok(());
        }
        self.evict(now);
        if self.roots.len() >= self.config.max_roots {
            let candidate = self
                .roots
                .iter()
                .filter(|(_, state)| state.active == 0)
                .min_by_key(|(_, state)| state.last_used)
                .map(|(id, _)| id.clone())
                .ok_or(DaemonError::Busy)?;
            self.remove_root(&candidate);
        }
        self.roots.insert(
            id.clone(),
            RootState::new(
                now,
                self.config.max_memo_per_root,
                self.config.max_event_queue,
            ),
        );
        Ok(())
    }

    fn total_bytes(&self) -> usize {
        self.roots.values().map(RootState::bytes).sum()
    }

    fn can_publish(&mut self, id: &RootId, candidate: &SourceIndex) -> bool {
        if self.check_rss().is_err() || candidate.logical_bytes() > self.config.max_logical_bytes {
            return false;
        }
        let old = self.roots.get(id).map_or(0, RootState::bytes);
        let mut total = self.total_bytes().saturating_sub(old);
        if total.saturating_add(candidate.logical_bytes()) <= self.config.max_logical_bytes {
            return true;
        }
        let mut candidates: Vec<_> = self
            .roots
            .iter()
            .filter(|(other, state)| *other != id && state.active == 0)
            .map(|(other, state)| (other.clone(), state.last_used, state.bytes()))
            .collect();
        candidates.sort_by_key(|(_, last_used, _)| *last_used);
        for (other, _, bytes) in candidates {
            self.remove_root(&other);
            total = total.saturating_sub(bytes);
            if total.saturating_add(candidate.logical_bytes()) <= self.config.max_logical_bytes {
                return true;
            }
        }
        false
    }

    fn install_watches_from_paths(
        &mut self,
        id: &RootId,
        paths: &[PathBuf],
    ) -> Result<(), DaemonError> {
        #[cfg(unix)]
        {
            let paths = self
                .watcher
                .replace_root(id, paths, self.config.max_watches_per_root)
                .map_err(|_| DaemonError::WatchUnavailable)?;
            if let Some(state) = self.roots.get_mut(id) {
                state.watches = paths;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (id, paths);
            Err(DaemonError::Unsupported)
        }
    }

    fn install_initial_watches(&mut self, id: &RootId) -> Result<(), DaemonError> {
        let mut paths = {
            #[cfg(unix)]
            {
                self.watcher.paths_for_root(id)
            }
            #[cfg(not(unix))]
            {
                Vec::new()
            }
        };
        paths.push(id.root.clone());
        if let Some(parent) = id.root.parent() {
            paths.push(parent.to_path_buf());
        }
        self.install_watches_from_paths(id, &paths)
    }

    fn next_generation(&mut self, prior: u64) -> u64 {
        self.generation = self.generation.max(prior).saturating_add(1);
        self.generation
    }

    fn reconcile(&mut self, id: &RootId, scan: &ScanConfig) -> Result<CacheState, DaemonError> {
        let prior = self.roots.get(id).map_or(0, RootState::generation);
        self.install_initial_watches(id)?;
        for _ in 0..self.config.stabilization_attempts {
            self.check_rss()?;
            if let Some(state) = self.roots.get_mut(id) {
                state.stale = false;
                state.events.clear();
            }
            self.drain_events();
            self.check_rss()?;

            let first_scan = self.bounded_scan_config(scan)?;
            let before = complete_scan_inputs_bounded(
                &id.root,
                &first_scan,
                self.config.max_watches_per_root,
            )?;
            self.check_rss()?;
            let before_paths = watch_paths(&before);
            self.install_watches_from_paths(id, &before_paths)?;
            let candidate_generation = self.next_generation(prior);
            match build_generation(&before, &first_scan, candidate_generation) {
                Ok(_) => {}
                Err(error) if retry_scan_error(&error) => continue,
                Err(error) => return Err(DaemonError::Scan(error)),
            }
            self.drain_events();
            self.check_rss()?;

            let second_scan = self.bounded_scan_config(scan)?;
            let second_inputs = complete_scan_inputs_bounded(
                &id.root,
                &second_scan,
                self.config.max_watches_per_root,
            )?;
            self.check_rss()?;
            let second_paths = watch_paths(&second_inputs);
            self.install_watches_from_paths(id, &second_paths)?;
            let second = match build_generation(&second_inputs, &second_scan, candidate_generation)
            {
                Ok(index) => index,
                Err(error) if retry_scan_error(&error) => continue,
                Err(error) => return Err(DaemonError::Scan(error)),
            };
            self.drain_events();
            self.check_rss()?;
            let after = complete_scan_inputs_bounded(
                &id.root,
                &second_scan,
                self.config.max_watches_per_root,
            )?;
            self.drain_events();
            self.check_rss()?;
            let unstable = self
                .roots
                .get(id)
                .is_some_and(|state| state.stale || !state.events.is_empty());
            if unstable || !second_inputs.same_watch_set(&after) || !second.root_identity_matches()
            {
                continue;
            }
            if !self.can_publish(id, &second) {
                return Err(DaemonError::Memory);
            }
            let published = Arc::new(second);
            if let Some(state) = self.roots.get_mut(id) {
                state.index = Some(published.clone());
                state.stale = false;
                state.events.clear();
                state.memo.clear(published.generation());
                state.last_used = Instant::now();
            }
            return Ok(CacheState::Reconciled);
        }
        if let Some(state) = self.roots.get_mut(id) {
            state.mark_stale();
        }
        Err(DaemonError::Unstable)
    }

    fn ensure(
        &mut self,
        request: &LookupRequest,
    ) -> Result<(Arc<SourceIndex>, CacheState, RootId), DaemonError> {
        let config: MapConfig = request.map_config.into();
        let mut scan = ScanConfig::from_map(&config);
        scan.max_index_bytes = self.config.max_logical_bytes;
        let id = RootId {
            root: PathBuf::from(&request.canonical_root),
            policy: scan.policy(),
        };
        let now = Instant::now();
        self.drain_events();
        self.evict(now);
        self.ensure_slot(&id, now)?;
        let needs_reconcile = self.roots.get(&id).is_some_and(|state| {
            state.index.is_none()
                || state.stale
                || state
                    .index
                    .as_ref()
                    .is_some_and(|index| ignore_environment_changed(index))
        });
        if needs_reconcile {
            let status = self.reconcile(&id, &scan)?;
            let index = self
                .roots
                .get(&id)
                .and_then(|state| state.index.clone())
                .ok_or(DaemonError::Unstable)?;
            return Ok((index, status, id));
        }
        let root_identity_ok = self
            .roots
            .get(&id)
            .and_then(|state| state.index.as_ref())
            .is_some_and(|index| index.root_identity_matches());
        if !root_identity_ok {
            if let Some(state) = self.roots.get_mut(&id) {
                state.stale = true;
            }
            let status = self.reconcile(&id, &scan)?;
            let index = self
                .roots
                .get(&id)
                .and_then(|state| state.index.clone())
                .ok_or(DaemonError::Unstable)?;
            return Ok((index, status, id));
        }
        let changes = self
            .roots
            .get_mut(&id)
            .map(|state| {
                state.last_used = now;
                state.events.drain(..).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if changes.is_empty() {
            self.check_rss()?;
            let index = self
                .roots
                .get(&id)
                .and_then(|state| state.index.clone())
                .ok_or(DaemonError::Unstable)?;
            return Ok((index, CacheState::Hit, id));
        }
        let refresh_scan = self.bounded_scan_config(&scan)?;
        let current = self
            .roots
            .get(&id)
            .and_then(|state| state.index.clone())
            .ok_or(DaemonError::Unstable)?;
        match refresh_replacement(&current, &changes, &refresh_scan) {
            RefreshOutcome::Refreshed(replacement) => {
                if !self.can_publish(&id, &replacement) {
                    if let Some(state) = self.roots.get_mut(&id) {
                        state.mark_stale();
                    }
                    return Err(DaemonError::Memory);
                }
                self.generation = self.generation.max(replacement.generation());
                let replacement = Arc::new(replacement);
                if let Some(state) = self.roots.get_mut(&id) {
                    state.index = Some(replacement.clone());
                    state.stale = false;
                    state.memo.clear(replacement.generation());
                    state.last_used = now;
                }
                Ok((replacement, CacheState::Refreshed, id))
            }
            RefreshOutcome::NeedsReconcile(reason) => {
                if matches!(reason, repomap_index::ReconcileReason::RootIdentityChanged) {
                    self.mark_needs_reconcile(Some(&id.root));
                } else if let Some(state) = self.roots.get_mut(&id) {
                    state.stale = true;
                }
                let status = self.reconcile(&id, &scan)?;
                let index = self
                    .roots
                    .get(&id)
                    .and_then(|state| state.index.clone())
                    .ok_or(DaemonError::Unstable)?;
                Ok((index, status, id))
            }
        }
    }

    fn direct_response(&self, request: &LookupRequest, timings: LookupTimings) -> LookupResponse {
        let root = PathBuf::from(&request.canonical_root);
        let config: MapConfig = request.map_config.into();
        let result = match request.operation {
            LookupOperation::Map => repomap::build_map_direct(&root, &config),
            LookupOperation::Sym => repomap::build_sym_direct(
                request.query.as_deref().unwrap_or_default(),
                &root,
                &config,
            ),
            LookupOperation::Refs => repomap::build_refs_direct(
                request.query.as_deref().unwrap_or_default(),
                &root,
                &config,
            ),
        };
        response_from_result(
            request.operation,
            request.query.as_deref(),
            result,
            CacheState::Bypassed,
            0,
            timings,
        )
    }

    fn dispatch(&mut self, request: LookupRequest) -> LookupResponse {
        let started = Instant::now();
        if request.validate().is_err() {
            return self.direct_response(
                &request,
                LookupTimings {
                    total_us: elapsed_us(started.elapsed()),
                    ..LookupTimings::default()
                },
            );
        }
        self.last_activity = started;
        if self.check_rss().is_err() {
            return self.direct_response(
                &request,
                LookupTimings {
                    total_us: elapsed_us(started.elapsed()),
                    ..LookupTimings::default()
                },
            );
        }
        let reconcile_started = Instant::now();
        let (index, status, id) = match self.ensure(&request) {
            Ok(value) => value,
            Err(_) => {
                let id = RootId {
                    root: PathBuf::from(&request.canonical_root),
                    policy: ScanConfig::from(&MapConfig::from(request.map_config)).policy(),
                };
                if let Some(state) = self.roots.get_mut(&id) {
                    state.mark_stale();
                }
                return self.direct_response(
                    &request,
                    LookupTimings {
                        reconcile_us: elapsed_us(reconcile_started.elapsed()),
                        total_us: elapsed_us(started.elapsed()),
                        ..LookupTimings::default()
                    },
                );
            }
        };
        if let Some(state) = self.roots.get_mut(&id) {
            state.active = state.active.saturating_add(1);
            state.last_used = Instant::now();
        }
        let query = request.query.as_deref();
        let memo_allowed = request.operation == LookupOperation::Refs
            && query.is_some_and(|query| !identifier_query(query));
        let key = memo_allowed.then(|| MemoKey {
            generation: index.generation(),
            operation: request.operation,
            query: request.query.clone(),
            config: request.map_config,
        });
        if let Some(key) = key.as_ref() {
            if let Some(state) = self.roots.get_mut(&id) {
                if let Some(value) = state.memo.get(key) {
                    state.active = state.active.saturating_sub(1);
                    state.last_used = Instant::now();
                    return LookupResponse {
                        version: PROTOCOL_VERSION,
                        status: CacheState::Hit,
                        stdout: value.stdout,
                        stderr: value.stderr,
                        exit_code: value.exit_code,
                        generation: index.generation(),
                        timings: LookupTimings {
                            reconcile_us: elapsed_us(reconcile_started.elapsed()),
                            total_us: elapsed_us(started.elapsed()),
                            ..LookupTimings::default()
                        },
                    };
                }
            }
        }
        let render_started = Instant::now();
        let config: MapConfig = request.map_config.into();
        let result = match request.operation {
            LookupOperation::Map => repomap::build_map(&index, &config),
            LookupOperation::Sym => repomap::build_sym(query.unwrap_or_default(), &index, &config),
            LookupOperation::Refs => {
                repomap::build_refs(query.unwrap_or_default(), &index, &config)
            }
        };
        let response = response_from_result(
            request.operation,
            query,
            result,
            status,
            index.generation(),
            LookupTimings {
                reconcile_us: elapsed_us(reconcile_started.elapsed()),
                render_us: elapsed_us(render_started.elapsed()),
                total_us: elapsed_us(started.elapsed()),
                ..LookupTimings::default()
            },
        );
        if self.check_rss().is_err() {
            if let Some(state) = self.roots.get_mut(&id) {
                state.active = state.active.saturating_sub(1);
                state.last_used = Instant::now();
            }
            return response;
        }
        if let Some(key) = key {
            let value = MemoValue {
                stdout: response.stdout.clone(),
                stderr: response.stderr.clone(),
                exit_code: response.exit_code,
            };
            let memo_bytes = key
                .bytes()
                .saturating_add(value.stdout.len())
                .saturating_add(value.stderr.len());
            if self.total_bytes().saturating_add(memo_bytes) <= self.config.max_logical_bytes {
                if let Some(state) = self.roots.get_mut(&id) {
                    state.memo.insert(key, value);
                }
            }
        }
        if self.check_rss().is_err() {
            if let Some(state) = self.roots.get_mut(&id) {
                let generation = state.generation();
                state.memo.clear(generation);
            }
        }
        if let Some(state) = self.roots.get_mut(&id) {
            state.active = state.active.saturating_sub(1);
            state.last_used = Instant::now();
        }
        response
    }

    fn prune(&mut self, now: Instant) {
        self.evict(now);
    }

    fn should_exit(&self, now: Instant) -> bool {
        self.shutdown
            || (self.roots.is_empty()
                && self.client_queue.is_empty()
                && self.events.is_empty()
                && now.saturating_duration_since(self.last_activity) >= self.config.daemon_idle)
    }

    #[cfg(unix)]
    fn serve_listener(&mut self, listener: &UnixListener) -> Result<(), DaemonError> {
        listener.set_nonblocking(true)?;
        loop {
            if self.shutdown || self.check_rss().is_err() {
                return Ok(());
            }
            self.drain_events();
            let mut accepted = false;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        accepted = true;
                        self.last_activity = Instant::now();
                        if !same_uid(&stream).unwrap_or(false) {
                            continue;
                        }
                        if self.client_queue_len() >= self.config.max_client_queue {
                            continue;
                        }
                        if stream
                            .set_read_timeout(Some(self.config.request_timeout))
                            .is_err()
                            || stream
                                .set_write_timeout(Some(self.config.request_timeout))
                                .is_err()
                        {
                            continue;
                        }
                        let request = match repomap_protocol::read_request(&mut stream) {
                            Ok(request) => request,
                            Err(_) => continue,
                        };
                        if self.submit_client_request(request).is_err() {
                            continue;
                        }
                        if let Some(request) = self.pop_client_request() {
                            let output_cap = request.map_config.max_bytes;
                            let response = self.dispatch(request);
                            let _ = repomap_protocol::write_response(
                                &mut stream,
                                &response,
                                output_cap,
                            );
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            let now = Instant::now();
            self.prune(now);
            if self.should_exit(now) {
                return Ok(());
            }
            if !accepted {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

fn identifier_query(query: &str) -> bool {
    !query.is_empty()
        && query
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
}

fn elapsed_us(duration: Duration) -> u32 {
    duration.as_micros().min(u32::MAX as u128) as u32
}

fn response_from_result(
    operation: LookupOperation,
    query: Option<&str>,
    result: repomap::ScanResult,
    status: CacheState,
    generation: u64,
    timings: LookupTimings,
) -> LookupResponse {
    let (stdout, stderr, exit_code) = match result {
        Ok(Some(output)) => (output, String::new(), 0),
        Ok(None) => match operation {
            LookupOperation::Map => (
                String::new(),
                "qc repo map: no source files found\n".to_owned(),
                1,
            ),
            LookupOperation::Sym => (
                String::new(),
                format!("qc repo symbol: {} not found\n", query.unwrap_or_default()),
                1,
            ),
            LookupOperation::Refs => (
                String::new(),
                format!(
                    "qc repo references: {} not found\n",
                    query.unwrap_or_default()
                ),
                1,
            ),
        },
        Err(error) => {
            let command = match operation {
                LookupOperation::Map => "map",
                LookupOperation::Sym => "sym",
                LookupOperation::Refs => "refs",
            };
            (String::new(), format!("qc repo {command}: {error}\n"), 1)
        }
    };
    LookupResponse {
        version: PROTOCOL_VERSION,
        status,
        stdout,
        stderr,
        exit_code,
        generation,
        timings,
    }
}

fn daemon_io_error(error: DaemonError) -> io::Error {
    let message = error.to_string();
    match error {
        DaemonError::Io(error) => error,
        #[cfg(not(unix))]
        DaemonError::Unsupported => io::Error::new(ErrorKind::Unsupported, message),
        DaemonError::Busy => io::Error::new(ErrorKind::WouldBlock, message),
        DaemonError::WatchUnavailable | DaemonError::Unstable | DaemonError::Memory => {
            io::Error::other(message)
        }
        DaemonError::Scan(_) => io::Error::new(ErrorKind::InvalidData, message),
    }
}

fn startup_io_error(error: StartupError) -> io::Error {
    let message = error.to_string();
    match error {
        StartupError::Io(error) => error,
        StartupError::Busy => io::Error::new(ErrorKind::WouldBlock, message),
        StartupError::UnsafeSocket => io::Error::new(ErrorKind::PermissionDenied, message),
        #[cfg(not(unix))]
        StartupError::Unsupported => io::Error::new(ErrorKind::Unsupported, message),
    }
}

#[cfg(unix)]
pub(crate) fn run_daemon() -> io::Result<()> {
    let paths = DaemonPaths::discover()?;
    let owner = DaemonOwner::acquire(paths).map_err(startup_io_error)?;
    let mut daemon = Daemon::new();
    daemon
        .serve_listener(&owner.listener)
        .map_err(daemon_io_error)
}

#[cfg(not(unix))]
pub(crate) fn run_daemon() -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        "daemon unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("qc-daemon-{suffix}"));
        fs::create_dir_all(&root).expect("root");
        root
    }

    fn write_file(root: &Path, relative: &str, source: &[u8]) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        fs::File::create(path)
            .and_then(|mut file| file.write_all(source))
            .expect("write");
    }

    fn request(root: &Path, operation: LookupOperation, query: Option<&str>) -> LookupRequest {
        LookupRequest::new(
            operation,
            root.to_string_lossy().to_string(),
            query.map(str::to_owned),
            EffectiveMapConfig::default(),
        )
        .expect("request")
    }

    fn scan_config(max_files: usize) -> ScanConfig {
        ScanConfig {
            max_files,
            ..ScanConfig::default()
        }
    }

    #[test]
    fn bounded_queue_rejects_without_dropping() {
        let mut queue = BoundedQueue::new(2);
        assert!(queue.push(1).is_ok());
        assert!(queue.push(2).is_ok());
        assert_eq!(queue.push(3), Err(3));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.capacity(), 2);
        assert_eq!(queue.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn notify_queue_filters_only_non_rescan_open_events() {
        use notify::event::{AccessMode, Flag};

        let open = Ok(NotifyEvent::new(EventKind::Access(AccessKind::Open(
            AccessMode::Any,
        ))));
        assert!(!should_queue_notify_event(&open));

        let rescan_open = Ok(NotifyEvent::new(EventKind::Access(AccessKind::Open(
            AccessMode::Any,
        )))
        .set_flag(Flag::Rescan));
        assert!(should_queue_notify_event(&rescan_open));
        assert!(should_queue_notify_event(&Err(notify::Error::generic(
            "backend"
        ))));

        let close_write = Ok(NotifyEvent::new(EventKind::Access(AccessKind::Close(
            AccessMode::Write,
        ))));
        assert!(should_queue_notify_event(&close_write));

        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::Any)),
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Other,
            EventKind::Any,
        ] {
            assert!(should_queue_notify_event(&Ok(NotifyEvent::new(kind))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn topology_discovery_ignores_max_files_cap() {
        let root = root();
        write_file(&root, "a.rs", b"pub fn first() {}\n");
        write_file(&root, "nested/deep.rs", b"pub fn second() {}\n");
        let inputs = complete_scan_inputs(&root, &scan_config(1)).expect("topology");
        assert!(inputs
            .watched_directories
            .iter()
            .any(|path| path == &root.join("nested")));
        let index = build_generation(&inputs, &scan_config(1), 1).expect("index");
        assert!(index.truncated());
        assert!(index
            .files()
            .iter()
            .all(|file| file.relative_path == "a.rs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn immutable_generation_and_exact_rendering() {
        let root = root();
        write_file(
            &root,
            "main.rs",
            b"pub fn old_name() {}\nlet x = old_name;\n",
        );
        let mut daemon = Daemon::new();
        let map = daemon.dispatch(request(&root, LookupOperation::Map, None));
        assert_eq!(map.status, CacheState::Reconciled);
        assert!(map.stdout.contains("main.rs"));
        let old = daemon
            .roots
            .values()
            .next()
            .and_then(|state| state.index.clone())
            .expect("published");
        write_file(&root, "main.rs", b"pub fn new_name() {}\n");
        daemon.enqueue_watch_event(WatchEvent::modified(root.join("main.rs")));
        let updated = daemon.dispatch(request(&root, LookupOperation::Sym, Some("new_name")));
        assert_eq!(updated.status, CacheState::Refreshed);
        assert!(old.declarations("old_name").is_some());
        assert!(updated.stdout.contains("new_name"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rename_both_invalidates_move_out_source_root() {
        let root = root();
        let source = root.join("old.rs");
        let outside = root.parent().expect("parent").join(format!(
            "{}-out.rs",
            root.file_name().expect("name").to_string_lossy()
        ));
        write_file(&root, "old.rs", b"pub fn moved_out() {}\n");
        let mut daemon = Daemon::new();
        let initial = daemon.dispatch(request(&root, LookupOperation::Sym, Some("moved_out")));
        assert_eq!(initial.status, CacheState::Reconciled);
        fs::rename(&source, &outside).expect("move out");
        assert!(daemon.enqueue_watch_event(WatchEvent::renamed(source, outside.clone())));
        let updated = daemon.dispatch(request(&root, LookupOperation::Sym, Some("moved_out")));
        assert_ne!(updated.status, CacheState::Hit);
        assert_eq!(updated.exit_code, 1);
        assert_eq!(updated.stderr, "qc repo symbol: moved_out not found\n");
        fs::remove_file(outside).ok();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rename_both_invalidates_move_in_destination_root() {
        let root = root();
        let outside = root.parent().expect("parent").join(format!(
            "{}-in.rs",
            root.file_name().expect("name").to_string_lossy()
        ));
        fs::write(&outside, b"pub fn moved_in() {}\n").expect("write outside");
        let mut daemon = Daemon::new();
        let initial = daemon.dispatch(request(&root, LookupOperation::Map, None));
        assert_eq!(initial.status, CacheState::Reconciled);
        let destination = root.join("new.rs");
        fs::rename(&outside, &destination).expect("move in");
        assert!(daemon.enqueue_watch_event(WatchEvent::renamed(outside, destination)));
        let updated = daemon.dispatch(request(&root, LookupOperation::Sym, Some("moved_in")));
        assert_ne!(updated.status, CacheState::Hit);
        assert_eq!(updated.exit_code, 0);
        assert!(updated.stdout.contains("new.rs"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rename_both_invalidates_both_cross_root_caches() {
        let source_root = root();
        let destination_root = root();
        let source = source_root.join("old.rs");
        let destination = destination_root.join("new.rs");
        write_file(&source_root, "old.rs", b"pub fn moved_between() {}\n");
        write_file(&destination_root, "stable.rs", b"pub fn stable() {}\n");
        let mut daemon = Daemon::new();
        let source_initial = daemon.dispatch(request(
            &source_root,
            LookupOperation::Sym,
            Some("moved_between"),
        ));
        let destination_initial = daemon.dispatch(request(
            &destination_root,
            LookupOperation::Sym,
            Some("stable"),
        ));
        assert_eq!(source_initial.status, CacheState::Reconciled);
        assert_eq!(destination_initial.status, CacheState::Reconciled);
        fs::rename(&source, &destination).expect("move across roots");
        assert!(daemon.enqueue_watch_event(WatchEvent::renamed(source, destination)));

        let source_updated = daemon.dispatch(request(
            &source_root,
            LookupOperation::Sym,
            Some("moved_between"),
        ));
        let destination_updated = daemon.dispatch(request(
            &destination_root,
            LookupOperation::Sym,
            Some("moved_between"),
        ));
        assert_ne!(source_updated.status, CacheState::Hit);
        assert_eq!(source_updated.exit_code, 1);
        assert_ne!(destination_updated.status, CacheState::Hit);
        assert_eq!(destination_updated.exit_code, 0);
        assert!(destination_updated.stdout.contains("new.rs"));
        fs::remove_dir_all(source_root).ok();
        fs::remove_dir_all(destination_root).ok();
    }
    #[test]
    fn unchanged_request_hits_same_generation() {
        let root = root();
        write_file(&root, "main.rs", b"pub fn stable() {}\n");
        let mut daemon = Daemon::new();
        let first = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable")));
        let second = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable")));
        assert_eq!(first.generation, second.generation);
        assert_eq!(second.status, CacheState::Hit);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_open_flood_publishes_then_hits_and_mutation_fences() {
        let root = root();
        for index in 0..256 {
            write_file(
                &root,
                &format!("nested-{index:03}/source.rs"),
                format!("pub fn stable_{index:03}() {{}}\n").as_bytes(),
            );
        }
        let mut daemon = Daemon::with_config(DaemonConfig {
            max_event_queue: 1,
            ..DaemonConfig::default()
        });

        let first = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable_000")));
        assert_eq!(first.status, CacheState::Reconciled);
        assert!(first.generation > 0);

        thread::sleep(Duration::from_millis(50));
        let second = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable_000")));
        assert_eq!(second.status, CacheState::Hit);
        assert_eq!(second.generation, first.generation);

        let changed = root.join("nested-000/source.rs");
        write_file(&root, "nested-000/source.rs", b"pub fn mutated() {}\n");
        assert!(daemon.enqueue_watch_event(WatchEvent::modified(changed)));
        let mutation = daemon.dispatch(request(&root, LookupOperation::Sym, Some("mutated")));
        assert_ne!(mutation.status, CacheState::Hit);
        assert!(mutation.generation > first.generation);
        assert!(mutation.stdout.contains("mutated"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn uncertain_event_reconciles_before_serve() {
        let root = root();
        write_file(&root, "main.rs", b"pub fn stable() {}\n");
        let mut daemon = Daemon::new();
        let _ = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable")));
        daemon.enqueue_watch_event(WatchEvent::overflow());
        let response = daemon.dispatch(request(&root, LookupOperation::Sym, Some("stable")));
        assert!(matches!(
            response.status,
            CacheState::Reconciled | CacheState::Bypassed
        ));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn memo_is_generation_scoped_and_bounded() {
        let root = root();
        write_file(&root, "main.rs", b"let left-right = 1;\n");
        let mut daemon = Daemon::new();
        let first = daemon.dispatch(request(&root, LookupOperation::Refs, Some("left-right")));
        let second = daemon.dispatch(request(&root, LookupOperation::Refs, Some("left-right")));
        assert_eq!(second.status, CacheState::Hit);
        assert_eq!(first.stdout, second.stdout);
        let policy = repomap_index::ScanPolicyKey::default();
        assert_eq!(daemon.memo_len(&root, &policy), 1);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn memo_admission_counts_key_bytes() {
        let root = root();
        write_file(&root, "main.rs", b"let left-right = 1;\n");
        let request = request(&root, LookupOperation::Refs, Some("left-right"));
        let mut probe = Daemon::new();
        let response = probe.dispatch(request.clone());
        let index_bytes = probe
            .roots
            .values()
            .next()
            .and_then(|state| state.index.as_ref())
            .expect("index")
            .logical_bytes();
        let max_logical_bytes = index_bytes
            .saturating_add(response.stdout.len())
            .saturating_add(response.stderr.len());
        let mut daemon = Daemon::with_config(DaemonConfig {
            max_logical_bytes,
            ..DaemonConfig::default()
        });
        let _ = daemon.dispatch(request);
        assert_eq!(
            daemon.memo_len(&root, &repomap_index::ScanPolicyKey::default()),
            0
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn direct_fallback_preserves_error_labels() {
        let root = root();
        let mut daemon = Daemon::with_config(DaemonConfig {
            max_logical_bytes: 1,
            ..DaemonConfig::default()
        });
        let response = daemon.dispatch(request(&root, LookupOperation::Map, None));
        assert_eq!(response.exit_code, 1);
        assert!(response.stderr.starts_with("qc repo map:"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rss_limit_bypasses_and_requests_shutdown() {
        let root = root();
        let mut daemon = Daemon::with_config(DaemonConfig {
            max_rss_bytes: 1,
            ..DaemonConfig::default()
        });
        let response = daemon.dispatch(request(&root, LookupOperation::Map, None));
        assert_eq!(response.status, CacheState::Bypassed);
        assert!(daemon.shutdown);
        fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn startup_socket_is_single_owner_and_stale_socket_recovers() {
        let root = root();
        let paths = DaemonPaths::from_state_dir(root.join("state"));
        let owner = DaemonOwner::acquire(paths.clone()).expect("owner");
        assert!(matches!(
            DaemonOwner::acquire(paths.clone()),
            Err(StartupError::Busy)
        ));
        drop(owner);
        let stale = UnixListener::bind(&paths.socket).expect("stale");
        drop(stale);
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .expect("stale permissions");
        let owner = DaemonOwner::acquire(paths.clone()).expect("recover");
        drop(owner);
        fs::remove_dir_all(root).ok();
    }
}
