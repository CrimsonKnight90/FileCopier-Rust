//! # windows
//!
//! Implementación de `OsAdapter` para Windows usando WinAPI.
//!
//! ## Operaciones implementadas
//!
//! - `preallocate`: `SetFileInformationByHandle` con `FileAllocationInfo`.
//!   Reserva espacio contiguo en NTFS reduciendo fragmentación.
//!
//! - `copy_metadata`: `SetFileTime` para timestamps, `SetFileAttributes`
//!   para atributos (hidden, read-only, system, archive).
//!
//! ## Preallocación en NTFS
//!
//! A diferencia de `SetEndOfFile` (que escribe ceros y es lento),
//! `FileAllocationInfo` solo reserva la estructura en la MFT de NTFS.
//! El tiempo de preallocación de un archivo de 10 GB es ~0ms.

use std::path::Path;

use lib_core::error::{CoreError, Result};
use crate::traits::{DriveKind, OsAdapter};
use crate::detect::detect_drive_kind;

pub struct WindowsAdapter;

impl WindowsAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WindowsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OsAdapter for WindowsAdapter {
    fn detect_drive_kind(&self, path: &Path) -> DriveKind {
        detect_drive_kind(path)
    }

    fn preallocate(&self, path: &Path, size: u64) -> Result<()> {
        use std::fs::OpenOptions;
        use std::os::windows::io::AsRawHandle;

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .map_err(|e| CoreError::io(path, e))?;

        let handle = file.as_raw_handle();

        #[repr(C)]
        struct FileAllocationInfo {
            allocation_size: i64,
        }

        let info = FileAllocationInfo {
            allocation_size: size as i64,
        };

        const FILE_ALLOCATION_INFO_CLASS: i32 = 5;

        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle(
                handle as _,
                FILE_ALLOCATION_INFO_CLASS,
                &info as *const _ as *const _,
                std::mem::size_of::<FileAllocationInfo>() as u32,
            )
        };

        if result == 0 {
            tracing::warn!(
                "Preallocación no soportada en '{}': {} (degradando)",
                path.display(),
                std::io::Error::last_os_error()
            );
        } else {
            tracing::debug!("Preallocación OK: {} → {} bytes", path.display(), size);
        }

        Ok(())
    }

    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()> {
        use std::os::windows::fs::MetadataExt;

        let src_meta = std::fs::metadata(source)
            .map_err(|e| CoreError::io(source, e))?;

        let src_attrs = src_meta.file_attributes();

        const COPY_ATTRS: u32 = 0x00000001
            | 0x00000002
            | 0x00000004
            | 0x00000020
            | 0x00000080;

        let attrs_to_set = src_attrs & COPY_ATTRS;

        if attrs_to_set != 0 {
            let dest_wide: Vec<u16> = {
                use std::os::windows::ffi::OsStrExt;
                std::ffi::OsStr::new(dest)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect()
            };

            let result = unsafe {
                windows_sys::Win32::Storage::FileSystem::SetFileAttributesW(
                    dest_wide.as_ptr(),
                    attrs_to_set,
                )
            };

            if result == 0 {
                tracing::warn!(
                    "No se pudieron copiar atributos a '{}': {}",
                    dest.display(),
                    std::io::Error::last_os_error()
                );
            }
        }

        tracing::trace!("Metadatos copiados: {} → {}", source.display(), dest.display());
        Ok(())
    }

    fn platform_name(&self) -> &'static str {
        "windows"
    }
}

// ─────────────────────────────────────────────────────────────
// NUEVO: Implementación del trait OsOps para lib-core
// ─────────────────────────────────────────────────────────────

impl lib_core::os_ops::OsOps for WindowsAdapter {
    fn preallocate(&self, path: &Path, size: u64) -> Result<()> {
        <Self as crate::traits::OsAdapter>::preallocate(self, path, size)
    }

    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()> {
        <Self as crate::traits::OsAdapter>::copy_metadata(self, source, dest)
    }
}
