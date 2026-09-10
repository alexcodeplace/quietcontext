use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct TerraformFilter;

impl OutputFilter for TerraformFilter {
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
            if trimmed.contains("(known after apply)") {
                stripped += 1;
                continue;
            }
            if trimmed.starts_with("Still ") || trimmed.starts_with("Waiting ") {
                stripped += 1;
                continue;
            }
            if trimmed.contains("Refreshing state...") || trimmed.contains("Reading...") {
                stripped += 1;
                continue;
            }
            if trimmed == "# (no changes)" || trimmed == "(no changes)" {
                stripped += 1;
                continue;
            }

            output.push_str(line);
            output.push('\n');
        }

        if stripped > 0 {
            output.push_str(&format!(
                "[{} noise lines stripped (known-after-apply, progress, refreshing)]\n",
                stripped
            ));
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
