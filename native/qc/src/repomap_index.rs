use crate::config::MapConfig;
use crate::outline::{self, Decl, Family};
use ignore::WalkBuilder;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

pub const SOURCE_EXTENSION_REVISION: u32 = 1;
pub const SKIP_DIRECTORY_REVISION: u32 = 1;
pub const PARSER_REVISION: u32 = 1;
pub const MAX_LOGICAL_INDEX_BYTES: usize = 384 * 1024 * 1024;
pub const MAX_IGNORE_DEPENDENCY_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_STABILIZATION_ATTEMPTS: usize = 3;
pub const MAX_STABILIZATION_ATTEMPTS: usize = 3;
pub const STATIC_SKIP_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    "coverage",
];
const SKIP_DIRS: &[&str] = STATIC_SKIP_DIRECTORIES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanConfig {
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_index_bytes: usize,
    pub stabilization_attempts: usize,
    pub source_extension_revision: u32,
    pub skip_directory_revision: u32,
    pub parser_revision: u32,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            max_files: ScanPolicyKey::default().max_files,
            max_file_bytes: ScanPolicyKey::default().max_file_bytes,
            max_index_bytes: MAX_LOGICAL_INDEX_BYTES,
            stabilization_attempts: DEFAULT_STABILIZATION_ATTEMPTS,
            source_extension_revision: SOURCE_EXTENSION_REVISION,
            skip_directory_revision: SKIP_DIRECTORY_REVISION,
            parser_revision: PARSER_REVISION,
        }
    }
}

impl ScanConfig {
    pub fn from_map(config: &MapConfig) -> Self {
        Self {
            max_files: config.max_files,
            max_file_bytes: config.max_file_bytes,
            ..Self::default()
        }
    }

    pub fn policy(&self) -> ScanPolicyKey {
        ScanPolicyKey {
            max_files: self.max_files,
            max_file_bytes: self.max_file_bytes,
            source_extension_revision: self.source_extension_revision,
            skip_directory_revision: self.skip_directory_revision,
            parser_revision: self.parser_revision,
        }
    }

    pub fn index_byte_limit(&self) -> usize {
        self.max_index_bytes.min(MAX_LOGICAL_INDEX_BYTES)
    }

    #[cfg(test)]
    pub fn stabilization_attempt_limit(&self) -> usize {
        self.stabilization_attempts
            .clamp(1, MAX_STABILIZATION_ATTEMPTS)
    }
}

