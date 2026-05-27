//! claude-shim library entry — exposes the translation modules so
//! integration tests (and any future embedders) can drive the same
//! request/response path the HTTP handler uses.

#![forbid(unsafe_code)]

pub mod anthropic;
pub mod claude;
pub mod translate;
