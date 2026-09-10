use ignore::WalkBuilder;
use regex::Regex;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::MapConfig;
use crate::outline;
use crate::repomap_index::SourceIndex;

const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    "coverage",
];

#[derive(Clone, Debug, Default)]
pub(crate) struct WalkDiagnostics {
    traversal_errors: usize,
    read_errors: usize,
    symlinks_skipped: usize,
    oversized_files: usize,
    binary_files: usize,
    invalid_utf8_files: usize,
}

impl WalkDiagnostics {
    fn has_errors(&self) -> bool {
        self.traversal_errors > 0 || self.read_errors > 0
    }

    fn summary(&self, truncated: bool, max_files: usize) -> String {
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

struct SourceFile {
    path: PathBuf,
    rel: String,
    text: String,
}

struct WalkResult {
    root: PathBuf,
    files: Vec<SourceFile>,
    truncated: bool,
    diagnostics: WalkDiagnostics,
}

#[derive(Debug)]
pub(crate) enum ScanError {
    RootUnavailable,
    RootNotDirectory,
    InvalidQuery,
    Incomplete {
        diagnostics: WalkDiagnostics,
        truncated: bool,
        max_files: usize,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => write!(f, "scan root is unavailable"),
            Self::RootNotDirectory => write!(f, "scan root is not a directory"),
            Self::InvalidQuery => write!(f, "invalid symbol query"),
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

pub(crate) type ScanResult = Result<Option<String>, ScanError>;

enum SourceRead {
    Included(String),
    Oversized,
    Binary,
    InvalidUtf8,
}

/// Walks ignore-aware entries without following symlinks. Source text is read
/// once through a bounded handle and reused by map, sym, and refs.
fn walk(root: &Path, cfg: &MapConfig) -> Result<WalkResult, ScanError> {
    let root = std::fs::canonicalize(root).map_err(|_| ScanError::RootUnavailable)?;
    let metadata = std::fs::metadata(&root).map_err(|_| ScanError::RootUnavailable)?;
    if !metadata.is_dir() {
        return Err(ScanError::RootNotDirectory);
    }

    let mut result = WalkResult {
        root: root.clone(),
        files: Vec::new(),
        truncated: false,
        diagnostics: WalkDiagnostics::default(),
    };
    let mut builder = WalkBuilder::new(&root);
    builder
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
                || !entry.file_type().is_some_and(|file_type| {
                    file_type.is_dir()
                        && SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
                })
        });

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                result.diagnostics.traversal_errors += 1;
                continue;
            }
        };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_symlink())
        {
            result.diagnostics.symlinks_skipped += 1;
            continue;
        }
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
            || !outline::is_source_ext(entry.path())
        {
            continue;
        }
        if result.files.len() >= cfg.max_files {
            result.truncated = true;
            break;
        }
        match read_source(entry.path(), cfg.max_file_bytes) {
            Ok(SourceRead::Included(text)) => {
                let path = entry.into_path();
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                result.files.push(SourceFile { path, rel, text });
            }
            Ok(SourceRead::Oversized) => result.diagnostics.oversized_files += 1,
            Ok(SourceRead::Binary) => result.diagnostics.binary_files += 1,
            Ok(SourceRead::InvalidUtf8) => result.diagnostics.invalid_utf8_files += 1,
            Err(()) => result.diagnostics.read_errors += 1,
        }
    }
    Ok(result)
}

#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

fn read_source(path: &Path, max_bytes: usize) -> Result<SourceRead, ()> {
    let file = open_no_follow(path).map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.is_file() {
        return Err(());
    }
    let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(max_bytes.min(8192).saturating_add(1));
    file.take(limit).read_to_end(&mut bytes).map_err(|_| ())?;
    if bytes.len() > max_bytes {
        return Ok(SourceRead::Oversized);
    }
    let scan = &bytes[..bytes.len().min(8192)];
    if scan.contains(&0u8) {
        return Ok(SourceRead::Binary);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(SourceRead::Included(text)),
        Err(_) => Ok(SourceRead::InvalidUtf8),
    }
}

fn scan_note(walked: &WalkResult, cfg: &MapConfig) -> String {
    let summary = walked.diagnostics.summary(walked.truncated, cfg.max_files);
    if summary.is_empty() {
        String::new()
    } else {
        format!("[qc-scan: partial — {summary}]\n")
    }
}

fn prepare_walk(root: &Path, cfg: &MapConfig) -> Result<WalkResult, ScanError> {
    let walked = walk(root, cfg)?;
    if walked.files.is_empty() && (walked.truncated || walked.diagnostics.has_errors()) {
        return Err(incomplete_error(&walked, cfg));
    }
    Ok(walked)
}

