use std::env;
use std::path::PathBuf;

pub fn state_dir() -> Option<PathBuf> {
    if let Ok(path) = env::var("QUIET_CONTEXT_NATIVE_STATE_DIR") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")))
        .map(|p| p.join("quietcontext").join("native"))
}

pub fn session_id() -> Option<String> {
    env::var("QUIET_CONTEXT_SESSION_ID")
        .ok()
        .or_else(|| env::var("CLAUDE_CODE_SESSION_ID").ok())
}

pub fn cap_bypassed() -> bool {
    matches!(env::var("QUIET_CONTEXT_FULL").as_deref(), Ok("1"))
}

pub fn should_filter() -> bool {
    !cap_bypassed()
}

pub fn apply_global_cap(s: String, max_bytes: usize, stream: &str) -> String {
    if max_bytes == 0 || s.len() <= max_bytes {
        return s;
    }
    let total = s.len();
    let head_budget = max_bytes / 4 * 3;
    let tail_budget = max_bytes - head_budget;
    let mut head_end = head_budget.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(tail_budget).max(head_end);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let kept = head_end + (total - tail_start);
    let mut out = String::with_capacity(max_bytes + 160);
    out.push_str(&s[..head_end]);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[qc: {stream} capped, kept {kept}/{total} bytes — narrow the command, or set QUIET_CONTEXT_FULL=1 for full output]\n"
    ));
    out.push_str(&s[tail_start..]);
    out
}
