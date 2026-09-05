use std::sync::{Arc, OnceLock};

use crate::lazy::byte_set::{byte_set_from_expr, ByteSet};
use crate::{BytesMode, Expr, Regex, RegexOptionsBuilder, Result};

fn default_options() -> &'static Arc<RegexOptionsBuilder> {
    static DEFAULT: OnceLock<Arc<RegexOptionsBuilder>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(RegexOptionsBuilder::default()))
}

/// TODO
#[derive(Debug)]
pub struct LazyRegex {
    pattern: String,
    start_bytes: Option<ByteSet>,
    regex: OnceLock<Result<Arc<Regex>>>,
    options: Arc<RegexOptionsBuilder>,
}

impl LazyRegex {
    /// TODO
    pub fn new(pattern: &str) -> Result<Self> {
        Self::new_with_options(pattern, Arc::clone(default_options()))
    }

    /// TODO
    pub fn new_with_options(
        pattern: &str,
        options_builder: Arc<RegexOptionsBuilder>,
    ) -> Result<Self> {
        let options = &options_builder.options;
        let tree = Expr::parse_tree_with_flags(pattern, options.compute_flags())?;
        let start_bytes = byte_set_from_expr(&tree.expr, options);

        Ok(Self {
            pattern: pattern.to_string(),
            start_bytes,
            regex: OnceLock::new(),
            options: options_builder,
        })
    }

    /// TODO
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// TODO
    pub fn regex(&self) -> Result<Arc<Regex>> {
        self.regex_ref().cloned()
    }

    /// TODO
    pub(crate) fn regex_ref(&self) -> Result<&Arc<Regex>> {
        self.regex
            .get_or_init(|| {
                // Same reasoning as to why RegexSet disable it: we only search anchored
                // and we have a prefilter
                let mut options = self.options.options.clone();
                options.delegate_prefilter = false;
                Regex::new_options(self.pattern.clone(), &options).map(Arc::new)
            })
            .as_ref()
            .map_err(|e| e.clone())
    }

    pub(crate) fn start_bytes(&self) -> Option<&ByteSet> {
        self.start_bytes.as_ref()
    }

    /// TODO
    pub(crate) fn bytes_mode(&self) -> BytesMode {
        self.options.options.bytes_mode
    }
}
