//! Implementación de `ChecksumAlgorithm` usando XxHash64.
//!
//! ## Versión de librería
//!
//! Usamos `twox-hash = "1.6"` que expone `XxHash64` (semilla fija en 0).
//! La v2.x renombra el tipo a `XxHash3_64`, pero introduce breaking changes
//! en el API. Mantenemos 1.6 para estabilidad del workspace.
//!
//! ## Cuándo usar XxHash
//!
//! XxHash64 es la opción cuando se prioriza **velocidad pura** sobre
//! seguridad criptográfica. Es significativamente más rápido que SHA-256
//! pero no es criptográficamente seguro.
//!
//! **Casos de uso**: verificación de integridad en redes de confianza,
//! deduplicación interna, entornos donde el adversario no puede manipular datos.
//!
//! **No usar para**: checksums expuestos como garantía de seguridad.

use std::hash::Hasher;

use twox_hash::XxHash64;

use super::ChecksumAlgorithm;

/// Hasher incremental basado en XxHash64 (semilla 0).
pub struct XxHasher {
    hasher: XxHash64,
}

impl XxHasher {
    pub fn new() -> Self {
        Self {
            // Semilla 0: determinista y reproducible entre ejecuciones.
            hasher: XxHash64::with_seed(0),
        }
    }
}

impl Default for XxHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ChecksumAlgorithm for XxHasher {
    #[inline]
    fn update(&mut self, data: &[u8]) {
        self.hasher.write(data);
    }

    fn finalize(self: Box<Self>) -> String {
        let hash = self.hasher.finish();
        // Formato: 16 hex chars para el u64
        format!("{hash:016x}")
    }

    fn name(&self) -> &'static str {
        "xxhash64"
    }
}