fn incomplete_error(walked: &WalkResult, cfg: &MapConfig) -> ScanError {
    ScanError::Incomplete {
        diagnostics: walked.diagnostics.clone(),
        truncated: walked.truncated,
        max_files: cfg.max_files,
    }
}

fn cap_explanation(kind: &str) -> String {
    let label = match kind {
        "sym" => "symbol",
        "refs" => "references",
        other => other,
    };
    format!("[qc-{label}: output cap too small for result entries; increase max_bytes]\n")
}

fn format_map_file_line(
    rel: &str,
    line_count: usize,
    decl_count: usize,
    names: &[String],
) -> String {
    if decl_count == 0 {
        return format!("{rel} ({line_count}L): —\n");
    }
    if names.len() > 20 {
        format!(
            "{rel} ({line_count}L): {}, +{} more\n",
            names[..20].join(", "),
            names.len() - 20
        )
    } else {
        format!("{rel} ({line_count}L): {}\n", names.join(", "))
    }
}

/// Testable core: builds `qc repo map` output for `root`, or `None` when no
/// source files are found. Traversal failures return `Err` and partial scans
/// carry a deterministic note. No stats recording — that belongs to `run_map`.
pub(crate) fn build_map_direct(root: &Path, cfg: &MapConfig) -> ScanResult {
    let walked = prepare_walk(root, cfg)?;
    if walked.files.is_empty() {
        return Ok(None);
    }

    let mut total_decls = 0usize;
    let mut file_lines: Vec<String> = Vec::with_capacity(walked.files.len());
    for source in &walked.files {
        let family = outline::family_of(&source.path);
        let decls = outline::decls_in(&source.text, family);
        total_decls += decls.len();
        let names: Vec<String> = decls.iter().filter_map(|decl| decl.name.clone()).collect();
        let line_count = source.text.lines().count();
        file_lines.push(format_map_file_line(
            &source.rel,
            line_count,
            decls.len(),
            &names,
        ));
    }

    let scan_note = scan_note(&walked, cfg);
    render_map_output(&walked.root, &file_lines, total_decls, &scan_note, cfg)
        .map(Some)
        .map_err(|_| incomplete_error(&walked, cfg))
}

/// Testable core: builds `qc repo symbol <name>` output, or `None` when `name` has
/// no exact declaration-site match. Traversal failures return `Err`.
pub(crate) fn build_sym_direct(name: &str, root: &Path, cfg: &MapConfig) -> ScanResult {
    let walked = prepare_walk(root, cfg)?;
    let mut matches: Vec<(String, usize, String)> = Vec::new();
    for source in &walked.files {
        let family = outline::family_of(&source.path);
        for declaration in outline::decls_in(&source.text, family) {
            if declaration.name.as_deref() == Some(name) {
                matches.push((
                    source.rel.clone(),
                    declaration.line_no,
                    declaration.rendered,
                ));
            }
        }
    }
    if matches.is_empty() {
        if walked.diagnostics.has_errors() || walked.truncated {
            return Err(incomplete_error(&walked, cfg));
        }
        return Ok(None);
    }

    let scan_note = scan_note(&walked, cfg);
    render_sym_output(name, &matches, &scan_note, cfg)
        .map(Some)
        .map_err(|_| incomplete_error(&walked, cfg))
}

enum RefsEntry {
    Line(String),
    FileMore(String),
}

/// Testable core: builds `qc repo references <name>` output, or `None` when `name` has
/// no ASCII-identifier-boundary match in any scanned file. Traversal failures
/// return `Err` so a partial scan is never reported as a complete miss.
pub(crate) fn build_refs_direct(name: &str, root: &Path, cfg: &MapConfig) -> ScanResult {
    if name.is_empty() {
        return Ok(None);
    }
    let re = match Regex::new(&format!(
        r"(?:^|[^$A-Za-z0-9_]){}(?:$|[^$A-Za-z0-9_])",
        regex::escape(name)
    )) {
        Ok(re) => re,
        Err(_) => return Err(ScanError::InvalidQuery),
    };
    let walked = prepare_walk(root, cfg)?;

    let mut matches_by_file = Vec::new();
    for source in &walked.files {
        let matches: Vec<_> = source
            .text
            .lines()
            .enumerate()
            .filter_map(|(line_no, line)| {
                re.is_match(line).then_some(RefLine {
                    line_no: line_no + 1,
                    line: line.to_owned(),
                })
            })
            .collect();
        if !matches.is_empty() {
            matches_by_file.push((source.rel.clone(), matches));
        }
    }

    let scan_note = scan_note(&walked, cfg);
    render_refs_output(
        name,
        &matches_by_file,
        &scan_note,
        walked.diagnostics.has_errors() || walked.truncated,
        cfg,
    )
    .map_err(|_| incomplete_error(&walked, cfg))
}

