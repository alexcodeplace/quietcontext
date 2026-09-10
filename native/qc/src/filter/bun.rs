use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct BunFilter;

impl OutputFilter for BunFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let subcommand = input.args.first().map(|s| s.as_str()).unwrap_or("");
        match subcommand {
            "test" => filter_bun_test(input, config),
            "build" => filter_bun_build(input, config),
            "install" | "add" | "remove" | "i" => filter_bun_install(input, config),
            _ => filter_bun_generic(input, config),
        }
    }
}

fn filter_bun_test(input: &FilterInput, config: &FilterConfig) -> FilterResult {
    let combined: Vec<u8> = [input.stderr, input.stdout].concat();
    let input_bytes = combined.len();
    let text = String::from_utf8_lossy(&combined);

    if combined.len() <= config.max_output_bytes {
        let output = text.into_owned();
        return FilterResult {
            output,
            input_bytes,
        };
    }

    if input.exit_code == 0 {
        let summary = text
            .lines()
            .rev()
            .find(|l| l.contains("pass") || l.contains("tests") || l.contains("total"))
            .unwrap_or("Tests: all passed");
        let output = format!("{}\n", summary.trim());
        return FilterResult {
            output,
            input_bytes,
        };
    }

    let mut output = String::new();
    let mut in_fail = false;
    for line in text.lines() {
        if line.contains("FAIL")
            || line.contains("\u{2717}")
            || line.contains("\u{00d7}")
            || line.contains("Error")
            || line.starts_with("  \u{25cf}")
        {
            in_fail = true;
        }
        if in_fail {
            output.push_str(line);
            output.push('\n');
        }
        if in_fail && line.trim().is_empty() && output.lines().count() > 3 {
            in_fail = false;
        }
    }

    if let Some(summary) = text
        .lines()
        .rev()
        .find(|l| l.contains("fail") || l.contains("pass"))
    {
        if !output.contains(summary.trim()) {
            output.push_str(summary.trim());
            output.push('\n');
        }
    }

    byte_cap(&mut output, config.max_output_bytes);
    FilterResult {
        output,
        input_bytes,
    }
}

fn filter_bun_build(input: &FilterInput, config: &FilterConfig) -> FilterResult {
    let combined: Vec<u8> = [input.stderr, input.stdout].concat();
    let input_bytes = combined.len();
    let text = String::from_utf8_lossy(&combined);

    if input.exit_code == 0 && combined.len() <= config.max_output_bytes {
        let output = text.into_owned();
        return FilterResult {
            output,
            input_bytes,
        };
    }

    if input.exit_code == 0 {
        let summary = text
            .lines()
            .rev()
            .find(|l| l.contains("built") || l.contains("done") || l.contains(".js"))
            .unwrap_or("Build: success");
        let output = format!("{}\n", summary.trim());
        return FilterResult {
            output,
            input_bytes,
        };
    }

    let mut output = String::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("error") || lower.contains("warn") || line.starts_with("  ") {
            output.push_str(line);
            output.push('\n');
        }
    }

    byte_cap(&mut output, config.max_output_bytes);
    FilterResult {
        output,
        input_bytes,
    }
}

fn filter_bun_install(input: &FilterInput, config: &FilterConfig) -> FilterResult {
    let stdout = String::from_utf8_lossy(input.stdout);
    let stderr = String::from_utf8_lossy(input.stderr);
    let input_bytes = input.stdout.len() + input.stderr.len();

    let mut output = String::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('\r') {
            continue;
        }
        if trimmed.starts_with("GET ") || trimmed.starts_with("HEAD ") {
            continue;
        }
        if !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| matches!(c, '#' | '-' | '=' | '>' | ' ' | '[' | ']' | '|'))
            && trimmed.len() > 5
        {
            continue;
        }
        output.push_str(trimmed);
        output.push('\n');
    }

    byte_cap(&mut output, config.max_output_bytes);
    FilterResult {
        output,
        input_bytes,
    }
}

fn filter_bun_generic(input: &FilterInput, config: &FilterConfig) -> FilterResult {
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
    let shown = lines.len().min(config.max_lines);
    let mut output: String = lines[..shown].join("\n");
    output.push('\n');
    if lines.len() > config.max_lines {
        output.push_str(&format!("[truncated: {}/{} lines]\n", shown, lines.len()));
    }

    byte_cap(&mut output, config.max_output_bytes);
    FilterResult {
        output,
        input_bytes,
    }
}

fn byte_cap(output: &mut String, max: usize) {
    if output.len() > max {
        safe_truncate(output, max.saturating_sub(40));
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("[truncated]\n");
    }
}
