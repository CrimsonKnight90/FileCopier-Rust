//! # reader
//!
//! Lector de bloques del archivo origen.
//!
//! ## Zero-allocation
//!
//! El reader adquiere un `PooledBuffer` del pool, lee directamente en él,
//! lo envuelve en un `Block` y lo envía por el canal. Cuando el writer
//! termina de procesar el bloque, el `drop(block)` devuelve el buffer al
//! pool automáticamente. No hay allocaciones en el hot path.
//!
//! ## Backpressure
//!
//! Dos mecanismos cooperan:
//! 1. `pool.acquire()` bloquea si todos los buffers están en el canal o en el writer.
//! 2. `tx.send()` bloquea si el canal está lleno.
//!
//! Esto garantiza que el uso de RAM está acotado a `pool_size × block_size`
//! sin importar el tamaño del archivo.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crossbeam::channel::Sender;

use crate::buffer_pool::BufferPool;
use crate::checkpoint::FlowControl;
use crate::config::EngineConfig;
use crate::error::{CoreError, Result};
use crate::hash::HasherDispatch;
use crate::pipeline::Block;
use crate::telemetry::TelemetryHandle;

/// Lee el archivo origen bloque a bloque y los envía al canal del pipeline.
pub struct BlockReader {
    config:    EngineConfig,
    flow:      FlowControl,
    telemetry: TelemetryHandle,
    pool:      BufferPool,
}

impl BlockReader {
    pub fn new(
        config:    EngineConfig,
        flow:      FlowControl,
        telemetry: TelemetryHandle,
        pool:      BufferPool,
    ) -> Self {
        Self { config, flow, telemetry, pool }
    }

    /// Lee `source_path` en bloques y los envía por `tx`.
    ///
    /// Retorna `Some(hash_hex)` si `config.verify == true`, `None` si no.
    /// El canal se cierra automáticamente cuando este método retorna.
    pub fn run(
        &self,
        source_path: &Path,
        tx: Sender<Block>,
    ) -> Result<Option<String>> {
        let file_size = std::fs::metadata(source_path)
            .map_err(|e| CoreError::read(source_path, e))?
            .len();

        let file = File::open(source_path)
            .map_err(|e| CoreError::read(source_path, e))?;

        // BufReader con capacidad = block_size. La syscall read() ya pide
        // un bloque completo, así que el buffer interno de BufReader evita
        // lecturas parciales del OS sin añadir overhead.
        let mut reader = BufReader::with_capacity(self.config.block_size_bytes, file);

        let mut hasher = if self.config.verify {
            Some(HasherDispatch::new(self.config.hash_algorithm))
        } else {
            None
        };

        let mut offset:   u64 = 0;
        let mut sequence: u64 = 0;

        loop {
            // ── Pausa/cancelación ENTRE bloques (nunca a mitad) ───────────
            match self.flow.check() {
                Ok(())                   => {}
                Err(CoreError::Paused)   => {
                    tracing::debug!("Reader: pausa en bloque {sequence}");
                    self.flow.wait_for_resume()?;
                    tracing::debug!("Reader: reanudado");
                }
                Err(e) => return Err(e),
            }

            // ── Adquirir buffer del pool (bloquea si pool vacío) ──────────
            // Este bloqueo es correcto: si el pool está vacío significa que
            // todos los buffers están en el canal o siendo procesados por el
            // writer. Es el mecanismo de backpressure del pool.
            let mut buf = self.pool.acquire();

            // ── Leer directamente en el buffer del pool (zero-copy) ───────
            let n = reader
                .read(buf.as_write_slice())
                .map_err(|e| CoreError::read(source_path, e))?;

            if n == 0 {
                // EOF — el canal se cierra cuando tx hace drop al salir.
                // El buffer se devuelve automáticamente al pool.
                tracing::debug!(
                    "Reader: EOF — {} bloques, {:.1} MB",
                    sequence,
                    offset as f64 / 1024.0 / 1024.0
                );
                break;
            }

            // Marcar cuántos bytes del buffer son válidos.
            buf.set_filled(n);

            // ── Hash incremental ──────────────────────────────────────────
            if let Some(ref mut h) = hasher {
                h.update(buf.as_slice());
            }

            // ── Telemetría ────────────────────────────────────────────────
            self.telemetry.add_bytes(n as u64);
            offset += n as u64;

            let progress = if file_size > 0 {
                (offset as f64 / file_size as f64).min(1.0)
            } else {
                1.0
            };
            self.telemetry.set_current_file(source_path, progress);

            sequence += 1;

            // ── Enviar al canal ───────────────────────────────────────────
            // Si el canal está lleno, send() bloquea hasta que el writer
            // consuma un slot (backpressure del canal).
            // El buffer NO se libera aquí — viaja con el Block hasta que
            // el writer lo hace drop.
            let block = Block::new(buf, offset, sequence);
            if tx.send(block).is_err() {
                // El writer cerró el canal — error fatal.
                return Err(CoreError::PipelineDisconnected);
            }
        }

        self.telemetry.clear_current_file();
        Ok(hasher.map(|h| h.finalize()))
    }
}
