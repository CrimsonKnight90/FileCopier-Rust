//! # block
//!
//! Motor de bloques para archivos grandes (>= umbral de triage).
//!
//! ## Pipeline
//!
//! ```text
//! ┌──────────────┐  crossbeam  ┌──────────────┐
//! │  BlockReader │ ──────────► │  BlockWriter │
//! │  (thread A)  │  backpress. │  (thread B)  │
//! └──────────────┘             └──────────────┘
//!        │                            │
//!      hash origen               hash destino
//!                                     │
//!                              preallocate() ← antes primer byte
//!                              copy_metadata() ← tras rename
//! ```

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
    config:      Arc<EngineConfig>,
    flow:        FlowControl,
    telemetry:   TelemetryHandle,
    os_ops:      Arc<dyn OsOps>,
    buffer_pool: Option<BufferPool>,
}

impl BlockEngine {
    pub fn new(
        config:      Arc<EngineConfig>,
        flow:        FlowControl,
        telemetry:   TelemetryHandle,
        os_ops:      Arc<dyn OsOps>,
        buffer_pool: Option<BufferPool>,
    ) -> Self {
        Self { config, flow, telemetry, os_ops, buffer_pool }
    }

    /// Copia un archivo grande usando el pipeline de bloques.
    ///
    /// Retorna `Some(hash_hex)` si `config.verify == true`, `None` si no.
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

        let (tx, rx) = crossbeam::channel::bounded(self.config.channel_capacity);

        let config_reader = (*self.config).clone();
        let flow_reader   = self.flow.clone();
        let telemetry_r   = self.telemetry.clone();
        let source_path   = source.to_path_buf();
        let buffer_pool   = self.buffer_pool.clone();

        // ── Thread A: Reader ──────────────────────────────────────────────
        let reader_handle = thread::Builder::new()
            .name(format!(
                "block-reader:{}",
                source.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
            ))
            .spawn(move || {
                let reader = BlockReader::new(config_reader, flow_reader, telemetry_r, buffer_pool);
                reader.run(&source_path, tx)
            })
            .map_err(|e| {
                CoreError::io(
                    source,
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("No se pudo crear thread reader: {e}"),
                    ),
                )
            })?;

        // ── Thread B: Writer (thread actual) ──────────────────────────────
        let config_writer = (*self.config).clone();
        
        // Crear throttle si está configurado límite de ancho de banda
        let throttle = if self.config.bandwidth_limit_bytes_per_sec > 0 {
            Some(crate::bandwidth::ThrottleHandle::new(
                self.config.bandwidth_limit_bytes_per_sec,
                self.config.bandwidth_burst_bytes,
            ))
        } else {
            None
        };
        
        let writer  = BlockWriter::new(config_writer, throttle);
        let write_result = writer.run(
            source,
            dest,
            rx,
            None,           // source_hash: la verificación cruzada la hacemos abajo
            self.os_ops.as_ref(),
            file_size,
        );

        // ── Recoger resultado del reader ──────────────────────────────────
        let source_hash = reader_handle
            .join()
            .map_err(|_| CoreError::PipelineDisconnected)??;

        let write_result = write_result?;

        // ── Verificación cruzada de hashes ────────────────────────────────
        if self.config.verify {
            match (&source_hash, &write_result.dest_hash) {
                (Some(src), Some(dst)) if src != dst => {
                    return Err(CoreError::HashMismatch {
                        path:     dest.to_path_buf(),
                        expected: src.clone(),
                        actual:   dst.clone(),
                    });
                }
                _ => {}
            }

            if let Some(ref hash) = source_hash {
                tracing::info!(
                    "Verificación OK [{}]: {}",
                    self.config.hash_algorithm,
                    hash
                );
            }
        }

        tracing::debug!(
            "BlockEngine: completado — {:.1} MB escritos",
            write_result.bytes_written as f64 / 1024.0 / 1024.0
        );

        Ok(source_hash)
    }
}