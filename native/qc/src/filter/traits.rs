use crate::config::LimitsConfig;

/// Input to a filter: raw captured output from a child process
pub struct FilterInput<'a> {
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub exit_code: i32,
    pub command: &'a str,
    pub args: &'a [String],
}

/// Result produced by a filter
pub struct FilterResult {
    /// The filtered/compressed output to print
    pub output: String,
    /// Number of input bytes before filtering
    pub input_bytes: usize,
}

/// Configuration limits for filters
pub struct FilterConfig {
    pub max_output_bytes: usize,
    pub max_lines: usize,
    pub max_width: usize,
    pub grep_max_results: usize,
    pub grep_max_per_file: usize,
    pub hint_threshold: usize,
}

impl From<&LimitsConfig> for FilterConfig {
    fn from(c: &LimitsConfig) -> Self {
        Self {
            max_output_bytes: c.max_output_bytes,
            max_lines: c.max_lines,
            max_width: c.max_width,
            grep_max_results: c.grep_max_results,
            grep_max_per_file: c.grep_max_per_file,
            hint_threshold: c.hint_threshold,
        }
    }
}

/// Trait implemented by every command-specific (and the generic) filter
pub trait OutputFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult;

    /// Returns a one-line compact-flag hint if applicable, else None.
    /// Called only when QUIET_CONTEXT_HINTS=1 env and input_bytes > hint_threshold.
    fn hint(&self, _input: &FilterInput) -> Option<String> {
        None
    }
}

/// Truncate a `&str` to at most `max_bytes` bytes without splitting a multi-byte char.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Truncate a `String` in-place to at most `max_bytes` bytes without splitting a multi-byte char.
pub fn safe_truncate(s: &mut String, max_bytes: usize) {
    if s.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}
