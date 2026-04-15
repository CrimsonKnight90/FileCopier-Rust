//! Implementación de `ChecksumAlgorithm` usando blake3.
//!
//! Blake3 es el hasher **por defecto** del motor.
//!
//! ## Por qué blake3
//!
//! - Hasta 6× más rápido que SHA-256 en x86_64 con SIMD.
//! - Paralelizable internamente: aprovecha múltiples cores.
//! - Resistente a colisiones: seguridad equivalente a SHA-3.
//! - Ideal para verificación de integridad de transferencias.

use super::ChecksumAlgorithm;

/// Hasher incremental basado en blake3.
pub struct Blake3Hasher {
    hasher: blake3::Hasher,
}

impl Blake3Hasher {
    pub fn new() -> Self {
        Self {
            hasher: blake3::Hasher::new(),
        }
    }
}

impl Default for Blake3Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ChecksumAlgorithm for Blake3Hasher {
    #[inline]
    fn update(&mut self, data: &[u8]) {
        // blake3::Hasher::update es SIMD-optimized y muy cache-friendly.
        self.hasher.update(data);
    }

    fn finalize(self: Box<Self>) -> String {
        // El digest de blake3 es 32 bytes (256 bits) → 64 chars hex.
        self.hasher.finalize().to_hex().to_string()
    }

    fn name(&self) -> &'static str {
        "blake3"
    }
}