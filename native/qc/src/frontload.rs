use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{outline, util};

const CACHE_VERSION: u32 = 1;
const CACHE_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const BUILD_LOCK_STALE: Duration = Duration::from_secs(2 * 60);
const MAX_SOURCE_FILES: usize = 5000;
const MAX_BLOB_BYTES: usize = 512 * 1024;
const MAX_LIVE_FILE_BYTES: usize = 1024 * 1024;
const MAX_CANDIDATES: usize = 12;
const MAX_OUTPUT_BYTES: usize = 4 * 1024;
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    "coverage",
];

#[derive(Debug)]
pub(crate) enum FrontloadError {
    RootUnavailable,
    RootNotDirectory,
    StateUnavailable,
    CacheWarming,
    GitUnavailable(String),
    Io(String),
    InvalidCache(String),
}

impl fmt::Display for FrontloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => write!(f, "scan root is unavailable"),
            Self::RootNotDirectory => write!(f, "scan root is not a directory"),
            Self::StateUnavailable => write!(f, "native state directory is unavailable"),
            Self::CacheWarming => write!(f, "frontload cache is warming"),
            Self::GitUnavailable(detail) => write!(f, "frontload Git index unavailable: {detail}"),
            Self::Io(detail) => write!(f, "frontload I/O error: {detail}"),
            Self::InvalidCache(detail) => write!(f, "frontload cache is invalid: {detail}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CacheFile {
    version: u32,
    built_at_secs: u64,
    root: String,
    files: Vec<String>,
    declarations: Vec<CachedDeclaration>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CachedDeclaration {
    file: u32,
    name: String,
    line_no: u32,
}

#[derive(Clone, Debug)]
struct Candidate {
    file: usize,
    name: String,
    cached_line: usize,
    score: u32,
}

#[derive(Clone, Debug)]
struct LiveCandidate {
    file: String,
    name: String,
    line_no: usize,
    score: u32,
    source: String,
}

#[derive(Clone, Debug)]
struct CachePaths {
    dir: PathBuf,
    cache: PathBuf,
    lock: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn canonical_root(root: &Path) -> Result<PathBuf, FrontloadError> {
    let canonical = fs::canonicalize(root).map_err(|_| FrontloadError::RootUnavailable)?;
    let metadata = fs::metadata(&canonical).map_err(|_| FrontloadError::RootUnavailable)?;
    if !metadata.is_dir() {
        return Err(FrontloadError::RootNotDirectory);
    }
    Ok(canonical)
}

fn root_key(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.as_os_str().to_string_lossy().as_bytes());
    hex::encode(hasher.finalize())
}

fn cache_paths(root: &Path) -> Result<CachePaths, FrontloadError> {
    let base = util::state_dir().ok_or(FrontloadError::StateUnavailable)?;
    let dir = base.join("frontload");
    let key = root_key(root);
    Ok(CachePaths {
        cache: dir.join(format!("frontload-v{CACHE_VERSION}-{key}.json")),
        lock: dir.join(format!("frontload-v{CACHE_VERSION}-{key}.lock")),
        dir,
    })
}

#[cfg(unix)]
fn secure_dir(path: &Path) -> Result<(), FrontloadError> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(path).map_err(|e| FrontloadError::Io(e.to_string()))?;
    let metadata = fs::symlink_metadata(path).map_err(|e| FrontloadError::Io(e.to_string()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(FrontloadError::Io("frontload state directory is not a real directory".to_owned()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|e| FrontloadError::Io(e.to_string()))
}

#[cfg(not(unix))]
fn secure_dir(path: &Path) -> Result<(), FrontloadError> {
    fs::create_dir_all(path).map_err(|e| FrontloadError::Io(e.to_string()))
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<File, FrontloadError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| FrontloadError::Io(e.to_string()))
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> Result<File, FrontloadError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| FrontloadError::Io(e.to_string()))
}

fn path_allowed(path: &Path) -> bool {
    if path.is_absolute()
        || path.components().any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return false;
    }
    if path.components().any(|component| {
        SKIP_DIRS.contains(&component.as_os_str().to_string_lossy().as_ref())
    }) {
        return false;
    }
    outline::is_source_ext(path)
}

