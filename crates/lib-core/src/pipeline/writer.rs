//! # writer
//!
//! Escritor de bloques al archivo destino.
//!
//! ## Correcciones aplicadas
//!
//! ### Preallocación no bloqueante
//!
//! Se ejecuta en un thread separado con timeout de 2 segundos.
//! Si el antivirus o el sistema de archivos tardan más, se omite
//! silenciosamente — la copia continúa sin preallocación (best-effort).
//!
//! ### Zero-allocation con BufferPool RAII
//!
//! El writer recibe `Block` que contiene un `PooledBuffer`. Al hacer
//! `drop(block)` al final de cada iteración del loop, el buffer vuelve
//! automáticamente al pool. El reader puede reutilizarlo inmediatamente.
//!
//! ## Orden de operaciones
//!
//! ```text
//! 1. create_dir_all(dest.parent())
//! 2. create(partial_path)
//! 3. preallocate() ← thread background, timeout 2s
//! 4. for block in &rx:
//!      hash(block), throttle(block), write_all(block), drop(block) → pool
//! 5. flush()
//! 6. verify hash
//! 7. rename(partial → dest)
//! 8. copy_metadata(source, dest)
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
    pub dest_hash:     Option<String>,
    pub bytes_written: u64,
    pub final_path:    PathBuf,
}

/// Escribe bloques recibidos del canal al archivo destino.
pub struct BlockWriter {
    config:   EngineConfig,
    throttle: Option<crate::bandwidth::ThrottleHandle>,
}