impl From<&MapConfig> for ScanConfig {
    fn from(config: &MapConfig) -> Self {
        Self::from_map(config)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ScanPolicyKey {
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub source_extension_revision: u32,
    pub skip_directory_revision: u32,
    pub parser_revision: u32,
}

impl ScanPolicyKey {
    pub fn new(max_files: usize, max_file_bytes: usize) -> Self {
        Self {
            max_files,
            max_file_bytes,
            source_extension_revision: SOURCE_EXTENSION_REVISION,
            skip_directory_revision: SKIP_DIRECTORY_REVISION,
            parser_revision: PARSER_REVISION,
        }
    }
}

impl Default for ScanPolicyKey {
    fn default() -> Self {
        Self::new(2000, 1024 * 1024)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IgnoreInputKey {
    pub path: PathBuf,
    pub present: bool,
    pub identity: Option<FileIdentity>,
    pub content_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IgnoreContextKey {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub git_config: Vec<(String, String)>,
    pub inputs: Vec<IgnoreInputKey>,
}

impl Default for IgnoreContextKey {
    fn default() -> Self {
        let mut git_config: Vec<_> = std::env::vars()
            .filter(|(key, _)| key.starts_with("GIT_CONFIG_"))
            .collect();
        git_config.sort();
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            git_config,
            inputs: Vec::new(),
        }
    }
}

impl IgnoreContextKey {
    pub fn capture(root: &Path) -> Self {
        let mut inputs: Vec<_> = ignore_inputs(root)
            .into_iter()
            .map(|path| IgnoreInputKey {
                present: path.exists(),
                identity: file_identity(&path),
                content_hash: file_hash(&path),
                path,
            })
            .collect();
        inputs.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            inputs,
            ..Self::default()
        }
    }

    pub fn is_supported(&self) -> bool {
        self.inputs.iter().all(|input| {
            !input.present || (input.identity.is_some() && input.content_hash.is_some())
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RootKey {
    pub canonical_root: PathBuf,
    pub identity: Option<FileIdentity>,
    pub scan_policy: ScanPolicyKey,
    pub ignore_context: IgnoreContextKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathDisposition {
    Included,
    Oversized,
    Binary,
    InvalidUtf8,
    Unreadable,
    Symlink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathDispositionRecord {
    pub relative_path: String,
    pub disposition: PathDisposition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LineMetadata {
    pub line_no: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationPosting {
    pub relative_path: String,
    pub line_no: usize,
    pub rendered: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReferenceCoordinate {
    source_index: u32,
    line_no: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferencePosting {
    pub relative_path: String,
    pub line_no: usize,
    pub line: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MapSummary {
    pub relative_path: String,
    pub line_count: usize,
    pub declaration_count: usize,
    pub names: Vec<String>,
}

#[derive(Clone)]
pub struct SourceRecord {
    pub canonical_path: PathBuf,
    pub relative_path: String,
    pub source: Arc<str>,
    pub lines: Arc<[LineMetadata]>,
    pub family: Family,
    pub declarations: Arc<[DeclarationPosting]>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WalkDiagnostics {
    pub traversal_errors: usize,
    pub read_errors: usize,
    pub symlinks_skipped: usize,
    pub oversized_files: usize,
    pub binary_files: usize,
    pub invalid_utf8_files: usize,
}

impl WalkDiagnostics {
    pub fn has_errors(&self) -> bool {
        self.traversal_errors > 0 || self.read_errors > 0
    }

    pub fn summary(&self, truncated: bool, max_files: usize) -> String {
        let mut parts = Vec::new();
        if self.traversal_errors > 0 {
            parts.push(format!("traversal errors: {}", self.traversal_errors));
        }
        if self.read_errors > 0 {
            parts.push(format!("unreadable files: {}", self.read_errors));
        }
        if self.symlinks_skipped > 0 {
            parts.push(format!("symlinks skipped: {}", self.symlinks_skipped));
        }
        if self.oversized_files > 0 {
            parts.push(format!(
                "oversized files filtered: {}",
                self.oversized_files
            ));
        }
        if self.binary_files > 0 {
            parts.push(format!("binary files filtered: {}", self.binary_files));
        }
        if self.invalid_utf8_files > 0 {
            parts.push(format!(
                "invalid UTF-8 files filtered: {}",
                self.invalid_utf8_files
            ));
        }
        if truncated {
            parts.push(format!("file scan capped at {max_files}"));
        }
        parts.join("; ")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanInputs {
    pub canonical_root: PathBuf,
    pub root_identity: Option<FileIdentity>,
    pub scan_policy: ScanPolicyKey,
    pub ignore_context: IgnoreContextKey,
    pub watched_directories: Vec<PathBuf>,
    pub dependency_parents: Vec<PathBuf>,
}

pub type StabilizedScanInputs = ScanInputs;

impl ScanInputs {
    pub fn capture(root: &Path, config: &ScanConfig) -> Result<Self, ScanError> {
        let canonical_root = std::fs::canonicalize(root).map_err(|_| ScanError::RootUnavailable)?;
        capture_canonical_inputs(canonical_root, config)
    }

    pub fn same_freshness(&self, other: &Self) -> bool {
        self.canonical_root == other.canonical_root
            && self.root_identity == other.root_identity
            && self.scan_policy == other.scan_policy
            && self.ignore_context == other.ignore_context
    }

    pub fn same_watch_set(&self, other: &Self) -> bool {
        self.same_freshness(other)
            && self.watched_directories == other.watched_directories
            && self.dependency_parents == other.dependency_parents
    }
}

#[derive(Clone)]
pub struct SourceIndex {
    root: RootKey,
    files: Arc<[SourceRecord]>,
    path_dispositions: Arc<[PathDispositionRecord]>,
    map_summaries: Arc<[MapSummary]>,
    declarations_by_name: Arc<BTreeMap<String, Arc<[DeclarationPosting]>>>,
    identifier_refs: Arc<BTreeMap<String, Arc<[ReferenceCoordinate]>>>,
    diagnostics: WalkDiagnostics,
    truncated: bool,
    generation: u64,
    logical_bytes: usize,
    scan_inputs: ScanInputs,
}

impl SourceIndex {
    pub fn files(&self) -> &[SourceRecord] {
        &self.files
    }

    #[cfg(test)]
    pub fn path_dispositions(&self) -> &[PathDispositionRecord] {
        &self.path_dispositions
    }

    pub fn map_summaries(&self) -> &[MapSummary] {
        &self.map_summaries
    }

    pub fn declarations(&self, name: &str) -> Option<&[DeclarationPosting]> {
        self.declarations_by_name.get(name).map(AsRef::as_ref)
    }

    pub fn verified_references(&self, query: &str) -> Option<Vec<ReferencePosting>> {
        if !identifier_chars(query) {
            return None;
        }
        let postings = self.identifier_refs.get(query)?;
        let regex = reference_regex(query)?;
        Some(
            postings
                .iter()
                .filter_map(|posting| {
                    let source_index = usize::try_from(posting.source_index).ok()?;
                    let file = self.files.get(source_index)?;
                    let line_no = usize::try_from(posting.line_no).ok()?;
                    let line = source_line(file, line_no)?;
                    regex.is_match(line).then(|| ReferencePosting {
                        relative_path: file.relative_path.clone(),
                        line_no,
                        line: line.to_owned(),
                    })
                })
                .collect(),
        )
    }

    pub fn diagnostics(&self) -> &WalkDiagnostics {
        &self.diagnostics
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn logical_bytes(&self) -> usize {
        self.logical_bytes
    }

    #[cfg(test)]
    pub fn scan_inputs(&self) -> &ScanInputs {
        &self.scan_inputs
    }

    pub fn canonical_root(&self) -> &Path {
        &self.root.canonical_root
    }

    pub fn policy(&self) -> &ScanPolicyKey {
        &self.root.scan_policy
    }

    pub fn ignore_context(&self) -> &IgnoreContextKey {
        &self.root.ignore_context
    }

    pub fn root_identity_matches(&self) -> bool {
        file_identity(&self.root.canonical_root) == self.root.identity
            && std::fs::metadata(&self.root.canonical_root)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false)
    }

    pub fn scan_note(&self) -> String {
        let summary = self
            .diagnostics
            .summary(self.truncated, self.root.scan_policy.max_files);
        if summary.is_empty() {
            String::new()
        } else {
            format!("[qc-scan: partial — {summary}]\n")
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathChange {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
    IgnoreChanged(PathBuf),
    DirectoryChanged(PathBuf),
    Error,
    RootReplaced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileReason {
    PolicyChanged,
    IgnoreContextChanged,
    RootIdentityChanged,
    AmbiguousChange,
    TruncatedMembership,
    RefreshFailed,
}

#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
pub enum RefreshOutcome {
    Refreshed(SourceIndex),
    NeedsReconcile(ReconcileReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScanError {
    RootUnavailable,
    RootNotDirectory,
    PolicyMismatch,
    IgnoreContextMismatch,
    UnsupportedIgnoreContext,
    TooLarge,
    BoundsExceeded {
        max_bytes: usize,
        observed_bytes: usize,
    },
    #[cfg(test)]
    Unstable {
        attempts: usize,
    },
    Incomplete {
        diagnostics: WalkDiagnostics,
        truncated: bool,
        max_files: usize,
    },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RootUnavailable => write!(f, "scan root is unavailable"),
            Self::RootNotDirectory => write!(f, "scan root is not a directory"),
            Self::PolicyMismatch => write!(f, "scan policy mismatch"),
            Self::IgnoreContextMismatch => write!(f, "ignore context mismatch"),
            Self::UnsupportedIgnoreContext => write!(f, "ignore context is unsupported"),
            Self::TooLarge => write!(f, "source index exceeds logical byte bound"),
            Self::BoundsExceeded {
                max_bytes,
                observed_bytes,
            } => write!(
                f,
                "source index exceeds {max_bytes} bytes ({observed_bytes} observed)"
            ),
            #[cfg(test)]
            Self::Unstable { attempts } => {
                write!(f, "scan remained unstable after {attempts} attempts")
            }
            Self::Incomplete {
                diagnostics,
                truncated,
                max_files,
            } => write!(
                f,
                "scan incomplete: {}",
                diagnostics.summary(*truncated, *max_files)
            ),
        }
    }
}

impl std::error::Error for ScanError {}

fn canonical_dependency_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn insert_dependency(paths: &mut BTreeSet<PathBuf>, path: PathBuf) {
    paths.insert(canonical_dependency_path(path));
}

fn config_path_value(value: &str, parent: &Path) -> Option<PathBuf> {
    let mut value = value.trim();
    if value.is_empty() || value.starts_with('%') {
        return None;
    }
    if let Some(unquoted) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        value = unquoted;
    } else if let Some(unquoted) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        value = unquoted;
    }
    if let Some(home) = std::env::var_os("HOME") {
        if value == "~" {
            return Some(PathBuf::from(home));
        }
        if let Some(rest) = value.strip_prefix("~/") {
            return Some(PathBuf::from(home).join(rest));
        }
    }
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        parent.join(path)
    })
}

fn config_dependencies(path: &Path) -> Vec<PathBuf> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) if bytes.len() <= MAX_IGNORE_DEPENDENCY_BYTES => bytes,
        _ => return Vec::new(),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let mut section = String::new();
    let mut paths = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            section = name
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let include = key == "path"
            && (section == "include" || section == "includeif" || section.starts_with("include"));
        let excludes = key == "excludesfile" && section == "core";
        if (include || excludes) && value.len() <= MAX_IGNORE_DEPENDENCY_BYTES {
            if let Some(path) = config_path_value(value, path.parent().unwrap_or(Path::new("."))) {
                paths.push(path);
            }
        }
    }
    paths
}

fn git_dependency_inputs(root: &Path, paths: &mut BTreeSet<PathBuf>) {
    let git = root.join(".git");
    let Ok(metadata) = std::fs::symlink_metadata(&git) else {
        insert_dependency(paths, git);
        return;
    };
    if metadata.is_file() {
        insert_dependency(paths, git.clone());
        let Ok(bytes) = std::fs::read(&git) else {
            return;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return;
        };
        let Some(value) = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("gitdir:")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        }) else {
            return;
        };
        let gitdir = PathBuf::from(value);
        let gitdir = if gitdir.is_absolute() {
            gitdir
        } else {
            git.parent().unwrap_or(root).join(gitdir)
        };
        insert_dependency(paths, gitdir.join("config"));
        insert_dependency(paths, gitdir.join("info/exclude"));
        let commondir = gitdir.join("commondir");
        insert_dependency(paths, commondir.clone());
        if let Ok(common) = std::fs::read_to_string(&commondir) {
            let common = PathBuf::from(common.trim());
            let common = if common.is_absolute() {
                common
            } else {
                gitdir.join(common)
            };
            insert_dependency(paths, common.join("config"));
            insert_dependency(paths, common.join("info/exclude"));
        }
        return;
    }
    if metadata.is_dir() {
        insert_dependency(paths, git.join("config"));
        insert_dependency(paths, git.join("info/exclude"));
        let commondir = git.join("commondir");
        insert_dependency(paths, commondir.clone());
        if let Ok(common) = std::fs::read_to_string(&commondir) {
            let common = PathBuf::from(common.trim());
            let common = if common.is_absolute() {
                common
            } else {
                git.join(common)
            };
            insert_dependency(paths, common.join("config"));
            insert_dependency(paths, common.join("info/exclude"));
        }
    }
}

fn ignore_inputs(root: &Path) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut current = Some(root);
    while let Some(dir) = current {
        insert_dependency(&mut paths, dir.join(".gitignore"));
        insert_dependency(&mut paths, dir.join(".ignore"));
        current = dir.parent();
    }
    git_dependency_inputs(root, &mut paths);
    if let Some(path) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        insert_dependency(&mut paths, PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("GIT_CONFIG_SYSTEM") {
        insert_dependency(&mut paths, PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(path);
        insert_dependency(&mut paths, path.join("git/ignore"));
        insert_dependency(&mut paths, path.join("git/config"));
    } else if let Some(path) = std::env::var_os("HOME") {
        let path = PathBuf::from(path);
        insert_dependency(&mut paths, path.join(".config/git/ignore"));
        insert_dependency(&mut paths, path.join(".config/git/config"));
    }
    if let Some(path) = std::env::var_os("HOME") {
        insert_dependency(&mut paths, PathBuf::from(path).join(".gitconfig"));
    }

    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|ty| {
                    ty.is_dir()
                        && (entry.file_name() == ".git"
                            || SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()))
                })
        });
    for entry in walker.build().flatten() {
        if entry.file_type().is_some_and(|ty| ty.is_file())
            && matches!(entry.file_name().to_str(), Some(".gitignore" | ".ignore"))
        {
            insert_dependency(&mut paths, entry.into_path());
        }
    }

    let mut queue: Vec<PathBuf> = paths.iter().cloned().collect();
    let mut seen_configs = BTreeSet::new();
    while let Some(path) = queue.pop() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if !(name == "config" || name == ".gitconfig" || name.ends_with(".config"))
            || !seen_configs.insert(path.clone())
        {
            continue;
        }
        for dependency in config_dependencies(&path) {
            let dependency = canonical_dependency_path(dependency);
            if paths.insert(dependency.clone()) {
                queue.push(dependency);
            }
        }
    }
    paths.into_iter().collect()
}

fn file_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    identity_from_metadata(&metadata)
}

fn file_hash(path: &Path) -> Option<[u8; 32]> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_IGNORE_DEPENDENCY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_IGNORE_DEPENDENCY_BYTES {
        return None;
    }
    Some(Sha256::digest(bytes).into())
}

fn identity_from_metadata(metadata: &std::fs::Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn normalize_paths(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}

fn capture_canonical_inputs(
    canonical_root: PathBuf,
    config: &ScanConfig,
) -> Result<ScanInputs, ScanError> {
    let metadata = std::fs::metadata(&canonical_root).map_err(|_| ScanError::RootUnavailable)?;
    if !metadata.is_dir() {
        return Err(ScanError::RootNotDirectory);
    }
    let ignore_context = IgnoreContextKey::capture(&canonical_root);
    if !ignore_context.is_supported() {
        return Err(ScanError::UnsupportedIgnoreContext);
    }
    let mut watched_directories = vec![canonical_root.clone()];
    if let Some(parent) = canonical_root.parent() {
        watched_directories.push(parent.to_path_buf());
    }
    normalize_paths(&mut watched_directories);
    let mut dependency_parents: Vec<_> = ignore_context
        .inputs
        .iter()
        .filter_map(|input| input.path.parent().map(Path::to_path_buf))
        .collect();
    normalize_paths(&mut dependency_parents);
    Ok(ScanInputs {
        canonical_root,
        root_identity: identity_from_metadata(&metadata),
        scan_policy: config.policy(),
        ignore_context,
        watched_directories,
        dependency_parents,
    })
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        File::open(path)
    }
}

enum ReadSourceError {
    Disposition(PathDisposition),
    Bounds(usize),
}

fn read_source(
    path: &Path,
    max_bytes: usize,
    max_index_bytes: usize,
) -> Result<Result<String, PathDisposition>, ReadSourceError> {
    let file = open_no_follow(path)
        .map_err(|_| ReadSourceError::Disposition(PathDisposition::Unreadable))?;
    let metadata = file
        .metadata()
        .map_err(|_| ReadSourceError::Disposition(PathDisposition::Unreadable))?;
    if !metadata.is_file() {
        return Err(ReadSourceError::Disposition(PathDisposition::Unreadable));
    }
    let limit = max_bytes.min(max_index_bytes).saturating_add(1);
    let mut bytes = Vec::with_capacity(limit.min(8192));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|_| ReadSourceError::Disposition(PathDisposition::Unreadable))?;
    if bytes.len() > max_index_bytes {
        return Err(ReadSourceError::Bounds(bytes.len()));
    }
    if bytes.len() > max_bytes {
        return Ok(Err(PathDisposition::Oversized));
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Ok(Err(PathDisposition::Binary));
    }
    Ok(match String::from_utf8(bytes) {
        Ok(source) => Ok(source),
        Err(_) => Err(PathDisposition::InvalidUtf8),
    })
}

fn lines(source: &str) -> Vec<LineMetadata> {
    let mut out = Vec::new();
    let mut start = 0;
    for (line_no, line) in source.split_inclusive('\n').enumerate() {
        let end = start + line.trim_end_matches('\n').len();
        out.push(LineMetadata {
            line_no: line_no + 1,
            start,
            end,
        });
        start += line.len();
    }
    if !source.is_empty() && !source.ends_with('\n') && out.is_empty() {
        out.push(LineMetadata {
            line_no: 1,
            start: 0,
            end: source.len(),
        });
    }
    out
}

fn source_line(file: &SourceRecord, line_no: usize) -> Option<&str> {
    let metadata = file.lines.get(line_no.checked_sub(1)?)?;
    let line = file.source.get(metadata.start..metadata.end)?;
    if file.source.as_bytes().get(metadata.end) == Some(&b'\n') {
        Some(line.strip_suffix('\r').unwrap_or(line))
    } else {
        Some(line)
    }
}

fn identifier_chars(query: &str) -> bool {
    !query.is_empty()
        && query
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

fn identifier_tokens(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, byte) in line.bytes().enumerate() {
        let valid = byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
        match (start, valid) {
            (None, true) => start = Some(i),
            (Some(begin), false) => {
                out.push(line[begin..i].to_owned());
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push(line[begin..].to_owned());
    }
    out
}

fn reference_regex(query: &str) -> Option<Regex> {
    if !identifier_chars(query) {
        return None;
    }
    Regex::new(&format!(
        r"(?:^|[^$A-Za-z0-9_]){}(?:$|[^$A-Za-z0-9_])",
        regex::escape(query)
    ))
    .ok()
}

fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn event_path(root: &Path, path: &Path) -> Option<(PathBuf, String)> {
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let relative = relative_path(root, &candidate)?;
    Some((root.join(Path::new(&relative)), relative))
}

fn source_record(
    canonical_root: &Path,
    path: PathBuf,
    source: String,
) -> (SourceRecord, PathDispositionRecord) {
    let relative_path = relative_path(canonical_root, &path).unwrap_or_default();
    let family = outline::family_of(&path);
    let declarations = outline::decls_in(&source, family);
    let postings = declarations
        .iter()
        .map(|decl| DeclarationPosting {
            relative_path: relative_path.clone(),
            line_no: decl.line_no,
            rendered: decl.rendered.clone(),
        })
        .collect::<Vec<_>>();
    (
        SourceRecord {
            canonical_path: path,
            relative_path: relative_path.clone(),
            lines: lines(&source).into(),
            source: Arc::from(source),
            family,
            declarations: postings.into(),
        },
        PathDispositionRecord {
            relative_path,
            disposition: PathDisposition::Included,
        },
    )
}

fn index_walker(root: &Path) -> WalkBuilder {
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|ty| {
                    ty.is_dir() && SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
                })
        });
    walker
}

fn add_watch_directory(watched: &mut BTreeSet<PathBuf>, path: &Path) {
    watched.insert(path.to_path_buf());
}

fn build_index(
    inputs: &ScanInputs,
    config: &ScanConfig,
    generation: u64,
) -> Result<SourceIndex, ScanError> {
    let metadata =
        std::fs::metadata(&inputs.canonical_root).map_err(|_| ScanError::RootUnavailable)?;
    if !metadata.is_dir() {
        return Err(ScanError::RootNotDirectory);
    }
    if inputs.scan_policy != config.policy() {
        return Err(ScanError::PolicyMismatch);
    }
    if inputs.ignore_context != IgnoreContextKey::capture(&inputs.canonical_root) {
        return Err(ScanError::IgnoreContextMismatch);
    }
    if !inputs.ignore_context.is_supported() {
        return Err(ScanError::UnsupportedIgnoreContext);
    }

    let mut diagnostics = WalkDiagnostics::default();
    let mut files = Vec::new();
    let mut dispositions = Vec::new();
    let mut watched: BTreeSet<PathBuf> = inputs.watched_directories.iter().cloned().collect();
    let walker = index_walker(&inputs.canonical_root);
    let mut truncated = false;
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                diagnostics.traversal_errors += 1;
                continue;
            }
        };
        if entry
            .file_type()
            .is_some_and(|ty| ty.is_dir() && !ty.is_symlink())
        {
            add_watch_directory(&mut watched, entry.path());
        }
        let rel = entry
            .path()
            .strip_prefix(&inputs.canonical_root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if entry.file_type().is_some_and(|ty| ty.is_symlink()) {
            diagnostics.symlinks_skipped += 1;
            dispositions.push(PathDispositionRecord {
                relative_path: rel,
                disposition: PathDisposition::Symlink,
            });
            continue;
        }
        if !entry.file_type().is_some_and(|ty| ty.is_file())
            || !outline::is_source_ext(entry.path())
        {
            continue;
        }
        if files.len() >= inputs.scan_policy.max_files {
            truncated = true;
            break;
        }
        match read_source(
            entry.path(),
            inputs.scan_policy.max_file_bytes,
            config.index_byte_limit(),
        ) {
            Ok(Ok(source)) => {
                let path = entry.into_path();
                let family = outline::family_of(&path);
                let declarations = outline::decls_in(&source, family);
                let postings: Vec<_> = declarations
                    .iter()
                    .map(|decl| DeclarationPosting {
                        relative_path: rel.clone(),
                        line_no: decl.line_no,
                        rendered: decl.rendered.clone(),
                    })
                    .collect();
                files.push(SourceRecord {
                    canonical_path: path,
                    relative_path: rel.clone(),
                    lines: lines(&source).into(),
                    source: Arc::from(source),
                    family,
                    declarations: postings.into(),
                });
                dispositions.push(PathDispositionRecord {
                    relative_path: rel,
                    disposition: PathDisposition::Included,
                });
            }
            Ok(Err(disposition)) => {
                match disposition {
                    PathDisposition::Oversized => diagnostics.oversized_files += 1,
                    PathDisposition::Binary => diagnostics.binary_files += 1,
                    PathDisposition::InvalidUtf8 => diagnostics.invalid_utf8_files += 1,
                    _ => {}
                }
                dispositions.push(PathDispositionRecord {
                    relative_path: rel,
                    disposition,
                });
            }
            Err(ReadSourceError::Disposition(disposition)) => {
                diagnostics.read_errors += 1;
                dispositions.push(PathDispositionRecord {
                    relative_path: rel,
                    disposition,
                });
            }
            Err(ReadSourceError::Bounds(observed_bytes)) => {
                return Err(bounds_error(config, observed_bytes));
            }
        }
    }
    let mut scan_inputs = inputs.clone();
    scan_inputs.watched_directories = watched.into_iter().collect();
    scan_inputs.dependency_parents = dependency_parents(&scan_inputs.ignore_context);
    if files.is_empty() && (truncated || diagnostics.has_errors()) {
        return Err(ScanError::Incomplete {
            diagnostics,
            truncated,
            max_files: inputs.scan_policy.max_files,
        });
    }
    assemble_index(
        scan_inputs,
        generation,
        files,
        dispositions,
        diagnostics,
        truncated,
        config.index_byte_limit(),
    )
}

fn dependency_parents(context: &IgnoreContextKey) -> Vec<PathBuf> {
    let mut paths: Vec<_> = context
        .inputs
        .iter()
        .filter_map(|input| input.path.parent().map(Path::to_path_buf))
        .collect();
    normalize_paths(&mut paths);
    paths
}

fn size_add(total: &mut usize, amount: usize) {
    *total = total.saturating_add(amount);
}

fn reference_index_logical_bytes(
    identifier_refs: &BTreeMap<String, Vec<ReferenceCoordinate>>,
) -> usize {
    let mut bytes = std::mem::size_of::<BTreeMap<String, Arc<[ReferenceCoordinate]>>>();
    for (token, postings) in identifier_refs {
        size_add(
            &mut bytes,
            std::mem::size_of::<(String, Arc<[ReferenceCoordinate]>)>(),
        );
        size_add(&mut bytes, token.capacity());
        size_add(&mut bytes, std::mem::size_of::<[usize; 2]>());
        size_add(
            &mut bytes,
            postings
                .capacity()
                .saturating_mul(std::mem::size_of::<ReferenceCoordinate>()),
        );
    }
    bytes
}

fn context_logical_bytes(context: &IgnoreContextKey) -> usize {
    let mut bytes = context
        .home
        .as_ref()
        .map_or(0, |path| path.as_os_str().len());
    size_add(
        &mut bytes,
        context
            .xdg_config_home
            .as_ref()
            .map_or(0, |path| path.as_os_str().len()),
    );
    for (key, value) in &context.git_config {
        size_add(&mut bytes, key.len().saturating_add(value.len()));
    }
    for input in &context.inputs {
        size_add(&mut bytes, input.path.as_os_str().len());
        size_add(&mut bytes, std::mem::size_of::<IgnoreInputKey>());
    }
    bytes
}

fn assemble_index(
    scan_inputs: ScanInputs,
    generation: u64,
    mut files: Vec<SourceRecord>,
    mut dispositions: Vec<PathDispositionRecord>,
    diagnostics: WalkDiagnostics,
    truncated: bool,
    max_index_bytes: usize,
) -> Result<SourceIndex, ScanError> {
    if files.is_empty() && (truncated || diagnostics.has_errors()) {
        return Err(ScanError::Incomplete {
            diagnostics,
            truncated,
            max_files: scan_inputs.scan_policy.max_files,
        });
    }
    files.sort_by(|a, b| {
        Path::new(&a.relative_path)
            .components()
            .cmp(Path::new(&b.relative_path).components())
    });
    dispositions.sort_by(|a, b| {
        Path::new(&a.relative_path)
            .components()
            .cmp(Path::new(&b.relative_path).components())
    });
    let mut map_summaries = Vec::with_capacity(files.len());
    let mut declarations_by_name: BTreeMap<String, Vec<DeclarationPosting>> = BTreeMap::new();
    let mut identifier_refs: BTreeMap<String, Vec<ReferenceCoordinate>> = BTreeMap::new();
    let mut logical_bytes = std::mem::size_of::<SourceIndex>();
    size_add(
        &mut logical_bytes,
        scan_inputs.canonical_root.as_os_str().len(),
    );
    size_add(
        &mut logical_bytes,
        context_logical_bytes(&scan_inputs.ignore_context),
    );
    for path in &scan_inputs.watched_directories {
        size_add(&mut logical_bytes, path.as_os_str().len());
    }
    for path in &scan_inputs.dependency_parents {
        size_add(&mut logical_bytes, path.as_os_str().len());
    }
    for (source_index, file) in files.iter().enumerate() {
        let source_index = u32::try_from(source_index).map_err(|_| ScanError::TooLarge)?;
        let declarations: Vec<Decl> = outline::decls_in(&file.source, file.family);
        let names: Vec<String> = declarations
            .iter()
            .filter_map(|decl| decl.name.clone())
            .collect();
        let mut summary_bytes = file
            .relative_path
            .len()
            .saturating_add(std::mem::size_of::<MapSummary>())
            .saturating_add(std::mem::size_of_val(file.source.as_ref()));
        for name in &names {
            size_add(&mut summary_bytes, name.len());
        }
        size_add(&mut logical_bytes, summary_bytes);
        map_summaries.push(MapSummary {
            relative_path: file.relative_path.clone(),
            line_count: file.source.lines().count(),
            declaration_count: declarations.len(),
            names,
        });
        for (posting, declaration) in file.declarations.iter().zip(declarations.iter()) {
            if let Some(name) = declaration.name.clone() {
                declarations_by_name
                    .entry(name)
                    .or_default()
                    .push(posting.clone());
            }
        }
        for line_metadata in file.lines.iter() {
            let Some(line) = source_line(file, line_metadata.line_no) else {
                continue;
            };
            let tokens: BTreeSet<String> = identifier_tokens(line).into_iter().collect();
            for token in tokens {
                let line_no =
                    u32::try_from(line_metadata.line_no).map_err(|_| ScanError::TooLarge)?;
                identifier_refs
                    .entry(token)
                    .or_default()
                    .push(ReferenceCoordinate {
                        source_index,
                        line_no,
                    });
            }
        }
        size_add(&mut logical_bytes, file.source.len());
        size_add(&mut logical_bytes, file.canonical_path.as_os_str().len());
        size_add(&mut logical_bytes, file.relative_path.len());
        size_add(
            &mut logical_bytes,
            file.lines
                .len()
                .saturating_mul(std::mem::size_of::<LineMetadata>()),
        );
        for posting in file.declarations.iter() {
            size_add(
                &mut logical_bytes,
                posting
                    .relative_path
                    .len()
                    .saturating_add(posting.rendered.len())
                    .saturating_add(std::mem::size_of::<DeclarationPosting>()),
            );
        }
    }
    for record in &dispositions {
        size_add(
            &mut logical_bytes,
            record
                .relative_path
                .len()
                .saturating_add(std::mem::size_of::<PathDispositionRecord>()),
        );
    }
    size_add(
        &mut logical_bytes,
        reference_index_logical_bytes(&identifier_refs),
    );
    if logical_bytes > MAX_LOGICAL_INDEX_BYTES {
        return Err(ScanError::TooLarge);
    }
    if logical_bytes > max_index_bytes {
        return Err(ScanError::BoundsExceeded {
            max_bytes: max_index_bytes,
            observed_bytes: logical_bytes,
        });
    }
    let declarations_by_name = declarations_by_name
        .into_iter()
        .map(|(name, postings)| (name, postings.into()))
        .collect();
    let identifier_refs = identifier_refs
        .into_iter()
        .map(|(token, postings)| (token, postings.into()))
        .collect();
    let root = RootKey {
        canonical_root: scan_inputs.canonical_root.clone(),
        identity: scan_inputs.root_identity,
        scan_policy: scan_inputs.scan_policy.clone(),
        ignore_context: scan_inputs.ignore_context.clone(),
    };
    Ok(SourceIndex {
        root,
        files: files.into(),
        path_dispositions: dispositions.into(),
        map_summaries: map_summaries.into(),
        declarations_by_name: Arc::new(declarations_by_name),
        identifier_refs: Arc::new(identifier_refs),
        diagnostics,
        truncated,
        generation,
        logical_bytes,
        scan_inputs,
    })
}

fn bounds_error(config: &ScanConfig, observed_bytes: usize) -> ScanError {
    if config.index_byte_limit() == MAX_LOGICAL_INDEX_BYTES {
        ScanError::TooLarge
    } else {
        ScanError::BoundsExceeded {
            max_bytes: config.index_byte_limit(),
            observed_bytes,
        }
    }
}

pub fn scan_inputs(root: &Path, config: &ScanConfig) -> Result<StabilizedScanInputs, ScanError> {
    ScanInputs::capture(root, config)
}

#[cfg(test)]
pub fn scan_root(root: &Path, config: &ScanConfig) -> Result<SourceIndex, ScanError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|_| ScanError::RootUnavailable)?;
    let attempts = config.stabilization_attempt_limit();
    for _ in 0..attempts {
        let before = capture_canonical_inputs(canonical_root.clone(), config)?;
        let index = build_index(&before, config, 1)?;
        let after = capture_canonical_inputs(canonical_root.clone(), config)?;
        if before.same_freshness(&after) {
            return Ok(index);
        }
    }
    Err(ScanError::Unstable { attempts })
}

