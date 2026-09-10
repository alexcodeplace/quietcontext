use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct ProcessFilter;

impl OutputFilter for ProcessFilter {
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
        let total = lines.len();
        let mut output = String::new();

        if let Some(header) = lines.first() {
            output.push_str(header.trim_end());
            output.push('\n');
        }

        let max_data = config.max_lines.saturating_sub(1);
        let data_lines = total.saturating_sub(1);
        let shown = data_lines.min(max_data);
        for line in lines.iter().skip(1).take(max_data) {
            output.push_str(line.trim_end());
            output.push('\n');
        }

        if data_lines > max_data {
            output.push_str(&format!(
                "[truncated: {}/{} processes shown]\n",
                shown, data_lines
            ));
        }

        FilterResult {
            output,
            input_bytes,
        }
    }

    fn hint(&self, input: &FilterInput) -> Option<String> {
        let args_str = input.args.join(" ");
        match input.command {
            "ps" => {
                if args_str.contains("-o") {
                    None // already using selective columns
                } else if args_str.contains("aux") {
                    Some("next time: ps -o pid,user,%cpu,%mem,cmd".to_string())
                } else if args_str.contains("-ef") {
                    Some("next time: ps -o pid,ppid,user,cmd".to_string())
                } else {
                    Some("next time: ps -o pid,user,%cpu,%mem,cmd".to_string())
                }
            }
            "lsof" => {
                if args_str.contains("-F") {
                    None // already using field mode
                } else {
                    Some("next time: lsof -F pcun  # field mode, ~70% smaller".to_string())
                }
            }
            _ => None,
        }
    }
}
