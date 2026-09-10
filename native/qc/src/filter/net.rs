use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use serde_json::Value;

/// Filter for HTTP client commands: curl, wget, httpie, http.
/// Auto-detects JSON responses and extracts schema instead of raw values.
pub struct NetFilter;

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

/// Walk a serde_json::Value and return a compact type-schema string.
fn json_schema(v: &Value, depth: usize) -> String {
    if depth > 4 {
        return "<…>".to_string();
    }
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::String(_) => "string".to_string(),
        Value::Array(arr) => {
            if arr.is_empty() {
                "Array (0 items)".to_string()
            } else {
                let inner = json_schema(&arr[0], depth + 1);
                format!("Array ({} items) [{}]", arr.len(), inner)
            }
        }
        Value::Object(map) => {
            let fields: Vec<String> = map
                .iter()
                .take(20) // don't blow up on huge objects
                .map(|(k, val)| format!("{}: {}", k, json_schema(val, depth + 1)))
                .collect();
            let mut s = format!("{{ {} }}", fields.join(", "));
            if map.len() > 20 {
                s.push_str(&format!(" (+{} more fields)", map.len() - 20));
            }
            s
        }
    }
}

/// Split HTTP response into (headers, body) at the blank line separator.
fn split_http_response(text: &str) -> (&str, &str) {
    // HTTP response: headers end at first blank line
    if let Some(pos) = text.find("\r\n\r\n") {
        return (&text[..pos], &text[pos + 4..]);
    }
    if let Some(pos) = text.find("\n\n") {
        return (&text[..pos], &text[pos + 2..]);
    }
    // No blank line separator — treat whole thing as body
    ("", text)
}

/// Extract HTTP status line and content-type from headers section.
fn parse_http_headers(headers: &str) -> (String, String) {
    let mut status = String::new();
    let mut content_type = String::new();

    for line in headers.lines() {
        if line.starts_with("HTTP/") {
            // "HTTP/1.1 200 OK"
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            if parts.len() >= 2 {
                status = parts[1].to_string();
            }
        }
        let lower = line.to_lowercase();
        if lower.starts_with("content-type:") {
            content_type = line[13..].trim().to_string();
        }
    }

    (status, content_type)
}

fn is_json_content(content_type: &str, body: &str) -> bool {
    if content_type.contains("json") {
        return true;
    }
    let trimmed = body.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

impl OutputFilter for NetFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();

        let text = String::from_utf8_lossy(raw);
        let (headers, body) = split_http_response(&text);

        let (status, content_type) = if headers.is_empty() {
            ("".to_string(), "".to_string())
        } else {
            parse_http_headers(headers)
        };

        let body_trimmed = body.trim();

        if is_json_content(&content_type, body_trimmed) {
            // Try to parse JSON and extract schema
            match serde_json::from_str::<Value>(body_trimmed) {
                Ok(val) => {
                    let size_kb = input_bytes as f64 / 1024.0;
                    let schema = json_schema(&val, 0);

                    let ct_display = if content_type.is_empty() {
                        "application/json".to_string()
                    } else {
                        content_type.clone()
                    };

                    let output = if !status.is_empty() {
                        format!(
                            "HTTP {} | {} | {:.1}KB\n{}\n",
                            status, ct_display, size_kb, schema
                        )
                    } else {
                        format!("{} | {:.1}KB\n{}\n", ct_display, size_kb, schema)
                    };

                    return FilterResult {
                        output,
                        input_bytes,
                    };
                }
                Err(_) => {
                    // JSON parse failed — fall through to byte cap
                }
            }
        }

        // Non-JSON or parse failure: byte cap only when over limit
        if input_bytes <= config.max_output_bytes {
            let output = String::from_utf8_lossy(raw).to_string();
            return FilterResult {
                output,
                input_bytes,
            };
        }

        let truncated = truncate_utf8_to(raw, config.max_output_bytes.saturating_sub(80));
        let shown = truncated.len();
        let mut output = String::from_utf8_lossy(truncated).to_string();
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&format!("[truncated: {}/{} bytes]\n", shown, input_bytes));

        FilterResult {
            output,
            input_bytes,
        }
    }
}
