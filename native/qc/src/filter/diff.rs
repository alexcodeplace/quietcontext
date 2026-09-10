use super::traits::{FilterConfig, FilterInput, FilterResult, OutputFilter};

pub struct DiffFilter;

impl OutputFilter for DiffFilter {
    fn filter(&self, input: &FilterInput, config: &FilterConfig) -> FilterResult {
        let raw = input.stdout;
        let input_bytes = raw.len();

        if raw.len() <= config.max_output_bytes {
            let output = String::from_utf8_lossy(raw).to_string();
            return FilterResult {
                output,
                input_bytes,
            };
        }

        super::git::filter_git_diff(raw, input_bytes, config)
    }
}
