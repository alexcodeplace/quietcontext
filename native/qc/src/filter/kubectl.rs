use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct KubectlFilter;

impl OutputFilter for KubectlFilter {
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

        if text.contains("apiVersion:") || text.contains("kind:") {
            return filter_kubectl_yaml(&text, input_bytes, config);
        }

        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(config.max_lines);
        let mut output: String = lines[..shown].join("\n");
        output.push('\n');
        if lines.len() > config.max_lines {
            output.push_str(&format!("[truncated: {}/{} rows]\n", shown, lines.len()));
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}

fn filter_kubectl_yaml(text: &str, input_bytes: usize, config: &FilterConfig) -> FilterResult {
    let mut output = String::new();
    let mut in_managed_fields = false;
    let mut in_last_applied = false;
    let mut managed_indent = 0usize;
    let mut stripped = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();

        if trimmed.starts_with("managedFields:") {
            in_managed_fields = true;
            managed_indent = indent;
            stripped += 1;
            continue;
        }
        if in_managed_fields {
            if indent > managed_indent || (trimmed.starts_with("- ") && indent >= managed_indent) {
                stripped += 1;
                continue;
            }
            in_managed_fields = false;
        }

        if trimmed.starts_with("kubectl.kubernetes.io/last-applied-configuration:") {
            in_last_applied = true;
            stripped += 1;
            continue;
        }
        if in_last_applied {
            if indent > 4 || trimmed.starts_with('{') || trimmed.starts_with('"') {
                stripped += 1;
                continue;
            }
            in_last_applied = false;
        }

        if trimmed.ends_with(": {}") || trimmed.ends_with(": []") {
            stripped += 1;
            continue;
        }

        output.push_str(line);
        output.push('\n');
    }

    if stripped > 0 {
        output.push_str(&format!(
            "[{} lines stripped (managedFields, last-applied-config, empty fields)]\n",
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
