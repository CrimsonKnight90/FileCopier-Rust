//! # windows
//!
//! Implementación de `OsAdapter` para Windows.
//!
//! ## Módulos
//!
//! - `fs`               → `WindowsAdapter`: preallocate + copy_metadata
//! - `vss`              → Volume Shadow Copy (Fase 3)
//! - `clipboard`        → Watcher legacy de portapapeles (polling)
//! - `clipboard_daemon` → Daemon profesional con `WM_CLIPBOARDUPDATE`
//! - `explorer_path`    → Resolución del path activo en Explorer

pub mod fs;
pub mod vss;
pub mod clipboard;
pub mod clipboard_daemon;
pub mod explorer_path;

pub use fs::WindowsAdapter;
