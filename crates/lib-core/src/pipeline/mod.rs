//! # pipeline
//!
//! Componentes del pipeline de copia para archivos grandes.
//!
//! ## Arquitectura
//!
//! ```text
//! ┌──────────────┐  crossbeam::channel  ┌──────────────┐
//! │  BlockReader │ ───── Block ────────► │  BlockWriter │
//! │  (OS thread) │   (backpressure)      │  (OS thread) │
//! └──────────────┘                       └──────────────┘
//!        │                                      │
//!        └── HasherDispatch (inline)            └── HasherDispatch (inline)
//!             (hash del origen)                      (hash del destino)
//! ```
//!
//! ## Backpressure
//!
//! El canal crossbeam tiene capacidad fija (`config.channel_capacity`).
//! Si el escritor no puede mantener el ritmo del lector, el lector se bloquea
//! en `send()`. Esto evita cargar el archivo completo en RAM.
//!
//! ## Fin de stream
//!
//! El fin del stream se señala cerrando el canal (el Sender hace drop),
//! no con un bloque sentinel. Esto es idiomático en Rust y evita
//! condiciones de carrera al detectar EOF.

pub mod reader;
pub mod writer;

pub use reader::BlockReader;
pub use writer::BlockWriter;

/// Un bloque de datos leído del origen, listo para ser escrito y hasheado.
///
/// El campo `data` es un `Vec<u8>`. En versiones futuras podría ser
/// reemplazado por un buffer pool para evitar allocaciones por bloque.
#[derive(Debug)]
pub struct Block {
    /// Datos del bloque (truncados al tamaño real leído).
    pub data: Vec<u8>,

    /// Offset dentro del archivo origen (para diagnóstico y Direct I/O futuro).
    pub offset: u64,

    /// Número de secuencia del bloque (0-indexed, para diagnóstico).
    pub sequence: u64,
}

impl Block {
    pub fn new(data: Vec<u8>, offset: u64, sequence: u64) -> Self {
        Self { data, offset, sequence }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}