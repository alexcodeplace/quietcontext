use super::traits::{safe_truncate, FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Filter for git subcommands.
/// Dispatches per subcommand detected from args[0].
pub struct GitFilter;

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

impl OutputFilter for GitFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw_stdout = input.stdout;
        let raw_stderr = input.stderr;
        let input_bytes = raw_stdout.len() + raw_stderr.len();

        // Detect subcommand from args
        let subcommand = input.args.first().map(|s| s.as_str()).unwrap_or("");

        match subcommand {
            "log" => filter_git_log(raw_stdout, input_bytes, config),
            "diff" => filter_git_diff(raw_stdout, input_bytes, config),
            "status" => filter_git_status(raw_stdout, input_bytes, config),
            "show" => filter_git_show(raw_stdout, input_bytes, config),
            "blame" => filter_git_blame(raw_stdout, input_bytes, config),
            _ => {
                // Generic fallback: combine stdout+stderr, cap at max_output_bytes
                let combined: Vec<u8> = [raw_stdout, raw_stderr].concat();
                let total = combined.len();
                if total <= config.max_output_bytes {
                    let output = String::from_utf8_lossy(&combined).to_string();
                    FilterResult {
                        output,
                        input_bytes,
                    }
                } else {
                    let truncated =
                        truncate_utf8_to(&combined, config.max_output_bytes.saturating_sub(80));
                    let shown = truncated.len();
                    let mut output = String::from_utf8_lossy(truncated).to_string();
                    if !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&format!("[truncated: {}/{} bytes]\n", shown, total));
                    FilterResult {
                        output,
                        input_bytes,
                    }
                }
            }
        }
    }

    fn hint(&self, input: &FilterInput) -> Option<String> {
        let args = &input.args;
        let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
        let args_str = args.join(" ");
        match subcmd {
            "log" => {
                if args_str.contains("--oneline") || args_str.contains("--format") {
                    None
                } else {
                    Some("next time: git log --oneline --max-count=20".to_string())
                }
            }
            "diff" => {
                if args_str.contains("--stat") {
                    None
                } else {
                    Some("next time: git diff --stat  # when full diff not needed".to_string())
                }
            }
            "show" => {
                if args_str.contains("--stat") {
                    None
                } else {
                    Some("next time: git show --stat  # commit summary only".to_string())
                }
            }
            _ => None,
        }
    }
}

fn filter_git_log(raw: &[u8], input_bytes: usize, config: &FilterConfig) -> FilterResult {
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.lines().collect();
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
    FilterResult {
        output,
        input_bytes,
    }
}

pub(crate) fn filter_git_diff(
    raw: &[u8],
    input_bytes: usize,
    config: &FilterConfig,
) -> FilterResult {
    let text = String::from_utf8_lossy(raw);

    if raw.len() <= config.max_output_bytes {
        let output = text.into_owned();
        return FilterResult {
            output,
            input_bytes,
        };
    }

    // Keep hunk headers (@@) + changed lines (+/-). Strip context beyond ±2 from changes.
    let mut output = String::new();
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();

    // Mark which lines are "interesting" (changed, or ±2 context around them)
    let mut keep = vec![false; n];

    // First pass: mark changed lines and hunk headers
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("@@")
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with('+')
            || line.starts_with('-')
        {
            keep[i] = true;
        }
    }

    // Second pass: add ±2 context around kept lines
    let mut extended = keep.clone();
    for (i, &k) in keep.iter().enumerate() {
        if k {
            let start = i.saturating_sub(2);
            let end = (i + 3).min(n);
            extended[start..end].fill(true);
        }
    }

    let mut prev_kept = true;
    for (i, line) in lines.iter().enumerate() {
        if extended[i] {
            if !prev_kept {
                output.push_str("...\n");
            }
            output.push_str(line);
            output.push('\n');
            prev_kept = true;
        } else {
            prev_kept = false;
        }

        if output.len() >= config.max_output_bytes {
            output.push_str("[truncated]\n");
            break;
        }
    }

    FilterResult {
        output,
        input_bytes,
    }
}

