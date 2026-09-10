use serde::Deserialize;
use std::path::PathBuf;

fn default_max_output_bytes() -> usize {
    4096
}
fn default_max_lines() -> usize {
    100
}
fn default_max_width() -> usize {
    120
}
fn default_grep_max_results() -> usize {
    200
}
fn default_grep_max_per_file() -> usize {
    25
}
fn default_hint_threshold() -> usize {
    4000
}
fn default_global_max_sent_bytes() -> usize {
    32768
}
fn default_dedupe_min_bytes() -> usize {
    8192
}
fn default_dedupe_ttl_secs() -> u64 {
    600
}
fn default_map_max_bytes() -> usize {
    12288
}
fn default_map_max_files() -> usize {
    2000
}
fn default_map_max_file_bytes() -> usize {
    1048576
}
fn default_map_refs_max_per_file() -> usize {
    5
}
fn default_map_refs_max_total() -> usize {
    200
}
fn default_outline_max_bytes() -> usize {
    6000
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_hint_threshold")]
    pub hint_threshold: usize,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    #[serde(default = "default_max_width")]
    pub max_width: usize,
    #[serde(default = "default_grep_max_results")]
    pub grep_max_results: usize,
    #[serde(default = "default_grep_max_per_file")]
    pub grep_max_per_file: usize,
    #[serde(default = "default_global_max_sent_bytes")]
    pub global_max_sent_bytes: usize,
}
impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_output_bytes: default_max_output_bytes(),
            hint_threshold: default_hint_threshold(),
            max_lines: default_max_lines(),
            max_width: default_max_width(),
            grep_max_results: default_grep_max_results(),
            grep_max_per_file: default_grep_max_per_file(),
            global_max_sent_bytes: default_global_max_sent_bytes(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DedupeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_dedupe_min_bytes")]
    pub min_bytes: usize,
    #[serde(default = "default_dedupe_ttl_secs")]
    pub ttl_secs: u64,
}
impl Default for DedupeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_bytes: default_dedupe_min_bytes(),
            ttl_secs: default_dedupe_ttl_secs(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct MapConfig {
    #[serde(default = "default_map_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_map_max_files")]
    pub max_files: usize,
    #[serde(default = "default_map_max_file_bytes")]
    pub max_file_bytes: usize,
    #[serde(default = "default_map_refs_max_per_file")]
    pub refs_max_per_file: usize,
    #[serde(default = "default_map_refs_max_total")]
    pub refs_max_total: usize,
}
impl Default for MapConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_map_max_bytes(),
            max_files: default_map_max_files(),
            max_file_bytes: default_map_max_file_bytes(),
            refs_max_per_file: default_map_refs_max_per_file(),
            refs_max_total: default_map_refs_max_total(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct OutlineConfig {
    #[serde(default = "default_outline_max_bytes")]
    pub max_bytes: usize,
}
impl Default for OutlineConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_outline_max_bytes(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub dedupe: DedupeConfig,
    #[serde(default)]
    pub map: MapConfig,
    #[serde(default)]
    pub outline: OutlineConfig,
}

impl Config {
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    fn config_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("QUIET_CONTEXT_NATIVE_CONFIG") {
            if !path.trim().is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
        Some(base.join("quietcontext").join("native.toml"))
    }

    fn try_load() -> Option<Self> {
        let path = Self::config_path()?;
        let contents = std::fs::read_to_string(&path).ok()?;
        match toml::from_str(&contents) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                eprintln!(
                    "qc-native: warning: ignoring malformed {}: {}",
                    path.display(),
                    e
                );
                None
            }
        }
    }
}
