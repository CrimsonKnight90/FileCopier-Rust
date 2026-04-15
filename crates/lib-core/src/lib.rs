//! # lib-core
//!
//! Motor principal de FileCopier-Rust.
//!
//! ## Módulos
//!
//! - [`hash`]       — Trait `ChecksumAlgorithm` e implementaciones (blake3, xxhash, sha2).
//! - [`pipeline`]   — Componentes del pipeline: lector, escritor, buffer.
//! - [`engine`]     — Motor dual: bloques grandes y enjambre asíncrono.
//! - [`checkpoint`] — Persistencia de estado para pausa/reanudar.
//! - [`telemetry`]  — Métricas diferenciadas en tiempo real.
//! - [`error`]      — Tipo de error unificado del crate.
//! - [`config`]     — Configuración centralizada del motor.
//! - [`bandwidth`]  — Throttling de ancho de banda con token bucket.

pub mod checkpoint;
pub mod config;
pub mod engine;
pub mod error;
pub mod hash;
pub mod pipeline;
pub mod telemetry;
pub mod os_ops;
pub mod buffer_pool;
pub mod bandwidth;

// Re-exportaciones convenientes para usuarios del crate
pub use config::EngineConfig;
pub use error::{CoreError, Result};
pub use os_ops::{NoOpOsOps, OsOps};
pub use buffer_pool::{Buffer, BufferPool};
pub use bandwidth::{Throttle, ThrottleHandle};