fn filter_git_status(raw: &[u8], input_bytes: usize, config: &FilterConfig) -> FilterResult {
    let text = String::from_utf8_lossy(raw);

    let mut staged: Vec<String> = Vec::new();
    let mut unstaged: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();
    let mut other_lines: Vec<String> = Vec::new();

    let mut in_staged = false;
    let mut in_unstaged = false;
    let mut in_untracked = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_staged = false;
            in_unstaged = false;
            in_untracked = false;
            continue;
        }

        // Section headers
        if trimmed.starts_with("Changes to be committed") {
            in_staged = true;
            in_unstaged = false;
            in_untracked = false;
            continue;
        }
        if trimmed.starts_with("Changes not staged") {
            in_staged = false;
            in_unstaged = true;
            in_untracked = false;
            continue;
        }
        if trimmed.starts_with("Untracked files") {
            in_staged = false;
            in_unstaged = false;
            in_untracked = true;
            continue;
        }
        // Skip git status hint lines and verbose state lines redundant with compact summary
        if trimmed.starts_with("(use ")
            || trimmed.starts_with("no changes")
            || trimmed.starts_with("nothing to commit")
            || trimmed.starts_with("Your branch is up to date")
            || trimmed.starts_with("Your branch is ahead")
            || trimmed.starts_with("Your branch is behind")
        {
            continue;
        }

        if in_staged {
            staged.push(trimmed.to_string());
        } else if in_unstaged {
            unstaged.push(trimmed.to_string());
        } else if in_untracked {
            untracked.push(trimmed.to_string());
        } else {
            other_lines.push(line.to_string());
        }
    }

    let mut output = String::new();

    // Other lines first (branch info etc.)
    for line in &other_lines {
        output.push_str(line);
        output.push('\n');
    }

    // Compact counts
    output.push_str(&format!(
        "staged={} unstaged={} untracked={}\n",
        staged.len(),
        unstaged.len(),
        untracked.len()
    ));

    let max = config.max_lines / 3;

    if !staged.is_empty() {
        output.push_str("Staged:\n");
        for (i, f) in staged.iter().enumerate() {
            if i >= max {
                output.push_str(&format!("  ... ({} more)\n", staged.len() - max));
                break;
            }
            output.push_str(&format!("  {}\n", f));
        }
    }
    if !unstaged.is_empty() {
        output.push_str("Unstaged:\n");
        for (i, f) in unstaged.iter().enumerate() {
            if i >= max {
                output.push_str(&format!("  ... ({} more)\n", unstaged.len() - max));
                break;
            }
            output.push_str(&format!("  {}\n", f));
        }
    }
    if !untracked.is_empty() {
        output.push_str("Untracked:\n");
        for (i, f) in untracked.iter().enumerate() {
            if i >= max {
                output.push_str(&format!("  ... ({} more)\n", untracked.len() - max));
                break;
            }
            output.push_str(&format!("  {}\n", f));
        }
    }

    FilterResult {
        output,
        input_bytes,
    }
}

fn filter_git_show(raw: &[u8], input_bytes: usize, config: &FilterConfig) -> FilterResult {
    // commit header lines + diff rules (same as git diff)
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.lines().collect();

    let mut output = String::new();
    let mut in_diff = false;

    // Collect header lines (commit info before the diff)
    for line in &lines {
        if line.starts_with("diff --git") {
            in_diff = true;
        }
        if !in_diff {
            output.push_str(line);
            output.push('\n');
        }
    }

    if in_diff {
        // Find where the diff starts
        let diff_start = lines
            .iter()
            .position(|l| l.starts_with("diff --git"))
            .unwrap_or(0);
        let diff_raw = lines[diff_start..].join("\n").into_bytes();
        let diff_result = filter_git_diff(&diff_raw, input_bytes, config);
        output.push_str(&diff_result.output);
    }

    if output.len() > config.max_output_bytes {
        safe_truncate(&mut output, config.max_output_bytes.saturating_sub(80));
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

fn filter_git_blame(raw: &[u8], input_bytes: usize, config: &FilterConfig) -> FilterResult {
    let text = String::from_utf8_lossy(raw);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let shown = total.min(config.max_lines);
    let mut output = String::new();

    for line in &lines[..shown] {
        // Strip commit metadata beyond hash + author: keep first 50 chars then the code
        // Typical blame line: "^abc1234 (Author Name        2024-01-01 10:00:00 +0000  1) code here"
        if let Some(paren_start) = line.find('(') {
            let hash = line[..paren_start.min(9)].trim();
            // Find closing paren
            if let Some(paren_end) = line[paren_start..].find(')') {
                let meta = &line[paren_start + 1..paren_start + paren_end];
                // Extract just the author (first word(s) before date)
                let author: String = meta
                    .split_whitespace()
                    .take_while(|w| {
                        !w.chars()
                            .next()
                            .map(|c| c.is_ascii_digit())
                            .unwrap_or(false)
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let code = &line[paren_start + paren_end + 1..];
                output.push_str(&format!("{} ({}) {}\n", hash, author, code.trim_start()));
            } else {
                output.push_str(line);
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    if total > config.max_lines {
        output.push_str(&format!("[truncated: {}/{} lines shown]\n", shown, total));
    }

    FilterResult {
        output,
        input_bytes,
    }
}