fn git_output(root: &Path, args: &[&str]) -> Result<Vec<u8>, FrontloadError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| FrontloadError::GitUnavailable(e.to_string()))?;
    if !output.status.success() {
        return Err(FrontloadError::GitUnavailable(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output.stdout)
}

fn indexed_sources(root: &Path) -> Result<Vec<(String, String)>, FrontloadError> {
    let raw = git_output(root, &["ls-files", "-s", "-z", "--cached"])?;
    let mut out = Vec::new();
    for record in raw.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let meta = &record[..tab];
        let path_raw = &record[tab + 1..];
        let mut fields = meta.split(|byte| *byte == b' ');
        let _mode = fields.next();
        let Some(sha) = fields.next() else { continue; };
        let Some(stage) = fields.next() else { continue; };
        if stage != b"0" {
            continue;
        }
        let path = String::from_utf8_lossy(path_raw).into_owned();
        if !path_allowed(Path::new(&path)) {
            continue;
        }
        let sha = String::from_utf8_lossy(sha).into_owned();
        out.push((sha, path));
        if out.len() >= MAX_SOURCE_FILES {
            break;
        }
    }
    Ok(out)
}

fn cache_from_git(root: &Path) -> Result<CacheFile, FrontloadError> {
    let sources = indexed_sources(root)?;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| FrontloadError::GitUnavailable(e.to_string()))?;

    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| FrontloadError::GitUnavailable("missing git cat-file stdin".to_owned()))?;
    let child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| FrontloadError::GitUnavailable("missing git cat-file stdout".to_owned()))?;
    let mut reader = BufReader::new(child_stdout);

    let mut files = Vec::with_capacity(sources.len());
    let mut declarations = Vec::new();
    let mut header = String::new();

    for (sha, relative) in sources {
        writeln!(child_stdin, "{sha}").map_err(|e| FrontloadError::Io(e.to_string()))?;
        child_stdin.flush().map_err(|e| FrontloadError::Io(e.to_string()))?;
        header.clear();
        reader
            .read_line(&mut header)
            .map_err(|e| FrontloadError::Io(e.to_string()))?;
        let parts: Vec<_> = header.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "blob" {
            return Err(FrontloadError::GitUnavailable(format!(
                "unexpected git cat-file response for {relative}"
            )));
        }
        let size = parts[2]
            .parse::<usize>()
            .map_err(|_| FrontloadError::GitUnavailable("invalid git blob size".to_owned()))?;
        let mut bytes = vec![0u8; size];
        reader
            .read_exact(&mut bytes)
            .map_err(|e| FrontloadError::Io(e.to_string()))?;
        let mut newline = [0u8; 1];
        reader
            .read_exact(&mut newline)
            .map_err(|e| FrontloadError::Io(e.to_string()))?;

        let file_index = files.len();
        files.push(relative.clone());
        if size > MAX_BLOB_BYTES || bytes[..bytes.len().min(8192)].contains(&0) {
            continue;
        }
        let Ok(source) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let family = outline::family_of(Path::new(&relative));
        for declaration in outline::decls_in(source, family) {
            let Some(name) = declaration.name else { continue; };
            let Ok(file) = u32::try_from(file_index) else { continue; };
            let Ok(line_no) = u32::try_from(declaration.line_no) else { continue; };
            declarations.push(CachedDeclaration { file, name, line_no });
        }
    }

    drop(child_stdin);
    let status = child
        .wait()
        .map_err(|e| FrontloadError::GitUnavailable(e.to_string()))?;
    if !status.success() {
        return Err(FrontloadError::GitUnavailable(
            "git cat-file batch failed".to_owned(),
        ));
    }

    Ok(CacheFile {
        version: CACHE_VERSION,
        built_at_secs: now_secs(),
        root: root.to_string_lossy().into_owned(),
        files,
        declarations,
    })
}

