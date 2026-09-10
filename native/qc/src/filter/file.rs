use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Filter for file-viewing commands: cat, head, tail, less, more.
/// Caps at max_lines lines, adds header/footer when truncating.
pub struct FileFilter;

impl OutputFilter for FileFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();

        let text = String::from_utf8_lossy(raw);
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        // Cap at max_lines; annotate only when truncating
        let shown = total.min(config.max_lines);
        let mut output = if total > config.max_lines {
            format!("[showing first {} of {} lines]\n", shown, total)
        } else {
            String::new()
        };

        for line in &lines[..shown] {
            output.push_str(line);
            output.push('\n');
        }

        if total > config.max_lines {
            output.push_str("[truncated]\n");
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}
