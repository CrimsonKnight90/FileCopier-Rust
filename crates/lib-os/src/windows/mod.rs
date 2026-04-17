//! # windows
//!
//! Implementación de `OsAdapter` para Windows.

pub mod fs;
pub mod vss;
pub mod clipboard;

pub use fs::WindowsAdapter;