pub fn build_generation(
    inputs: &ScanInputs,
    config: &ScanConfig,
    generation: u64,
) -> Result<SourceIndex, ScanError> {
    if inputs.scan_policy != config.policy() {
        return Err(ScanError::PolicyMismatch);
    }
    build_index(inputs, config, generation)
}

enum VisiblePath {
    Source,
    Symlink,
    NotVisible,
    Directory,
    Unknown,
}

fn visible_path(root: &Path, target: &Path) -> VisiblePath {
    let walker = index_walker(root);
    for entry in walker.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return VisiblePath::Unknown,
        };
        if entry.path() != target {
            continue;
        }
        if entry.file_type().is_some_and(|ty| ty.is_symlink()) {
            return VisiblePath::Symlink;
        }
        if entry.file_type().is_some_and(|ty| ty.is_dir()) {
            return VisiblePath::Directory;
        }
        if entry.file_type().is_some_and(|ty| ty.is_file()) && outline::is_source_ext(entry.path())
        {
            return VisiblePath::Source;
        }
        return VisiblePath::NotVisible;
    }
    VisiblePath::NotVisible
}

fn is_ignore_dependency(index: &SourceIndex, path: &Path) -> bool {
    index
        .ignore_context()
        .inputs
        .iter()
        .any(|input| input.path == path)
        || path.components().any(|component| {
            component.as_os_str() == ".gitignore" || component.as_os_str() == ".ignore"
        })
}

