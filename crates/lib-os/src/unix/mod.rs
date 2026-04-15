//! # unix
//!
//! Implementación de `OsAdapter` para Linux/macOS usando POSIX.
//!
//! ## Preallocación en Linux
//!
//! Usa `posix_fallocate(fd, 0, size)`. En ext4/xfs/btrfs reserva bloques
//! reales sin escribir ceros (a diferencia de `ftruncate`).
//! En tmpfs y NFS no está soportado → degradación silenciosa.
//!
//! ## Metadatos en Unix
//!
//! Copia `mode` (permisos), `uid`/`gid` (si tenemos privilegios root)
//! y `atime`/`mtime` con resolución de nanosegundos via `futimens`.

use std::path::Path;

use lib_core::error::{CoreError, Result};

use crate::detect::detect_drive_kind;
use crate::traits::{DriveKind, OsAdapter};

/// Adapter Unix (Linux + macOS) usando llamadas POSIX.
pub struct UnixAdapter;

impl UnixAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for UnixAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OsAdapter for UnixAdapter {
    fn detect_drive_kind(&self, path: &Path) -> DriveKind {
        detect_drive_kind(path)
    }

    fn preallocate(&self, path: &Path, size: u64) -> Result<()> {
        use std::fs::OpenOptions;
        use std::os::unix::io::AsRawFd;

        if size == 0 {
            return Ok(());
        }

        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| CoreError::io(path, e))?;

        let fd = file.as_raw_fd();

        #[cfg(target_os = "linux")]
        {
            let result = unsafe {
                libc::posix_fallocate(fd, 0, size as libc::off_t)
            };

            if result != 0 {
                let err = std::io::Error::from_raw_os_error(result);
                tracing::warn!(
                    "posix_fallocate no soportado en '{}': {} (degradando)",
                    path.display(),
                    err
                );
            } else {
                tracing::debug!("posix_fallocate OK: {} → {} bytes", path.display(), size);
            }
        }

        #[cfg(target_os = "macos")]
        {
            #[repr(C)]
            struct FStore {
                fst_flags:      u32,
                fst_posmode:    i32,
                fst_offset:     i64,
                fst_length:     i64,
                fst_bytesalloc: i64,
            }

            const F_PREALLOCATE:   libc::c_int = 42;
            const F_ALLOCATECONTIG: u32 = 0x00000002;
            const F_PEOFPOSMODE:    i32 = 3;

            let mut fst = FStore {
                fst_flags:      F_ALLOCATECONTIG,
                fst_posmode:    F_PEOFPOSMODE,
                fst_offset:     0,
                fst_length:     size as i64,
                fst_bytesalloc: 0,
            };

            let result = unsafe {
                libc::fcntl(fd, F_PREALLOCATE, &mut fst as *mut _)
            };

            if result == -1 {
                tracing::warn!(
                    "F_PREALLOCATE no soportado en '{}': {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            }

            unsafe { libc::ftruncate(fd, size as libc::off_t) };
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = fd;
            tracing::warn!("Preallocación no implementada para esta variante Unix");
        }

        Ok(())
    }

    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let src_meta = std::fs::metadata(source)
            .map_err(|e| CoreError::io(source, e))?;

        let mode = src_meta.mode();
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
            .map_err(|e| CoreError::io(dest, e))?;

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;

            let atime = libc::timespec {
                tv_sec:  src_meta.atime(),
                tv_nsec: src_meta.atime_nsec(),
            };
            let mtime = libc::timespec {
                tv_sec:  src_meta.mtime(),
                tv_nsec: src_meta.mtime_nsec(),
            };

            let dest_file = std::fs::OpenOptions::new()
                .write(true)
                .open(dest)
                .map_err(|e| CoreError::io(dest, e))?;

            let times = [atime, mtime];
            unsafe {
                libc::futimens(dest_file.as_raw_fd(), times.as_ptr());
            }
        }

        #[cfg(target_os = "linux")]
        {
            use std::ffi::CString;
            use std::os::unix::ffi::OsStrExt;

            let uid = src_meta.uid();
            let gid = src_meta.gid();

            if let Ok(dest_cstr) = CString::new(dest.as_os_str().as_bytes()) {
                let result = unsafe { libc::lchown(dest_cstr.as_ptr(), uid, gid) };
                if result != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() != Some(libc::EPERM) {
                        tracing::warn!("lchown falló en '{}': {}", dest.display(), err);
                    }
                }
            }
        }

        tracing::trace!(
            "Metadatos POSIX copiados: {} → {}",
            source.display(),
            dest.display()
        );

        Ok(())
    }

    fn platform_name(&self) -> &'static str {
        "unix"
    }
}

// ─────────────────────────────────────────────────────────────
// NUEVO: Implementación del trait OsOps para lib-core
// ─────────────────────────────────────────────────────────────

impl lib_core::os_ops::OsOps for UnixAdapter {
    fn preallocate(&self, path: &Path, size: u64) -> Result<()> {
        <Self as crate::traits::OsAdapter>::preallocate(self, path, size)
    }

    fn copy_metadata(&self, source: &Path, dest: &Path) -> Result<()> {
        <Self as crate::traits::OsAdapter>::copy_metadata(self, source, dest)
    }
}