fn incomplete_error_from_index(snapshot: &SourceIndex, cfg: &MapConfig) -> ScanError {
    let diagnostics = snapshot.diagnostics();
    ScanError::Incomplete {
        diagnostics: WalkDiagnostics {
            traversal_errors: diagnostics.traversal_errors,
            read_errors: diagnostics.read_errors,
            symlinks_skipped: diagnostics.symlinks_skipped,
            oversized_files: diagnostics.oversized_files,
            binary_files: diagnostics.binary_files,
            invalid_utf8_files: diagnostics.invalid_utf8_files,
        },
        truncated: snapshot.truncated(),
        max_files: cfg.max_files,
    }
}

fn render_map_output(
    root: &Path,
    file_lines: &[String],
    total_decls: usize,
    scan_note: &str,
    cfg: &MapConfig,
) -> Result<String, ()> {
    let header = format!(
        "[qc-map v1] {} — {} files, {total_decls} declarations\n",
        root.display(),
        file_lines.len()
    );
    let footer = "[qc-map: `qc repo outline <file>` for line numbers; `qc repo symbol <name>` to locate a symbol; `qc repo references <name>` for usages]\n";

    let k_total = file_lines.len();
    let mut prefix = vec![0usize; k_total + 1];
    for (i, line) in file_lines.iter().enumerate() {
        prefix[i + 1] = prefix[i] + line.len();
    }

    let mut shown = k_total;
    loop {
        let marker = if shown == 0 {
            cap_explanation("map")
        } else if shown < k_total {
            format!(
                "[qc-map: +{} more files — run on a subdirectory]\n",
                k_total - shown
            )
        } else {
            String::new()
        };
        let total = header.len() + scan_note.len() + prefix[shown] + marker.len() + footer.len();
        if total <= cfg.max_bytes || shown == 0 {
            if shown == 0 && total > cfg.max_bytes {
                if !scan_note.is_empty() {
                    return Err(());
                }
                return Ok(cap_explanation("map"));
            }
            break;
        }
        shown -= 1;
    }

    let mut out =
        String::with_capacity(header.len() + scan_note.len() + prefix[shown] + footer.len() + 96);
    out.push_str(&header);
    out.push_str(scan_note);
    for line in &file_lines[..shown] {
        out.push_str(line);
    }
    if shown < k_total {
        if shown == 0 {
            out.push_str(&cap_explanation("map"));
        } else {
            out.push_str(&format!(
                "[qc-map: +{} more files — run on a subdirectory]\n",
                k_total - shown
            ));
        }
    }
    out.push_str(footer);
    Ok(out)
}

fn render_sym_output(
    name: &str,
    matches: &[(String, usize, String)],
    scan_note: &str,
    cfg: &MapConfig,
) -> Result<String, ()> {
    let header = format!("[qc-symbol v1] {name} — {} declarations\n", matches.len());
    let lines_text: Vec<String> = matches
        .iter()
        .map(|(relative_path, line_no, rendered)| {
            format!("{relative_path}:{line_no}: {rendered}\n")
        })
        .collect();
    let k_total = lines_text.len();
    let mut prefix = vec![0usize; k_total + 1];
    for (i, line) in lines_text.iter().enumerate() {
        prefix[i + 1] = prefix[i] + line.len();
    }

    let mut shown = k_total;
    loop {
        let marker = if shown == 0 {
            cap_explanation("sym")
        } else if shown < k_total {
            format!("[qc-symbol: +{} more]\n", k_total - shown)
        } else {
            String::new()
        };
        let total = header.len() + scan_note.len() + prefix[shown] + marker.len();
        if total <= cfg.max_bytes || shown == 0 {
            if shown == 0 && total > cfg.max_bytes {
                if !scan_note.is_empty() {
                    return Err(());
                }
                return Ok(cap_explanation("sym"));
            }
            break;
        }
        shown -= 1;
    }

    let mut out = String::with_capacity(header.len() + scan_note.len() + prefix[shown] + 64);
    out.push_str(&header);
    out.push_str(scan_note);
    for line in &lines_text[..shown] {
        out.push_str(line);
    }
    if shown < k_total {
        if shown == 0 {
            out.push_str(&cap_explanation("sym"));
        } else {
            out.push_str(&format!("[qc-symbol: +{} more]\n", k_total - shown));
        }
    }
    Ok(out)
}

