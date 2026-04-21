//! Jinja template runtime for BAML prompts.
//!
//! This module provides:
//! - `render_prompt()` - Main entry point for rendering templates
//! - `OutputFormatObject` - Template-accessible output format renderer
//! - Value conversion from `BexExternalValue` to minijinja values
//! - Magic delimiter handling for chat messages and media

mod assert_eval;
mod error;
mod filters;
mod output_format_object;
mod render;
mod value_conversion;

pub use assert_eval::{AssertOutcome, evaluate_assertion};
pub use error::RenderPromptError;
pub use render::{
    RenderContext, RenderContextClient, RenderEnum, RenderEnumVariant, preprocess_template,
    render_prompt,
};
pub use value_conversion::external_value_to_jinja;

pub use crate::types::OutputFormatContent;

/// Magic delimiter for chat role markers.
pub(crate) const MAGIC_CHAT_ROLE_DELIMITER: &str = "BAML_CHAT_ROLE_MAGIC_STRING_DELIMITER";

/// Magic delimiter for media content.
pub(crate) const MAGIC_MEDIA_DELIMITER: &str = "BAML_MEDIA_MAGIC_STRING_DELIMITER";
