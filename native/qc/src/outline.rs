use regex::Regex;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::OnceLock;

const SOURCE_EXTS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "astro", "vue", "svelte", "rs", "py", "go",
];

const CONTROL_FLOW: &[&str] = &["if", "for", "while", "switch", "return", "catch", "else"];

#[derive(Clone, Copy, PartialEq)]
pub enum Family {
    Js,
    Rust,
    Python,
    Go,
    Generic,
}

/// One matched declaration line. `name` is the declared identifier when it
/// could be isolated from the matched line, `None` otherwise (e.g. anonymous
/// `export default class {`).
pub(crate) struct Decl {
    pub(crate) line_no: usize,
    pub(crate) rendered: String,
    pub(crate) name: Option<String>,
}

/// True when `path`'s extension is one `read-gate` will outline instead of
/// falling through to a plain allow (js-family/rust/python/go only, not the
/// generic fallback family `build_outline` also supports).
pub(crate) fn is_source_ext(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => SOURCE_EXTS.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

pub(crate) fn family_of(path: &Path) -> Family {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "astro" | "vue" | "svelte" => Family::Js,
        "rs" => Family::Rust,
        "py" => Family::Python,
        "go" => Family::Go,
        _ => Family::Generic,
    }
}

fn leading_ws_len(line: &str) -> usize {
    line.chars().take_while(|&c| c == ' ' || c == '\t').count()
}

fn indent_level(line: &str) -> usize {
    let mut level = 0usize;
    let mut space_run = 0usize;
    for c in line.chars() {
        match c {
            '\t' => level += 1,
            ' ' => {
                space_run += 1;
                if space_run == 2 {
                    level += 1;
                    space_run = 0;
                }
            }
            _ => break,
        }
    }
    level.min(4)
}

pub(crate) fn cap_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn cap_explanation() -> &'static str {
    "[qc-outline: output cap too small for declaration entries; increase outline_max_bytes]\n"
}

fn render_declaration(line: &str) -> String {
    let indent = indent_level(line);
    let mut content = line.trim().to_string();

    if let Some(rest) = content.strip_suffix(" {") {
        content = rest.to_string();
    } else if let Some(rest) = content.strip_suffix(" =>") {
        content = rest.to_string();
    }
    if let Some(rest) = content.strip_suffix('=') {
        content = rest.trim_end().to_string();
    }

    let out = format!("{}{}", "  ".repeat(indent), content);
    cap_chars(&out, 160).to_string()
}

fn js_re_top() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(export\s+(default\s+)?)?(declare\s+)?(abstract\s+)?(async\s+)?(function|class|interface|type|enum|namespace)\b",
        )
        .unwrap()
    })
}

fn js_re_const_prefix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(export\s+(default\s+)?)?(const|let|var)\s+[A-Za-z_$][\w$]*\s*=\s*").unwrap()
    })
}

fn js_re_member() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^((?:static|async|get|set|private|public|protected|readonly)\s+)*([A-Za-z_$][\w$]*)\s*\([^)]*\)[^{;]*\{",
        )
        .unwrap()
    })
}

fn rust_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(pub(\([^)]*\))?\s+)?(unsafe\s+)?(async\s+)?(const\s+)?(fn|struct|enum|trait|impl|mod|type|const|static|macro_rules!)\b",
        )
        .unwrap()
    })
}

fn py_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(async\s+def|def)\s+\w+|^class\s+\w+").unwrap())
}

fn go_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(func|type|const|var)\s+").unwrap())
}

fn generic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(export\s+)?(function|class|interface|type|enum|namespace|struct|trait|impl|mod|def|func|fn)\b")
            .unwrap()
    })
}

fn js_const_match(trimmed: &str) -> bool {
    let m = match js_re_const_prefix().find(trimmed) {
        Some(m) => m,
        None => return false,
    };
    let rest = &trimmed[m.end()..];
    let after_async = rest.strip_prefix("async ").unwrap_or(rest);
    if after_async.starts_with("function") {
        return true;
    }
    if rest.starts_with('(') {
        return true;
    }
    trimmed.contains("=>")
}

