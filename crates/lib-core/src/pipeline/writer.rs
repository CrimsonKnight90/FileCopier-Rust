//! # writer
//!
//! Escritor de bloques al archivo destino.
//!
//! ## Responsabilidades
//!
//! - Crear el archivo destino como `.partial` (si `config.use_partial_files`).
//! - Recibir bloques del canal crossbeam y escribirlos secuencialmente.
//! - Computar el hash del destino si `config.verify == true`.
//! - Pre-allocar espacio antes del primer byte (reduce fragmentación NTFS).
//! - Realizar rename atómico del `.partial` al nombre final.
//! - Copiar metadatos (timestamps, atributos) tras el rename.
//!
//! ## Garantías de escritura
//!
//! El writer usa `BufWriter` para agrupar escrituras pequeñas.
//! Al finalizar llama a `flush()` explícitamente antes del rename.
//! Esto garantiza que no hay datos en el buffer del OS que puedan perderse.
//!
//! ## Convención de nombre `.partial`
//!
//! Se añade `.partial` como **sufijo al nombre completo**, NO se reemplaza
//! la extensión existente.
//!
//!   foto.jpg  → foto.jpg.partial   ✓
//!   Makefile  → Makefile.partial   ✓
//!
//! ## Orden de operaciones
//!
//! ```text
//! 1. create_dir_all(dest.parent())
//! 2. create(partial_path)          ← archivo existe en disco
//! 3. preallocate(partial_path, size_hint)  ← reservar espacio
//! 4. write_all(blocks...)          ← escribir datos
//! 5. flush()                       ← vaciar buffer OS
//! 6. drop(BufWriter)
//! 7. verify hash (si config.verify)
//! 8. rename(partial → dest)        ← atómico
//! 9. copy_metadata(source, dest)   ← timestamps y atributos
//! ```

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crossbeam::channel::Receiver;

use crate::config::EngineConfig;
use crate::error::{CoreError, Result};
use crate::hash::HasherDispatch;
use crate::os_ops::OsOps;
use crate::pipeline::Block;

/// Resultado de una operación de escritura completada.
pub struct WriteResult {
    /// Hash del destino (si `config.verify == true`).
    pub dest_hash: Option<String>,

    /// Bytes totales escritos.
    pub bytes_written: u64,

    /// Path final del archivo destino (ya renombrado desde `.partial`).
    pub final_path: PathBuf,
}

/// Escribe bloques recibidos del canal al archivo destino.
pub struct BlockWriter {
    config:  EngineConfig,
    throttle: Option<crate::bandwidth::ThrottleHandle>,
}

impl BlockWriter {
    pub fn new(config: EngineConfig, throttle: Option<crate::bandwidth::ThrottleHandle>) -> Self {
        Self { config, throttle }
    }

