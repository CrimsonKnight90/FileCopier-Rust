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
//! ## Limpieza de directorios vacíos
//!
//! Después de mover todos los archivos de un árbol, los directorios del origen
//! quedan vacíos. `remove_empty_dirs_after_move()` los elimina de hoja a raíz:
//! - Recorre el árbol en orden depth-first postorder (hijos antes que padres).
//! - Solo elimina directorios que están vacíos (`remove_dir` falla si no lo están).
//! - Nunca elimina el directorio raíz de origen si no está vacío.
//! - Es best-effort: un fallo de permisos en un subdir no detiene el resto.

use std::path::{Path, PathBuf};

use crate::os_ops::OsOps;

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

/// Resultado de mover un archivo individual.
#[derive(Debug)]
pub struct MoveResult {
    pub source:        PathBuf,
    pub dest:          PathBuf,
    pub bytes_moved:   u64,
    pub hash:          Option<String>,
    /// Si `true`, la copia fue exitosa pero el borrado del origen falló.
    pub delete_failed: bool,
    pub delete_error:  Option<String>,
}

/// Resultado de la limpieza de directorios vacíos.
#[derive(Debug)]
pub struct CleanupResult {
    /// Directorios eliminados con éxito.
    pub removed:  usize,
    /// Directorios que no se pudieron eliminar (no vacíos o sin permisos).
    pub skipped:  usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// delete_source_after_copy
// ─────────────────────────────────────────────────────────────────────────────

/// Borra el archivo origen después de verificar que el destino es correcto.
pub fn delete_source_after_copy(
    source:      &Path,
    dest:        &Path,
    source_hash: Option<String>,
    bytes:       u64,
    _os_ops:     &dyn OsOps,
) -> MoveResult {
    let (delete_failed, delete_error) = match std::fs::remove_file(source) {
        Ok(()) => {
            tracing::debug!("Move: origen eliminado — {}", source.display());
            (false, None)
        }
        Err(e) => {
            tracing::warn!(
                "Move: copia OK pero no se pudo borrar el origen '{}': {}",
                source.display(), e
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

// ─────────────────────────────────────────────────────────────────────────────
// remove_empty_dirs_after_move — FIX: limpiar carpetas vacías
// ─────────────────────────────────────────────────────────────────────────────

/// Elimina los directorios vacíos que quedan después de mover archivos.
///
/// ## Algoritmo
///
/// 1. Recolecta todos los subdirectorios del árbol origen.
/// 2. Los ordena por profundidad descendente (hijos antes que padres).
/// 3. Intenta `remove_dir()` en cada uno — solo tiene éxito si está vacío.
/// 4. El directorio raíz (`source_root`) se elimina al final si quedó vacío.
///
/// ## Ejemplo
///
/// ```text
/// origen/
///   ├─ subA/          → eliminado (vacío)
///   │   └─ deep/      → eliminado (vacío)
///   └─ subB/          → NO eliminado (contiene archivos que fallaron)
/// ```
///
/// ## Best-effort
///
/// Los errores de eliminación se registran como warnings pero no propagan.
/// Esto garantiza que un directorio con permisos especiales no bloquea la
/// limpieza del resto del árbol.
pub fn remove_empty_dirs_after_move(source_root: &Path) -> CleanupResult {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // Recolectar todos los directorios del árbol (incluido el raíz)
    collect_dirs_recursive(source_root, &mut dirs);

    // Ordenar por profundidad descendente: los más profundos primero.
    // Esto garantiza que eliminamos hijos antes que padres.
    dirs.sort_by(|a, b| {
        let depth_a = a.components().count();
        let depth_b = b.components().count();
        depth_b.cmp(&depth_a) // descendente
    });

    let mut removed = 0usize;
    let mut skipped = 0usize;

    for dir in &dirs {
        match std::fs::remove_dir(dir) {
            Ok(()) => {
                tracing::debug!("Move: directorio vacío eliminado — {}", dir.display());
                removed += 1;
            }
            Err(e) => {
                // El directorio no está vacío (archivos que fallaron al moverse)
                // o hay un problema de permisos — es normal, no es un error.
                tracing::debug!(
                    "Move: directorio no eliminado '{}': {} (no vacío o sin permisos)",
                    dir.display(), e
                );
                skipped += 1;
            }
        }
    }

    if removed > 0 {
        tracing::info!(
            "Move: {} directorio(s) vacío(s) eliminados, {} omitidos",
            removed, skipped
        );
    }

    CleanupResult { removed, skipped }
}

/// Recolecta recursivamente todos los directorios bajo `path` (incluido él mismo).
fn collect_dirs_recursive(path: &Path, dirs: &mut Vec<PathBuf>) {
    if !path.is_dir() {
        return;
    }
    dirs.push(path.to_path_buf());

    let read_dir = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("Move cleanup: no se pudo leer '{}': {}", path.display(), e);
            return;
        }
    };

    for entry in read_dir.filter_map(|e| e.ok()) {
        if entry.path().is_dir() {
            collect_dirs_recursive(&entry.path(), dirs);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// try_atomic_move
// ─────────────────────────────────────────────────────────────────────────────

/// Intenta un rename atómico. Si falla (cross-device), retorna `None`.
pub fn try_atomic_move(source: &Path, dest: &Path) -> Option<MoveResult> {
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::debug!("try_atomic_move: no se pudo crear dir '{}': {}", parent.display(), e);
            return None;
        }
    }

    match std::fs::rename(source, dest) {
        Ok(()) => {
            let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
            tracing::debug!(
                "Move atómico: {} → {}",
                source.display(), dest.display()
            );
            Some(MoveResult {
                source:        source.to_path_buf(),
                dest:          dest.to_path_buf(),
                bytes_moved:   bytes,
                hash:          None,
                delete_failed: false,
                delete_error:  None,
            })
        }
        Err(e) => {
            tracing::debug!("rename() falló (cross-device probable): {}", e);
            None
        }
    }
}

/// Determina si dos paths están en el mismo volumen/filesystem.
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let ra = get_windows_root(a);
        let rb = get_windows_root(b.parent().unwrap_or(b));
        match (ra, rb) {
            (Some(ra), Some(rb)) => ra.eq_ignore_ascii_case(&rb),
            _ => false,
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let da = std::fs::metadata(a).map(|m| m.dev()).ok();
        let db = std::fs::metadata(b.parent().unwrap_or(b)).map(|m| m.dev()).ok();
        matches!((da, db), (Some(a), Some(b)) if a == b)
    }

    #[cfg(not(any(windows, unix)))]
    false
}

#[cfg(windows)]
fn get_windows_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let b = s.as_bytes();
    if s.starts_with("\\\\") || s.starts_with("//") {
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

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn remove_empty_dirs_removes_all_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Crear árbol de directorios sin archivos
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::create_dir_all(root.join("a/d")).unwrap();

        let result = remove_empty_dirs_after_move(root);

        // El raíz y todos los subdirectorios deben haberse eliminado
        assert!(!root.join("a/b/c").exists());
        assert!(!root.join("a/b").exists());
        assert!(!root.join("a/d").exists());
        assert!(!root.join("a").exists());
        assert!(!root.exists()); // el raíz también
        assert!(result.removed >= 4);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn remove_empty_dirs_skips_non_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        std::fs::create_dir_all(root.join("sub")).unwrap();
        // Dejar un archivo en sub → no debe eliminarse
        std::fs::write(root.join("sub/remaining.txt"), b"I stay").unwrap();
        // Otro subdir vacío
        std::fs::create_dir(root.join("empty")).unwrap();

        let result = remove_empty_dirs_after_move(root);

        assert!(!root.join("empty").exists()); // eliminado
        assert!(root.join("sub").exists());    // no eliminado (tiene archivo)
        assert!(root.join("sub/remaining.txt").exists());
        assert_eq!(result.removed, 1); // solo "empty"
        assert!(result.skipped >= 1);  // "sub" y root
    }

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
    fn try_atomic_move_within_same_dir() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"data").unwrap();

        let result = try_atomic_move(&src, &dst);
        assert!(result.is_some());
        assert!(!src.exists());
        assert!(dst.exists());
    }
}