fn js_declaration(line: &str) -> bool {
    let ws = leading_ws_len(line);
    let trimmed = line.trim_start();
    if ws <= 8 && (js_re_top().is_match(trimmed) || js_const_match(trimmed)) {
        return true;
    }
    if (2..=8).contains(&ws) {
        if let Some(caps) = js_re_member().captures(trimmed) {
            let name = &caps[2];
            if !CONTROL_FLOW.contains(&name) {
                return true;
            }
        }
    }
    false
}

fn rust_declaration(line: &str) -> bool {
    leading_ws_len(line) <= 8 && rust_re().is_match(line.trim_start())
}

fn py_declaration(line: &str) -> bool {
    leading_ws_len(line) <= 8 && py_re().is_match(line.trim_start())
}

fn go_declaration(line: &str) -> bool {
    leading_ws_len(line) == 0 && go_re().is_match(line.trim_start())
}

fn generic_declaration(line: &str) -> bool {
    leading_ws_len(line) <= 4 && generic_re().is_match(line.trim_start())
}

fn is_declaration(line: &str, family: Family) -> bool {
    match family {
        Family::Js => js_declaration(line),
        Family::Rust => rust_declaration(line),
        Family::Python => py_declaration(line),
        Family::Go => go_declaration(line),
        Family::Generic => generic_declaration(line),
    }
}

fn js_re_top_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(export\s+(default\s+)?)?(declare\s+)?(abstract\s+)?(async\s+)?(function|class|interface|type|enum|namespace)\s+([A-Za-z_$][\w$]*)",
        )
        .unwrap()
    })
}

fn js_re_const_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(export\s+(default\s+)?)?(const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*")
            .unwrap()
    })
}

fn rust_re_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(pub(\([^)]*\))?\s+)?(unsafe\s+)?(async\s+)?(const\s+)?(fn|struct|enum|trait|impl|mod|type|const|static|macro_rules!)\s+([A-Za-z_]\w*)",
        )
        .unwrap()
    })
}

fn py_re_def_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(async\s+def|def)\s+(\w+)").unwrap())
}

fn py_re_class_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^class\s+(\w+)").unwrap())
}

fn go_re_func_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^func\s+(?:\([^)]*\)\s+)?(\w+)").unwrap())
}

fn go_re_other_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:type|const|var)\s+(\w+)").unwrap())
}

fn generic_re_name() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(export\s+)?(function|class|interface|type|enum|namespace|struct|trait|impl|mod|def|func|fn)\s+(\w+)")
            .unwrap()
    })
}

fn js_name(line: &str) -> Option<String> {
    let ws = leading_ws_len(line);
    let trimmed = line.trim_start();
    if ws <= 8 {
        if let Some(caps) = js_re_top_name().captures(trimmed) {
            return Some(caps[7].to_string());
        }
        if js_const_match(trimmed) {
            return js_re_const_name()
                .captures(trimmed)
                .map(|c| c[4].to_string());
        }
    }
    if (2..=8).contains(&ws) {
        if let Some(caps) = js_re_member().captures(trimmed) {
            let name = &caps[2];
            if !CONTROL_FLOW.contains(&name) {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn rust_name(line: &str) -> Option<String> {
    rust_re_name()
        .captures(line.trim_start())
        .map(|c| c[7].to_string())
}

fn py_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(caps) = py_re_def_name().captures(trimmed) {
        return Some(caps[2].to_string());
    }
    py_re_class_name()
        .captures(trimmed)
        .map(|c| c[1].to_string())
}

fn go_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if let Some(caps) = go_re_func_name().captures(trimmed) {
        return Some(caps[1].to_string());
    }
    go_re_other_name()
        .captures(trimmed)
        .map(|c| c[1].to_string())
}

fn generic_name(line: &str) -> Option<String> {
    generic_re_name()
        .captures(line.trim_start())
        .map(|c| c[3].to_string())
}

fn extract_name(line: &str, family: Family) -> Option<String> {
    match family {
        Family::Js => js_name(line),
        Family::Rust => rust_name(line),
        Family::Python => py_name(line),
        Family::Go => go_name(line),
        Family::Generic => generic_name(line),
    }
}

/// Declaration lines in `text` for `family`, in file order. Sole decl matcher
/// for the crate — `build_outline` and the map/sym/refs subcommands both go
/// through this.
pub(crate) fn decls_in(text: &str, family: Family) -> Vec<Decl> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if is_declaration(line, family) {
            out.push(Decl {
                line_no: i + 1,
                rendered: render_declaration(line),
                name: extract_name(line, family),
            });
        }
    }
    out
}

