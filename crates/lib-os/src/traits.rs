//! # traits
//!
//! Trait `OsAdapter` — interfaz portable de operaciones de SO.
//!
//! ## Principio de diseño
//!
//! Todo lo que depende del sistema operativo pasa por este trait.
//! `lib-core` no tiene `#[cfg(windows)]` ni `#[cfg(unix)]` en ningún lugar.

use std::path::Path;
use lib_core::error::Result;

/// Tipo de dispositivo de almacenamiento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    /// Disco de estado sólido (SATA SSD o NVMe).
    Ssd,
    /// Disco duro mecánico.
    Hdd,
    /// Unidad de red (NAS, SMB, NFS).
    Network,
    /// No se pudo determinar — fallback conservador = HDD.
    Unknown,
}

impl DriveKind {
    /// Retorna `true` si el dispositivo soporta paralelismo de I/O eficiente.
    pub fn supports_parallel_io(&self) -> bool {
        matches!(self, DriveKind::Ssd)
    }

    /// Retorna `true` si conviene serializar escrituras (evitar seek penalty).
    pub fn prefers_sequential(&self) -> bool {
        matches!(self, DriveKind::Hdd | DriveKind::Network | DriveKind::Unknown)
    }
}

/// Estrategia de copia recomendada para un par origen/destino.
#[derive(Debug, Clone, Copy)]
pub struct CopyStrategy {
    pub source_kind: DriveKind,
    pub dest_kind:   DriveKind,

    /// Concurrencia del enjambre sin verificación de hash.
    pub recommended_swarm_concurrency: usize,

    /// Concurrencia del enjambre CON --verify activo.
    ///
    /// Con --verify cada tarea hace: leer + hash origen + escribir + hash destino.
    /// En SSD con 128 tareas concurrentes esto satura el page cache del OS
    /// y produce cache misses masivos → rendimiento peor que con menos tareas.
    /// Benchmarks reales muestran que 32 tareas es el punto óptimo para SSD+hash.
    pub recommended_swarm_concurrency_verify: usize,

    /// Tamaño de bloque recomendado en bytes.
    pub recommended_block_size: usize,
}

impl CopyStrategy {
    /// Calcula la estrategia óptima según origen y destino.
    ///
    /// ## Heurísticas basadas en benchmarks
    ///
    /// | Escenario  | Sin verify | Con verify | Bloque | Razón                            |
    /// |------------|-----------|------------|--------|----------------------------------|
    /// | HDD→HDD    | 1         | 1          | 8 MB   | Monohilo: evita seek doble       |
    /// | SSD→HDD    | 4         | 4          | 8 MB   | HDD es cuello de botella         |
    /// | HDD→SSD    | 1         | 1          | 4 MB   | HDD es cuello de botella         |
    /// | SSD→SSD    | 128       | 32         | 4 MB   | 128+verify satura page cache     |
    /// | *→Network  | 8         | 8          | 16 MB  | Latencia de red domina           |
    /// | Network→*  | 16        | 16         | 4 MB   | Latencia de red domina           |
    /// | Unknown    | 4         | 4          | 4 MB   | Conservador                      |
    pub fn compute(source_kind: DriveKind, dest_kind: DriveKind) -> Self {
        let (concurrency, concurrency_verify, block_size) = match (source_kind, dest_kind) {
            (DriveKind::Hdd,     DriveKind::Hdd)     => (1,   1,   8 * 1024 * 1024),
            (DriveKind::Ssd,     DriveKind::Hdd)     => (4,   4,   8 * 1024 * 1024),
            (DriveKind::Hdd,     DriveKind::Ssd)     => (1,   1,   4 * 1024 * 1024),
            (DriveKind::Ssd,     DriveKind::Ssd)     => (128, 32,  4 * 1024 * 1024),
            (_,                  DriveKind::Network)  => (8,   8,  16 * 1024 * 1024),
            (DriveKind::Network, _)                   => (16,  16,  4 * 1024 * 1024),
            _                                         => (4,   4,   4 * 1024 * 1024),
        };

        Self {
            source_kind,
            dest_kind,
            recommended_swarm_concurrency:        concurrency,
            recommended_swarm_concurrency_verify: concurrency_verify,
            recommended_block_size:               block_size,
        }
    }
}

/// Interfaz portable de operaciones dependientes del SO.
///
/// Implementada por `WindowsAdapter` (Win32) y `UnixAdapter` (POSIX).
pub trait OsAdapter: Send + Sync {
    // ── Detección de hardware ─────────────────────────────────────────────────

    /// Detecta el tipo de unidad que contiene `path`.
    fn detect_drive_kind(&self, path: &Path) -> DriveKind;

    /// Calcula la estrategia óptima para una operación origen→destino.
    fn compute_strategy(&self, source: &Path, dest: &Path) -> CopyStrategy {
        let source_kind = self.detect_drive_kind(source);
        let dest_kind   = self.detect_drive_kind(dest);
        CopyStrategy::compute(source_kind, dest_kind)
    }

    // ── Preallocación ─────────────────────────────────────────────────────────

    /// Pre-alloca espacio en disco para `path` de `size` bytes.
    ///
    /// Si el sistema de archivos no soporta preallocación (FAT32, exFAT),
    /// retorna `Ok(())` sin error (degradación suave).
    fn preallocate(&self, path: &Path, size: u64) -> Result<()>;

    // ── Permisos y metadatos ──────────────────────────────────────────────────

    /// Copia los permisos y timestamps del origen al destino.
    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()>;

    // ── Nombre de la plataforma (para logging) ────────────────────────────────
    fn platform_name(&self) -> &'static str;
}
