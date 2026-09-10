use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use regex::Regex;

pub struct DbFilter;

impl OutputFilter for DbFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let text = String::from_utf8_lossy(raw);

        if raw.len() <= config.max_output_bytes {
            let output = text.into_owned();
            return FilterResult {
                output,
                input_bytes,
            };
        }

        let border_re = Regex::new(r"^[\s]*[├┤┼─┬┴┌┐└┘│+\-|=]+[\s]*$").unwrap();
        let padding_re = Regex::new(r" {3,}").unwrap();

        let mut output = String::new();
        let mut row_count = 0usize;
        let mut header_done = false;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if border_re.is_match(trimmed) {
                continue;
            }

            let compressed = padding_re.replace_all(trimmed, "  ");
            output.push_str(&compressed);
            output.push('\n');

            if header_done {
                row_count += 1;
            } else {
                header_done = true;
            }

            if row_count >= config.max_lines {
                let total_rows = text
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !border_re.is_match(l.trim()))
                    .count();
                output.push_str(&format!(
                    "[truncated: {}/{} rows shown]\n",
                    row_count,
                    total_rows.saturating_sub(1)
                ));
                break;
            }
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}
