use super::traits::{truncate_str, FilterConfig, FilterInput, FilterResult, OutputFilter};
use std::collections::HashMap;

/// Filter for search/find commands: grep, rg, egrep, fgrep, ack, find, tree.
pub struct GrepFilter;

/// Detect if a line matches the `filename:linenum:content` pattern.
/// Returns (filename, linenum, content) if matched.
fn parse_grep_line(line: &str) -> Option<(&str, &str, &str)> {
    // Pattern: filename:linenum:content  (rg/grep default output)
    let first_colon = line.find(':')?;
    let rest = &line[first_colon + 1..];
    let second_colon = rest.find(':')?;
    let linenum = &rest[..second_colon];
    // linenum must be all digits
    if linenum.is_empty() || !linenum.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let filename = &line[..first_colon];
    let content = &rest[second_colon + 1..];
    Some((filename, linenum, content))
}

fn truncate_line(line: &str, max_width: usize) -> String {
    let trimmed = line.trim_end();
    if trimmed.len() <= max_width {
        trimmed.to_string()
    } else {
        format!("{}…", truncate_str(trimmed, max_width.saturating_sub(1)))
    }
}

impl OutputFilter for GrepFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let command = input.command;

        let text = String::from_utf8_lossy(raw);

        // find and tree: simple line-cap, no grouping
        if command == "find" || command == "tree" {
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
            return FilterResult {
                output,
                input_bytes,
            };
        }

        // grep/rg/egrep/fgrep/ack: group by filename
        // Two modes: lines with filename:linenum prefix, or bare lines (no filename in output)
        //
        // Ordered map to preserve first-seen filename order
        let mut file_matches: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let mut file_index: HashMap<String, usize> = HashMap::new();
        let mut bare_lines: Vec<String> = Vec::new();
        let mut total_matches = 0usize;

        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }

            if let Some((filename, linenum, content)) = parse_grep_line(line) {
                total_matches += 1;
                let fname = filename.to_string();
                if let Some(&idx) = file_index.get(&fname) {
                    let entry = &mut file_matches[idx];
                    if entry.1.len() < config.grep_max_per_file {
                        entry.1.push((
                            linenum.to_string(),
                            truncate_line(content, config.max_width),
                        ));
                    }
                } else {
                    let idx = file_matches.len();
                    file_index.insert(fname.clone(), idx);
                    file_matches.push((
                        fname,
                        vec![(
                            linenum.to_string(),
                            truncate_line(content, config.max_width),
                        )],
                    ));
                }
            } else {
                // No filename prefix — bare match line
                total_matches += 1;
                bare_lines.push(truncate_line(line, config.max_width));
            }
        }

        let mut output = String::new();
        let mut shown_matches = 0usize;

        if !file_matches.is_empty() {
            // Cap total shown matches at grep_max_results
            'outer: for (filename, matches) in &file_matches {
                output.push_str(&format!("{} ({} matches):\n", filename, matches.len()));
                for (linenum, content) in matches {
                    if shown_matches >= config.grep_max_results {
                        break 'outer;
                    }
                    output.push_str(&format!("  {}: {}\n", linenum, content));
                    shown_matches += 1;
                }
            }
        } else {
            // Bare lines
            for line in &bare_lines {
                if shown_matches >= config.grep_max_results {
                    break;
                }
                output.push_str(line);
                output.push('\n');
                shown_matches += 1;
            }
        }

        if total_matches > config.grep_max_results {
            output.push_str(&format!(
                "[truncated: {}/{} matches shown]\n",
                shown_matches, total_matches
            ));
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}