fn insert_changed_path(
    index: &SourceIndex,
    config: &ScanConfig,
    path: PathBuf,
    relative: String,
    files: &mut BTreeMap<String, SourceRecord>,
    dispositions: &mut BTreeMap<String, PathDispositionRecord>,
) -> Result<bool, ReconcileReason> {
    let old_known = dispositions.contains_key(&relative);
    let old_included = files.contains_key(&relative);
    match visible_path(index.canonical_root(), &path) {
        VisiblePath::NotVisible => {
            files.remove(&relative);
            dispositions.remove(&relative);
            Ok(old_known || old_included)
        }
        VisiblePath::Symlink => {
            files.remove(&relative);
            dispositions.insert(
                relative.clone(),
                PathDispositionRecord {
                    relative_path: relative,
                    disposition: PathDisposition::Symlink,
                },
            );
            Ok(!old_known || old_included)
        }
        VisiblePath::Source => {
            match read_source(&path, config.max_file_bytes, config.index_byte_limit()) {
                Ok(Ok(source)) => {
                    let (record, disposition) = source_record(index.canonical_root(), path, source);
                    files.insert(relative.clone(), record);
                    dispositions.insert(relative, disposition);
                    Ok(!old_included)
                }
                Ok(Err(disposition)) => {
                    files.remove(&relative);
                    dispositions.insert(
                        relative.clone(),
                        PathDispositionRecord {
                            relative_path: relative,
                            disposition,
                        },
                    );
                    Ok(old_included)
                }
                Err(ReadSourceError::Disposition(disposition)) => {
                    files.remove(&relative);
                    dispositions.insert(
                        relative.clone(),
                        PathDispositionRecord {
                            relative_path: relative,
                            disposition,
                        },
                    );
                    Ok(old_included)
                }
                Err(ReadSourceError::Bounds(_)) => Err(ReconcileReason::RefreshFailed),
            }
        }
        VisiblePath::Directory | VisiblePath::Unknown => Err(ReconcileReason::AmbiguousChange),
    }
}

