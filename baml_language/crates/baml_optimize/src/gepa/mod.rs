//! GEPA runtime defaults - embedded BAML source files.
//!
//! `gepa.baml` defines the reflection functions (`ProposeImprovements`,
//! `MergeVariants`, `AnalyzeFailurePatterns`) and `clients.baml` declares
//! the default reflection client. Both are embedded at build time so the
//! optimizer ships as a self-contained binary.

pub(crate) const GEPA_BAML: &str = include_str!("gepa.baml");
pub(crate) const CLIENTS_BAML: &str = include_str!("clients.baml");