fn write_cache(root: &Path, cache: &CacheFile) -> Result<(), FrontloadError> {
    let paths = cache_paths(root)?;
    secure_dir(&paths.dir)?;
    let temp = paths.dir.join(format!(
        ".frontload-v{CACHE_VERSION}-{}-{}.tmp",
        root_key(root),
        std::process::id()
    ));
    let _ = fs::remove_file(&temp);
    let bytes = serde_json::to_vec(cache).map_err(|e| FrontloadError::InvalidCache(e.to_string()))?;
    let mut file = create_private(&temp)?;
    file.write_all(&bytes)
        .map_err(|e| FrontloadError::Io(e.to_string()))?;
    file.sync_all().map_err(|e| FrontloadError::Io(e.to_string()))?;
    drop(file);
    fs::rename(&temp, &paths.cache).map_err(|e| FrontloadError::Io(e.to_string()))
}

pub(crate) fn build_cache(root: &Path) -> Result<usize, FrontloadError> {
    let root = canonical_root(root)?;
    let paths = cache_paths(&root)?;
    secure_dir(&paths.dir)?;
    let result = cache_from_git(&root).and_then(|cache| {
        let count = cache.declarations.len();
        write_cache(&root, &cache)?;
        Ok(count)
    });
    let _ = fs::remove_file(paths.lock);
    result
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else { return true; };
    let Ok(modified) = metadata.modified() else { return true; };
    modified.elapsed().unwrap_or_default() >= BUILD_LOCK_STALE
}

fn spawn_builder(root: &Path) -> Result<(), FrontloadError> {
    let paths = cache_paths(root)?;
    secure_dir(&paths.dir)?;
    if paths.lock.exists() {
        if !lock_is_stale(&paths.lock) {
            return Ok(());
        }
        let _ = fs::remove_file(&paths.lock);
    }
    let mut lock = match create_private(&paths.lock) {
        Ok(file) => file,
        Err(_) => return Ok(()),
    };
    writeln!(lock, "{}", std::process::id()).ok();
    drop(lock);

    let executable = std::env::current_exe().map_err(|e| FrontloadError::Io(e.to_string()))?;
    let spawned = Command::new(executable)
        .args(["repo", "frontload-build", "--root"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(error) = spawned {
        let _ = fs::remove_file(paths.lock);
        return Err(FrontloadError::Io(error.to_string()));
    }
    Ok(())
}

fn load_cache(root: &Path) -> Result<Option<(CacheFile, bool)>, FrontloadError> {
    let paths = cache_paths(root)?;
    let bytes = match fs::read(&paths.cache) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FrontloadError::Io(error.to_string())),
    };
    let cache: CacheFile =
        serde_json::from_slice(&bytes).map_err(|e| FrontloadError::InvalidCache(e.to_string()))?;
    if cache.version != CACHE_VERSION || cache.root != root.to_string_lossy().as_ref() {
        return Ok(None);
    }
    let stale = now_secs().saturating_sub(cache.built_at_secs) >= CACHE_MAX_AGE.as_secs();
    Ok(Some((cache, stale)))
}

fn terms(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "can", "code", "does", "do",
        "for", "from", "how", "i", "in", "into", "is", "it", "me", "of", "on", "or",
        "our", "please", "show", "the", "this", "to", "trace", "use", "what", "where",
        "which", "who", "why", "with", "work", "works", "working", "explain", "find",
        "understand", "flow", "architecture", "dependency", "dependencies", "caller",
        "callers", "callee", "callees", "impact", "repo", "repository",
    ];
    let mut tokens = BTreeSet::new();
    let mut current = String::new();
    let flush = |current: &mut String, tokens: &mut BTreeSet<String>| {
        if current.is_empty() {
            return;
        }
        let lowered = current.to_lowercase();
        if lowered.chars().count() >= 3 && !STOP.contains(&lowered.as_str()) {
            tokens.insert(lowered.clone());
        }
        for part in lowered.split(|ch: char| matches!(ch, '.' | ':' | '/' | '_' | '-')) {
            if part.chars().count() >= 3 && !STOP.contains(&part) {
                tokens.insert(part.to_owned());
            }
        }
        current.clear();
    };
    for ch in query.chars().take(1200) {
        if ch.is_alphanumeric() || matches!(ch, '_' | '$' | '.' | ':' | '/' | '-') {
            current.push(ch);
        } else {
            flush(&mut current, &mut tokens);
        }
    }
    flush(&mut current, &mut tokens);
    tokens.into_iter().take(16).collect()
}

