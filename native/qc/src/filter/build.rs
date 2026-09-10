use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use std::collections::HashMap;

/// Filter for build/compile commands: tsc, cargo, go, make.
/// Groups errors by file, deduplicates, shows error count header.
pub struct BuildFilter;

fn truncate_utf8_to(s: &[u8], limit: usize) -> &[u8] {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while end > 0 && (s[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    &s[..end]
}

impl OutputFilter for BuildFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        // Build tools emit errors to stderr — use stderr first, fall back to stdout
        let raw: Vec<u8> = if !input.stderr.is_empty() {
            [input.stderr, input.stdout].concat()
        } else {
            input.stdout.to_vec()
        };
        let input_bytes = raw.len();

        let command = input.command;
        let subcommand = input.args.first().map(|s| s.as_str()).unwrap_or("");

        let text = String::from_utf8_lossy(&raw);

        // Route to test output filter logic if it's `cargo test` or `go test`
        if (command == "cargo" || command == "go") && subcommand == "test" {
            return filter_test_output(&text, input_bytes, input.exit_code, config);
        }

        let output = match command {
            "tsc" => filter_tsc(&text, config),
            "cargo" => filter_cargo(&text, config),
            "go" => filter_go(&text, config),
            _ => filter_generic_build(&text, config),
        };

        let output = if output.len() > config.max_output_bytes {
            let truncated = truncate_utf8_to(
                output.as_bytes(),
                config.max_output_bytes.saturating_sub(80),
            );
            let mut s = String::from_utf8_lossy(truncated).to_string();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("[truncated]\n");
            s
        } else {
            output
        };

        FilterResult {
            output,
            input_bytes,
        }
    }
}

/// Group errors by file for tsc output.
/// tsc error format: `path/to/file.ts(line,col): error TS2345: message`
fn filter_tsc(text: &str, config: &FilterConfig) -> String {
    // file_errors: ordered list of (filename, Vec<(location, message)>)
    let mut file_order: Vec<String> = Vec::new();
    let mut file_errors: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_errors = 0usize;
    let mut seen_messages: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in text.lines() {
        // Match: `file.ts(line,col): error TSxxxx: message`
        if let Some(paren_pos) = line.find('(') {
            if let Some(colon_pos) = line[paren_pos..].find("):") {
                let filename = line[..paren_pos].to_string();
                let location = &line[paren_pos..paren_pos + colon_pos + 2];
                let rest = line[paren_pos + colon_pos + 2..].trim();
                if rest.contains("error") || rest.contains("warning") {
                    total_errors += 1;
                    let dedup_key = format!("{}:{}", filename, rest);
                    if seen_messages.insert(dedup_key) {
                        let entry = format!("  {}{}", location, rest);
                        if !file_errors.contains_key(&filename) {
                            file_order.push(filename.clone());
                            file_errors.insert(filename.clone(), Vec::new());
                        }
                        file_errors.get_mut(&filename).unwrap().push(entry);
                    }
                }
            }
        }
    }

    if file_errors.is_empty() {
        // No structured errors found — fall back to generic
        return filter_generic_build(text, config);
    }

    let file_count = file_errors.len();
    let mut output = format!("tsc: {} errors in {} files\n", total_errors, file_count);

    for filename in &file_order {
        let errors = &file_errors[filename];
        output.push_str(&format!("{}:\n", filename));
        for e in errors {
            output.push_str(e);
            output.push('\n');
        }
    }

    output
}

