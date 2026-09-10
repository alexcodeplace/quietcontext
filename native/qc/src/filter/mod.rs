mod ansible;
mod build;
mod bun;
mod db;
mod diff;
mod env;
mod file;
mod generic;
mod gh;
pub(crate) mod git;
mod grep;
pub(crate) mod json;
mod kubectl;
mod logs;
mod ls;
mod net;
mod pkg;
mod process;
mod sed;
mod terraform;
mod test;
mod traits;

pub use generic::GenericFilter;
pub use traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

/// Commands `detect` routes to a dedicated filter. Advertised in `qc --help`;
/// `every_wrapped_command_has_a_dedicated_filter` guards against drift.
#[cfg(test)]
pub const WRAPPED_COMMANDS: &[&str] = &[
    "grep",
    "rg",
    "egrep",
    "fgrep",
    "ack",
    "find",
    "tree",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "bat",
    "git",
    "gh",
    "glab",
    "diff",
    "colordiff",
    "ls",
    "exa",
    "eza",
    "cargo",
    "go",
    "tsc",
    "swc",
    "make",
    "cmake",
    "ninja",
    "jest",
    "vitest",
    "mocha",
    "playwright",
    "pytest",
    "phpunit",
    "npm",
    "pnpm",
    "yarn",
    "npx",
    "bun",
    "curl",
    "wget",
    "httpie",
    "http",
    "journalctl",
    "dmesg",
    "docker",
    "podman",
    "kubectl",
    "terraform",
    "tofu",
    "ansible",
    "ansible-playbook",
    "jq",
    "yq",
    "sqlite3",
    "psql",
    "mysql",
    "sed",
    "ps",
    "lsof",
    "top",
    "htop",
    "env",
    "printenv",
];

pub struct FilterRegistry;

impl FilterRegistry {
    pub fn detect(command: &str, args: &[String]) -> Box<dyn OutputFilter> {
        match command {
            "grep" | "rg" | "egrep" | "fgrep" | "ack" | "find" | "tree" => {
                Box::new(grep::GrepFilter)
            }
            "git" => Box::new(git::GitFilter),
            "gh" | "glab" => Box::new(gh::GhFilter),
            "cat" | "head" | "tail" | "less" | "more" | "bat" => Box::new(file::FileFilter),
            "ls" | "exa" | "eza" => Box::new(ls::LsFilter),
            "ps" | "lsof" | "top" | "htop" => Box::new(process::ProcessFilter),
            "env" | "printenv" => Box::new(env::EnvFilter),
            "diff" | "colordiff" => Box::new(diff::DiffFilter),
            "curl" | "wget" | "httpie" | "http" => Box::new(net::NetFilter),
            "npm" | "pnpm" | "yarn" | "npx" => Box::new(pkg::PkgFilter),
            "bun" => Box::new(bun::BunFilter),
            "jest" | "vitest" | "mocha" | "playwright" => Box::new(test::TestFilter),
            "pytest" | "phpunit" => Box::new(test::TestFilter),
            "python" | "python3" => {
                let has_pytest = args.windows(2).any(|w| w[0] == "-m" && w[1] == "pytest");
                if has_pytest {
                    Box::new(test::TestFilter)
                } else {
                    Box::new(GenericFilter)
                }
            }
            "tsc" | "swc" => Box::new(build::BuildFilter),
            "cargo" | "go" => Box::new(build::BuildFilter),
            "make" | "cmake" | "ninja" => Box::new(build::BuildFilter),
            "docker" | "podman" => {
                if args.first().map(|s| s.as_str()) == Some("logs") {
                    Box::new(logs::LogFilter)
                } else {
                    Box::new(GenericFilter)
                }
            }
            "kubectl" | "k" => Box::new(kubectl::KubectlFilter),
            "terraform" | "tofu" => Box::new(terraform::TerraformFilter),
            "ansible-playbook" | "ansible" => Box::new(ansible::AnsibleFilter),
            "journalctl" | "dmesg" => Box::new(logs::LogFilter),
            "sed" => Box::new(sed::SedFilter),
            "sqlite3" | "psql" | "mysql" => Box::new(db::DbFilter),
            "jq" | "yq" => Box::new(json::JsonFilter),
            _ => Box::new(GenericFilter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WRAPPED_COMMANDS;

    /// `k` is a `kubectl` alias; `python`/`python3` only reach a dedicated
    /// filter for `-m pytest` and otherwise fall through to `GenericFilter`.
    const UNADVERTISED: &[&str] = &["k", "python", "python3"];

    fn detect_arm_patterns() -> Vec<String> {
        let src = include_str!("mod.rs");
        let body = src.split_once("pub fn detect").expect("detect not found").1;
        let arms = body
            .split_once("_ => Box::new(GenericFilter)")
            .expect("catch-all arm not found")
            .0;
        arms.lines()
            .filter_map(|line| line.split_once("=>"))
            .flat_map(|(pattern, _)| {
                pattern
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn every_wrapped_command_has_a_dedicated_filter() {
        let patterns = detect_arm_patterns();
        for cmd in WRAPPED_COMMANDS {
            assert!(
                patterns.iter().any(|p| p == cmd),
                "`{cmd}` is advertised in `qc --help` but `detect` has no arm for it"
            );
        }
    }

    #[test]
    fn every_dedicated_filter_is_advertised() {
        for pattern in detect_arm_patterns() {
            assert!(
                WRAPPED_COMMANDS.contains(&pattern.as_str())
                    || UNADVERTISED.contains(&pattern.as_str()),
                "`{pattern}` has a dedicated filter in `detect` but is missing from WRAPPED_COMMANDS, so `qc --help` never mentions it"
            );
        }
    }
}