fn rank(query: &str, cache: &CacheFile) -> Vec<Candidate> {
    let query_lower = query.to_lowercase();
    let query_terms = terms(query);
    if query_terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for declaration in &cache.declarations {
        let Ok(file_index) = usize::try_from(declaration.file) else { continue; };
        let Some(file) = cache.files.get(file_index) else { continue; };
        let name = declaration.name.to_lowercase();
        let file_lower = file.to_lowercase();
        let base = Path::new(file)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let mut score = if name.chars().count() >= 3 && query_lower.contains(&name) {
            160
        } else {
            0
        };
        for term in &query_terms {
            if term == &name {
                score += 200;
            } else if name.contains(term) {
                score += 60;
            } else if term.chars().count() >= 4 && name.chars().count() >= 3 && term.contains(&name) {
                score += 30;
            }
            if term == &base {
                score += 80;
            } else if file_lower.contains(term) {
                score += 20;
            }
        }
        if score > 0 {
            candidates.push(Candidate {
                file: file_index,
                name: declaration.name.clone(),
                cached_line: declaration.line_no as usize,
                score,
            });
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| cache.files[a.file].cmp(&cache.files[b.file]))
            .then_with(|| a.cached_line.cmp(&b.cached_line))
    });
    candidates.dedup_by(|a, b| {
        a.file == b.file && a.name == b.name && a.cached_line == b.cached_line
    });
    candidates.truncate(MAX_CANDIDATES);
    candidates
}

#[cfg(unix)]
fn open_live(path: &Path) -> Result<File, FrontloadError> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| FrontloadError::Io(e.to_string()))
}

#[cfg(not(unix))]
fn open_live(path: &Path) -> Result<File, FrontloadError> {
    File::open(path).map_err(|e| FrontloadError::Io(e.to_string()))
}

fn read_live(root: &Path, relative: &str) -> Result<String, FrontloadError> {
    let relative_path = Path::new(relative);
    if !path_allowed(relative_path) {
        return Err(FrontloadError::Io("unsafe cached path".to_owned()));
    }
    let path = root.join(relative_path);
    let mut file = open_live(&path)?;
    let metadata = file.metadata().map_err(|e| FrontloadError::Io(e.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_LIVE_FILE_BYTES as u64 {
        return Err(FrontloadError::Io("cached source is unavailable or oversized".to_owned()));
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_LIVE_FILE_BYTES));
    file.take((MAX_LIVE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| FrontloadError::Io(e.to_string()))?;
    if bytes.len() > MAX_LIVE_FILE_BYTES || bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(FrontloadError::Io("cached source is not bounded text".to_owned()));
    }
    String::from_utf8(bytes).map_err(|e| FrontloadError::Io(e.to_string()))
}

fn validate_candidate(root: &Path, cache: &CacheFile, candidate: &Candidate) -> Option<LiveCandidate> {
    let file = cache.files.get(candidate.file)?.clone();
    let source = read_live(root, &file).ok()?;
    let family = outline::family_of(Path::new(&file));
    let declaration = outline::decls_in(&source, family)
        .into_iter()
        .find(|decl| decl.name.as_deref() == Some(candidate.name.as_str()))?;
    Some(LiveCandidate {
        file,
        name: candidate.name.clone(),
        line_no: declaration.line_no,
        score: candidate.score,
        source,
    })
}

fn excerpt(source: &str, line_no: usize, radius: usize) -> String {
    let start = line_no.saturating_sub(radius).max(1);
    let end = line_no.saturating_add(radius);
    let mut out = String::new();
    for (index, line) in source.lines().enumerate() {
        let current = index + 1;
        if current < start {
            continue;
        }
        if current > end {
            break;
        }
        out.push_str(&format!("{current}\t{line}\n"));
    }
    out
}

fn cap_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    while value.chars().last().is_some_and(char::is_whitespace) {
        value.pop();
    }
    value.push_str("\n[qc-frontload: capped]\n");
    value
}

