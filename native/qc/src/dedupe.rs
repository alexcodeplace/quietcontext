use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::DedupeConfig;
use crate::util;

/// Entries older than this are dropped from the index on load.
const MAX_ENTRY_AGE_SECS: u64 = 86_400;

#[derive(Serialize, Deserialize)]
struct DedupeEntry {
    output_hash: String,
    exit_code: i32,
    ts: u64,
    spill_path: String,
    bytes: usize,
}

/// cmd_hash -> entry. BTreeMap for a deterministic on-disk key order.
type Index = BTreeMap<String, DedupeEntry>;

/// Returns Some(marker) when this exact output was already emitted for this
/// exact command in this session and the spill copy is on disk; None means
/// "emit the output unchanged" (miss, disabled, or any IO failure).
pub fn maybe_dedupe(
    cfg: &DedupeConfig,
    binary: &str,
    args: &[String],
    exit_code: i32,
    output: &str,
) -> Option<String> {
    if util::cap_bypassed() {
        return None;
    }
    // QUIET_CONTEXT_PIPED=1 is set by bash-gate when the command has a downstream pipe:
    // the consumer is a program, not the agent, and a marker instead of the
    // real bytes would silently corrupt the pipeline.
    if matches!(std::env::var("QUIET_CONTEXT_PIPED").as_deref(), Ok("1")) {
        return None;
    }
    if !cfg.enabled {
        return None;
    }
    if output.len() < cfg.min_bytes {
        return None;
    }
    let session = util::session_id()?;
    let session = sanitize_session(&session)?;
    let now = now_secs()?;
    let ipath = index_path(&session)?;

    let ohash = sha256_hex(output.as_bytes());
    let chash = cmd_hash(binary, args);
    let spath = spill_path_for(&ohash)?;

    let _lock = acquire_lock(&lock_path_for(&ipath));

    let (mut index, pruned) = load_index(&ipath, now);

    let marker = index.get(&chash).and_then(|e| {
        let fresh = e.output_hash == ohash
            && e.exit_code == exit_code
            && now.saturating_sub(e.ts) <= cfg.ttl_secs
            && spill_ok(Path::new(&e.spill_path), e.bytes);
        fresh.then(|| format_marker(now.saturating_sub(e.ts), e.bytes, &e.spill_path))
    });

    if let Some(marker) = marker {
        if pruned {
            let _ = save_index(&ipath, &index);
        }
        return Some(marker);
    }

    // Same cmd_hash previously pointed at a different spill (content changed,
    // or cwd-sensitive collision): that spill is about to become unreachable
    // from this session's index. Remember it so it can be reclaimed below
    // instead of leaking until the 24h prune (which can never see it, since
    // it is dropped from the map by the insert on the next line).
    let new_spill_str = spath.to_string_lossy().into_owned();
    let stale_spill = index
        .get(&chash)
        .map(|e| e.spill_path.clone())
        .filter(|p| p != &new_spill_str);

    // MISS: fail-closed — never insert an entry unless the spill is confirmed written.
    if !write_spill(&spath, output.as_bytes()) {
        return None;
    }
    index.insert(
        chash,
        DedupeEntry {
            output_hash: ohash,
            exit_code,
            ts: now,
            spill_path: spath.to_string_lossy().into_owned(),
            bytes: output.len(),
        },
    );
    let _ = save_index(&ipath, &index);

    if let Some(stale) = stale_spill {
        if !index.values().any(|e| e.spill_path == stale) {
            let _ = std::fs::remove_file(&stale);
        }
    }

    None
}

fn now_secs() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    hex::encode(hash)
}

fn cmd_hash(binary: &str, args: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(binary.as_bytes());
    hasher.update([0u8]);
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            hasher.update([0u8]);
        }
        hasher.update(arg.as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn sanitize_session(id: &str) -> Option<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(128);
    if out.is_empty() || out.chars().all(|c| c == '.') {
        return None;
    }
    Some(out)
}

fn index_path(session: &str) -> Option<PathBuf> {
    util::state_dir().map(|d| {
        d.join("cache")
            .join("dedupe")
            .join(format!("{session}.json"))
    })
}

fn lock_path_for(index_path: &Path) -> PathBuf {
    index_path.with_extension("lock")
}

/// Best-effort advisory lock across concurrent `qc` processes sharing the
/// same session index: bounds the read-modify-write race that otherwise
/// loses a concurrent insert. Fails OPEN (no lock held) on any IO error or
/// timeout — this only narrows a lost-update window, atomic rename already
/// guarantees the on-disk file is never torn, so a missed lock never
/// corrupts state or changes behavior of the wrapped command. The flock(2) is
/// held for the guard's lifetime; the kernel releases it when the fd closes,
/// including on process death.
struct LockGuard(Option<std::fs::File>);

