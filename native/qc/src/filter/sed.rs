use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use regex::Regex;
use std::sync::LazyLock;

const REGEX_INPUT_CAP_BYTES: usize = 262_144;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07").unwrap());

/// Filter for read-only sed line-range extraction.
/// Only matches: `sed -n '<int>[,<int>]p' <file>`
/// ANSI strip + byte-cap only. No content transformation — code output must be preserved.
pub struct SedFilter;

impl OutputFilter for SedFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let text = String::from_utf8_lossy(raw);
        let capped = cap_regex_input(&text);

        // Strip ANSI only — no dedup, no padding collapse, no border strip, no blank-line collapse.
        // Code indentation and repeated lines are semantically significant.
        let stripped = ANSI_RE.replace_all(capped, "");

        // Passthrough if within byte cap
        if stripped.len() <= config.max_output_bytes {
            let output = stripped.into_owned();
            return FilterResult {
                output,
                input_bytes,
            };
        }

        // Byte-cap with truncation footer
        let limit = config.max_output_bytes.saturating_sub(80);
        let mut output = truncate_to_utf8_boundary(&stripped, limit);
        if !output.ends_with('\n') {
            output.push('\n');
        }
        let total = stripped.len();
        output.push_str(&format!("[truncated: {}/{} bytes]\n", limit, total));

        FilterResult {
            output,
            input_bytes,
        }
    }
}

fn cap_regex_input(s: &str) -> &str {
    if s.len() <= REGEX_INPUT_CAP_BYTES {
        return s;
    }
    let mut end = REGEX_INPUT_CAP_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &s[..end]
}

fn truncate_to_utf8_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    s[..end].to_string()
}
