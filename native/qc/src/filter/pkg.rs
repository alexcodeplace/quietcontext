use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Filter for package manager commands: npm, pnpm, yarn, bun, npx.
/// Strips progress/spinner/download lines, keeps warnings and errors, shows summary.
pub struct PkgFilter;

/// Spinner characters used by various package managers
const SPINNER_CHARS: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn is_progress_line(line: &str) -> bool {
    // Lines with spinner characters
    if line.chars().any(|c| SPINNER_CHARS.contains(&c)) {
        return true;
    }
    // Lines that start with \r (carriage return overwrites)
    if line.starts_with('\r') {
        return true;
    }
    // "Done in X.Xs" timing lines
    if line.trim_start().starts_with("Done in ") {
        return true;
    }
    // Download progress lines: "GET https://..." or "npm http ..."
    let trimmed = line.trim();
    if trimmed.starts_with("GET ") || trimmed.starts_with("HEAD ") {
        return true;
    }
    if trimmed.starts_with("npm http") || trimmed.starts_with("npm timing") {
        return true;
    }
    // Lines that are pure progress bars: contain only ASCII progress chars and spaces
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| matches!(c, '#' | '-' | '=' | '>' | ' ' | '[' | ']' | '|'))
        && trimmed.len() > 5
    {
        return true;
    }
    false
}

fn is_warning_line(line: &str) -> bool {
    let upper = line.to_uppercase();
    upper.contains("WARN") || upper.contains("WARNING")
}

fn is_error_line(line: &str) -> bool {
    let upper = line.to_uppercase();
    upper.contains("ERR!") || upper.contains("ERROR") || upper.contains("ERR ")
}

impl OutputFilter for PkgFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        // Combine stdout + stderr, normalizing carriage returns first
        let stdout = String::from_utf8_lossy(input.stdout);
        let stderr = String::from_utf8_lossy(input.stderr);

        // Scrub \r before splitting into lines so progress-overwrite lines are visible
        let combined = format!("{}\n{}", stdout, stderr);
        let cleaned = combined.replace('\r', "\n");

        let input_bytes = input.stdout.len() + input.stderr.len();

        let mut kept_lines: Vec<&str> = Vec::new();
        let mut warning_count = 0usize;
        let mut package_count = 0usize;

        for line in cleaned.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if is_progress_line(line) {
                continue;
            }

            if is_warning_line(line) {
                warning_count += 1;
                kept_lines.push(line);
            } else if is_error_line(line) {
                kept_lines.push(line);
            } else if line.trim().starts_with("added ")
                || line.trim().starts_with("removed ")
                || line.trim().starts_with("changed ")
                || line.trim().starts_with("audited ")
                || line.trim().starts_with("found ")
                || line.trim().starts_with("packages installed")
                || line.trim().starts_with("packages removed")
            {
                // Summary-like lines — parse package count heuristic
                if let Some(n) = extract_leading_number(line.trim()) {
                    package_count = package_count.max(n);
                }
                kept_lines.push(line);
            } else {
                kept_lines.push(line);
            }
        }

        let mut output = String::new();
        for line in &kept_lines {
            output.push_str(line);
            output.push('\n');
        }

        // Only append summary if it carries useful info (something was installed/warned)
        if package_count > 0 || warning_count > 0 {
            let summary = format!(
                "{} install: {} packages, {} warnings\n",
                input.command, package_count, warning_count
            );
            output.push_str(&summary);
        }

        // Byte-cap
        if output.len() > config.max_output_bytes {
            safe_truncate(&mut output, config.max_output_bytes);
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str("[truncated]\n");
        }

        // Pass through if fits and we didn't actually filter much
        FilterResult {
            output,
            input_bytes,
        }
    }

    fn hint(&self, input: &FilterInput) -> Option<String> {
        let args_str = input.args.join(" ");
        match input.command {
            "npm" | "pnpm" => {
                if (args_str.contains("ls") || args_str.contains("list"))
                    && !args_str.contains("--depth=0")
                {
                    Some(format!(
                        "next time: {} ls --depth=0  # top-level only, ~85% smaller",
                        input.command
                    ))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

fn extract_leading_number(s: &str) -> Option<usize> {
    let num_str: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    num_str.parse().ok()
}