fn refresh_replacement_impl(
    index: &SourceIndex,
    changes: &[PathChange],
    config: &ScanConfig,
) -> RefreshOutcome {
    if index.policy() != &config.policy() {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::PolicyChanged);
    }
    if config.index_byte_limit() < index.logical_bytes() {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::PolicyChanged);
    }
    if !index.root_identity_matches() {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::RootIdentityChanged);
    }
    let context = IgnoreContextKey::capture(index.canonical_root());
    if !context.is_supported() {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
    }
    if context != *index.ignore_context() {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
    }
    if changes.is_empty() {
        return RefreshOutcome::Refreshed(index.clone());
    }
    if changes
        .iter()
        .any(|change| matches!(change, PathChange::Error | PathChange::DirectoryChanged(_)))
    {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange);
    }
    if changes
        .iter()
        .any(|change| matches!(change, PathChange::RootReplaced))
    {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::RootIdentityChanged);
    }
    if changes.iter().any(|change| {
        matches!(change, PathChange::IgnoreChanged(path) if is_ignore_dependency(index, path))
    }) {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
    }
    if index.truncated
        && changes.iter().any(|change| {
            matches!(
                change,
                PathChange::Created(_) | PathChange::Removed(_) | PathChange::Renamed { .. }
            )
        })
    {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::TruncatedMembership);
    }

    let mut files: BTreeMap<String, SourceRecord> = index
        .files
        .iter()
        .cloned()
        .map(|file| (file.relative_path.clone(), file))
        .collect();
    let mut dispositions: BTreeMap<String, PathDispositionRecord> = index
        .path_dispositions
        .iter()
        .cloned()
        .map(|record| (record.relative_path.clone(), record))
        .collect();
    let mut membership_changed = false;

    for change in changes {
        match change {
            PathChange::Created(path) | PathChange::Modified(path) => {
                let Some((canonical, relative)) = event_path(index.canonical_root(), path) else {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange);
                };
                if is_ignore_dependency(index, &canonical) {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
                }
                match insert_changed_path(
                    index,
                    config,
                    canonical,
                    relative,
                    &mut files,
                    &mut dispositions,
                ) {
                    Ok(changed) => membership_changed |= changed,
                    Err(reason) => return RefreshOutcome::NeedsReconcile(reason),
                }
            }
            PathChange::Removed(path) => {
                let Some((_, relative)) = event_path(index.canonical_root(), path) else {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange);
                };
                if is_ignore_dependency(index, path) {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
                }
                membership_changed |= files.remove(&relative).is_some();
                membership_changed |= dispositions.remove(&relative).is_some();
            }
            PathChange::Renamed { from, to } => {
                let Some((from_path, from_relative)) = event_path(index.canonical_root(), from)
                else {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange);
                };
                let Some((to_path, to_relative)) = event_path(index.canonical_root(), to) else {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange);
                };
                if is_ignore_dependency(index, &from_path) || is_ignore_dependency(index, &to_path)
                {
                    return RefreshOutcome::NeedsReconcile(ReconcileReason::IgnoreContextChanged);
                }
                membership_changed |= files.remove(&from_relative).is_some();
                membership_changed |= dispositions.remove(&from_relative).is_some();
                match insert_changed_path(
                    index,
                    config,
                    to_path,
                    to_relative,
                    &mut files,
                    &mut dispositions,
                ) {
                    Ok(changed) => membership_changed |= changed,
                    Err(reason) => return RefreshOutcome::NeedsReconcile(reason),
                }
            }
            PathChange::IgnoreChanged(_) => {
                return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange)
            }
            PathChange::Error | PathChange::DirectoryChanged(_) | PathChange::RootReplaced => {
                return RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange)
            }
        }
    }
    if index.truncated && membership_changed {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::TruncatedMembership);
    }
    if files.len() > config.max_files {
        return RefreshOutcome::NeedsReconcile(ReconcileReason::TruncatedMembership);
    }
    let diagnostics =
        diagnostics_from_dispositions(index.diagnostics.traversal_errors, &dispositions);
    let mut scan_inputs = index.scan_inputs.clone();
    scan_inputs.root_identity = file_identity(index.canonical_root());
    match assemble_index(
        scan_inputs,
        index.generation.saturating_add(1),
        files.into_values().collect(),
        dispositions.into_values().collect(),
        diagnostics,
        index.truncated,
        config.index_byte_limit(),
    ) {
        Ok(replacement) => RefreshOutcome::Refreshed(replacement),
        Err(_) => RefreshOutcome::NeedsReconcile(ReconcileReason::RefreshFailed),
    }
}

