//! # windows
//!
//! Implementación de `OsAdapter` para Windows usando WinAPI (windows-sys).
//!
//! ## Módulos
//!
//! - `fs`  → `WindowsAdapter`: preallocate + copy_metadata.
//! - `vss` → Volume Shadow Copy (Fase 3, vacío por ahora).

pub mod fs;
pub mod vss;

// Re-exportar el adapter como el tipo público de este módulo
pub use fs::WindowsAdapter;