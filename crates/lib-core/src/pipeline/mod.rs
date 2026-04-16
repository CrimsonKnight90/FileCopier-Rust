//! # pipeline
//!
//! Componentes del pipeline de copia para archivos grandes.
//!
//! ## Arquitectura
//!
//! ```text
//! ┌──────────────┐  crossbeam::channel  ┌──────────────┐
//! │  BlockReader │ ──── Block ─────────► │  BlockWriter │
//! │  (OS thread) │   (backpressure)      │  (OS thread) │
//! └──────────────┘                       └──────────────┘
//!        │                                      │
//!        └── HasherDispatch (inline)            └── HasherDispatch (inline)
//!             (hash del origen)                      (hash del destino)
//! ```
//!
//! ## Zero-allocation en el hot path
//!
//! `Block` envuelve un `PooledBuffer` RAII. Cuando el writer hace `drop(block)`,
//! el buffer vuelve automáticamente al pool. El reader puede adquirir ese mismo
//! buffer en la siguiente iteración sin ninguna allocación.
//!
//! ## Backpressure
//!
//! El canal crossbeam tiene capacidad fija (`config.channel_capacity`).
//! Si el writer no puede mantener el ritmo, el reader se bloquea en `send()`.
//! Si el pool se agota (todos los buffers en el canal), el reader se bloquea
//! en `pool.acquire()`. Ambos mecanismos cooperan para limitar RAM máxima a
//! `pool_size × block_size`.

pub mod reader;
pub mod writer;

pub use reader::BlockReader;
pub use writer::BlockWriter;

use crate::buffer_pool::PooledBuffer;

/// Un bloque de datos leído del origen, listo para ser escrito y hasheado.
///
/// Contiene un `PooledBuffer` RAII: cuando se hace `drop(block)`, el buffer
/// vuelve automáticamente al pool sin ninguna llamada explícita.
pub struct Block {
    /// Buffer RAII con los datos del bloque (ya truncado al tamaño real leído).
    pub buf: PooledBuffer,

    /// Offset dentro del archivo origen (byte inicial de este bloque).
    pub offset: u64,

    /// Número de secuencia del bloque (0-indexed).
    pub sequence: u64,
}

impl Block {
    pub fn new(buf: PooledBuffer, offset: u64, sequence: u64) -> Self {
        Self { buf, offset, sequence }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Acceso al slice de bytes del bloque.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.buf.as_slice()
    }
}
