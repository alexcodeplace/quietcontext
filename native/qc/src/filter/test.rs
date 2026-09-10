use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Filter for test runner commands: jest, vitest, mocha, pytest, phpunit.
pub struct TestFilter;

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

impl OutputFilter for TestFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        // Test runners emit most important output to stderr (e.g. jest, pytest).
        // Combine stderr first, then stdout.
        let combined: Vec<u8> = [input.stderr, input.stdout].concat();
        let input_bytes = combined.len();

        let text = String::from_utf8_lossy(&combined);
        let command = input.command;

        let result = match command {
            "jest" | "vitest" => filter_jest_vitest(&text, input.exit_code, config),
            "mocha" => filter_mocha(&text, input.exit_code, config),
            "pytest" => filter_pytest(&text, input.exit_code, config),
            "phpunit" => filter_phpunit(&text, input.exit_code, config),
            _ => filter_jest_vitest(&text, input.exit_code, config),
        };

        let output = if result.len() > config.max_output_bytes {
            let truncated = truncate_utf8_to(
                result.as_bytes(),
                config.max_output_bytes.saturating_sub(80),
            );
            let mut s = String::from_utf8_lossy(truncated).to_string();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str("[truncated]\n");
            s
        } else {
            result
        };

        FilterResult {
            output,
            input_bytes,
        }
    }
}

fn filter_jest_vitest(text: &str, exit_code: i32, _config: &FilterConfig) -> String {
    let lines: Vec<&str> = text.lines().collect();

    // Find summary line (jest: "Tests:" or "Test Suites:", vitest: "Test Files:")
    let summary = lines
        .iter()
        .rev()
        .find(|l| {
            l.contains("Tests:")
                || l.contains("Test Suites:")
                || l.contains("Test Files:")
                || l.contains("passed")
                || l.contains("failed")
        })
        .copied()
        .unwrap_or("");

    // If all passing, single-line summary
    if exit_code == 0 {
        if !summary.is_empty() {
            return format!("{}\n", summary.trim());
        }
        // Find any pass summary
        if let Some(line) = lines.iter().find(|l| l.contains("passed")) {
            return format!("{}\n", line.trim());
        }
        return "Tests: all passed\n".to_string();
    }

    // Failures: collect FAIL blocks
    let mut output = String::new();
    let mut in_fail_block = false;
    let mut fail_block_lines: Vec<&str> = Vec::new();

    for line in &lines {
        if line.starts_with("FAIL ") || line.starts_with(" FAIL ") {
            // Start of a new fail block — flush previous
            if !fail_block_lines.is_empty() {
                for bl in &fail_block_lines {
                    output.push_str(bl);
                    output.push('\n');
                }
                fail_block_lines.clear();
            }
            in_fail_block = true;
            fail_block_lines.push(line);
        } else if line.starts_with("PASS ") || line.starts_with(" PASS ") {
            // Flush any collected fail block
            if !fail_block_lines.is_empty() {
                for bl in &fail_block_lines {
                    output.push_str(bl);
                    output.push('\n');
                }
                fail_block_lines.clear();
            }
            in_fail_block = false;
        } else if in_fail_block {
            fail_block_lines.push(line);
        }
        // Skip PASS lines and lines outside a fail block
    }

    // Flush remaining fail block
    for bl in &fail_block_lines {
        output.push_str(bl);
        output.push('\n');
    }

    if !summary.is_empty() {
        output.push_str(summary.trim());
        output.push('\n');
    }

    output
}

fn filter_mocha(text: &str, exit_code: i32, _config: &FilterConfig) -> String {
    let lines: Vec<&str> = text.lines().collect();

    // Find summary (mocha: "N passing", "N failing")
    let passing = lines
        .iter()
        .find(|l| l.trim().contains("passing"))
        .copied()
        .unwrap_or("");
    let failing = lines
        .iter()
        .find(|l| l.trim().contains("failing"))
        .copied()
        .unwrap_or("");

    if exit_code == 0 {
        return format!("{}\n", passing.trim());
    }

    // Collect failure sections — mocha labels them with numbers like "  1) Suite > test:"
    let mut output = String::new();
    let mut in_failure = false;

    for line in &lines {
        let trimmed = line.trim();
        // Mocha failure marker: "  N) Suite title"
        if trimmed.len() > 2
            && trimmed.starts_with(|c: char| c.is_ascii_digit())
            && trimmed.contains(')')
        {
            in_failure = true;
        }
        if in_failure {
            output.push_str(line);
            output.push('\n');
        }
        // Stop collecting at summary lines
        if in_failure && (trimmed.contains("passing") || trimmed.contains("failing")) {
            in_failure = false;
        }
    }

    if !failing.is_empty() {
        output.push_str(failing.trim());
        output.push('\n');
    }
    if !passing.is_empty() {
        output.push_str(passing.trim());
        output.push('\n');
    }

    output
}

fn filter_pytest(text: &str, exit_code: i32, _config: &FilterConfig) -> String {
    let lines: Vec<&str> = text.lines().collect();

    // Find summary line (pytest: "=== N failed, N passed in Xs ===")
    let summary = lines
        .iter()
        .rev()
        .find(|l| l.contains("passed") || l.contains("failed") || l.contains("error"))
        .copied()
        .unwrap_or("");

    if exit_code == 0 {
        return format!("{}\n", summary.trim());
    }

    // Collect FAILED sections
    let mut output = String::new();
    let mut in_fail_section = false;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("FAILED ")
            || trimmed == "FAILURES"
            || trimmed.starts_with("====") && trimmed.contains("FAILURES")
        {
            in_fail_section = true;
        }
        if in_fail_section {
            output.push_str(line);
            output.push('\n');
        }
        // End on short summary line
        if in_fail_section
            && trimmed.starts_with("=====")
            && (trimmed.contains("passed") || trimmed.contains("failed"))
        {
            in_fail_section = false;
        }
    }

    if !summary.is_empty() && !output.contains(summary.trim()) {
        output.push_str(summary.trim());
        output.push('\n');
    }

    output
}

fn filter_phpunit(text: &str, exit_code: i32, _config: &FilterConfig) -> String {
    let lines: Vec<&str> = text.lines().collect();

    // Find summary line (phpunit: "Tests: N, Assertions: N, Failures: N")
    let summary = lines
        .iter()
        .rev()
        .find(|l| l.contains("Tests:") || l.contains("OK (") || l.contains("FAILURES!"))
        .copied()
        .unwrap_or("");

    if exit_code == 0 {
        return format!("{}\n", summary.trim());
    }

    // Collect FAILURES! section
    let mut output = String::new();
    let mut in_failures = false;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed == "FAILURES!" || trimmed.starts_with("There was") {
            in_failures = true;
        }
        if in_failures {
            output.push_str(line);
            output.push('\n');
        }
    }

    if !summary.is_empty() && !output.contains(summary.trim()) {
        output.push_str(summary.trim());
        output.push('\n');
    }

    output
}
