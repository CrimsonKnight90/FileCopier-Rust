//! # error
//!
//! Tipo de error unificado para `lib-core`.
//!
//! Usamos `thiserror` para derivar implementaciones de `std::error::Error`
//! sin boilerplate. Cada variante representa una categoría de fallo distinta,
//! lo que permite al llamador discriminar y recuperarse selectivamente.

use std::path::PathBuf;
use thiserror::Error;

/// Alias de resultado estándar para todo `lib-core`.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errores que puede producir el motor de copia.
#[derive(Debug, Error)]
pub enum CoreError {
    // ── I/O ──────────────────────────────────────────────────────────────────

    /// Error genérico de I/O con contexto de ruta.
    #[error("Error de I/O en '{path}': {source}")]
    Io {
        path:   PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Error al leer el archivo origen.
    #[error("No se puede leer el origen '{path}': {source}")]
    ReadSource {
        path:   PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Error al escribir en el destino.
    #[error("No se puede escribir en destino '{path}': {source}")]
    WriteDestination {
        path:   PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// El rename atómico falló (el archivo `.partial` no pudo ser renombrado).
    #[error("Rename atómico fallido de '{from}' a '{to}': {source}")]
    AtomicRename {
        from:   PathBuf,
        to:     PathBuf,
        #[source]
        source: std::io::Error,
    },

    // ── Verificación ─────────────────────────────────────────────────────────

    /// El hash del archivo copiado no coincide con el del origen.
    /// Indica corrupción durante la transferencia.
    #[error("Verificación fallida para '{path}': esperado={expected}, obtenido={actual}")]
    HashMismatch {
        path:     PathBuf,
        expected: String,
        actual:   String,
    },

    // ── Checkpoint ───────────────────────────────────────────────────────────

    /// No se pudo leer o parsear el archivo de checkpoint.
    #[error("Checkpoint corrupto o inaccesible en '{path}': {source}")]
    CheckpointLoad {
        path:   PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// No se pudo persistir el checkpoint en disco.
    #[error("No se pudo guardar checkpoint en '{path}': {source}")]
    CheckpointSave {
        path:   PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    // ── Pipeline ─────────────────────────────────────────────────────────────

    /// El canal del pipeline se cerró inesperadamente (el receptor o emisor colgó).
    #[error("Pipeline interrumpido: el canal fue cerrado prematuramente")]
    PipelineDisconnected,

    /// El motor fue pausado externamente y la operación fue cancelada limpiamente.
    #[error("Operación pausada por el usuario")]
    Paused,

    /// Timeout esperando que el semáforo del enjambre libere un slot.
    #[error("Timeout esperando slot en el motor de enjambre")]
    SwarmTimeout,

    // ── Configuración ────────────────────────────────────────────────────────

    /// Parámetro de configuración inválido.
    #[error("Configuración inválida: {message}")]
    InvalidConfig { message: String },

    // ── OS / Permisos ─────────────────────────────────────────────────────────

    /// Operación no soportada en esta plataforma.
    #[error("Operación no soportada en esta plataforma: {operation}")]
    UnsupportedPlatform { operation: &'static str },
}

impl CoreError {
    /// Construye un `CoreError::Io` con contexto de ruta.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    /// Construye un `CoreError::ReadSource`.
    pub fn read(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::ReadSource { path: path.into(), source }
    }

    /// Construye un `CoreError::WriteDestination`.
    pub fn write(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::WriteDestination { path: path.into(), source }
    }

    /// Construye un `CoreError::AtomicRename`.
    pub fn rename(
        from: impl Into<PathBuf>,
        to: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::AtomicRename { from: from.into(), to: to.into(), source }
    }

    /// Retorna `true` si el error es recuperable (el usuario puede reintentar).
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Paused | Self::SwarmTimeout)
    }
}