pub(crate) fn build_map(snapshot: &SourceIndex, cfg: &MapConfig) -> ScanResult {
    if snapshot.files().is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() {
            return Err(incomplete_error_from_index(snapshot, cfg));
        }
        return Ok(None);
    }

    let mut total_decls = 0usize;
    let mut file_lines = Vec::with_capacity(snapshot.map_summaries().len());
    for summary in snapshot.map_summaries() {
        total_decls += summary.declaration_count;
        file_lines.push(format_map_file_line(
            &summary.relative_path,
            summary.line_count,
            summary.declaration_count,
            &summary.names,
        ));
    }

    render_map_output(
        snapshot.canonical_root(),
        &file_lines,
        total_decls,
        &snapshot.scan_note(),
        cfg,
    )
    .map(Some)
    .map_err(|_| incomplete_error_from_index(snapshot, cfg))
}

pub(crate) fn build_sym(name: &str, snapshot: &SourceIndex, cfg: &MapConfig) -> ScanResult {
    let postings = snapshot.declarations(name).unwrap_or(&[]);
    if postings.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() {
            return Err(incomplete_error_from_index(snapshot, cfg));
        }
        return Ok(None);
    }

    let matches: Vec<_> = postings
        .iter()
        .map(|posting| {
            (
                posting.relative_path.clone(),
                posting.line_no,
                posting.rendered.clone(),
            )
        })
        .collect();
    render_sym_output(name, &matches, &snapshot.scan_note(), cfg)
        .map(Some)
        .map_err(|_| incomplete_error_from_index(snapshot, cfg))
}

struct RefLine {
    line_no: usize,
    line: String,
}

fn render_refs_output(
    name: &str,
    files: &[(String, Vec<RefLine>)],
    scan_note: &str,
    incomplete: bool,
    cfg: &MapConfig,
) -> Result<Option<String>, ()> {
    let mut entries: Vec<RefsEntry> = Vec::new();
    let mut total_lines = 0usize;
    let mut files_with_matches = 0usize;
    let mut logical_cap_hit = false;

    'outer: for (relative_path, matches) in files {
        let mut file_count = 0usize;
        let mut file_total = 0usize;
        let mut file_had_match = false;
        for reference in matches {
            file_had_match = true;
            file_total += 1;
            if file_count < cfg.refs_max_per_file && total_lines < cfg.refs_max_total {
                let trimmed = outline::cap_chars(reference.line.trim(), 160);
                entries.push(RefsEntry::Line(format!(
                    "{}:{}: {trimmed}\n",
                    relative_path, reference.line_no
                )));
                file_count += 1;
                total_lines += 1;
            }
            if total_lines >= cfg.refs_max_total {
                logical_cap_hit = true;
                break;
            }
        }
        if file_had_match {
            files_with_matches += 1;
            if file_total > file_count {
                entries.push(RefsEntry::FileMore(format!(
                    "{}: +{} more\n",
                    relative_path,
                    file_total - file_count
                )));
                logical_cap_hit = true;
            }
        }
        if total_lines >= cfg.refs_max_total {
            break 'outer;
        }
    }

    if total_lines == 0 {
        if incomplete || (files_with_matches > 0 && !scan_note.is_empty()) {
            return Err(());
        }
        if files_with_matches > 0 {
            return Ok(Some(cap_explanation("refs")));
        }
        return Ok(None);
    }

    let header =
        format!("[qc-references v1] {name} — {total_lines} lines in {files_with_matches} files\n");
    let body: Vec<String> = entries
        .into_iter()
        .map(|entry| match entry {
            RefsEntry::Line(line) => line,
            RefsEntry::FileMore(line) => line,
        })
        .collect();
    let footer_marker = format!(
        "[qc-references: capped at {} per file / {} total]\n",
        cfg.refs_max_per_file, cfg.refs_max_total
    );

    let k_total = body.len();
    let mut prefix = vec![0usize; k_total + 1];
    for (i, line) in body.iter().enumerate() {
        prefix[i + 1] = prefix[i] + line.len();
    }

    let mut shown = k_total;
    let mut footer_needed = logical_cap_hit;
    loop {
        let marker = if shown == 0 {
            cap_explanation("refs")
        } else {
            String::new()
        };
        let footer_len = if footer_needed {
            footer_marker.len()
        } else {
            0
        };
        let total = header.len() + scan_note.len() + prefix[shown] + marker.len() + footer_len;
        if total <= cfg.max_bytes || shown == 0 {
            if shown == 0 && total > cfg.max_bytes {
                if !scan_note.is_empty() {
                    return Err(());
                }
                return Ok(Some(cap_explanation("refs")));
            }
            break;
        }
        shown -= 1;
        footer_needed = true;
    }

    let mut out = String::with_capacity(
        header.len() + scan_note.len() + prefix[shown] + footer_marker.len() + 32,
    );
    out.push_str(&header);
    out.push_str(scan_note);
    for line in &body[..shown] {
        out.push_str(line);
    }
    if shown == 0 && shown < k_total {
        out.push_str(&cap_explanation("refs"));
    }
    if footer_needed {
        out.push_str(&footer_marker);
    }
    Ok(Some(out))
}

