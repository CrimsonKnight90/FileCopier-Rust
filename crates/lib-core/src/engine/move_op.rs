//! # move_op
//!
//! Operación de movimiento de archivos: copiar → verificar → borrar origen.
//!
//! ## Semántica de seguridad
//!
//! El origen **nunca se borra** hasta que:
//! 1. La copia al destino se completó sin errores de I/O.
//! 2. Si `config.verify == true`: el hash del destino coincide con el del origen.
//! 3. El rename atómico del `.partial` al nombre final tuvo éxito.
//!
//! Si cualquiera de estos pasos falla, el origen permanece intacto y
//! el archivo `.partial` (si existe) es eliminado.
//!
//! ## Diferencia con rename(2) / MoveFile
//!
//! `rename(2)` es atómico solo dentro del mismo sistema de archivos.
//! Cross-device (NVMe→USB, local→red) siempre requiere copy+delete.
//! Esta implementación funciona en ambos casos de forma uniforme.
//!
//! ## Integración
//!
//! `MoveEngine` es usado por el `Orchestrator` cuando `config.operation_mode == Move`.
//! Internamente delega la copia al `BlockEngine` o al `SwarmEngine` según el triage,
//! y añade la fase de eliminación post-copia.

use std::path::{Path, PathBuf};

use crate::os_ops::OsOps;

/// Resultado de mover un archivo.
#[derive(Debug)]
pub struct MoveResult {
    pub source:        PathBuf,
    pub dest:          PathBuf,
    pub bytes_moved:   u64,
    pub hash:          Option<String>,
    /// Si `true`, la copia fue exitosa pero el borrado del origen falló.
    /// El archivo está duplicado — el usuario debe borrar el origen manualmente.
    pub delete_failed: bool,
    pub delete_error:  Option<String>,
}

/// Borra el archivo origen después de verificar que el destino es correcto.
///
/// Llamado por el Orchestrator después de que cada `copy_file()` o
/// `copy_small_file()` retorna exitosamente.
///
/// # Argumentos
///
/// * `source`      — path al archivo origen a borrar
/// * `dest`        — path al archivo destino (ya escrito y renombrado)
/// * `source_hash` — hash del origen calculado durante la copia (puede ser None)
/// * `bytes`       — bytes copiados (para el resultado)
/// * `os_ops`      — para operaciones de SO
pub fn delete_source_after_copy(
    source:      &Path,
    dest:        &Path,
    source_hash: Option<String>,
    bytes:       u64,
    _os_ops:     &dyn OsOps,
) -> MoveResult {
    // El destino ya existe y fue verificado por el motor de copia.
    // Solo queda borrar el origen.
    let (delete_failed, delete_error) = match std::fs::remove_file(source) {
        Ok(()) => {
            tracing::debug!(
                "Move: origen eliminado — {}",
                source.display()
            );
            (false, None)
        }
        Err(e) => {
            // El borrado falló (permisos, archivo en uso, etc.)
            // El archivo está duplicado — reportar pero no propagar como error
            // porque el destino está correcto.
            tracing::warn!(
                "Move: copia OK pero no se pudo borrar el origen '{}': {}",
                source.display(),
                e
            );
            (true, Some(e.to_string()))
        }
    };

    MoveResult {
        source:      source.to_path_buf(),
        dest:        dest.to_path_buf(),
        bytes_moved: bytes,
        hash:        source_hash,
        delete_failed,
        delete_error,
    }
}

/// Intenta un rename atómico primero (mismo filesystem).
/// Si falla con EXDEV/cross-device, delega a copy+delete.
///
/// Esta función es una optimización: si origen y destino están en el mismo
/// volumen, `rename()` es instantáneo (O(1)) y no usa I/O de disco.
pub fn try_atomic_move(source: &Path, dest: &Path) -> Option<MoveResult> {
    // Asegurar directorio destino
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::debug!("try_atomic_move: no se pudo crear dir '{}': {}", parent.display(), e);
            return None;
        }
    }

    match std::fs::rename(source, dest) {
        Ok(()) => {
            tracing::debug!(
                "Move atómico (mismo filesystem): {} → {}",
                source.display(), dest.display()
            );
            let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
            Some(MoveResult {
                source:       source.to_path_buf(),
                dest:         dest.to_path_buf(),
                bytes_moved:  bytes,
                hash:         None, // rename no calcula hash
                delete_failed: false,
                delete_error:  None,
            })
        }
        Err(e) => {
            // EXDEV = cross-device, también puede ser cross-filesystem en Windows
            tracing::debug!(
                "rename() falló (probablemente cross-device): {} — usará copy+delete",
                e
            );
            None
        }
    }
}

/// Determina si dos paths están en el mismo volumen/filesystem.
///
/// En Windows: compara la letra de unidad o el prefijo UNC.
/// En Unix: compara el device ID del `stat()`.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let root_a = get_windows_root(a);
        let root_b = get_windows_root(b);
        match (root_a, root_b) {
            (Some(ra), Some(rb)) => ra.eq_ignore_ascii_case(&rb),
            _ => false,
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dev_a = std::fs::metadata(a).map(|m| m.dev()).ok();
        let dev_b = std::fs::metadata(b.parent().unwrap_or(b))
            .map(|m| m.dev())
            .ok();
        match (dev_a, dev_b) {
            (Some(da), Some(db)) => da == db,
            _ => false,
        }
    }

    #[cfg(not(any(windows, unix)))]
    false
}

#[cfg(windows)]
fn get_windows_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    if s.starts_with("\\\\") || s.starts_with("//") {
        // UNC: \\server\share
        let parts: Vec<&str> = s.trim_start_matches('\\').splitn(3, '\\').collect();
        if parts.len() >= 2 {
            return Some(format!("\\\\{}\\{}", parts[0], parts[1]).to_lowercase());
        }
        return None;
    }
    if b.len() >= 2 && b[1] == b':' {
        return Some(format!("{}:", (b[0] as char).to_ascii_uppercase()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn same_filesystem_same_dir() {
        let dir = tempdir().unwrap();
        let a   = dir.path().join("a.txt");
        let b   = dir.path().join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        assert!(same_filesystem(&a, &b));
    }

    #[test]
    fn try_atomic_move_same_fs() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("source.txt");
        let dst = dir.path().join("dest.txt");
        std::fs::write(&src, b"content").unwrap();

        let result = try_atomic_move(&src, &dst);
        assert!(result.is_some());
        assert!(!src.exists());
        assert!(dst.exists());
    }
}
