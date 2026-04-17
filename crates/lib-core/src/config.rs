//! # config
//!
//! Configuración centralizada del motor de copia.

use crate::checkpoint::ResumePolicy;
use crate::error::{CoreError, Result};
use crate::hash::Algorithm;

/// Configuración completa del motor de copia.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    // ── Triage ───────────────────────────────────────────────────────────────
    /// Umbral en bytes para decidir entre motor de bloques y motor de enjambre.
    /// Default: 16 MB
    pub triage_threshold_bytes: u64,

    // ── Motor de bloques ──────────────────────────────────────────────────────
    /// Tamaño de cada bloque en bytes. Default: 4 MB
    pub block_size_bytes: usize,

    /// Capacidad del canal crossbeam. Default: 8
    pub channel_capacity: usize,

    // ── Motor de enjambre ─────────────────────────────────────────────────────
    /// Tareas tokio concurrentes. Default: 128
    pub swarm_concurrency: usize,

    // ── Verificación de integridad ────────────────────────────────────────────
    /// Si `true`, calcula y verifica hash post-copia. Default: false
    pub verify: bool,

    /// Algoritmo de hashing. Default: blake3
    pub hash_algorithm: Algorithm,

    // ── Resiliencia ───────────────────────────────────────────────────────────
    /// Si `true`, intenta reanudar desde checkpoint existente.
    pub resume: bool,

    /// Política de validación al reanudar.
    ///
    /// Controla qué verificaciones se hacen sobre los archivos marcados
    /// como completados en el checkpoint antes de saltarlos.
    ///
    /// Default: `ResumePolicy::VerifySize` — verifica existencia y tamaño
    /// con una sola syscall `stat()` por archivo.
    pub resume_policy: ResumePolicy,

    /// Si `true`, escribe archivos como `.partial` hasta completarse.
    /// Default: true
    pub use_partial_files: bool,

    // ── Throttling ────────────────────────────────────────────────────────────
    /// Límite de ancho de banda en bytes/segundo. 0 = sin límite.
    pub bandwidth_limit_bytes_per_sec: u64,

    /// Burst inicial para el token bucket. Default: 1 MB
    pub bandwidth_burst_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            triage_threshold_bytes:        16 * 1024 * 1024,
            block_size_bytes:               4 * 1024 * 1024,
            channel_capacity:               8,
            swarm_concurrency:              128,
            verify:                         false,
            hash_algorithm:                 Algorithm::Blake3,
            resume:                         false,
            resume_policy:                  ResumePolicy::VerifySize,
            use_partial_files:              true,
            bandwidth_limit_bytes_per_sec:  0,
            bandwidth_burst_bytes:          1 * 1024 * 1024,
        }
    }
}

impl EngineConfig {
    pub fn validate(&self) -> Result<()> {
        if self.block_size_bytes == 0 {
            return Err(CoreError::InvalidConfig {
                message: "block_size_bytes no puede ser cero".into(),
            });
        }
        if self.block_size_bytes > 64 * 1024 * 1024 {
            return Err(CoreError::InvalidConfig {
                message: "block_size_bytes excede 64 MB: riesgo de OOM".into(),
            });
        }
        if self.channel_capacity == 0 {
            return Err(CoreError::InvalidConfig {
                message: "channel_capacity no puede ser cero".into(),
            });
        }
        if self.swarm_concurrency == 0 {
            return Err(CoreError::InvalidConfig {
                message: "swarm_concurrency no puede ser cero".into(),
            });
        }
        if self.swarm_concurrency > 1024 {
            return Err(CoreError::InvalidConfig {
                message: "swarm_concurrency > 1024: riesgo de saturación de file descriptors".into(),
            });
        }
        let max_ram = self.channel_capacity * self.block_size_bytes;
        if max_ram > 512 * 1024 * 1024 {
            return Err(CoreError::InvalidConfig {
                message: format!(
                    "Pipeline consumiría {} MB de RAM. Máximo: 512 MB",
                    max_ram / 1024 / 1024
                ),
            });
        }
        Ok(())
    }

    pub fn max_pipeline_ram_bytes(&self) -> usize {
        self.channel_capacity * self.block_size_bytes
    }

    #[inline]
    pub fn is_large_file(&self, size: u64) -> bool {
        size >= self.triage_threshold_bytes
    }
}
