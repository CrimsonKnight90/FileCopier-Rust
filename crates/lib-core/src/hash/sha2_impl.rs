//! Implementación de `ChecksumAlgorithm` usando SHA-256.
//!
//! ## Cuándo usar SHA-256
//!
//! SHA-256 es la opción cuando se necesita **interoperabilidad** con
//! sistemas externos que esperan checksums estándar (verificación
//! de ISOs, hashes publicados en manifiestos oficiales, etc.).
//!
//! Es ~3-4× más lento que blake3. No hay razón de usarlo internamente
//! salvo compatibilidad con terceros.

use sha2::{Digest, Sha256};

use super::ChecksumAlgorithm;

/// Hasher incremental basado en SHA-256.
pub struct Sha2Hasher {
    hasher: Sha256,
}

impl Sha2Hasher {
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }
}

impl Default for Sha2Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ChecksumAlgorithm for Sha2Hasher {
    #[inline]
    fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    fn finalize(self: Box<Self>) -> String {
        // SHA-256 produce 32 bytes → 64 chars hex
        format!("{:x}", self.hasher.finalize())
    }

    fn name(&self) -> &'static str {
        "sha2-256"
    }
}