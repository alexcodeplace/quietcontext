use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct LsFilter;

impl OutputFilter for LsFilter {
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

        let lines: Vec<&str> = text.lines().collect();
        let mut output = String::new();
        let mut shown = 0usize;

        for line in &lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if trimmed.starts_with("total ") {
                output.push_str(trimmed);
                output.push('\n');
                continue;
            }

            if trimmed.ends_with(':') && !trimmed.contains(' ') {
                output.push_str(trimmed);
                output.push('\n');
                continue;
            }

            let fields: Vec<&str> = trimmed
                .splitn(9, char::is_whitespace)
                .filter(|f| !f.is_empty())
                .collect();

            if fields.len() >= 9 && fields[0].starts_with(['-', 'd', 'l']) {
                let size = fields[4];
                let name = fields[8];
                let prefix = if fields[0].starts_with('d') {
                    "d "
                } else {
                    "  "
                };
                output.push_str(&format!("{}{:>8}  {}\n", prefix, size, name));
            } else {
                output.push_str(trimmed);
                output.push('\n');
            }

            shown += 1;
            if shown >= config.max_lines {
                output.push_str(&format!(
                    "[truncated: {}/{} entries shown]\n",
                    shown,
                    lines.len()
                ));
                break;
            }
        }

        FilterResult {
            output,
            input_bytes,
        }
    }

    fn hint(&self, input: &FilterInput) -> Option<String> {
        let args_str = input.args.join(" ");
        if args_str.contains("-R") || args_str.contains("--recursive") {
            None // recursive is intentional
        } else {
            Some("next time: ls -la --sort=size | head -30  # size-sorted, top 30".to_string())
        }
    }
}
