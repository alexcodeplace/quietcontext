use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct GhFilter;

impl OutputFilter for GhFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let subcommand = input.args.first().map(|s| s.as_str()).unwrap_or("");
        let sub2 = input.args.get(1).map(|s| s.as_str()).unwrap_or("");

        match (subcommand, sub2) {
            ("pr", "diff") | ("mr", "diff") => filter_gh_diff(input, config),
            ("pr", "view") | ("issue", "view") | ("pr", "checks") => filter_gh_view(input, config),
            ("run", "view") => filter_gh_run_log(input, config),
            _ => filter_gh_generic(input, config),
        }
    }
}

fn filter_gh_diff(input: &FilterInput, config: &FilterConfig) -> FilterResult {
    let input_bytes = input.stdout.len();
    super::git::filter_git_diff(input.stdout, input_bytes, config)
}

fn filter_gh_view(input: &FilterInput, config: &FilterConfig) -> FilterResult {
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

    let mut output = String::new();
    let mut body_started = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("--") && trimmed.len() > 10 && trimmed.chars().all(|c| c == '-') {
            continue;
        }
        if trimmed.is_empty() && !body_started && !output.is_empty() {
            body_started = true;
        }
        output.push_str(line);
        output.push('\n');
        if output.len() >= config.max_output_bytes {
            break;
        }
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

fn filter_gh_run_log(input: &FilterInput, config: &FilterConfig) -> FilterResult {
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
    let mut in_error_section = false;

    for line in text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("error") || lower.contains("fail") || lower.contains("exit code") {
            in_error_section = true;
        }
        if in_error_section || lower.contains("##[error]") || lower.contains("::error::") {
            output.push_str(line);
            output.push('\n');
        }
        if in_error_section && line.trim().is_empty() {
            in_error_section = false;
        }
    }

    if output.is_empty() {
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(config.max_lines);
        for line in &lines[start..] {
            output.push_str(line);
            output.push('\n');
        }
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

fn filter_gh_generic(input: &FilterInput, config: &FilterConfig) -> FilterResult {
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
        output.push_str(&format!("[truncated: {}/{} entries]\n", shown, lines.len()));
    }

    FilterResult {
        output,
        input_bytes,
    }
}
