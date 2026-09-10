use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct EnvFilter;

const NOISE_PREFIXES: &[&str] = &[
    "PATH=",
    "MANPATH=",
    "INFOPATH=",
    "LANG=",
    "LC_",
    "LANGUAGE=",
    "TERM=",
    "TERM_PROGRAM",
    "COLORTERM=",
    "XDG_",
    "DBUS_",
    "DESKTOP_",
    "GTK_",
    "QT_",
    "GDK_",
    "SSH_AUTH_SOCK=",
    "SSH_AGENT_PID=",
    "DISPLAY=",
    "WAYLAND_DISPLAY=",
    "LS_COLORS=",
    "LSCOLORS=",
    "LESS=",
    "PAGER=",
    "SHELL=",
    "SHLVL=",
    "OLDPWD=",
    "_=",
    "COMP_WORDBREAKS=",
    "WINDOWID=",
    "WINDOWPATH=",
    "SESSION_MANAGER=",
];

impl OutputFilter for EnvFilter {
    fn filter(&self, input: &FilterInput, _config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();
        let text = String::from_utf8_lossy(raw);

        let mut kept: Vec<&str> = Vec::new();
        let mut stripped = 0usize;

        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let is_noise = NOISE_PREFIXES.iter().any(|p| trimmed.starts_with(p));
            if is_noise {
                stripped += 1;
            } else {
                kept.push(trimmed);
            }
        }

        let mut output = String::new();
        for line in &kept {
            output.push_str(line);
            output.push('\n');
        }
        if stripped > 0 {
            output.push_str(&format!(
                "[{} noise variables stripped (PATH, LANG, XDG_*, etc.)]\n",
                stripped
            ));
        }

        FilterResult {
            output,
            input_bytes,
        }
    }
}
