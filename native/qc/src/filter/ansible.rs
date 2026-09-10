use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct AnsibleFilter;

impl OutputFilter for AnsibleFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let combined: Vec<u8> = [input.stdout, input.stderr].concat();
        let input_bytes = combined.len();
        let text = String::from_utf8_lossy(&combined);

        if combined.len() <= config.max_output_bytes {
            let output = text.into_owned();
            return FilterResult {
                output,
                input_bytes,
            };
        }

        let mut output = String::new();
        let mut stripped = 0usize;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("ok:") || trimmed.starts_with("skipping:") {
                stripped += 1;
                continue;
            }
            if !trimmed.is_empty()
                && trimmed.chars().all(|c| c == '=' || c == '*' || c == ' ')
                && trimmed.len() > 5
            {
                continue;
            }
            output.push_str(line);
            output.push('\n');
        }

        if stripped > 0 {
            output.push_str(&format!("[{} ok/skipped tasks stripped]\n", stripped));
        }

        if output.len() > config.max_output_bytes {
            safe_truncate(&mut output, config.max_output_bytes.saturating_sub(40));
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("[truncated]\n");
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}
