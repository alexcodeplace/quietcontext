use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};
use serde_json::Value;

pub struct JsonFilter;

impl OutputFilter for JsonFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let text = String::from_utf8_lossy(raw);
        let trimmed = text.trim();

        if trimmed.contains('\n') && !trimmed.starts_with('{') && !trimmed.starts_with('[') {
            return filter_ndjson(trimmed, input_bytes, config);
        }

        match serde_json::from_str::<Value>(trimmed) {
            Ok(val) => {
                let schema = extract_json_schema(&val, 0);
                let output = format!("JSON ({} bytes):\n{}\n", input_bytes, schema);
                FilterResult {
                    output,
                    input_bytes,
                }
            }
            Err(_) => {
                let lines: Vec<&str> = text.lines().collect();
                let shown = lines.len().min(config.max_lines);
                let mut output: String = lines[..shown].join("\n");
                output.push('\n');
                if lines.len() > config.max_lines {
                    output.push_str(&format!("[truncated: {}/{} lines]\n", shown, lines.len()));
                }
                FilterResult {
                    output,
                    input_bytes,
                }
            }
        }
    }
}

fn filter_ndjson(text: &str, input_bytes: usize, config: &FilterConfig) -> FilterResult {
    let mut count = 0usize;
    let mut first_schema = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            count += 1;
            if first_schema.is_empty() {
                first_schema = extract_json_schema(&val, 0);
            }
        }
    }

    let output = if count > 0 {
        format!(
            "NDJSON ({} records, {} bytes):\n{}\n",
            count, input_bytes, first_schema
        )
    } else {
        let lines: Vec<&str> = text.lines().collect();
        let shown = lines.len().min(config.max_lines);
        let mut s: String = lines[..shown].join("\n");
        s.push('\n');
        s
    };

    FilterResult {
        output,
        input_bytes,
    }
}

pub(crate) fn extract_json_schema(val: &Value, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match val {
        Value::Object(map) => {
            if map.is_empty() {
                return format!("{}{{  }}", indent);
            }
            let mut parts = Vec::new();
            for (key, v) in map {
                let type_str = match v {
                    Value::Null => "null".to_string(),
                    Value::Bool(_) => "bool".to_string(),
                    Value::Number(_) => "number".to_string(),
                    Value::String(_) => "string".to_string(),
                    Value::Array(arr) => {
                        if arr.is_empty() {
                            "[]".to_string()
                        } else {
                            let inner = extract_json_schema(&arr[0], depth + 1);
                            format!("Array<{}> ({} items)", inner.trim(), arr.len())
                        }
                    }
                    Value::Object(_) => {
                        if depth < 3 {
                            format!("{{\n{}\n{}}}", extract_json_schema(v, depth + 1), indent)
                        } else {
                            "{...}".to_string()
                        }
                    }
                };
                parts.push(format!("{}  \"{}\": {}", indent, key, type_str));
            }
            format!("{}{{\n{}\n{}}}", indent, parts.join(",\n"), indent)
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                format!("{}[]", indent)
            } else {
                let inner = extract_json_schema(&arr[0], depth);
                format!("{}Array<{}> ({} items)", indent, inner.trim(), arr.len())
            }
        }
        Value::String(_) => "string".to_string(),
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Null => "null".to_string(),
    }
}
