use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Filter for log-streaming commands: journalctl, dmesg, docker logs, podman logs, kubectl logs.
/// Takes the most recent `max_lines` lines from combined output (tail behavior).
pub struct LogFilter;

impl OutputFilter for LogFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = [input.stdout, input.stderr].concat();
        let input_bytes = raw.len();

        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();

        let total = lines.len();

        if total == 0 {
            return FilterResult {
                output: String::new(),
                input_bytes,
            };
        }

        // Take from END (most recent entries)
        let shown_lines: Vec<&str> = if total > config.max_lines {
            lines[total - config.max_lines..].to_vec()
        } else {
            lines.clone()
        };

        let mut output = String::new();

        if total > config.max_lines {
            output.push_str(&format!(
                "[showing last {} of {} lines]\n",
                shown_lines.len(),
                total
            ));
        }

        for line in &shown_lines {
            output.push_str(line);
            output.push('\n');
        }

        // Byte-cap the final output
        if output.len() > config.max_output_bytes {
            safe_truncate(&mut output, config.max_output_bytes);
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

    fn hint(&self, input: &FilterInput) -> Option<String> {
        if input.command == "journalctl" {
            let args_str = input.args.join(" ");
            if !args_str.contains("-o cat") && !args_str.contains("-o=cat") {
                return Some(
                    "next time: journalctl -o cat  # message-only, no timestamps/metadata"
                        .to_string(),
                );
            }
        }
        None
    }
}