pub(crate) fn query(query: &str, root: &Path) -> Result<String, FrontloadError> {
    let root = canonical_root(root)?;
    let loaded = load_cache(&root)?;
    let Some((cache, stale)) = loaded else {
        spawn_builder(&root)?;
        return Err(FrontloadError::CacheWarming);
    };
    if stale {
        let _ = spawn_builder(&root);
    }

    let ranked = rank(query, &cache);
    if ranked.is_empty() {
        return Ok(format!(
            "[qc-frontload v1] {query}\nNo high-confidence symbol/file match. Use repo explore with a more specific symbol/file name.\n"
        ));
    }

    let mut live = Vec::new();
    for candidate in &ranked {
        if let Some(candidate) = validate_candidate(&root, &cache, candidate) {
            live.push(candidate);
            if live.len() >= 3 {
                break;
            }
        }
    }
    if live.is_empty() {
        let _ = spawn_builder(&root);
        return Err(FrontloadError::CacheWarming);
    }
    live.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.file.cmp(&b.file)));
    let primary = &live[0];
    let mut out = format!(
        "[qc-frontload v1] {query}\n## {} - {}:{}\n[source]\n{}",
        primary.name,
        primary.file,
        primary.line_no,
        excerpt(&primary.source, primary.line_no, 5),
    );
    if live.len() > 1 {
        out.push_str("[related candidates]\n");
        for candidate in &live[1..] {
            out.push_str(&format!(
                "  {} - {}:{}\n",
                candidate.name, candidate.file, candidate.line_no
            ));
        }
    }
    out.push_str(
        "[qc-frontload] Source context is already inspected; use repo explore for graph relationships or broader context.\n",
    );
    Ok(cap_utf8(out, MAX_OUTPUT_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn terms_extract_symbol_parts() {
        let got = terms("How does AuthService.open_session work?");
        assert!(got.contains(&"authservice.open_session".to_owned()));
        assert!(got.contains(&"open_session".to_owned()));
        assert!(got.contains(&"session".to_owned()));
    }

    #[test]
    fn rank_prefers_exact_symbol() {
        let cache = CacheFile {
            version: CACHE_VERSION,
            built_at_secs: now_secs(),
            root: "/tmp/repo".to_owned(),
            files: vec!["a.py".to_owned(), "b.py".to_owned()],
            declarations: vec![
                CachedDeclaration { file: 0, name: "session".to_owned(), line_no: 1 },
                CachedDeclaration { file: 1, name: "open_session".to_owned(), line_no: 9 },
            ],
        };
        let got = rank("How does open_session work?", &cache);
        assert_eq!(got[0].name, "open_session");
        assert_eq!(got[0].file, 1);
    }

    #[test]
    fn cache_paths_are_root_scoped() {
        let a = Path::new("/tmp/a");
        let b = Path::new("/tmp/b");
        assert_ne!(root_key(a), root_key(b));
    }

    #[test]
    fn private_path_rejects_parent_components() {
        assert!(!path_allowed(Path::new("../secret.py")));
        assert!(path_allowed(Path::new("src/app.py")));
    }

    #[test]
    fn cache_round_trip_and_live_validation() {
        let _guard = env_lock().lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().to_path_buf();
        let repo = state.join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("app.py"), "def open_session(name):\n    return name\n").unwrap();
        std::env::set_var("QUIET_CONTEXT_NATIVE_STATE_DIR", state.join("state"));
        let canonical = canonical_root(&repo).unwrap();
        let cache = CacheFile {
            version: CACHE_VERSION,
            built_at_secs: now_secs(),
            root: canonical.to_string_lossy().into_owned(),
            files: vec!["app.py".to_owned()],
            declarations: vec![CachedDeclaration {
                file: 0,
                name: "open_session".to_owned(),
                line_no: 1,
            }],
        };
        write_cache(&canonical, &cache).unwrap();
        let out = query("How does open_session work?", &canonical).unwrap();
        assert!(out.contains("## open_session - app.py:1"));
        assert!(out.contains("1\tdef open_session(name):"));
        std::env::remove_var("QUIET_CONTEXT_NATIVE_STATE_DIR");
    }
}
