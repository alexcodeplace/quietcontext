use ignore::WalkBuilder;
use regex::Regex;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::MapConfig;
use crate::outline;
use crate::semantic::{EdgeKind, NodeId, SemanticGraph, TraversalStep};
use crate::repomap_index::{self, SourceIndex};

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
    Index(String),
    Incomplete {
        diagnostics: WalkDiagnostics,
        truncated: bool,
        max_files: usize,
    },
}

impl From<repomap_index::ScanError> for ScanError {
    fn from(error: repomap_index::ScanError) -> Self {
        Self::Index(error.to_string())
    }
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => write!(f, "scan root is unavailable"),
            Self::RootNotDirectory => write!(f, "scan root is not a directory"),
            Self::InvalidQuery => write!(f, "invalid symbol query"),
            Self::Index(error) => write!(f, "semantic index unavailable: {error}"),
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
fn direct_snapshot(root: &Path, cfg: &MapConfig) -> Result<SourceIndex, ScanError> {
    let scan = repomap_index::ScanConfig::from_map(cfg);
    let inputs = repomap_index::scan_inputs(root, &scan).map_err(|error| ScanError::Index(error.to_string()))?;
    repomap_index::build_generation(&inputs, &scan, 1).map_err(|error| ScanError::Index(error.to_string()))
}

pub(crate) fn build_refs_direct(name: &str, root: &Path, cfg: &MapConfig) -> ScanResult {
    let snapshot = direct_snapshot(root, cfg)?;
    build_refs(name, &snapshot, cfg)
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

pub(crate) fn build_explore_direct(query: &str, root: &Path, cfg: &MapConfig) -> ScanResult {
    let snapshot = direct_snapshot(root, cfg)?;
    build_explore(query, &snapshot, cfg)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrontloadCandidate {
    file_index: usize,
    file: String,
    name: String,
    line_no: usize,
    score: u32,
}

fn frontload_terms(query: &str) -> Vec<String> {
    let mut terms = explore_terms(query);
    terms.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then_with(|| a.cmp(b)));
    let strong: Vec<String> = terms
        .iter()
        .filter(|term| term.chars().count() >= 5 || term.contains('_') || term.contains('$'))
        .cloned()
        .collect();
    if strong.is_empty() { terms } else { strong }
}

fn frontload_excerpt(source: &str, line_no: usize, radius: usize) -> String {
    let start = line_no.saturating_sub(radius).max(1);
    let end = line_no.saturating_add(radius);
    let mut out = String::new();
    for (index, line) in source.lines().enumerate() {
        let current = index + 1;
        if current < start { continue; }
        if current > end { break; }
        out.push_str(&format!("{current}\t{line}\n"));
    }
    out
}

pub(crate) fn build_frontload_direct(query: &str, root: &Path, cfg: &MapConfig) -> ScanResult {
    let mut scan_cfg = cfg.clone();
    scan_cfg.max_file_bytes = scan_cfg.max_file_bytes.min(256 * 1024);
    let walked = walk(root, &scan_cfg)?;
    let terms = frontload_terms(query);
    if terms.is_empty() {
        return Ok(Some(format!(
            "[qc-frontload v1] {query}\nNo high-confidence symbol/file match. Use repo explore with a more specific symbol/file name.\n"
        )));
    }

    let query_lower = query.to_lowercase();
    let mut candidates: Vec<FrontloadCandidate> = Vec::new();
    let mut exact_found = false;

    for (file_index, file) in walked.files.iter().enumerate() {
        let file_lower = file.rel.to_lowercase();
        let source_lower = file.text.to_lowercase();
        let matching_terms: Vec<&String> = terms
            .iter()
            .filter(|term| file_lower.contains(term.as_str()) || source_lower.contains(term.as_str()))
            .collect();
        if matching_terms.is_empty() { continue; }

        let family = outline::family_of(&file.path);
        let declarations = outline::decls_in(&file.text, family);
        let mut file_had_declaration = false;
        for declaration in declarations {
            let Some(name) = declaration.name else { continue; };
            let name_lower = name.to_lowercase();
            let mut score = 0u32;
            if name_lower.chars().count() >= 3 && query_lower.contains(&name_lower) { score += 120; }
            for term in &matching_terms {
                if name_lower == term.as_str() { score += 160; }
                else if name_lower.contains(term.as_str()) { score += 55; }
                if file_lower.contains(term.as_str()) { score += 20; }
            }
            if score == 0 { continue; }
            file_had_declaration = true;
            exact_found |= score >= 260;
            candidates.push(FrontloadCandidate {
                file_index,
                file: file.rel.clone(),
                name,
                line_no: declaration.line_no,
                score,
            });
        }

        if !file_had_declaration {
            let mut best: Option<(usize, &String)> = None;
            for (line_index, line) in source_lower.lines().enumerate() {
                if let Some(term) = matching_terms.iter().find(|term| line.contains(term.as_str())) {
                    best = Some((line_index + 1, *term));
                    break;
                }
            }
            if let Some((line_no, term)) = best {
                let score = 25 + if file_lower.contains(term.as_str()) { 30 } else { 0 };
                candidates.push(FrontloadCandidate {
                    file_index,
                    file: file.rel.clone(),
                    name: term.clone(),
                    line_no,
                    score,
                });
            }
        }

        if exact_found { break; }
    }

    candidates.sort_by(|a, b| {
        b.score.cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line_no.cmp(&b.line_no))
    });
    candidates.dedup_by(|a, b| a.file == b.file && a.line_no == b.line_no && a.name == b.name);
    candidates.truncate(3);

    let Some(primary) = candidates.first() else {
        return Ok(Some(format!(
            "[qc-frontload v1] {query}\nNo high-confidence symbol/file match. Use repo explore with a more specific symbol/file name.\n"
        )));
    };
    let source = &walked.files[primary.file_index].text;
    let excerpt = frontload_excerpt(source, primary.line_no, 5);
    let mut lines = vec![
        format!("[qc-frontload v1] {query}\n"),
        format!("## {} - {}:{}\n", primary.name, primary.file, primary.line_no),
    ];
    if !excerpt.is_empty() { lines.push(format!("[source]\n{excerpt}")); }
    if candidates.len() > 1 {
        lines.push("[related candidates]\n".to_owned());
        for candidate in &candidates[1..] {
            lines.push(format!("  {} - {}:{}\n", candidate.name, candidate.file, candidate.line_no));
        }
    }
    if walked.truncated {
        lines.push(format!("[scan bounded at {} source files]\n", scan_cfg.max_files));
    }
    lines.push("[qc-frontload] Source context is already inspected; use repo explore for graph relationships or broader context.\n".to_owned());
    Ok(Some(bounded_graph_output(lines, cfg.max_bytes.min(4 * 1024), "frontload")))
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

    let graph = snapshot.semantic_graph()?;
    let semantic_roots = graph.select_symbols(name, None);
    if !semantic_roots.is_empty() {
        let mut lines = vec![format!("[qc-references v2] {name} - {} definitions\n", semantic_roots.len())];
        let mut semantic_hits = 0usize;
        for root in semantic_roots {
            lines.push(format!("{}\n", semantic_node_label(graph, root)));
            let mut seen = std::collections::BTreeSet::new();
            for edge in graph.resolved_reference_edges(root) {
                let Some(edge_file) = graph.edge_file_path(edge) else { continue; };
                if !seen.insert((edge_file.to_owned(), edge.line)) { continue; }
                let source_line = snapshot.files().iter()
                    .find(|file| file.relative_path == edge_file)
                    .and_then(|file| file.source.lines().nth(edge.line.saturating_sub(1)))
                    .unwrap_or_default()
                    .trim();
                lines.push(format!("  {}:{}: {} [{}]\n", edge_file, edge.line, outline::cap_chars(source_line, 160), edge.kind.as_str()));
                semantic_hits += 1;
            }
        }
        if semantic_hits == 0 {
            lines.push("  (no resolved semantic references)\n".to_owned());
        }
        return Ok(Some(bounded_graph_output(lines, cfg.max_bytes, "references")));
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



#[derive(Clone, Debug, Eq, PartialEq)]
struct ExploreCandidate {
    file: String,
    name: String,
    score: u32,
}

fn explore_terms(query: &str) -> Vec<String> {
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
        if current.is_empty() { return; }
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

fn rank_explore_candidates(query: &str, snapshot: &SourceIndex, limit: usize) -> Vec<ExploreCandidate> {
    let query_lower = query.to_lowercase();
    let terms = explore_terms(query);
    if terms.is_empty() { return Vec::new(); }
    let mut candidates = Vec::new();
    for summary in snapshot.map_summaries() {
        let file_lower = summary.relative_path.to_lowercase();
        let base = Path::new(&summary.relative_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase();
        for name in &summary.names {
            let symbol = name.to_lowercase();
            if symbol.chars().count() < 2 { continue; }
            let mut score = if symbol.chars().count() >= 3 && query_lower.contains(&symbol) { 120 } else { 0 };
            for term in &terms {
                if term == &symbol { score += 100; }
                else if symbol.contains(term) { score += 42; }
                else if term.chars().count() >= 4 && symbol.chars().count() >= 3 && term.contains(&symbol) { score += 28; }
                if term == &base { score += 70; }
                else if file_lower.contains(term) { score += 20; }
            }
            if score > 0 {
                candidates.push(ExploreCandidate {
                    file: summary.relative_path.clone(),
                    name: name.clone(),
                    score,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.score.cmp(&a.score)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    candidates.dedup_by(|a, b| a.file == b.file && a.name == b.name);
    candidates.truncate(limit);
    candidates
}

fn explore_declaration_line(snapshot: &SourceIndex, candidate: &ExploreCandidate) -> Option<usize> {
    snapshot
        .declarations(&candidate.name)?
        .iter()
        .find(|posting| posting.relative_path == candidate.file)
        .map(|posting| posting.line_no)
}

fn explore_source_excerpt(snapshot: &SourceIndex, file: &str, line_no: usize, radius: usize) -> Option<String> {
    let record = snapshot.files().iter().find(|record| record.relative_path == file)?;
    let start = line_no.saturating_sub(radius).max(1);
    let end = line_no.saturating_add(radius);
    let mut out = String::new();
    for (index, line) in record.source.lines().enumerate() {
        let current = index + 1;
        if current < start { continue; }
        if current > end { break; }
        out.push_str(&format!("{current}\t{line}\n"));
    }
    (!out.is_empty()).then_some(out)
}

fn append_warm_relation(
    out: &mut Vec<String>,
    label: &str,
    steps: &[TraversalStep],
    graph: &SemanticGraph,
    max_items: usize,
) {
    if steps.is_empty() { return; }
    out.push(format!("[{label}]\n"));
    for step in steps.iter().take(max_items) {
        out.push(format!(
            "  {} {}\n",
            if label == "callers" || label == "impact" { "<-" } else { "->" },
            semantic_node_label(graph, step.node),
        ));
    }
}

pub(crate) fn build_explore(query: &str, snapshot: &SourceIndex, cfg: &MapConfig) -> ScanResult {
    let candidates = rank_explore_candidates(query, snapshot, 3);
    if candidates.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() {
            return Err(incomplete_error_from_index(snapshot, cfg));
        }
        return Ok(Some(format!(
            "[qc-explore v1] {query}\nNo high-confidence symbol/file match. Use a more specific symbol/file name.\n"
        )));
    }

    let primary = &candidates[0];
    let line_no = explore_declaration_line(snapshot, primary).unwrap_or(1);
    let mut lines = vec![
        format!("[qc-explore v1] {query}\n"),
        format!("## {} - {}:{}\n", primary.name, primary.file, line_no),
    ];
    if let Some(source) = explore_source_excerpt(snapshot, &primary.file, line_no, 5) {
        lines.push(format!("[source]\n{source}"));
    }
    if candidates.len() > 1 {
        lines.push("[related candidates]\n".to_owned());
        for candidate in &candidates[1..] {
            let line = explore_declaration_line(snapshot, candidate).unwrap_or(1);
            lines.push(format!("  {} - {}:{}\n", candidate.name, candidate.file, line));
        }
    }

    if let Some(Ok(graph)) = snapshot.semantic_graph_if_ready() {
        let roots = graph.select_symbols(&primary.name, Some(&primary.file));
        if !roots.is_empty() {
            let callers = graph.callers(&roots, 1, 8);
            let callees = graph.callees(&roots, 1, 8);
            let impact = graph.impact(&roots, 2, 10);
            append_warm_relation(&mut lines, "callers", &callers, graph, 5);
            append_warm_relation(&mut lines, "callees", &callees, graph, 5);
            append_warm_relation(&mut lines, "impact", &impact, graph, 6);
        }
    } else {
        lines.push("[semantic graph cold: use callers/callees/impact for deeper follow-up]\n".to_owned());
    }

    lines.push("[qc-explore] Repository context above is already inspected; avoid duplicate exploratory Read/Grep unless incomplete or stale.\n".to_owned());
    Ok(Some(bounded_graph_output(lines, cfg.max_bytes.min(8 * 1024), "explore")))
}

fn semantic_node_label(graph: &SemanticGraph, id: NodeId) -> String {
    let Some(node) = graph.node(id) else { return format!("node:{id}"); };
    let file = graph.file_path(id).unwrap_or("?");
    if node.kind == crate::semantic::NodeKind::File {
        return file.to_owned();
    }
    format!("{} [{}] - {}:{}", node.qualified_name, node.kind.as_str(), file, node.start_line)
}
fn bounded_graph_output(mut lines: Vec<String>, max_bytes: usize, label: &str) -> String {
    let total: usize = lines.iter().map(String::len).sum();
    if total <= max_bytes { return lines.concat(); }
    let marker = format!("[qc-{label}: output capped; narrow the query or reduce depth]\n");
    while lines.len() > 1 && lines.iter().map(String::len).sum::<usize>().saturating_add(marker.len()) > max_bytes {
        lines.pop();
    }
    let retained = lines.iter().map(String::len).sum::<usize>();
    if retained.saturating_add(marker.len()) > max_bytes {
        return cap_explanation(label);
    }
    let mut out = lines.concat();
    out.push_str(&marker);
    out
}

fn graph_roots(
    graph: &SemanticGraph,
    query: &str,
    file_filter: Option<&str>,
    target_may_be_file: bool,
) -> Vec<NodeId> {
    if target_may_be_file { graph.select_target(query, file_filter) } else { graph.select_symbols(query, file_filter) }
}

fn render_steps(
    label: &str,
    query: &str,
    roots: &[NodeId],
    steps: &[TraversalStep],
    graph: &SemanticGraph,
    depth: usize,
    max_bytes: usize,
) -> String {
    let mut lines = vec![format!("[qc-{label} v1] {query} - {} definitions, depth {depth}\n", roots.len())];
    for &root in roots {
        lines.push(format!("{}\n", semantic_node_label(graph, root)));
        for step in steps.iter().filter(|step| step.root == root) {
            lines.push(format!(
                "  d{} {} {} [{} @{}:{}]\n",
                step.depth,
                if label == "callers" || label == "impact" || label == "dependents" { "<-" } else { "->" },
                semantic_node_label(graph, step.node),
                step.via.as_str(),
                step.line,
                step.column,
            ));
        }
    }
    bounded_graph_output(lines, max_bytes, label)
}

fn semantic_incomplete(snapshot: &SourceIndex, cfg: &MapConfig) -> ScanError {
    incomplete_error_from_index(snapshot, cfg)
}

pub(crate) fn build_callers(
    query: &str,
    file_filter: Option<&str>,
    depth: usize,
    max_nodes: usize,
    snapshot: &SourceIndex,
    cfg: &MapConfig,
) -> ScanResult {
    let graph = snapshot.semantic_graph()?;
    let roots = graph_roots(graph, query, file_filter, false);
    if roots.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() { return Err(semantic_incomplete(snapshot, cfg)); }
        return Ok(None);
    }
    let steps = graph.callers(&roots, depth, max_nodes);
    Ok(Some(render_steps("callers", query, &roots, &steps, graph, depth, cfg.max_bytes)))
}

pub(crate) fn build_callees(
    query: &str,
    file_filter: Option<&str>,
    depth: usize,
    max_nodes: usize,
    snapshot: &SourceIndex,
    cfg: &MapConfig,
) -> ScanResult {
    let graph = snapshot.semantic_graph()?;
    let roots = graph_roots(graph, query, file_filter, false);
    if roots.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() { return Err(semantic_incomplete(snapshot, cfg)); }
        return Ok(None);
    }
    let steps = graph.callees(&roots, depth, max_nodes);
    Ok(Some(render_steps("callees", query, &roots, &steps, graph, depth, cfg.max_bytes)))
}

pub(crate) fn build_impact(
    query: &str,
    file_filter: Option<&str>,
    depth: usize,
    max_nodes: usize,
    snapshot: &SourceIndex,
    cfg: &MapConfig,
) -> ScanResult {
    let graph = snapshot.semantic_graph()?;
    let roots = graph_roots(graph, query, file_filter, false);
    if roots.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() { return Err(semantic_incomplete(snapshot, cfg)); }
        return Ok(None);
    }
    let steps = graph.impact(&roots, depth, max_nodes);
    Ok(Some(render_steps("impact", query, &roots, &steps, graph, depth, cfg.max_bytes)))
}

pub(crate) fn build_dependencies(
    query: &str,
    file_filter: Option<&str>,
    incoming: bool,
    depth: usize,
    max_nodes: usize,
    snapshot: &SourceIndex,
    cfg: &MapConfig,
) -> ScanResult {
    let graph = snapshot.semantic_graph()?;
    let roots = graph_roots(graph, query, file_filter, true);
    if roots.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() { return Err(semantic_incomplete(snapshot, cfg)); }
        return Ok(None);
    }
    let steps = graph.dependencies(&roots, incoming, depth, max_nodes);
    let label = if incoming { "dependents" } else { "deps" };
    Ok(Some(render_steps(label, query, &roots, &steps, graph, depth, cfg.max_bytes)))
}

pub(crate) fn build_path(
    from: &str,
    to: &str,
    max_depth: usize,
    max_nodes: usize,
    snapshot: &SourceIndex,
    cfg: &MapConfig,
) -> ScanResult {
    let graph = snapshot.semantic_graph()?;
    let starts = graph.select_target(from, None);
    let targets = graph.select_target(to, None);
    if starts.is_empty() || targets.is_empty() {
        if snapshot.diagnostics().has_errors() || snapshot.truncated() { return Err(semantic_incomplete(snapshot, cfg)); }
        return Ok(None);
    }
    let Some(path) = graph.shortest_path(&starts, &targets, max_depth, max_nodes) else {
        return Ok(Some(format!("[qc-path v1] {from} -> {to}: no semantic path within depth {max_depth}\n")));
    };
    let mut lines = vec![format!("[qc-path v1] {from} -> {to} - {} nodes\n", path.len())];
    for (i, hop) in path.iter().enumerate() {
        if i == 0 { lines.push(format!("  {}\n", semantic_node_label(graph, hop.node))); }
        else {
            lines.push(format!("  -> {} [{} @{}:{}]\n", semantic_node_label(graph, hop.node), hop.via.map(EdgeKind::as_str).unwrap_or("?"), hop.line, hop.column));
        }
    }
    Ok(Some(bounded_graph_output(lines, cfg.max_bytes, "path")))
}

pub(crate) fn build_semantic_direct(
    operation: crate::repomap_protocol::LookupOperation,
    query: &str,
    secondary_query: Option<&str>,
    file_filter: Option<&str>,
    depth: usize,
    max_nodes: usize,
    root: &Path,
    cfg: &MapConfig,
) -> ScanResult {
    let snapshot = direct_snapshot(root, cfg)?;
    match operation {
        crate::repomap_protocol::LookupOperation::Callers => build_callers(query, file_filter, depth, max_nodes, &snapshot, cfg),
        crate::repomap_protocol::LookupOperation::Callees => build_callees(query, file_filter, depth, max_nodes, &snapshot, cfg),
        crate::repomap_protocol::LookupOperation::Impact => build_impact(query, file_filter, depth, max_nodes, &snapshot, cfg),
        crate::repomap_protocol::LookupOperation::Deps => build_dependencies(query, file_filter, false, depth, max_nodes, &snapshot, cfg),
        crate::repomap_protocol::LookupOperation::Dependents => build_dependencies(query, file_filter, true, depth, max_nodes, &snapshot, cfg),
        crate::repomap_protocol::LookupOperation::Path => build_path(query, secondary_query.unwrap_or_default(), depth, max_nodes, &snapshot, cfg),
        _ => Ok(None),
    }
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
        assert!(out.starts_with("[qc-references v2] helper"));
        assert!(out.contains("src/main.rs:2:"));
        assert!(out.contains("src/main.rs:5"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn semantic_refs_do_not_fall_back_to_ambiguous_lexical_hits() {
        let dir = tempdir();
        write_file(&dir, "a.ts", "export function save() {}\n");
        write_file(&dir, "b.ts", "export function save() {}\n");
        write_file(&dir, "c.ts", "export function run(){ save(); }\n");
        let out = output(build_refs_direct("save", &dir, &cfg()));
        assert!(out.starts_with("[qc-references v2] save - 2 definitions"));
        assert!(out.contains("no resolved semantic references"));
        assert!(!out.contains("c.ts:"));
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

    #[test]
    fn frontload_finds_exact_declaration_and_returns_bounded_source() {
        let dir = tempdir();
        write_file(&dir, "src/early.py", "def unrelated():\n    return 1\n");
        write_file(
            &dir,
            "src/remote_dispatch.py",
            "def helper():\n    return 1\n\ndef open_session(name):\n    return name\n",
        );
        let c = cfg();
        let out = output(build_frontload_direct("How does open_session work?", &dir, &c));
        assert!(out.contains("[qc-frontload v1]"));
        assert!(out.contains("## open_session - src/remote_dispatch.py:4"));
        assert!(out.contains("4\tdef open_session(name):"));
        assert!(out.contains("use repo explore for graph relationships"));
        assert!(out.len() <= c.max_bytes.min(4 * 1024));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frontload_prefers_exact_definition_over_earlier_reference() {
        let dir = tempdir();
        write_file(
            &dir,
            "a_ref.py",
            "def caller():\n    return open_session('x')\n",
        );
        write_file(
            &dir,
            "z_def.py",
            "def open_session(name):\n    return name\n",
        );
        let c = cfg();
        let out = output(build_frontload_direct("trace open_session", &dir, &c));
        assert!(out.contains("## open_session - z_def.py:1"));
        assert!(out.contains("1\tdef open_session(name):"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frontload_no_match_is_explicit_and_bounded() {
        let dir = tempdir();
        write_file(&dir, "src/auth.ts", "export function LoginService() {}\n");
        let c = cfg();
        let out = output(build_frontload_direct("weather forecast tomorrow", &dir, &c));
        assert!(out.contains("No high-confidence symbol/file match"));
        assert!(out.len() <= c.max_bytes.min(4 * 1024));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explore_ranks_primary_symbol_and_returns_compact_source_without_warming_semantics() {
        let dir = tempdir();
        write_file(
            &dir,
            "src/auth.ts",
            "export function PersistUser() { return 1; }\n\nexport function LoginService() { return PersistUser(); }\n",
        );
        let c = cfg();
        let index = repomap_index::scan_root(&dir, &repomap_index::ScanConfig::from(&c)).unwrap();
        assert!(index.semantic_graph_if_ready().is_none());

        let out = output(build_explore("How does LoginService work?", &index, &c));
        assert!(out.contains("[qc-explore v1]"));
        assert!(out.contains("## LoginService - src/auth.ts:3"));
        assert!(out.contains("3\texport function LoginService()"));
        assert!(out.contains("[semantic graph cold:"));
        assert!(index.semantic_graph_if_ready().is_none());
        assert!(out.len() <= c.max_bytes.min(8 * 1024));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explore_reuses_warm_semantic_graph_for_relationships() {
        let dir = tempdir();
        write_file(
            &dir,
            "src/store.ts",
            "export function PersistUser() { return 1; }\n",
        );
        write_file(
            &dir,
            "src/app.ts",
            "import { PersistUser } from './store';\nexport function LoginService() { return PersistUser(); }\n",
        );
        let c = cfg();
        let index = repomap_index::scan_root(&dir, &repomap_index::ScanConfig::from(&c)).unwrap();
        index.semantic_graph().unwrap();

        let out = output(build_explore("trace LoginService", &index, &c));
        assert!(out.contains("## LoginService - src/app.ts:2"));
        assert!(out.contains("[callees]"));
        assert!(out.contains("PersistUser"));
        assert!(!out.contains("[semantic graph cold:"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explore_no_match_is_explicit_and_bounded() {
        let dir = tempdir();
        write_file(&dir, "src/auth.ts", "export function LoginService() {}\n");
        let c = cfg();
        let out = output(build_explore_direct("weather forecast tomorrow", &dir, &c));
        assert!(out.contains("No high-confidence symbol/file match"));
        assert!(out.len() <= c.max_bytes.min(8 * 1024));
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