fn diagnostics_from_dispositions(
    traversal_errors: usize,
    dispositions: &BTreeMap<String, PathDispositionRecord>,
) -> WalkDiagnostics {
    let mut diagnostics = WalkDiagnostics {
        traversal_errors,
        ..WalkDiagnostics::default()
    };
    for record in dispositions.values() {
        match record.disposition {
            PathDisposition::Included => {}
            PathDisposition::Oversized => diagnostics.oversized_files += 1,
            PathDisposition::Binary => diagnostics.binary_files += 1,
            PathDisposition::InvalidUtf8 => diagnostics.invalid_utf8_files += 1,
            PathDisposition::Unreadable => diagnostics.read_errors += 1,
            PathDisposition::Symlink => diagnostics.symlinks_skipped += 1,
        }
    }
    diagnostics
}

pub fn refresh_replacement(
    index: &SourceIndex,
    changes: &[PathChange],
    config: &ScanConfig,
) -> RefreshOutcome {
    refresh_replacement_impl(index, changes, config)
}

#[cfg(test)]
pub fn refresh_paths(
    index: &mut SourceIndex,
    changes: &[PathChange],
    config: &ScanConfig,
) -> RefreshOutcome {
    let outcome = refresh_replacement_impl(index, changes, config);
    if let RefreshOutcome::Refreshed(replacement) = &outcome {
        *index = replacement.clone();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("qc-index-{suffix}"));
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    fn write_file(root: &Path, relative: &str, source: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
        std::fs::File::create(path)
            .and_then(|mut file| file.write_all(source))
            .expect("write");
    }

    fn config(max_files: usize, max_file_bytes: usize) -> ScanConfig {
        ScanConfig {
            max_files,
            max_file_bytes,
            ..ScanConfig::default()
        }
    }

    #[test]
    fn scan_builds_ordered_immutable_postings() {
        let root = temp_root();
        write_file(&root, "z.rs", b"pub fn target() {}\nlet x = target;\n");
        write_file(&root, "a.rs", b"pub fn target() {}\n");
        let index = scan_root(&root, &config(10, 1024)).expect("scan");
        assert_eq!(
            index
                .files()
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["a.rs", "z.rs"]
        );
        assert_eq!(index.declarations("target").expect("decls").len(), 2);
        assert_eq!(index.verified_references("target").expect("refs").len(), 3);
        assert_eq!(index.generation(), 1);
        assert!(index.logical_bytes() >= 45);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn dispositions_preserve_filtered_file_reasons() {
        let root = temp_root();
        write_file(&root, "large.rs", b"0123456789");
        write_file(&root, "binary.rs", b"x\0y");
        write_file(&root, "bad.rs", &[0xff, 0xfe]);
        let index = scan_root(&root, &config(10, 4)).expect("scan");
        assert!(index
            .path_dispositions()
            .iter()
            .any(|record| record.disposition == PathDisposition::Oversized));
        assert!(index
            .path_dispositions()
            .iter()
            .any(|record| record.disposition == PathDisposition::Binary));
        assert!(index
            .path_dispositions()
            .iter()
            .any(|record| record.disposition == PathDisposition::InvalidUtf8));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refresh_publishes_replacement_without_mutating_old_generation() {
        let root = temp_root();
        write_file(&root, "main.rs", b"pub fn old() {}\n");
        let index = scan_root(&root, &config(10, 1024)).expect("scan");
        write_file(&root, "main.rs", b"pub fn new_name() {}\n");
        let outcome = refresh_replacement(
            &index,
            &[PathChange::Modified(root.join("main.rs"))],
            &config(10, 1024),
        );
        let replacement = match outcome {
            RefreshOutcome::Refreshed(replacement) => replacement,
            RefreshOutcome::NeedsReconcile(reason) => panic!("refresh failed: {reason:?}"),
        };
        assert_eq!(index.generation(), 1);
        assert_eq!(replacement.generation(), 2);
        assert!(index.declarations("old").is_some());
        assert!(replacement.declarations("new_name").is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refresh_handles_add_delete_and_rename() {
        let root = temp_root();
        write_file(&root, "old.rs", b"pub fn old_name() {}\n");
        let mut index = scan_root(&root, &config(10, 1024)).expect("scan");
        write_file(&root, "added.rs", b"pub fn added() {}\n");
        assert!(matches!(
            refresh_paths(
                &mut index,
                &[PathChange::Created(root.join("added.rs"))],
                &config(10, 1024)
            ),
            RefreshOutcome::Refreshed(_)
        ));
        assert!(index.declarations("added").is_some());
        std::fs::rename(root.join("old.rs"), root.join("renamed.rs")).expect("rename");
        assert!(matches!(
            refresh_paths(
                &mut index,
                &[PathChange::Renamed {
                    from: root.join("old.rs"),
                    to: root.join("renamed.rs")
                }],
                &config(10, 1024)
            ),
            RefreshOutcome::Refreshed(_)
        ));
        assert!(index.declarations("old_name").is_some());
        assert!(index
            .files()
            .iter()
            .any(|file| file.relative_path == "renamed.rs"));
        std::fs::remove_file(root.join("added.rs")).expect("remove");
        assert!(matches!(
            refresh_paths(
                &mut index,
                &[PathChange::Removed(root.join("added.rs"))],
                &config(10, 1024)
            ),
            RefreshOutcome::Refreshed(_)
        ));
        assert!(index.declarations("added").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn refresh_reconciles_uncertain_changes_and_policy() {
        let root = temp_root();
        write_file(&root, "main.rs", b"pub fn main_name() {}\n");
        let mut index = scan_root(&root, &config(10, 1024)).expect("scan");
        assert!(matches!(
            refresh_paths(&mut index, &[PathChange::Error], &config(10, 1024)),
            RefreshOutcome::NeedsReconcile(ReconcileReason::AmbiguousChange)
        ));
        assert!(matches!(
            refresh_paths(&mut index, &[], &config(9, 1024)),
            RefreshOutcome::NeedsReconcile(ReconcileReason::PolicyChanged)
        ));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn verified_reference_postings_keep_ascii_boundary_rules() {
        let root = temp_root();
        write_file(
            &root,
            "main.rs",
            b"let $foo = 1;\nlet foo = 2;\nlet prefixfoo = 3;\nlet foobar = 4;\n",
        );
        let index = scan_root(&root, &config(10, 1024)).expect("scan");
        let postings = index.verified_references("foo").expect("refs");
        assert_eq!(postings.len(), 1);
        assert_eq!(postings[0].line_no, 2);
        assert!(index.verified_references("not-found").is_none());
        assert!(index.verified_references("foo-").is_none());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn reference_index_uses_coordinates_for_duplicate_heavy_lines() {
        let root = temp_root();
        let relative_path = "nested/repeated.rs";
        let repeated_line = "target target target target target\n";
        let source = repeated_line.repeat(4096);
        write_file(&root, relative_path, source.as_bytes());
        let index = scan_root(&root, &config(10, source.len() + 1)).expect("scan");
        let postings = index.identifier_refs.get("target").expect("postings");
        let coordinate_bytes = postings.len() * std::mem::size_of::<ReferenceCoordinate>();
        let duplicated_payload_bytes =
            postings.len() * (relative_path.len() + repeated_line.trim_end().len());

        assert_eq!(postings.len(), 4096);
        assert!(postings.iter().all(|posting| posting.source_index == 0));
        assert_eq!(std::mem::size_of_val(postings.as_ref()), coordinate_bytes);
        assert!(coordinate_bytes < duplicated_payload_bytes);
        assert!(index.logical_bytes() < source.len().saturating_mul(4));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn verified_references_materialize_exact_order_and_boundaries() {
        let root = temp_root();
        write_file(&root, "z.rs", b"target\r\n");
        write_file(
            &root,
            "a.rs",
            b"target\nprefix_target\n$target\nfoo-target\r\ntarget target\n",
        );
        let index = scan_root(&root, &config(10, 1024)).expect("scan");

        assert_eq!(
            index.verified_references("target"),
            Some(vec![
                ReferencePosting {
                    relative_path: "a.rs".to_owned(),
                    line_no: 1,
                    line: "target".to_owned(),
                },
                ReferencePosting {
                    relative_path: "a.rs".to_owned(),
                    line_no: 4,
                    line: "foo-target".to_owned(),
                },
                ReferencePosting {
                    relative_path: "a.rs".to_owned(),
                    line_no: 5,
                    line: "target target".to_owned(),
                },
                ReferencePosting {
                    relative_path: "z.rs".to_owned(),
                    line_no: 1,
                    line: "target".to_owned(),
                },
            ])
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scan_inputs_capture_ignore_and_watch_parents() {
        let root = temp_root();
        write_file(&root, ".gitignore", b"ignored.rs\n");
        write_file(&root, "src/main.rs", b"pub fn main_name() {}\n");
        let index = scan_root(&root, &config(10, 1024)).expect("scan");
        assert!(index
            .scan_inputs()
            .watched_directories
            .iter()
            .any(|path| path == &root));
        assert!(index
            .scan_inputs()
            .dependency_parents
            .iter()
            .any(|path| path == &root));
        assert!(index
            .path_dispositions()
            .iter()
            .all(|record| record.relative_path != "ignored.rs"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn empty_incomplete_generation_is_not_published() {
        let root = temp_root();
        let config = config(10, 1024);
        let inputs = ScanInputs::capture(&root, &config).expect("inputs");
        let error = match assemble_index(
            inputs,
            1,
            Vec::new(),
            Vec::new(),
            WalkDiagnostics {
                read_errors: 1,
                ..WalkDiagnostics::default()
            },
            false,
            config.index_byte_limit(),
        ) {
            Ok(_) => panic!("empty incomplete generation"),
            Err(error) => error,
        };
        assert!(matches!(error, ScanError::Incomplete { .. }));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn index_byte_bound_is_explicit() {
        let root = temp_root();
        write_file(&root, "main.rs", b"pub fn main_name() {}\n");
        let mut cfg = config(10, 1024);
        cfg.max_index_bytes = 1;
        let error = match scan_root(&root, &cfg) {
            Ok(_) => panic!("bound"),
            Err(error) => error,
        };
        assert!(matches!(error, ScanError::BoundsExceeded { .. }));
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn max_files_cap_stops_before_later_entries() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        write_file(&root, "a.rs", b"pub fn first() {}\n");
        write_file(&root, "b.rs", b"pub fn second() {}\n");
        symlink(root.join("a.rs"), root.join("z.rs")).expect("symlink");
        let index = scan_root(&root, &config(1, 1024)).expect("scan");

        assert!(index.truncated());
        assert_eq!(
            index
                .files()
                .iter()
                .map(|file| file.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["a.rs"]
        );
        assert!(index
            .path_dispositions()
            .iter()
            .all(|record| record.relative_path != "z.rs"));
        assert_eq!(index.diagnostics().symlinks_skipped, 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_disposition_is_indexed_without_following() {
        use std::os::unix::fs::symlink;
        let root = temp_root();
        write_file(&root, "real.rs", b"pub fn real_name() {}\n");
        symlink(root.join("real.rs"), root.join("link.rs")).expect("symlink");
        let index = scan_root(&root, &config(10, 1024)).expect("scan");
        assert!(index
            .path_dispositions()
            .iter()
            .any(|record| record.relative_path == "link.rs"
                && record.disposition == PathDisposition::Symlink));
        assert!(index
            .files()
            .iter()
            .all(|file| file.relative_path != "link.rs"));
        std::fs::remove_dir_all(root).ok();
    }
}