impl LockGuard {
    #[cfg(test)]
    fn is_held(&self) -> bool {
        self.0.is_some()
    }
}

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(f) = &self.0 {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(f.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

#[cfg(unix)]
fn acquire_lock(lock_path: &Path) -> LockGuard {
    use std::os::unix::io::AsRawFd;

    if let Some(parent) = lock_path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return LockGuard(None);
        }
    }
    // Concurrent openers must share one inode to contend on the same lock,
    // so not create_new; and never unlinked, or a later opener could flock an
    // orphaned inode.
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(_) => return LockGuard(None),
    };
    const MAX_ATTEMPTS: u32 = 200;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);
    let fd = file.as_raw_fd();
    for _ in 0..MAX_ATTEMPTS {
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return LockGuard(Some(file));
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EWOULDBLOCK) => std::thread::sleep(RETRY_DELAY),
            Some(libc::EINTR) => continue,
            _ => return LockGuard(None),
        }
    }
    // Persistent contention: proceed unlocked rather than block the wrapped
    // command's output forever.
    LockGuard(None)
}

#[cfg(not(unix))]
fn acquire_lock(_lock_path: &Path) -> LockGuard {
    LockGuard(None)
}

fn spill_path_for(output_hash: &str) -> Option<PathBuf> {
    util::state_dir().map(|d| {
        d.join("cache")
            .join("spill")
            .join(format!("{output_hash}.txt"))
    })
}

/// Returns the pruned index and whether the prune actually dropped anything
/// (the caller persists on true even for a hit, so cleanup is bounded).
fn load_index(path: &Path, now: u64) -> (Index, bool) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (Index::new(), false),
    };
    let index: Index = match serde_json::from_str(&contents) {
        Ok(i) => i,
        Err(_) => return (Index::new(), false),
    };

    let mut kept = Index::new();
    let mut dropped: Vec<DedupeEntry> = Vec::new();
    for (k, v) in index {
        if now.saturating_sub(v.ts) <= MAX_ENTRY_AGE_SECS {
            kept.insert(k, v);
        } else {
            dropped.push(v);
        }
    }

    if dropped.is_empty() {
        return (kept, false);
    }

    let kept_spills: BTreeSet<&str> = kept.values().map(|e| e.spill_path.as_str()).collect();
    for d in &dropped {
        if !kept_spills.contains(d.spill_path.as_str()) {
            let _ = std::fs::remove_file(&d.spill_path);
        }
    }

    (kept, true)
}

fn save_index(path: &Path, index: &Index) -> bool {
    let bytes = match serde_json::to_vec(index) {
        Ok(b) => b,
        Err(_) => return false,
    };
    write_atomic(path, &bytes)
}

fn write_spill(path: &Path, bytes: &[u8]) -> bool {
    write_atomic(path, bytes)
}

fn spill_ok(path: &Path, expect_bytes: usize) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.len() == expect_bytes as u64,
        Err(_) => false,
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    let parent = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let tmp_name = match path.file_name() {
        Some(n) => format!("{}.tmp.{}", n.to_string_lossy(), std::process::id()),
        None => return false,
    };
    let tmp = parent.join(tmp_name);
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

fn format_marker(age_secs: u64, bytes: usize, spill_path: &str) -> String {
    let mins = age_secs / 60;
    format!(
        "[qc: output identical to same command {mins}min ago ({bytes} bytes, unchanged). Full copy: {spill_path} — or rerun with QUIET_CONTEXT_FULL=1]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qc-dedupe-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn orphaned_lock_file_is_acquired_immediately_not_wedged() {
        let dir = tempdir();
        let lock = dir.join("session.lock");
        std::fs::File::create(&lock).unwrap();

        let start = std::time::Instant::now();
        let guard = acquire_lock(&lock);
        assert!(
            guard.is_held(),
            "a lock file left behind by a dead process carries no flock and must acquire immediately"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "acquisition must not wait out any retry budget against an orphaned lock file"
        );
    }

    #[test]
    fn sequential_acquisitions_against_released_lock_both_succeed() {
        let dir = tempdir();
        let lock = dir.join("session.lock");

        let first = acquire_lock(&lock);
        assert!(first.is_held());
        drop(first);

        let second = acquire_lock(&lock);
        assert!(
            second.is_held(),
            "a second acquisition against the same, now-released lock file must also succeed"
        );
    }

    #[test]
    fn concurrent_acquisitions_are_mutually_exclusive() {
        let dir = tempdir();
        let lock = dir.join("session.lock");

        let intervals: std::sync::Arc<
            std::sync::Mutex<Vec<(std::time::Instant, std::time::Instant)>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let lock = lock.clone();
                let intervals = intervals.clone();
                std::thread::spawn(move || {
                    let guard = acquire_lock(&lock);
                    assert!(guard.is_held(), "must acquire within the retry budget");
                    let start = std::time::Instant::now();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let end = std::time::Instant::now();
                    intervals.lock().unwrap().push((start, end));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let intervals = intervals.lock().unwrap();
        assert_eq!(intervals.len(), 4);
        for i in 0..intervals.len() {
            for j in (i + 1)..intervals.len() {
                let (s1, e1) = intervals[i];
                let (s2, e2) = intervals[j];
                assert!(
                    e1 <= s2 || e2 <= s1,
                    "overlapping hold intervals violate mutual exclusion"
                );
            }
        }
    }

    #[test]
    fn uncreatable_parent_fails_open_immediately() {
        let dir = tempdir();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let lock = blocker.join("session.lock");

        let start = std::time::Instant::now();
        let guard = acquire_lock(&lock);
        assert!(!guard.is_held());
        assert!(
            start.elapsed() < std::time::Duration::from_millis(200),
            "an unwritable parent dir must fail open immediately, not spend the retry budget"
        );
    }
}
