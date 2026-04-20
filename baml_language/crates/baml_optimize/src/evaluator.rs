//! Evaluator - runs tests and collects metrics

/// Evaluator configuration
pub struct Evaluator {
    pub parallel: usize,
}

impl Evaluator {
    pub fn new(parallel: usize) -> Self {
        Self { parallel }
    }
}
