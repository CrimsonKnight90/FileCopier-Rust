//! # os_ops
//!
//! Trait mínimo que `lib-core` necesita del sistema operativo.
//!
//! ## Por qué existe este trait y no usamos `lib-os` directamente
//!
//! `lib-os` depende de `lib-core` (para `CoreError`). Si `lib-core`
//! dependiera de `lib-os`, se formaría un ciclo de dependencias que
//! Cargo rechaza. La solución es Dependency Inversion:
//!
//! - `lib-core` define el contrato mínimo (`OsOps`).
//! - `lib-os` implementa ese contrato en sus adapters concretos.
//! - `app-cli` / `app-gui` conectan los dos en el punto de entrada.
//!
//! ## Qué incluye este trait
//!
//! Solo las operaciones que el motor necesita en su hot path:
//! - `preallocate`: reservar espacio antes de escribir (reduce fragmentación NTFS).
//! - `copy_metadata`: preservar timestamps y atributos tras cada copia.
//!
//! Operaciones de detección de hardware (`detect_drive_kind`) pertenecen
//! a `lib-os` directamente y se usan solo en el punto de entrada.

use std::path::Path;
use crate::error::Result;

/// Operaciones de sistema operativo necesarias en el motor de copia.
///
/// Implementado por `lib-os::WindowsAdapter` y `lib-os::UnixAdapter`.
/// En tests se puede implementar con un stub no-op.
pub trait OsOps: Send + Sync {
    /// Pre-alloca `size` bytes en disco para `path`.
    ///
    /// Debe llamarse DESPUÉS de crear el archivo (con `OpenOptions::create`)
    /// y ANTES de escribir el primer byte. Esto permite al sistema de archivos
    /// reservar espacio contiguo, reduciendo fragmentación en NTFS/ext4.
    ///
    /// Si el sistema de archivos no soporta preallocación (FAT32, exFAT, red),
    /// la implementación degrada silenciosamente retornando `Ok(())`.
    fn preallocate(&self, path: &Path, size: u64) -> Result<()>;

    /// Copia permisos, timestamps y atributos del origen al destino.
    ///
    /// Debe llamarse DESPUÉS del rename atómico (cuando el archivo ya tiene
    /// su nombre final). Aplicar metadatos al `.partial` sería incorrecto
    /// porque el rename reset los timestamps en algunos sistemas de archivos.
    ///
    /// Si la operación no está soportada (FAT32, red sin permisos POSIX),
    /// la implementación degrada silenciosamente retornando `Ok(())`.
    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()>;
}

/// Implementación no-op para tests y entornos donde no se necesitan
/// estas operaciones (benchmarks de throughput puro, por ejemplo).
pub struct NoOpOsOps;

impl OsOps for NoOpOsOps {
    fn preallocate(&self, _path: &Path, _size: u64) -> Result<()> {
        Ok(())
    }

    fn copy_metadata(&self, _source: &Path, _dest: &Path) -> Result<()> {
        Ok(())
    }
}