fn identifier_chars(query: &str) -> bool {
    !query.is_empty()
        && query
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$')
}

fn reference_regex(query: &str) -> Result<Regex, ScanError> {
    Regex::new(&format!(
        r"(?:^|[^$A-Za-z0-9_]){}(?:$|[^$A-Za-z0-9_])",
        regex::escape(query)
    ))
    .map_err(|_| ScanError::InvalidQuery)
}

pub(crate) fn build_refs(name: &str, snapshot: &SourceIndex, cfg: &MapConfig) -> ScanResult {
    if name.is_empty() {
        return Ok(None);
    }

    let mut matches_by_file = std::collections::BTreeMap::<String, Vec<RefLine>>::new();
    if identifier_chars(name) {
        if let Some(postings) = snapshot.verified_references(name) {
            for posting in postings {
                matches_by_file
                    .entry(posting.relative_path)
                    .or_default()
                    .push(RefLine {
                        line_no: posting.line_no,
                        line: posting.line,
                    });
            }
        }
    } else {
        let re = reference_regex(name)?;
        for file in snapshot.files() {
            for (line_no, line) in file.source.lines().enumerate() {
                if re.is_match(line) {
                    matches_by_file
                        .entry(file.relative_path.clone())
                        .or_default()
                        .push(RefLine {
                            line_no: line_no + 1,
                            line: line.to_owned(),
                        });
                }
            }
        }
    }

    let files: Vec<_> = snapshot
        .files()
        .iter()
        .filter_map(|file| {
            matches_by_file
                .remove(&file.relative_path)
                .map(|matches| (file.relative_path.clone(), matches))
        })
        .collect();
    let incomplete = snapshot.diagnostics().has_errors() || snapshot.truncated();
    render_refs_output(name, &files, &snapshot.scan_note(), incomplete, cfg)
        .map_err(|_| incomplete_error_from_index(snapshot, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repomap_index;
    use std::io::Write;

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "qc-repomap-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_file(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn cfg() -> MapConfig {
        MapConfig::default()
    }

    fn output(result: ScanResult) -> String {
        result.unwrap().expect("expected output")
    }

    fn fixture(dir: &Path) {
        write_file(
            dir,
            "src/main.rs",
            "pub fn main() {\n    helper();\n}\n\npub fn helper() {\n    println!(\"hi\");\n}\n",
        );
        write_file(
            dir,
            "src/lib.rs",
            "pub struct Widget {\n    pub name: String,\n}\n",
        );
        write_file(dir, ".hidden.rs", "pub fn hidden() {}\n");
        write_file(
            dir,
            "node_modules/pkg/index.js",
            "export function skip() {}\n",
        );
        write_file(dir, "notes.txt", "just plain text, not source\n");
        let mut big = String::new();
        for i in 0..10 {
            big.push_str(&format!("line {i} filler filler filler\n"));
        }
        write_file(dir, "big.rs", &big);
        let mut binary = Vec::new();
        binary.extend_from_slice(b"pub fn bin() {\0garbage");
        std::fs::write(dir.join("bin.rs"), &binary).unwrap();
    }

    #[test]
    fn walker_honors_root_and_nested_ignores_without_consuming_cap() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        write_file(&dir, ".git/HEAD", "ref: refs/heads/main\n");
        write_file(&dir, ".gitignore", "root-ignored.rs\n!root-kept.rs\n");
        write_file(&dir, ".ignore", "ignore-ignored.rs\n");
        write_file(
            &dir,
            "nested/.gitignore",
            "nested-ignored.rs\n!nested-kept.rs\n",
        );
        write_file(&dir, "root-ignored.rs", "pub fn root_ignored() {}\n");
        write_file(&dir, "ignore-ignored.rs", "pub fn ignore_ignored() {}\n");
        write_file(&dir, "root-kept.rs", "pub fn root_kept() {}\n");
        write_file(
            &dir,
            "nested/nested-ignored.rs",
            "pub fn nested_ignored() {}\n",
        );
        write_file(&dir, "nested/nested-kept.rs", "pub fn nested_kept() {}\n");

        let mut c = cfg();
        c.max_files = 2;
        let walked = walk(&dir, &c).unwrap();
        let rels: Vec<&str> = walked
            .files
            .iter()
            .map(|source| source.rel.as_str())
            .collect();
        assert_eq!(rels, ["nested/nested-kept.rs", "root-kept.rs"]);
        assert!(!walked.truncated);
        assert!(build_map_direct(&dir, &c)
            .unwrap()
            .unwrap()
            .contains("nested_kept"));
        assert!(build_sym_direct("nested_ignored", &dir, &c)
            .unwrap()
            .is_none());
        assert!(build_refs_direct("nested_ignored", &dir, &c)
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_scans_explicit_root_named_like_skip_dir() {
        let parent = tempdir();
        let root = parent.join("target");
        write_file(&root, "inside.rs", "pub fn inside() {}\n");

        let walked = walk(&root, &cfg()).unwrap();
        assert_eq!(walked.files.len(), 1);
        assert_eq!(walked.files[0].rel, "inside.rs");
        std::fs::remove_dir_all(&parent).ok();
    }

    #[test]
    fn walker_uses_skip_dirs_without_git_repository() {
        let dir = tempdir();
        write_file(&dir, "src/kept.rs", "pub fn kept() {}\n");
        for skipped in SKIP_DIRS {
            write_file(
                &dir,
                &format!("{skipped}/ignored.rs"),
                "pub fn ignored() {}\n",
            );
        }

        let walked = walk(&dir, &cfg()).unwrap();
        assert_eq!(walked.files.len(), 1);
        assert_eq!(walked.files[0].rel, "src/kept.rs");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_skips_and_sorts() {
        let dir = tempdir();
        fixture(&dir);
        let walked = walk(&dir, &cfg()).unwrap();
        let rels: Vec<&str> = walked
            .files
            .iter()
            .map(|source| source.rel.as_str())
            .collect();
        assert!(!rels.iter().any(|r| r.contains("node_modules")));
        assert!(!rels.iter().any(|r| r.starts_with('.')));
        assert!(!rels.contains(&"notes.txt"));
        assert!(!rels.contains(&"bin.rs"));
        assert!(rels.contains(&"src/main.rs"));
        assert!(rels.contains(&"src/lib.rs"));
        let mut sorted = rels.clone();
        sorted.sort();
        assert_eq!(rels, sorted);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_respects_max_file_bytes() {
        let dir = tempdir();
        write_file(dir.as_path(), "small.rs", "pub fn a() {}\n");
        let mut c = cfg();
        c.max_file_bytes = 5;
        let walked = walk(&dir, &c).unwrap();
        assert!(walked.files.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_max_files_cap_truncates() {
        let dir = tempdir();
        for i in 0..5 {
            write_file(&dir, &format!("f{i}.rs"), "pub fn x() {}\n");
        }
        let mut c = cfg();
        c.max_files = 2;
        let walked = walk(&dir, &c).unwrap();
        assert_eq!(walked.files.len(), 2);
        assert!(walked.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn map_deterministic_and_content() {
        let dir = tempdir();
        fixture(&dir);
        let out1 = output(build_map_direct(&dir, &cfg()));
        let out2 = output(build_map_direct(&dir, &cfg()));
        assert_eq!(out1, out2);
        assert!(out1.starts_with("[qc-map v1]"));
        assert!(out1.contains("src/main.rs (7L): main, helper"));
        assert!(out1.contains("src/lib.rs (3L): Widget"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn map_empty_dir_returns_none() {
        let dir = tempdir();
        assert!(build_map_direct(&dir, &cfg()).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn map_byte_cap_truncates() {
        let dir = tempdir();
        for i in 0..50 {
            write_file(
                &dir,
                &format!("f{i}.rs"),
                &format!("pub fn decl_{i}() {{}}\n"),
            );
        }
        let mut c = cfg();
        c.max_bytes = 300;
        let out = output(build_map_direct(&dir, &c));
        assert!(out.contains("[qc-map: +"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sym_finds_exact_name_only() {
        let dir = tempdir();
        fixture(&dir);
        let out = output(build_sym_direct("helper", &dir, &cfg()));
        assert!(out.starts_with("[qc-symbol v1] helper"));
        assert!(out.contains("src/main.rs:5: pub fn helper()"));
        assert!(!out.contains("main.rs:1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sym_unknown_returns_none() {
        let dir = tempdir();
        fixture(&dir);
        assert!(build_sym_direct("nosuchsymbol", &dir, &cfg())
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sym_deterministic() {
        let dir = tempdir();
        fixture(&dir);
        let out1 = output(build_sym_direct("main", &dir, &cfg()));
        let out2 = output(build_sym_direct("main", &dir, &cfg()));
        assert_eq!(out1, out2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_whole_word_match() {
        let dir = tempdir();
        fixture(&dir);
        let out = output(build_refs_direct("helper", &dir, &cfg()));
        assert!(out.starts_with("[qc-references v1] helper"));
        assert!(out.contains("src/main.rs:2:"));
        assert!(out.contains("src/main.rs:5:"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_unknown_returns_none() {
        let dir = tempdir();
        fixture(&dir);
        assert!(build_refs_direct("nosuchsymbol", &dir, &cfg())
            .unwrap()
            .is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_deterministic() {
        let dir = tempdir();
        fixture(&dir);
        let out1 = output(build_refs_direct("main", &dir, &cfg()));
        let out2 = output(build_refs_direct("main", &dir, &cfg()));
        assert_eq!(out1, out2);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn assert_render_equivalent(direct: ScanResult, indexed: ScanResult) {
        match (direct, indexed) {
            (Ok(left), Ok(right)) => assert_eq!(left, right),
            (Err(left), Err(right)) => assert_eq!(left.to_string(), right.to_string()),
            (left, right) => panic!("render results differ: {left:?} vs {right:?}"),
        }
    }

    #[test]
    fn direct_and_index_renderers_are_byte_identical() {
        let dir = tempdir();
        fixture(&dir);
        write_file(&dir, "commerce/a.rs", "pub fn commerce() {}\n");
        write_file(&dir, "commerce-storefront/a.rs", "pub fn storefront() {}\n");
        let mut map_cfg = cfg();
        let index =
            repomap_index::scan_root(&dir, &repomap_index::ScanConfig::from_map(&map_cfg)).unwrap();

        assert_render_equivalent(
            build_map_direct(&dir, &map_cfg),
            build_map(&index, &map_cfg),
        );
        for name in ["helper", "nosuchsymbol"] {
            assert_render_equivalent(
                build_sym_direct(name, &dir, &map_cfg),
                build_sym(name, &index, &map_cfg),
            );
            assert_render_equivalent(
                build_refs_direct(name, &dir, &map_cfg),
                build_refs(name, &index, &map_cfg),
            );
        }

        map_cfg.max_bytes = 300;
        map_cfg.refs_max_per_file = 1;
        map_cfg.refs_max_total = 2;
        assert_render_equivalent(
            build_map_direct(&dir, &map_cfg),
            build_map(&index, &map_cfg),
        );
        assert_render_equivalent(
            build_refs_direct("helper", &dir, &map_cfg),
            build_refs("helper", &index, &map_cfg),
        );

        let mut partial_cfg = cfg();
        partial_cfg.max_files = 1;
        let partial_index =
            repomap_index::scan_root(&dir, &repomap_index::ScanConfig::from_map(&partial_cfg))
                .unwrap();
        assert_render_equivalent(
            build_map_direct(&dir, &partial_cfg),
            build_map(&partial_index, &partial_cfg),
        );
        assert_render_equivalent(
            build_sym_direct("nosuchsymbol", &dir, &partial_cfg),
            build_sym("nosuchsymbol", &partial_index, &partial_cfg),
        );
        assert_render_equivalent(
            build_refs_direct("nosuchsymbol", &dir, &partial_cfg),
            build_refs("nosuchsymbol", &partial_index, &partial_cfg),
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_per_file_cap_truncates() {
        let dir = tempdir();
        let mut src = String::new();
        for _ in 0..10 {
            src.push_str("let target = target + 1;\n");
        }
        write_file(&dir, "many.rs", &src);
        let mut c = cfg();
        c.refs_max_per_file = 3;
        let out = output(build_refs_direct("target", &dir, &c));
        assert!(out.contains("many.rs: +"));
        assert!(out.contains("[qc-references: capped at 3 per file"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_total_cap_truncates() {
        let dir = tempdir();
        for i in 0..5 {
            let mut src = String::new();
            for _ in 0..3 {
                src.push_str("let needle = needle + 1;\n");
            }
            write_file(&dir, &format!("f{i}.rs"), &src);
        }
        let mut c = cfg();
        c.refs_max_total = 4;
        let out = output(build_refs_direct("needle", &dir, &c));
        assert!(out.contains("[qc-references: capped at"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn map_byte_cap_without_file_room_returns_explanation() {
        let dir = tempdir();
        write_file(&dir, "main.rs", "pub fn main() {}\n");
        let mut c = cfg();
        c.max_bytes = 32;

        let out = output(build_map_direct(&dir, &c));
        assert!(out.contains("[qc-map: output cap too small"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sym_byte_cap_without_line_room_returns_explanation() {
        let dir = tempdir();
        write_file(&dir, "main.rs", "pub fn main() {}\n");
        let mut c = cfg();
        c.max_bytes = 32;

        let out = output(build_sym_direct("main", &dir, &c));
        assert!(out.contains("[qc-symbol: output cap too small"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_byte_cap_without_line_room_returns_explanation() {
        let dir = tempdir();
        write_file(&dir, "main.rs", "fn main() { main(); }\n");
        let mut c = cfg();
        c.max_bytes = 32;

        let out = output(build_refs_direct("main", &dir, &c));
        assert!(out.contains("[qc-references: output cap too small"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_traversal_error_is_explicit() {
        let dir = tempdir();
        let file = write_file(&dir, "not-a-directory.rs", "pub fn main() {}\n");

        let result = build_map_direct(&file, &cfg());
        assert!(format!("{result:?}").starts_with("Err("));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_and_are_reported() {
        use std::os::unix::fs::symlink;

        let dir = tempdir();
        write_file(&dir, "real.rs", "pub fn real() {}\n");
        symlink(dir.join("real.rs"), dir.join("file-link.rs")).unwrap();
        std::fs::create_dir_all(dir.join("real-dir")).unwrap();
        write_file(&dir, "real-dir/nested.rs", "pub fn nested() {}\n");
        symlink(dir.join("real-dir"), dir.join("dir-link")).unwrap();

        let out = output(build_map_direct(&dir, &cfg()));
        assert!(out.contains("symlinks skipped: 2"));
        assert!(!out.contains("file-link.rs"));
        assert!(!out.contains("dir-link/nested.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refs_use_ascii_identifier_boundaries_for_dollar_names() {
        let dir = tempdir();
        write_file(
            &dir,
            "symbols.rs",
            "let $foo = 1;\nlet foo = 2;\nlet prefixfoo = 3;\nlet $foobar = 4;\n",
        );

        let dollar = output(build_refs_direct("$foo", &dir, &cfg()));
        assert!(dollar.contains("symbols.rs:1:"));
        assert!(!dollar.contains("symbols.rs:4:"));

        let plain = output(build_refs_direct("foo", &dir, &cfg()));
        assert!(!plain.contains("symbols.rs:1:"));
        assert!(plain.contains("symbols.rs:2:"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_max_files_exact_boundary_not_truncated() {
        let dir = tempdir();
        for i in 0..3 {
            write_file(&dir, &format!("f{i}.rs"), "pub fn x() {}\n");
        }
        let mut c = cfg();
        c.max_files = 3;
        let walked = walk(&dir, &c).unwrap();
        assert_eq!(walked.files.len(), 3);
        assert!(!walked.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_max_files_boundary_plus_one_is_truncated() {
        let dir = tempdir();
        for i in 0..4 {
            write_file(&dir, &format!("f{i}.rs"), "pub fn x() {}\n");
        }
        let mut c = cfg();
        c.max_files = 3;
        let walked = walk(&dir, &c).unwrap();
        assert_eq!(walked.files.len(), 3);
        assert!(walked.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn walker_max_files_exact_boundary_with_trailing_ineligible_entries_not_truncated() {
        let dir = tempdir();
        for i in 0..3 {
            write_file(&dir, &format!("f{i}.rs"), "pub fn x() {}\n");
        }
        write_file(&dir, "zz-notes.txt", "not source\n");
        write_file(&dir, ".zz-hidden.rs", "pub fn hidden() {}\n");
        std::fs::create_dir_all(dir.join("zz-emptydir")).unwrap();
        let mut c = cfg();
        c.max_files = 3;
        let walked = walk(&dir, &c).unwrap();
        assert_eq!(walked.files.len(), 3);
        assert!(!walked.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn read_source_rejects_symlink_via_no_follow() {
        use std::os::unix::fs::symlink;
        let dir = tempdir();
        write_file(&dir, "real.rs", "pub fn real() {}\n");
        let link = dir.join("link.rs");
        symlink(dir.join("real.rs"), &link).unwrap();
        assert!(read_source(&link, 4096).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn read_source_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let dir = tempdir();
        let link = dir.join("dangling.rs");
        symlink(dir.join("does-not-exist.rs"), &link).unwrap();
        assert!(read_source(&link, 4096).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn read_source_rejects_symlink_loop() {
        use std::os::unix::fs::symlink;
        let dir = tempdir();
        let link = dir.join("loop.rs");
        symlink(&link, &link).unwrap();
        assert!(read_source(&link, 4096).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn nested_read_error_is_surfaced_not_discarded() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        write_file(&dir, "src/ok.rs", "pub fn ok() {}\n");
        let unreadable = write_file(&dir, "src/nested/blocked.rs", "pub fn blocked() {}\n");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let out = output(build_map_direct(&dir, &cfg()));
        assert!(out.contains("[qc-scan: partial"));
        assert!(out.contains("unreadable files: 1"));
        assert!(out.contains("src/ok.rs"));

        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