/// Filter cargo build/check output.
/// cargo error format: `error[E0xxx]: message\n  --> src/file.rs:line:col`
fn filter_cargo(text: &str, config: &FilterConfig) -> String {
    let mut file_order: Vec<String> = Vec::new();
    let mut file_errors: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_errors = 0usize;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        // cargo error line starts with "error[E" or "error:"
        if line.starts_with("error[") || line.starts_with("error:") {
            let error_msg = line.trim().to_string();
            // Next line may be "  --> src/file.rs:line:col"
            let mut filename = String::from("<unknown>");
            let mut location = String::new();
            if i + 1 < lines.len() {
                let next = lines[i + 1].trim();
                if let Some(path) = next.strip_prefix("--> ") {
                    // path is "src/file.rs:line:col"
                    if let Some(colon) = path.find(':') {
                        filename = path[..colon].to_string();
                        location = path[colon..].to_string();
                    } else {
                        filename = path.to_string();
                    }
                    i += 1; // consume the --> line
                }
            }

            total_errors += 1;
            let dedup_key = format!("{}:{}", filename, error_msg);
            if seen.insert(dedup_key) {
                let entry = format!("  {}{}: {}", filename, location, error_msg);
                if !file_errors.contains_key(&filename) {
                    file_order.push(filename.clone());
                    file_errors.insert(filename.clone(), Vec::new());
                }
                file_errors.get_mut(&filename).unwrap().push(entry);
            }
        }
        i += 1;
    }

    if file_errors.is_empty() {
        return filter_generic_build(text, config);
    }

    let file_count = file_errors.len();
    let mut output = format!("cargo: {} errors in {} files\n", total_errors, file_count);

    for filename in &file_order {
        let errors = &file_errors[filename];
        output.push_str(&format!("{}:\n", filename));
        for e in errors {
            output.push_str(e);
            output.push('\n');
        }
    }

    output
}

/// Filter go build output.
/// go error format: `path/to/file.go:line:col: message`
fn filter_go(text: &str, config: &FilterConfig) -> String {
    let mut file_order: Vec<String> = Vec::new();
    let mut file_errors: HashMap<String, Vec<String>> = HashMap::new();
    let mut total_errors = 0usize;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in text.lines() {
        // go errors: file.go:line:col: message
        if line.ends_with(".go") {
            continue;
        } // skip bare file lines
        if let Some(go_pos) = line.find(".go:") {
            let after_go = &line[go_pos + 4..]; // after ".go:"
            let filename = line[..go_pos + 3].to_string(); // includes ".go"
                                                           // Rest is "line:col: message"
            total_errors += 1;
            let dedup_key = format!("{}:{}", filename, after_go);
            if seen.insert(dedup_key) {
                let entry = format!("  :{}", after_go);
                if !file_errors.contains_key(&filename) {
                    file_order.push(filename.clone());
                    file_errors.insert(filename.clone(), Vec::new());
                }
                file_errors.get_mut(&filename).unwrap().push(entry);
            }
        }
    }

    if file_errors.is_empty() {
        return filter_generic_build(text, config);
    }

    let file_count = file_errors.len();
    let mut output = format!("go: {} errors in {} files\n", total_errors, file_count);

    for filename in &file_order {
        let errors = &file_errors[filename];
        output.push_str(&format!("{}:\n", filename));
        for e in errors {
            output.push_str(e);
            output.push('\n');
        }
    }

    output
}

/// Generic build filter: strip blank lines, cap lines.
fn filter_generic_build(text: &str, config: &FilterConfig) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let shown = total.min(config.max_lines);
    let mut output = String::new();
    for line in &lines[..shown] {
        output.push_str(line);
        output.push('\n');
    }
    if total > config.max_lines {
        output.push_str(&format!("[truncated: {}/{} lines shown]\n", shown, total));
    }
    output
}

/// Fallback for `cargo test` / `go test` — just show failures + summary.
fn filter_test_output(
    text: &str,
    input_bytes: usize,
    exit_code: i32,
    config: &FilterConfig,
) -> FilterResult {
    let lines: Vec<&str> = text.lines().collect();

    let summary = lines
        .iter()
        .rev()
        .find(|l| l.contains("passed") || l.contains("failed") || l.contains("test result:"))
        .copied()
        .unwrap_or("")
        .to_string();

    if exit_code == 0 {
        let output = format!("{}\n", summary.trim());
        return FilterResult {
            output,
            input_bytes,
        };
    }

    let mut output = String::new();
    let mut in_failure = false;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("FAILED")
            || trimmed.starts_with("---- ")
            || trimmed.starts_with("failures:")
        {
            in_failure = true;
        }
        if in_failure {
            output.push_str(line);
            output.push('\n');
        }
        if in_failure && trimmed.starts_with("test result:") {
            in_failure = false;
        }
    }

    if !summary.is_empty() {
        output.push_str(summary.trim());
        output.push('\n');
    }

    if output.len() > config.max_output_bytes {
        let truncated = truncate_utf8_to(
            output.as_bytes(),
            config.max_output_bytes.saturating_sub(80),
        );
        let mut s = String::from_utf8_lossy(truncated).to_string();
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("[truncated]\n");
        output = s;
    }

    FilterResult {
        output,
        input_bytes,
    }
}