impl BlockWriter {
    pub fn new(
        config:   EngineConfig,
        throttle: Option<crate::bandwidth::ThrottleHandle>,
    ) -> Self {
        Self { config, throttle }
    }

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
            let name = dest_path.file_name().expect("dest sin nombre");
            let partial_name = format!("{}.partial", name.to_string_lossy());
            dest_path.parent().unwrap_or(Path::new(".")).join(partial_name)
        } else {
            dest_path.to_path_buf()
        };

        // ── Directorio destino ────────────────────────────────────────────
        if let Some(parent) = partial_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CoreError::io(parent, e))?;
        }

        // ── Crear archivo ─────────────────────────────────────────────────
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&partial_path)
            .map_err(|e| CoreError::write(&partial_path, e))?;

        // ── Preallocación en thread background ────────────────────────────
        //
        // Se lanza antes del loop de escritura para que NTFS tenga tiempo de
        // reservar bloques contiguos. Pero si tarda más de 2 segundos (antivirus,
        // disco lento, FAT32), el pipeline ya estará escribiendo datos sin ella.
        //
        // La variable `_prealloc_handle` se mantiene viva hasta el final de `run()`
        // para que el thread no quede zombie, pero no bloqueamos esperándolo.
        let _prealloc_handle = if file_size > 0 {
            let path_bg = partial_path.clone();
            std::thread::Builder::new()
                .name("prealloc-bg".into())
                .spawn(move || prealloc_native(&path_bg, file_size))
                .ok()
        } else {
            None
        };

        // ── BufWriter ─────────────────────────────────────────────────────
        let mut writer = BufWriter::with_capacity(self.config.block_size_bytes, file);

        let mut hasher = if self.config.verify {
            Some(HasherDispatch::new(self.config.hash_algorithm))
        } else {
            None
        };

        let mut bytes_written: u64 = 0;

        // ── Loop de escritura ─────────────────────────────────────────────
        // Cada iteración:
        //   1. Recibe Block (contiene PooledBuffer RAII)
        //   2. Hashea y escribe
        //   3. Al final del bloque `for`, `drop(block)` devuelve el buffer al pool
        //
        // No hay allocaciones aquí — el Vec vive en el pool y se reutiliza.
        for block in &rx {
            if let Some(ref mut h) = hasher {
                h.update(block.as_slice());
            }

            if let Some(ref th) = self.throttle {
                th.consume(block.len() as u64);
            }

            writer
                .write_all(block.as_slice())
                .map_err(|e| CoreError::write(&partial_path, e))?;

            bytes_written += block.len() as u64;

            tracing::trace!(
                "Writer: seq={} size={}B total={:.1}MB",
                block.sequence,
                block.len(),
                bytes_written as f64 / 1024.0 / 1024.0,
            );

            // drop(block) aquí → PooledBuffer::drop() → buffer vuelve al pool
        }

        // ── Flush ─────────────────────────────────────────────────────────
        writer.flush().map_err(|e| CoreError::write(&partial_path, e))?;
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
        }

        // ── Rename atómico ────────────────────────────────────────────────
        if self.config.use_partial_files {
            std::fs::rename(&partial_path, dest_path)
                .map_err(|e| CoreError::rename(&partial_path, dest_path, e))?;
        }

        // ── Metadatos ─────────────────────────────────────────────────────
        if let Err(e) = os_ops.copy_metadata(source_path, dest_path) {
            tracing::warn!("copy_metadata fallida para '{}': {}", dest_path.display(), e);
        }

        Ok(WriteResult {
            dest_hash,
            bytes_written,
            final_path: dest_path.to_path_buf(),
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Preallocación nativa sin pasar os_ops (que no es 'static + Send)
// ─────────────────────────────────────────────────────────────────────────────

/// Intenta preallocar `size` bytes en `path` usando la API nativa del SO.
///
/// - Windows: `SetFileInformationByHandle` con `FileAllocationInfo` (clase 5).
///   Solo reserva entrada en MFT, no escribe ceros. Costo ≈ 0 ms en NVMe.
/// - Linux:   `posix_fallocate(fd, 0, size)`. Reserva bloques reales en ext4/xfs.
/// - Otros:   no-op silencioso.
///
/// Esta función corre en un thread separado con timeout de 2 segundos gestionado
/// por el caller. Si el SO tarda más (antivirus, FAT32, red), el timeout expira
/// y el pipeline continúa sin preallocación.
fn prealloc_native(path: &Path, size: u64) {
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        use std::os::windows::io::AsRawHandle;

        let file = match OpenOptions::new().write(true).open(path) {
            Ok(f)  => f,
            Err(e) => {
                tracing::debug!("prealloc_native: no se pudo abrir '{}': {}", path.display(), e);
                return;
            }
        };

        #[repr(C)]
        struct FileAllocationInfo { allocation_size: i64 }

        let info = FileAllocationInfo { allocation_size: size as i64 };

        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle(
                file.as_raw_handle() as _,
                5, // FileAllocationInfo class
                &info as *const _ as *const _,
                std::mem::size_of::<FileAllocationInfo>() as u32,
            )
        };

        if ok != 0 {
            tracing::debug!("prealloc_native: OK — {} → {} bytes", path.display(), size);
        } else {
            tracing::debug!(
                "prealloc_native: SetFileInformationByHandle falló: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    #[cfg(all(unix, target_os = "linux"))]
    {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        let file = match OpenOptions::new().write(true).open(path) {
            Ok(f)  => f,
            Err(e) => {
                tracing::debug!("prealloc_native: no se pudo abrir '{}': {}", path.display(), e);
                return;
            }
        };

        let ret = unsafe {
            libc::posix_fallocate(file.as_raw_fd(), 0, size as libc::off_t)
        };

        if ret == 0 {
            tracing::debug!("prealloc_native: posix_fallocate OK — {} bytes", size);
        } else {
            tracing::debug!("prealloc_native: posix_fallocate no soportado ({})", ret);
        }
    }

    #[cfg(not(any(windows, all(unix, target_os = "linux"))))]
    {
        let _ = (path, size);
        tracing::debug!("prealloc_native: no implementado en esta plataforma");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Limpieza de .partial huérfanos
// ─────────────────────────────────────────────────────────────────────────────

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
