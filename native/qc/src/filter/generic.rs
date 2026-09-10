use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use regex::Regex;
use std::sync::LazyLock;

const REGEX_INPUT_CAP_BYTES: usize = 262_144;

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07").unwrap());

static TABLE_BORDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s]*[├┤┼─┬┴┌┐└┘│╔╗╚╝╠╣╬═╤╧╟╢║+\-|]+[\s]*$").unwrap());

static COLUMN_PADDING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" {3,}").unwrap());

pub struct GenericFilter;

impl OutputFilter for GenericFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let text = String::from_utf8_lossy(raw);
        let capped = cap_regex_input(&text);

        // JSON auto-detect: if entire output is valid JSON, extract schema
        let trimmed_text = capped.trim();
        if (trimmed_text.starts_with('{') || trimmed_text.starts_with('['))
            && serde_json::from_str::<serde_json::Value>(trimmed_text).is_ok()
        {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed_text) {
                let schema = crate::filter::json::extract_json_schema(&val, 0);
                let output = format!("JSON output ({} bytes):\n{}\n", input_bytes, schema);
                return FilterResult {
                    output,
                    input_bytes,
                };
            }
        }

        // Apply 7 universal strategies in order
        let mut output = strip_ansi(capped);
        output = collapse_blank_lines(&output);
        output = strip_trailing_whitespace(&output);
        output = dedup_consecutive_lines(&output);
        output = strip_table_borders(&output);
        output = collapse_column_padding(&output);

        // Truncate if still over limit
        if output.len() > config.max_output_bytes {
            let limit = config.max_output_bytes.saturating_sub(100);
            let mut truncated = truncate_to_utf8_boundary(&output, limit);
            if !truncated.ends_with('\n') {
                truncated.push('\n');
            }
            let total = output.len();
            truncated.push_str(&format!(
                "[truncated: {}/{} bytes. Full raw output is available through QuietContext evidence when archived]\n",
                limit, total
            ));
            output = truncated;
        }

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

fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").to_string()
}

fn collapse_blank_lines(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_blank = false;
    for line in s.lines() {
        if line.trim().is_empty() {
            if !prev_blank {
                result.push('\n');
                prev_blank = true;
            }
        } else {
            result.push_str(line);
            result.push('\n');
            prev_blank = false;
        }
    }
    result
}

fn strip_trailing_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        result.push_str(line.trim_end());
        result.push('\n');
    }
    result
}

fn dedup_consecutive_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let mut result = String::with_capacity(s.len());
    let mut prev = lines[0];
    let mut count: usize = 1;

    for &line in &lines[1..] {
        if line == prev {
            count += 1;
        } else {
            flush_line(&mut result, prev, count);
            prev = line;
            count = 1;
        }
    }
    flush_line(&mut result, prev, count);
    result
}

fn flush_line(result: &mut String, line: &str, count: usize) {
    if count > 1 {
        result.push_str(&format!("{} (×{})\n", line, count));
    } else {
        result.push_str(line);
        result.push('\n');
    }
}

fn strip_table_borders(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && TABLE_BORDER_RE.is_match(line) {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

fn collapse_column_padding(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for line in s.lines() {
        result.push_str(&COLUMN_PADDING_RE.replace_all(line, "  "));
        result.push('\n');
    }
    result
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
