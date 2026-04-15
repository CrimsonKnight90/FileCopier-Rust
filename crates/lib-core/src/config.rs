//! # config
//!
//! Configuración centralizada del motor de copia.
//!
//! `EngineConfig` es la única fuente de verdad para todos los parámetros
//! del motor. Se construye una vez (desde CLI o GUI) y se pasa por
//! referencia a todos los subsistemas.
//!
//! ## Diseño
//!
//! - Todos los campos tienen defaults razonados (ver documentación inline).
//! - La validación ocurre en `EngineConfig::validate()`, no en el motor,
//!   lo que simplifica el manejo de errores downstream.

use crate::error::{CoreError, Result};
use crate::hash::Algorithm;

/// Configuración completa del motor de copia.
///
/// # Ejemplo
///
/// ```rust
/// use lib_core::config::EngineConfig;
///
/// let config = EngineConfig::default();
/// config.validate().expect("Configuración inválida");
/// ```
#[derive(Debug, Clone)]
pub struct EngineConfig {
    // ── Triage ───────────────────────────────────────────────────────────────

    /// Umbral en bytes para decidir entre motor de bloques y motor de enjambre.
    /// Archivos >= threshold → motor de bloques.
    /// Archivos <  threshold → motor de enjambre.
    ///
    /// Default: 16 MB
    pub triage_threshold_bytes: u64,

    // ── Motor de bloques grandes ──────────────────────────────────────────────

    /// Tamaño de cada bloque de lectura/escritura en bytes.
    ///
    /// Default: 4 MB
    /// Rango recomendado: 2 MB – 8 MB
    pub block_size_bytes: usize,

    /// Capacidad del canal crossbeam entre lector y escritor.
    /// Define cuántos bloques pueden estar en vuelo simultáneamente.
    /// Limita el uso de RAM: `channel_capacity * block_size_bytes`.
    ///
    /// Default: 8 bloques (32 MB RAM máx. con block_size=4MB)
    pub channel_capacity: usize,

    // ── Motor de enjambre ─────────────────────────────────────────────────────

    /// Número máximo de tareas tokio concurrentes en el motor de enjambre.
    ///
    /// Default: 128
    pub swarm_concurrency: usize,

    // ── Verificación de integridad ────────────────────────────────────────────

    /// Si `true`, calcula y verifica hash post-copia.
    /// Si `false`, el pipeline de hashing se omite completamente (zero cost).
    ///
    /// Default: false (opt-in con --verify)
    pub verify: bool,

    /// Algoritmo de hashing a usar cuando `verify == true`.
    ///
    /// Default: blake3
    pub hash_algorithm: Algorithm,

    // ── Resiliencia ───────────────────────────────────────────────────────────

    /// Si `true`, intenta cargar un checkpoint existente y reanudar la copia.
    pub resume: bool,

    /// Si `true`, escribe archivos como `.partial` y hace rename atómico al final.
    /// Recomendado: siempre `true` en producción.
    ///
    /// Default: true
    pub use_partial_files: bool,

    // ── Throttling de ancho de banda ───────────────────────────────────────────

    /// Límite de ancho de banda en bytes por segundo. Si 0, sin límite.
    ///
    /// Default: 0 (sin throttling)
    pub bandwidth_limit_bytes_per_sec: u64,

    /// Burst inicial para el token bucket (bytes disponibles inmediatamente).
    ///
    /// Default: 1 MB
    pub bandwidth_burst_bytes: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            triage_threshold_bytes: 16 * 1024 * 1024, // 16 MB
            block_size_bytes:        4 * 1024 * 1024,  //  4 MB
            channel_capacity:        8,
            swarm_concurrency:       128,
            verify:                  false,
            hash_algorithm:          Algorithm::Blake3,
            resume:                  false,
            use_partial_files:       true,
            bandwidth_limit_bytes_per_sec: 0,           // Sin throttling por defecto
            bandwidth_burst_bytes:   1 * 1024 * 1024,   // 1 MB burst
        }
    }
}

impl EngineConfig {
    /// Valida que los parámetros sean coherentes y seguros.
    ///
    /// Debe llamarse una vez al construir la config, antes de iniciar el motor.
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

        // RAM máxima que puede consumir el canal del pipeline
        let max_pipeline_ram = self.channel_capacity * self.block_size_bytes;
        if max_pipeline_ram > 512 * 1024 * 1024 {
            return Err(CoreError::InvalidConfig {
                message: format!(
                    "Pipeline consumiría {} MB de RAM (channel_capacity={} × block_size={}MB). Máximo: 512 MB",
                    max_pipeline_ram / 1024 / 1024,
                    self.channel_capacity,
                    self.block_size_bytes / 1024 / 1024
                ),
            });
        }

        Ok(())
    }

    /// Retorna el uso máximo de RAM del pipeline en bytes.
    pub fn max_pipeline_ram_bytes(&self) -> usize {
        self.channel_capacity * self.block_size_bytes
    }

    /// Retorna `true` si un archivo de `size` bytes debe ir al motor de bloques.
    #[inline]
    pub fn is_large_file(&self, size: u64) -> bool {
        size >= self.triage_threshold_bytes
    }
}