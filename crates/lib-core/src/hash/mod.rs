//! # hash
//!
//! Trait `ChecksumAlgorithm` e implementaciones para blake3, xxhash y sha2.
//!
//! ## Diseño
//!
//! El trait es la única interfaz que conoce el motor. Las implementaciones
//! concretas quedan aisladas en sus propios submódulos.
//!
//! El enum `Algorithm` permite selección en runtime sin boxing innecesario.
//! `HasherDispatch` ofrece dispatch estático (sin vtable) para el hot path.

pub mod blake3_impl;
pub mod sha2_impl;
pub mod xxhash_impl;

/// Identifica qué algoritmo de hashing usar.
/// Serializable para persistir en checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Algorithm {
    Blake3,
    XxHash,
    Sha2,
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Algorithm::Blake3 => write!(f, "blake3"),
            Algorithm::XxHash => write!(f, "xxhash"),
            Algorithm::Sha2   => write!(f, "sha2-256"),
        }
    }
}

impl std::str::FromStr for Algorithm {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "blake3"           => Ok(Algorithm::Blake3),
            "xxhash" | "xx"   => Ok(Algorithm::XxHash),
            "sha2" | "sha256" => Ok(Algorithm::Sha2),
            other => Err(format!(
                "Algoritmo desconocido: '{other}'. Usa: blake3, xxhash, sha2"
            )),
        }
    }
}

/// Trait que debe implementar cualquier hasher compatible con el motor.
///
/// Los hashers son **incrementales**: el motor los alimenta bloque a bloque
/// para no cargar el archivo completo en memoria.
///
/// ```text
/// let mut h = MyHasher::new();
/// h.update(&block1);
/// h.update(&block2);
/// let digest = Box::new(h).finalize();
/// ```
pub trait ChecksumAlgorithm: Send {
    /// Incorpora `data` al estado interno del hasher.
    fn update(&mut self, data: &[u8]);

    /// Finaliza el cálculo y retorna el digest como string hex.
    /// Esta operación consume el hasher (no se puede reusar).
    fn finalize(self: Box<Self>) -> String;

    /// Retorna el nombre del algoritmo para logging y persistencia.
    fn name(&self) -> &'static str;
}

/// Construye un hasher concreto según el algoritmo seleccionado.
///
/// Retorna `Box<dyn ChecksumAlgorithm>` para contextos dinámicos.
/// Para el hot path del pipeline, usar `HasherDispatch` (sin vtable).
pub fn new_hasher(algorithm: Algorithm) -> Box<dyn ChecksumAlgorithm> {
    match algorithm {
        Algorithm::Blake3 => Box::new(blake3_impl::Blake3Hasher::new()),
        Algorithm::XxHash => Box::new(xxhash_impl::XxHasher::new()),
        Algorithm::Sha2   => Box::new(sha2_impl::Sha2Hasher::new()),
    }
}

/// Dispatch estático para el hot path del pipeline.
///
/// Evita vtable overhead manteniendo el mismo API que `dyn ChecksumAlgorithm`.
/// El compilador puede hacer inlining completo de `update()` con esta forma.
pub enum HasherDispatch {
    Blake3(blake3_impl::Blake3Hasher),
    XxHash(xxhash_impl::XxHasher),
    Sha2(sha2_impl::Sha2Hasher),
}

impl HasherDispatch {
    pub fn new(algorithm: Algorithm) -> Self {
        match algorithm {
            Algorithm::Blake3 => Self::Blake3(blake3_impl::Blake3Hasher::new()),
            Algorithm::XxHash => Self::XxHash(xxhash_impl::XxHasher::new()),
            Algorithm::Sha2   => Self::Sha2(sha2_impl::Sha2Hasher::new()),
        }
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Self::Blake3(h) => h.update(data),
            Self::XxHash(h) => h.update(data),
            Self::Sha2(h)   => h.update(data),
        }
    }

    pub fn finalize(self) -> String {
        match self {
            Self::Blake3(h) => Box::new(h).finalize(),
            Self::XxHash(h) => Box::new(h).finalize(),
            Self::Sha2(h)   => Box::new(h).finalize(),
        }
    }
}