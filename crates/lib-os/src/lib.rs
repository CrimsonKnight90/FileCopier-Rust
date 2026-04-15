//! # lib-os
//!
//! Abstracciones de sistema operativo para FileCopier-Rust.
//!
//! ## Diseño de portabilidad
//!
//! Este crate expone un `OsAdapter` trait que el motor consume.
//! Las implementaciones concretas se compilan condicionalmente:
//!
//! - `cfg(windows)` → `windows/mod.rs` usa WinAPI directamente.
//! - `cfg(unix)`    → `unix/mod.rs` usa llamadas POSIX.
//!
//! El motor en `lib-core` nunca importa `lib-os` directamente: recibe
//! un `Box<dyn OsAdapter>` desde el punto de entrada (CLI o GUI).
//! Esto mantiene `lib-core` libre de dependencias de plataforma y
//! permite testear el motor con un adapter mock.
//!
//! ## Módulos
//!
//! - [`traits`]   — Trait `OsAdapter` portable.
//! - [`windows`]  — Implementación Win32 (solo en Windows).
//! - [`unix`]     — Implementación POSIX (Linux/macOS).
//! - [`detect`]   — Detección de hardware (HDD vs SSD vs NVMe).

pub mod detect;
pub mod traits;

#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub mod unix;

pub use traits::OsAdapter;

/// Construye el adapter correcto para la plataforma actual.
/// Devuelve un `Box<dyn OsAdapter>` usado para detección de hardware.
pub fn platform_adapter() -> Box<dyn OsAdapter> {
    #[cfg(windows)]
    {
        Box::new(windows::WindowsAdapter::new())
    }

    #[cfg(unix)]
    {
        Box::new(unix::UnixAdapter::new())
    }

    #[cfg(not(any(windows, unix)))]
    {
        compile_error!(
            "Plataforma no soportada. FileCopier-Rust requiere Windows o Unix."
        );
    }
}

/// Construye el adapter de la plataforma actual como `OsOps`
/// para inyectarlo en lib-core (SwarmEngine, BlockEngine, Writer).
pub fn platform_adapter_os_ops() -> Box<dyn lib_core::os_ops::OsOps> {
    #[cfg(windows)]
    {
        Box::new(windows::WindowsAdapter::new())
    }

    #[cfg(unix)]
    {
        Box::new(unix::UnixAdapter::new())
    }
}