    /// Recibe bloques de `rx` y los escribe en `dest_path`.
    ///
    /// `source_path` se usa para copiar metadatos tras el rename.
    /// `source_hash` es el hash del origen calculado por el `BlockReader`.
    /// `os_ops` provee preallocación y copia de metadatos (plataforma-específico).
    /// `file_size` es el tamaño total esperado en bytes (hint para preallocación).
    pub fn run(
        &self,
        source_path: &Path,
        dest_path:   &Path,
        rx:          Receiver<Block>,
        source_hash: Option<&str>,
        os_ops:      &dyn OsOps,
        file_size:   u64,
    ) -> Result<WriteResult> {
        // ── Path del archivo .partial ─────────────────────────────────────
        let partial_path = if self.config.use_partial_files {
            let file_name = dest_path
                .file_name()
                .expect("dest_path debe tener nombre de archivo");
            let partial_name = format!("{}.partial", file_name.to_string_lossy());
            dest_path
                .parent()
                .unwrap_or(Path::new("."))
                .join(partial_name)
        } else {
            dest_path.to_path_buf()
        };

        // ── Asegurar directorio destino ───────────────────────────────────
        if let Some(parent) = partial_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::io(parent, e))?;
        }

        // ── Crear archivo destino ─────────────────────────────────────────
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&partial_path)
            .map_err(|e| CoreError::write(&partial_path, e))?;

        // ── Preallocación ─────────────────────────────────────────────────
        // Llamar ANTES del primer write, DESPUÉS de crear el archivo.
        // En NTFS: reserva bloques contiguos → elimina fragmentación.
        // En FAT32/red: no-op silencioso.
        if file_size > 0 {
            if let Err(e) = os_ops.preallocate(&partial_path, file_size) {
                // Preallocación es best-effort: un fallo no cancela la copia.
                tracing::warn!(
                    "Preallocación fallida para '{}': {} (continuando sin ella)",
                    partial_path.display(),
                    e
                );
            }
        }

        // ── BufWriter ─────────────────────────────────────────────────────
        let mut writer = BufWriter::with_capacity(self.config.block_size_bytes, file);

        let mut hasher = if self.config.verify {
            Some(HasherDispatch::new(self.config.hash_algorithm))
        } else {
            None
        };

        let mut bytes_written: u64 = 0;

        // ── Loop de escritura ─────────────────────────────────────────────
        for block in &rx {
            if let Some(ref mut h) = hasher {
                h.update(&block.data);
            }

            // Aplicar throttling si está configurado
            if let Some(ref throttle) = self.throttle {
                throttle.consume(block.len() as u64);
            }

            writer
                .write_all(&block.data)
                .map_err(|e| CoreError::write(&partial_path, e))?;

            bytes_written += block.len() as u64;

            tracing::trace!(
                "Writer: seq={} offset={} size={}B",
                block.sequence, block.offset, block.len()
            );
        }

        // ── Flush explícito antes del rename ──────────────────────────────
        writer
            .flush()
            .map_err(|e| CoreError::write(&partial_path, e))?;

        drop(writer);

        // ── Verificación de integridad ────────────────────────────────────
        let dest_hash = hasher.map(|h| h.finalize());

        if let (Some(src), Some(dst)) = (source_hash, dest_hash.as_deref()) {
            if src != dst {
                let _ = std::fs::remove_file(&partial_path);
                return Err(CoreError::HashMismatch {
                    path:     dest_path.to_path_buf(),
                    expected: src.to_string(),
                    actual:   dst.to_string(),
                });
            }
            tracing::debug!("Verificación OK: {}", dst);
        }

        // ── Rename atómico ────────────────────────────────────────────────
        if self.config.use_partial_files {
            std::fs::rename(&partial_path, dest_path)
                .map_err(|e| CoreError::rename(&partial_path, dest_path, e))?;

            tracing::debug!(
                "Rename atómico: {} → {}",
                partial_path.display(),
                dest_path.display()
            );
        }

        // ── Copiar metadatos ──────────────────────────────────────────────
        // DESPUÉS del rename: aplicar al archivo con su nombre final.
        // Timestamps y atributos se preservan aquí.
        if let Err(e) = os_ops.copy_metadata(source_path, dest_path) {
            // Best-effort: un fallo en metadatos no cancela la copia.
            tracing::warn!(
                "copy_metadata fallida para '{}': {}",
                dest_path.display(),
                e
            );
        }

        Ok(WriteResult {
            dest_hash,
            bytes_written,
            final_path: dest_path.to_path_buf(),
        })
    }
}

/// Limpia archivos `.partial` huérfanos en un directorio destino.
pub fn cleanup_partial_files(dest_root: &Path) -> std::io::Result<usize> {
    let mut count = 0;
    for entry in walkdir::WalkDir::new(dest_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".partial"))
            .unwrap_or(false)
        {
            std::fs::remove_file(path)?;
            count += 1;
            tracing::warn!("Limpiado archivo partial huérfano: {}", path.display());
        }
    }
    Ok(count)
}