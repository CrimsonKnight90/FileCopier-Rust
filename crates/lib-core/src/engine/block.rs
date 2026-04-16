//! # block
//!
//! Motor de bloques para archivos grandes (>= umbral de triage).
//!
//! ## Pipeline con BufferPool RAII
//!
//! ```text
//!  BufferPool (pool_size = channel_cap + 2)
//!       │
//!       ├─ acquire() ──► BlockReader ──► Block(PooledBuffer) ──► canal ──► BlockWriter
//!       │                                                                        │
//!       └──────────────────── drop(block) ──────────────────────────────────────┘
//!                              (auto-release al pool)
//! ```
//!
//! El pool tiene `channel_cap + 2` buffers:
//! - `channel_cap` buffers pueden estar en el canal
//! - 1 buffer en el reader (siendo leído)
//! - 1 buffer en el writer (siendo escrito)
//! - El pool nunca se agota → no hay deadlock por pool vacío

use std::path::Path;
use std::sync::Arc;
use std::thread;

use crate::buffer_pool::BufferPool;
use crate::checkpoint::FlowControl;
use crate::config::EngineConfig;
use crate::error::{CoreError, Result};
use crate::os_ops::OsOps;
use crate::pipeline::{BlockReader, BlockWriter};
use crate::telemetry::TelemetryHandle;

/// Motor de copia para archivos grandes.
pub struct BlockEngine {
    config:    Arc<EngineConfig>,
    flow:      FlowControl,
    telemetry: TelemetryHandle,
    os_ops:    Arc<dyn OsOps>,
}

impl BlockEngine {
    pub fn new(
        config:    Arc<EngineConfig>,
        flow:      FlowControl,
        telemetry: TelemetryHandle,
        os_ops:    Arc<dyn OsOps>,
    ) -> Self {
        Self { config, flow, telemetry, os_ops }
    }

    /// Copia un archivo grande usando el pipeline de bloques con BufferPool RAII.
    pub fn copy_file(
        &self,
        source:    &Path,
        dest:      &Path,
        file_size: u64,
    ) -> Result<Option<String>> {
        tracing::debug!(
            "BlockEngine: {} → {} ({:.1} MB)",
            source.display(),
            dest.display(),
            file_size as f64 / 1024.0 / 1024.0,
        );

        // ── Canal crossbeam ───────────────────────────────────────────────
        let (tx, rx) = crossbeam::channel::bounded(self.config.channel_capacity);

        // ── BufferPool compartido entre reader y writer ───────────────────
        //
        // Tamaño del pool = channel_cap + 2:
        //   - channel_cap: buffers que pueden estar en el canal simultáneamente
        //   - 1: buffer que el reader está llenando en este momento
        //   - 1: buffer que el writer está vaciando en este momento
        //
        // Con este tamaño el pool NUNCA se agota en uso normal.
        // El backpressure lo da el canal (lleno → reader bloquea en tx.send),
        // no el pool (que siempre tiene al menos 1 buffer libre).
        let pool_size = self.config.channel_capacity + 2;
        let pool = BufferPool::new(self.config.block_size_bytes, pool_size);

        // ── Thread A: Reader ──────────────────────────────────────────────
        let config_reader = (*self.config).clone();
        let flow_reader   = self.flow.clone();
        let telemetry_r   = self.telemetry.clone();
        let source_path   = source.to_path_buf();
        let pool_reader   = pool.clone(); // clon barato — mismo Arc interno

        let reader_handle = thread::Builder::new()
            .name(format!(
                "block-reader:{}",
                source.file_name().and_then(|n| n.to_str()).unwrap_or("?")
            ))
            .spawn(move || {
                let reader = BlockReader::new(
                    config_reader,
                    flow_reader,
                    telemetry_r,
                    pool_reader,
                );
                reader.run(&source_path, tx)
                // tx hace drop aquí → canal se cierra → writer termina su loop
            })
            .map_err(|e| CoreError::io(
                source,
                std::io::Error::new(std::io::ErrorKind::Other, format!("thread reader: {e}")),
            ))?;

        // ── Thread B: Writer (thread actual) ──────────────────────────────
        // El writer corre en el thread actual. Recibe Blocks del canal.
        // Cada drop(block) devuelve el PooledBuffer al pool → reader puede
        // reutilizarlo sin allocar.
        let config_writer = (*self.config).clone();

        let throttle = if self.config.bandwidth_limit_bytes_per_sec > 0 {
            Some(crate::bandwidth::ThrottleHandle::new(
                self.config.bandwidth_limit_bytes_per_sec,
                self.config.bandwidth_burst_bytes,
            ))
        } else {
            None
        };

        let writer = BlockWriter::new(config_writer, throttle);
        let write_result = writer.run(
            source,
            dest,
            rx,
            None,
            self.os_ops.as_ref(),
            file_size,
        );

        // ── Join del reader ───────────────────────────────────────────────
        // El writer termina cuando el canal se cierra (reader hace drop de tx).
        // Join aquí es seguro — el reader ya terminó.
        let source_hash = reader_handle
            .join()
            .map_err(|_| CoreError::PipelineDisconnected)??;

        let write_result = write_result?;

        // ── Verificación cruzada ──────────────────────────────────────────
        if self.config.verify {
            if let (Some(src), Some(dst)) = (&source_hash, &write_result.dest_hash) {
                if src != dst {
                    return Err(CoreError::HashMismatch {
                        path:     dest.to_path_buf(),
                        expected: src.clone(),
                        actual:   dst.clone(),
                    });
                }
            }
            if let Some(ref hash) = source_hash {
                tracing::info!("Verificación OK [{}]: {}", self.config.hash_algorithm, hash);
            }
        }

        tracing::debug!(
            "BlockEngine: OK — {:.1} MB | pool disponible: {}/{}",
            write_result.bytes_written as f64 / 1024.0 / 1024.0,
            pool.available(),
            pool_size,
        );

        Ok(source_hash)
    }
}