/// Deterministic declaration outline for `path`, rendered with `display_path`
/// in the header. `None` when the file is unreadable, not valid UTF-8, looks
/// binary, or contains zero declarations — callers treat all of these the
/// same (not outlinable).
pub fn build_outline(path: &Path, display_path: &str, max_bytes: usize) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    build_outline_from_bytes(&bytes, display_path, path, max_bytes)
}

/// Same as `build_outline`, but operates on bytes the caller already read
/// (e.g. read-gate's `process`), so the outline is built from the exact same
/// bytes that were hashed — no second `fs::read` that could race an edit.
/// `ext_path` is used only for language detection from its extension.
pub fn build_outline_from_bytes(
    bytes: &[u8],
    display_path: &str,
    ext_path: &Path,
    max_bytes: usize,
) -> Option<String> {
    let scan = &bytes[..bytes.len().min(8192)];
    if scan.contains(&0u8) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let family = family_of(ext_path);

    let decls = decls_in(text, family);
    if decls.is_empty() {
        return None;
    }

    let total_lines = text.lines().count();
    let total_bytes = bytes.len();
    let hash = hex::encode(Sha256::digest(bytes));
    let hash8 = &hash[..8];

    let header = format!(
        "[qc-outline v1] {display_path} — {total_lines} lines, {total_bytes} bytes, sha256:{hash8}\n"
    );
    let k_total = decls.len();
    let footer = format!(
        "[qc-outline: {k_total} declarations. Read(file, offset=N, limit=M) for a body range; repeat the same full Read to get the whole file]\n"
    );

    let lines_text: Vec<String> = decls
        .iter()
        .map(|d| format!("{}: {}\n", d.line_no, d.rendered))
        .collect();
    let mut prefix = vec![0usize; k_total + 1];
    for (i, l) in lines_text.iter().enumerate() {
        prefix[i + 1] = prefix[i] + l.len();
    }

    let mut shown = k_total;
    loop {
        let marker = if shown == 0 {
            cap_explanation().to_string()
        } else if shown < k_total {
            format!(
                "[qc-outline: +{} more declarations — Read(offset,limit) ranges instead]\n",
                k_total - shown
            )
        } else {
            String::new()
        };
        let total = header.len() + prefix[shown] + marker.len() + footer.len();
        if total <= max_bytes || shown == 0 {
            if shown == 0 && total > max_bytes {
                return Some(cap_explanation().to_string());
            }
            break;
        }
        shown -= 1;
    }

    let mut out = String::with_capacity(header.len() + prefix[shown] + footer.len() + 96);
    out.push_str(&header);
    for l in &lines_text[..shown] {
        out.push_str(l);
    }
    if shown < k_total {
        if shown == 0 {
            out.push_str(cap_explanation());
        } else {
            out.push_str(&format!(
                "[qc-outline: +{} more declarations — Read(offset,limit) ranges instead]\n",
                k_total - shown
            ));
        }
    }
    out.push_str(&footer);

    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(ext: &str, content: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "qc-outline-test-{}-{}.{}",
            std::process::id(),
            rand_suffix(),
            ext
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }

    #[test]
    fn js_fixture_matches_declarations_only() {
        let src = r#"export function foo(a, b) {
  if (a) {
    return b;
  }
}

const handler = (req, res) => {
  return res;
};

class Widget {
  static async build(x) {
    return x;
  }
}

export interface Props {
  name: string;
}
"#;
        let path = write_tmp("ts", src);
        let out = build_outline(&path, "f.ts", 6000).unwrap();
        assert!(out.contains("export function foo"));
        assert!(out.contains("const handler ="));
        assert!(out.contains("class Widget"));
        assert!(out.contains("static async build"));
        assert!(out.contains("export interface Props"));
        assert!(!out.contains("if (a)"));
        assert!(!out.contains("return b"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn python_fixture() {
        let src = "def top(x):\n    if x:\n        return x\n\nclass Foo:\n    def method(self):\n        pass\n";
        let path = write_tmp("py", src);
        let out = build_outline(&path, "f.py", 6000).unwrap();
        assert!(out.contains("def top"));
        assert!(out.contains("class Foo"));
        assert!(out.contains("def method"));
        assert!(!out.contains("if x"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rust_fixture() {
        let src = "pub fn top() {\n    if true {\n        return;\n    }\n}\n\npub struct Foo {\n    x: i32,\n}\n\nimpl Foo {\n    pub fn method(&self) {}\n}\n";
        let path = write_tmp("rs", src);
        let out = build_outline(&path, "f.rs", 6000).unwrap();
        assert!(out.contains("pub fn top"));
        assert!(out.contains("pub struct Foo"));
        assert!(out.contains("impl Foo"));
        assert!(out.contains("pub fn method"));
        assert!(!out.contains("if true"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deterministic_across_calls() {
        let src = "pub fn a() {}\npub fn b() {}\n";
        let path = write_tmp("rs", src);
        let out1 = build_outline(&path, "f.rs", 6000).unwrap();
        let out2 = build_outline(&path, "f.rs", 6000).unwrap();
        assert_eq!(out1, out2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn zero_declarations_returns_none() {
        let src = "just some plain text\nno declarations here at all\n";
        let path = write_tmp("txt", src);
        assert!(build_outline(&path, "f.txt", 6000).is_none());
        std::fs::remove_file(&path).ok();
    }

    fn names(text: &str, family: Family) -> Vec<Option<String>> {
        decls_in(text, family).into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn js_decl_names() {
        let src = "export function foo(a) {\n}\nconst handler = (req) => {\n};\nclass Widget {\n  static async build(x) {\n    return x;\n  }\n}\nexport interface Props {\n  name: string;\n}\n";
        let n = names(src, Family::Js);
        assert_eq!(
            n,
            vec![
                Some("foo".to_string()),
                Some("handler".to_string()),
                Some("Widget".to_string()),
                Some("build".to_string()),
                Some("Props".to_string()),
            ]
        );
    }

    #[test]
    fn rust_decl_names() {
        let src = "pub fn top() {\n}\npub struct Foo {\n}\npub trait Bar {\n}\nimpl Foo {\n  pub fn method(&self) {}\n}\n";
        let n = names(src, Family::Rust);
        assert_eq!(
            n,
            vec![
                Some("top".to_string()),
                Some("Foo".to_string()),
                Some("Bar".to_string()),
                Some("Foo".to_string()),
                Some("method".to_string()),
            ]
        );
    }

    #[test]
    fn py_decl_names() {
        let src = "def top(x):\n    pass\n\nclass Foo:\n    def method(self):\n        pass\n";
        let n = names(src, Family::Python);
        assert_eq!(
            n,
            vec![
                Some("top".to_string()),
                Some("Foo".to_string()),
                Some("method".to_string()),
            ]
        );
    }

    #[test]
    fn go_decl_names() {
        let src = "func Top() {\n}\nfunc (r *Receiver) Method() {\n}\ntype Widget struct {\n}\nconst Max = 10\n";
        let n = names(src, Family::Go);
        assert_eq!(
            n,
            vec![
                Some("Top".to_string()),
                Some("Method".to_string()),
                Some("Widget".to_string()),
                Some("Max".to_string()),
            ]
        );
    }

    #[test]
    fn decls_in_matches_build_outline_output() {
        let src = "pub fn a() {}\npub fn b() {}\n";
        let path = write_tmp("rs", src);
        let out = build_outline(&path, "f.rs", 6000).unwrap();
        let decls = decls_in(src, Family::Rust);
        assert_eq!(decls.len(), 2);
        assert!(out.contains("1: pub fn a()"));
        assert!(out.contains("2: pub fn b()"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn byte_cap_without_declaration_room_returns_explanation() {
        let bytes = b"pub fn a() {}\n";
        let first = build_outline_from_bytes(bytes, "f.rs", Path::new("f.rs"), 96).unwrap();
        let second = build_outline_from_bytes(bytes, "f.rs", Path::new("f.rs"), 96).unwrap();

        assert_eq!(first, second);
        assert!(first.contains("[qc-outline: output cap too small"));
    }
}
