//! # checkpoint
//!
//! Persistencia de estado para operaciones de pausa y reanudar.
//!
//! ## Flujo de vida de un checkpoint
//!
//! ```text
//! 1. Inicio de copia  → CheckpointState::new(job_id, files)
//! 2. Archivo copiado  → state.mark_completed(path, hash)
//! 3. Archivo falla    → state.mark_failed(path, error)
//! 4. Pausa / señal    → state.save(checkpoint_path)
//! 5. Reanuda          → CheckpointState::load(checkpoint_path)
//!                     → state.validate_completed(source_root, dest_root, policy)
//! 6. Operación final  → state.delete(checkpoint_path)
//! ```
//!
//! ## Validación en reanudación (`--resume`)
//!
//! La confianza ciega en el checkpoint puede causar corrupción silenciosa:
//! si un archivo fue borrado, truncado o corrompido después del checkpoint,
//! el motor lo saltaría creyendo que ya está completo.
//!
//! `validate_completed()` verifica cada entrada del checkpoint contra el disco
//! antes de que el Orchestrator filtre los archivos pendientes. La política
//! `ResumePolicy` controla qué nivel de confianza se aplica.
//!
//! ## Control de flujo (Pausa/Reanudar)
//!
//! `FlowControl` expone `AtomicBool`s compartidos que todos los threads
//! del motor leen en cada iteración. Cuando se activa `paused`, los threads
//! terminan su bloque actual y esperan en `wait_for_resume()`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// ResumePolicy
// ─────────────────────────────────────────────────────────────────────────────

/// Política de confianza aplicada al validar el checkpoint en `--resume`.
///
/// ## Elección de política
///
/// | Escenario                               | Política recomendada      |
/// |----------------------------------------|--------------------------|
/// | Red confiable, sin cambios externos     | `TrustCheckpoint`        |
/// | Copia local con riesgo de corrupción    | `VerifySize` (default)   |
/// | Entorno adverso o datos críticos        | `VerifyHash`             |
/// | Debugging / auditoría completa          | `VerifyHash`             |
///
/// `VerifySize` es el default porque cuesta una sola syscall `stat()` por
/// archivo y detecta el 99% de los casos problemáticos (borrado, truncado,
/// escritura parcial, reemplazo por archivo distinto del mismo nombre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumePolicy {
    /// Confiar en el checkpoint sin verificar el disco.
    ///
    /// Comportamiento anterior (ahora solo para compatibilidad o flags explícitos).
    /// **No recomendado**: no detecta archivos borrados o corrompidos post-checkpoint.
    TrustCheckpoint,

    /// Verificar existencia + tamaño del archivo destino (default).
    ///
    /// - Costo: 1 syscall `stat()` por archivo completado.
    /// - Detecta: borrado, truncado, reemplazo por archivo de tamaño distinto.
    /// - No detecta: corrupción bit-a-bit de un archivo del mismo tamaño.
    #[default]
    VerifySize,

    /// Verificar existencia + tamaño + hash del contenido completo.
    ///
    /// - Costo: leer todo el archivo destino (puede ser lento en archivos grandes).
    /// - Detecta: cualquier corrupción del contenido.
    /// - Requiere que el hash haya sido guardado en el checkpoint (copia con `--verify`).
    ///   Si no hay hash en el checkpoint, degrada a `VerifySize` para ese archivo.
    VerifyHash,
}

/// Resultado de validar un archivo completado contra el disco.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// El archivo está OK — puede saltarse en el resume.
    Ok,
    /// El archivo no existe en el destino — debe copiarse de nuevo.
    Missing,
    /// El archivo existe pero tiene tamaño incorrecto — debe copiarse de nuevo.
    SizeMismatch { expected: u64, found: u64 },
    /// El archivo existe y tiene el tamaño correcto pero el hash no coincide.
    HashMismatch { expected: String, found: String },
    /// No se pudo verificar (error de I/O, permisos) — se trata como pendiente.
    VerifyError(String),
}

impl ValidationResult {
    /// `true` si el archivo puede considerarse válido y saltarse.
    pub fn is_valid(&self) -> bool {
        matches!(self, ValidationResult::Ok)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// FlowControl
// ─────────────────────────────────────────────────────────────────────────────

/// Control de flujo compartido entre el motor y el frontend (CLI/GUI).
#[derive(Clone, Debug)]
pub struct FlowControl {
    paused:    Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl FlowControl {
    pub fn new() -> Self {
        Self {
            paused:    Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        tracing::info!("FlowControl: motor pausado");
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        tracing::info!("FlowControl: motor reanudado");
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        tracing::info!("FlowControl: motor cancelado");
    }

    #[inline]
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(CoreError::PipelineDisconnected);
        }
        if self.paused.load(Ordering::Acquire) {
            return Err(CoreError::Paused);
        }
        Ok(())
    }

    pub fn wait_for_resume(&self) -> Result<()> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(CoreError::PipelineDisconnected);
            }
            if !self.paused.load(Ordering::Acquire) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

impl Default for FlowControl {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CheckpointEntry — entrada individual con metadatos de verificación
// ─────────────────────────────────────────────────────────────────────────────

/// Información almacenada para cada archivo completado en el checkpoint.
///
/// Contiene todo lo necesario para verificar la integridad del archivo
/// destino en una reanudación posterior sin acceder al origen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Hash del contenido (hex). `None` si la copia se hizo sin `--verify`.
    pub hash: Option<String>,

    /// Tamaño del archivo en bytes en el momento de la copia.
    pub size_bytes: u64,

    /// Timestamp de cuándo se completó la copia.
    pub completed_at: DateTime<Utc>,
}

impl CheckpointEntry {
    pub fn new(hash: Option<String>, size_bytes: u64) -> Self {
        Self {
            hash,
            size_bytes,
            completed_at: Utc::now(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CheckpointState
// ─────────────────────────────────────────────────────────────────────────────

/// Estado serializable de una operación de copia.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    pub job_id:         String,
    pub created_at:     DateTime<Utc>,
    pub updated_at:     DateTime<Utc>,
    pub source_root:    PathBuf,
    pub dest_root:      PathBuf,

    /// Archivos completados: path relativo → `CheckpointEntry`.
    ///
    /// `CheckpointEntry` almacena hash + tamaño, permitiendo validación
    /// sin volver al origen en una reanudación.
    pub completed: HashMap<PathBuf, CheckpointEntry>,

    /// Archivos que fallaron: path relativo → mensaje de error.
    pub failed: HashMap<PathBuf, String>,

    /// Archivos pendientes al momento del checkpoint.
    pub pending: HashSet<PathBuf>,

    pub format_version: u32,
}

impl CheckpointState {
    const FORMAT_VERSION: u32 = 2; // Bumpeado: CheckpointEntry reemplaza Option<String>

    pub fn new(
        job_id: impl Into<String>,
        source_root: PathBuf,
        dest_root: PathBuf,
        all_files: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        let now = Utc::now();
        Self {
            job_id: job_id.into(),
            created_at: now,
            updated_at: now,
            source_root,
            dest_root,
            completed: HashMap::new(),
            failed: HashMap::new(),
            pending: all_files.into_iter().collect(),
            format_version: Self::FORMAT_VERSION,
        }
    }

    /// Marca un archivo como completado con su entrada de metadatos.
    pub fn mark_completed(
        &mut self,
        relative_path: PathBuf,
        hash: Option<String>,
        size_bytes: u64,
    ) {
        self.pending.remove(&relative_path);
        self.completed.insert(
            relative_path,
            CheckpointEntry::new(hash, size_bytes),
        );
        self.updated_at = Utc::now();
    }

    pub fn mark_failed(&mut self, relative_path: PathBuf, error: String) {
        self.pending.remove(&relative_path);
        self.failed.insert(relative_path, error);
        self.updated_at = Utc::now();
    }

    pub fn is_complete(&self) -> bool {
        self.pending.is_empty()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Validación en reanudación — el núcleo del fix
    // ─────────────────────────────────────────────────────────────────────

    /// Valida todos los archivos marcados como completados contra el disco.
    ///
    /// Los archivos que no pasen la validación se mueven de `completed`
    /// de vuelta a `pending` para que el motor los copie de nuevo.
    ///
    /// Retorna el número de archivos que fallaron la validación
    /// (fueron revertidos a pendientes).
    ///
    /// ## Algoritmo
    ///
    /// Para cada entrada en `completed`:
    /// 1. Construir `dest_path = dest_root / relative_path`
    /// 2. `stat(dest_path)` — si falla → `Missing` → revertir a pending
    /// 3. Comparar `metadata.len()` con `entry.size_bytes` → `SizeMismatch` → revertir
    /// 4. Si `policy == VerifyHash` y hay hash guardado → leer archivo y hashear
    /// 5. Si pasa todo → `Ok` → dejar en completed
    pub fn validate_completed(
        &mut self,
        dest_root: &Path,
        policy:    ResumePolicy,
    ) -> usize {
        if policy == ResumePolicy::TrustCheckpoint {
            tracing::info!("Resume policy: TrustCheckpoint — sin validación de disco");
            return 0;
        }

        // Recoger qué rutas hay que revalidar (no podemos mutar completed mientras iteramos)
        let paths_to_check: Vec<PathBuf> = self.completed.keys().cloned().collect();
        let total = paths_to_check.len();
        let mut reverted = 0usize;

        tracing::info!(
            "Validando {} archivos completados con política {:?}...",
            total, policy
        );

        for relative in paths_to_check {
            let entry = match self.completed.get(&relative) {
                Some(e) => e.clone(),
                None    => continue,
            };

            let dest_path = dest_root.join(&relative);
            let result    = validate_file(&dest_path, &entry, policy);

            match &result {
                ValidationResult::Ok => {
                    tracing::trace!("✓ {}", relative.display());
                }
                ValidationResult::Missing => {
                    tracing::warn!(
                        "Resume: '{}' no existe en destino — se copiará de nuevo",
                        relative.display()
                    );
                    self.revert_to_pending(relative);
                    reverted += 1;
                }
                ValidationResult::SizeMismatch { expected, found } => {
                    tracing::warn!(
                        "Resume: '{}' tamaño incorrecto (esperado={}, encontrado={}) — se copiará de nuevo",
                        relative.display(), expected, found
                    );
                    self.revert_to_pending(relative);
                    reverted += 1;
                }
                ValidationResult::HashMismatch { expected, found } => {
                    tracing::warn!(
                        "Resume: '{}' hash incorrecto (esperado={:.8}…, encontrado={:.8}…) — se copiará de nuevo",
                        relative.display(), expected, found
                    );
                    self.revert_to_pending(relative);
                    reverted += 1;
                }
                ValidationResult::VerifyError(msg) => {
                    // En caso de error de I/O durante la validación, ser conservador:
                    // tratar el archivo como pendiente para que se vuelva a copiar.
                    tracing::warn!(
                        "Resume: error verificando '{}': {} — se copiará de nuevo",
                        relative.display(), msg
                    );
                    self.revert_to_pending(relative);
                    reverted += 1;
                }
            }
        }

        if reverted == 0 {
            tracing::info!("Validación completa: todos los {} archivos OK", total);
        } else {
            tracing::warn!(
                "Validación completa: {}/{} archivos revertidos a pendientes",
                reverted, total
            );
        }

        reverted
    }

    /// Mueve una entrada de `completed` de vuelta a `pending`.
    fn revert_to_pending(&mut self, relative: PathBuf) {
        self.completed.remove(&relative);
        self.pending.insert(relative);
        self.updated_at = Utc::now();
    }

    // ─────────────────────────────────────────────────────────────────────
    // Persistencia
    // ─────────────────────────────────────────────────────────────────────

    pub fn save(&self, checkpoint_path: &Path) -> Result<()> {
        let tmp_path = checkpoint_path.with_extension("tmp");

        let json = serde_json::to_string_pretty(self).map_err(|e| {
            CoreError::CheckpointSave {
                path:   checkpoint_path.to_path_buf(),
                source: Box::new(e),
            }
        })?;

        std::fs::write(&tmp_path, &json).map_err(|e| CoreError::io(&tmp_path, e))?;
        std::fs::rename(&tmp_path, checkpoint_path)
            .map_err(|e| CoreError::rename(&tmp_path, checkpoint_path, e))?;

        tracing::debug!(
            "Checkpoint guardado: {} completados, {} pendientes",
            self.completed.len(), self.pending.len()
        );
        Ok(())
    }

    /// Carga un checkpoint desde disco.
    ///
    /// Soporta el formato v1 (campo `completed: HashMap<PathBuf, Option<String>>`)
    /// migrándolo automáticamente al formato v2 con tamaño 0 y sin hash si el
    /// archivo destino no existe aún.
    pub fn load(checkpoint_path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(checkpoint_path)
            .map_err(|e| CoreError::io(checkpoint_path, e))?;

        // Intentar deserializar formato v2 directamente
        if let Ok(state) = serde_json::from_str::<CheckpointState>(&json) {
            tracing::info!(
                "Checkpoint v{} cargado: {} completados, {} pendientes, {} fallidos",
                state.format_version,
                state.completed.len(),
                state.pending.len(),
                state.failed.len()
            );
            return Ok(state);
        }

        // Fallback: intentar migrar desde formato v1
        // En v1, `completed` era `HashMap<PathBuf, Option<String>>`
        tracing::warn!(
            "Checkpoint en formato legacy — migrando a v2 (tamaño no disponible, \
             la validación de tamaño se omitirá para entradas migradas)"
        );

        #[derive(Deserialize)]
        struct CheckpointV1 {
            job_id:         String,
            created_at:     DateTime<Utc>,
            updated_at:     DateTime<Utc>,
            source_root:    PathBuf,
            dest_root:      PathBuf,
            completed:      HashMap<PathBuf, Option<String>>,
            failed:         HashMap<PathBuf, String>,
            pending:        HashSet<PathBuf>,
            #[serde(rename = "format_version")]
            _format_version: u32,
        }

        let v1: CheckpointV1 = serde_json::from_str(&json).map_err(|e| {
            CoreError::CheckpointLoad {
                path:   checkpoint_path.to_path_buf(),
                source: Box::new(e),
            }
        })?;

        // Migrar: para entradas v1 sin tamaño, usar size_bytes=0.
        // validate_completed() con VerifySize comprobará stat() en disco,
        // que dará el tamaño real — si es >0 el archivo existe y pasa.
        // Si no existe, se detecta como Missing.
        let completed_v2: HashMap<PathBuf, CheckpointEntry> = v1.completed
            .into_iter()
            .map(|(path, hash)| {
                let entry = CheckpointEntry {
                    hash,
                    size_bytes:    0, // desconocido en v1 — se validará con stat()
                    completed_at:  v1.updated_at,
                };
                (path, entry)
            })
            .collect();

        Ok(CheckpointState {
            job_id:         v1.job_id,
            created_at:     v1.created_at,
            updated_at:     v1.updated_at,
            source_root:    v1.source_root,
            dest_root:      v1.dest_root,
            completed:      completed_v2,
            failed:         v1.failed,
            pending:        v1.pending,
            format_version: Self::FORMAT_VERSION,
        })
    }

    pub fn delete(checkpoint_path: &Path) -> Result<()> {
        if checkpoint_path.exists() {
            std::fs::remove_file(checkpoint_path)
                .map_err(|e| CoreError::io(checkpoint_path, e))?;
            tracing::info!("Checkpoint eliminado: {}", checkpoint_path.display());
        }
        Ok(())
    }

    pub fn default_path(dest_root: &Path, job_id: &str) -> PathBuf {
        dest_root.join(format!(".filecopier_{job_id}.checkpoint"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// validate_file — lógica de validación por archivo
// ─────────────────────────────────────────────────────────────────────────────

/// Valida un archivo destino contra su entrada de checkpoint.
fn validate_file(
    dest_path: &Path,
    entry:     &CheckpointEntry,
    policy:    ResumePolicy,
) -> ValidationResult {
    // ── 1. Stat — existencia y tamaño ─────────────────────────────────────
    let metadata = match std::fs::metadata(dest_path) {
        Ok(m)  => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ValidationResult::Missing;
        }
        Err(e) => {
            return ValidationResult::VerifyError(e.to_string());
        }
    };

    // ── 2. Validación de tamaño ───────────────────────────────────────────
    // Los checkpoints v1 migrados tienen size_bytes=0 — en ese caso, si el
    // archivo existe, lo consideramos válido (no tenemos referencia de tamaño).
    if entry.size_bytes > 0 && metadata.len() != entry.size_bytes {
        return ValidationResult::SizeMismatch {
            expected: entry.size_bytes,
            found:    metadata.len(),
        };
    }

    // ── 3. Validación de hash (solo si VerifyHash + hash disponible) ──────
    if policy == ResumePolicy::VerifyHash {
        match &entry.hash {
            None => {
                // Sin hash en checkpoint — degradar a VerifySize (ya pasó).
                tracing::trace!(
                    "VerifyHash: sin hash en checkpoint para '{}', aceptando por tamaño",
                    dest_path.display()
                );
            }
            Some(expected_hash) => {
                match hash_file_blake3(dest_path) {
                    Ok(found_hash) => {
                        if &found_hash != expected_hash {
                            return ValidationResult::HashMismatch {
                                expected: expected_hash.clone(),
                                found:    found_hash,
                            };
                        }
                    }
                    Err(e) => {
                        return ValidationResult::VerifyError(e.to_string());
                    }
                }
            }
        }
    }

    ValidationResult::Ok
}

/// Calcula el hash BLAKE3 de un archivo completo.
///
/// Usado durante la validación de reanudación con `ResumePolicy::VerifyHash`.
fn hash_file_blake3(path: &Path) -> std::io::Result<String> {
    use std::io::Read;

    let file   = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(4 * 1024 * 1024, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf    = vec![0u8; 4 * 1024 * 1024];

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(files: &[&str]) -> CheckpointState {
        let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        CheckpointState::new(
            "test-job-001",
            PathBuf::from("/origen"),
            PathBuf::from("/destino"),
            paths,
        )
    }

    // ── mark_completed / mark_failed ──────────────────────────────────────────

    #[test]
    fn mark_completed_stores_entry() {
        let mut state = make_state(&["a.txt", "b.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), Some("abc123".into()), 1024);

        let entry = state.completed.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(entry.hash.as_deref(), Some("abc123"));
        assert_eq!(entry.size_bytes, 1024);
        assert!(!state.pending.contains(&PathBuf::from("a.txt")));
    }

    #[test]
    fn mark_completed_without_hash() {
        let mut state = make_state(&["file.bin"]);
        state.mark_completed(PathBuf::from("file.bin"), None, 512);
        let entry = state.completed.get(&PathBuf::from("file.bin")).unwrap();
        assert!(entry.hash.is_none());
        assert_eq!(entry.size_bytes, 512);
    }

    #[test]
    fn is_complete_when_all_processed() {
        let mut state = make_state(&["a.txt", "b.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), None, 10);
        state.mark_failed(PathBuf::from("b.txt"), "error".into());
        assert!(state.is_complete());
    }

    // ── validate_completed — política TrustCheckpoint ─────────────────────────

    #[test]
    fn trust_checkpoint_skips_all_validation() {
        let dir   = tempfile::tempdir().unwrap();
        let mut state = make_state(&[]);
        // Marcar un archivo que NO existe en disco
        state.mark_completed(
            PathBuf::from("ghost.txt"),
            None,
            999,
        );
        // Con TrustCheckpoint, no se verifica nada — sigue en completed
        let reverted = state.validate_completed(dir.path(), ResumePolicy::TrustCheckpoint);
        assert_eq!(reverted, 0);
        assert!(state.completed.contains_key(&PathBuf::from("ghost.txt")));
    }

    // ── validate_completed — Missing ──────────────────────────────────────────

    #[test]
    fn verify_size_detects_missing_file() {
        let dir   = tempfile::tempdir().unwrap();
        let mut state = make_state(&[]);
        state.mark_completed(PathBuf::from("missing.txt"), None, 100);

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 1);
        assert!(!state.completed.contains_key(&PathBuf::from("missing.txt")));
        assert!(state.pending.contains(&PathBuf::from("missing.txt")));
    }

    // ── validate_completed — SizeMismatch ────────────────────────────────────

    #[test]
    fn verify_size_detects_truncated_file() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.txt");
        std::fs::write(&path, b"truncated").unwrap(); // 9 bytes

        let mut state = make_state(&[]);
        state.mark_completed(PathBuf::from("partial.txt"), None, 1000); // esperaba 1000

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 1);
        assert!(state.pending.contains(&PathBuf::from("partial.txt")));
    }

    // ── validate_completed — archivo OK ──────────────────────────────────────

    #[test]
    fn verify_size_accepts_correct_file() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"hello world";
        std::fs::write(dir.path().join("ok.txt"), data).unwrap();

        let mut state = make_state(&[]);
        state.mark_completed(PathBuf::from("ok.txt"), None, data.len() as u64);

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 0);
        assert!(state.completed.contains_key(&PathBuf::from("ok.txt")));
    }

    // ── validate_completed — VerifyHash ───────────────────────────────────────

    #[test]
    fn verify_hash_detects_corrupted_file() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupted.bin");
        let data = b"original content";
        std::fs::write(&path, data).unwrap();

        // Hash correcto del contenido
        let correct_hash = {
            let mut h = blake3::Hasher::new();
            h.update(data);
            h.finalize().to_hex().to_string()
        };

        // Ahora corromper el archivo
        std::fs::write(&path, b"corrupted!!!!!!!").unwrap(); // mismo tamaño

        let mut state = make_state(&[]);
        state.mark_completed(
            PathBuf::from("corrupted.bin"),
            Some(correct_hash),
            data.len() as u64,
        );

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifyHash);

        assert_eq!(reverted, 1);
        assert!(state.pending.contains(&PathBuf::from("corrupted.bin")));
    }

    #[test]
    fn verify_hash_accepts_intact_file() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"intact content here";
        std::fs::write(dir.path().join("intact.txt"), data).unwrap();

        let hash = {
            let mut h = blake3::Hasher::new();
            h.update(data);
            h.finalize().to_hex().to_string()
        };

        let mut state = make_state(&[]);
        state.mark_completed(
            PathBuf::from("intact.txt"),
            Some(hash),
            data.len() as u64,
        );

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifyHash);
        assert_eq!(reverted, 0);
    }

    // ── validate_completed — sin hash, VerifyHash degrada a VerifySize ────────

    #[test]
    fn verify_hash_without_stored_hash_falls_back_to_size() {
        let dir  = tempfile::tempdir().unwrap();
        let data = b"some data";
        std::fs::write(dir.path().join("nohash.bin"), data).unwrap();

        let mut state = make_state(&[]);
        state.mark_completed(PathBuf::from("nohash.bin"), None, data.len() as u64);

        // Sin hash en el checkpoint, VerifyHash debe aceptar por tamaño
        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifyHash);
        assert_eq!(reverted, 0);
    }

    // ── validate_completed — múltiples archivos, mix de resultados ────────────

    #[test]
    fn validate_multiple_files_mixed_results() {
        let dir = tempfile::tempdir().unwrap();

        // ok.txt — correcto
        std::fs::write(dir.path().join("ok.txt"), b"correct").unwrap();
        // bad.txt — no existe
        // small.txt — tamaño incorrecto
        std::fs::write(dir.path().join("small.txt"), b"x").unwrap();

        let mut state = make_state(&[]);
        state.mark_completed(PathBuf::from("ok.txt"),    None, 7);
        state.mark_completed(PathBuf::from("bad.txt"),   None, 100);
        state.mark_completed(PathBuf::from("small.txt"), None, 500);

        let reverted = state.validate_completed(dir.path(), ResumePolicy::VerifySize);

        assert_eq!(reverted, 2);
        assert!(state.completed.contains_key(&PathBuf::from("ok.txt")));
        assert!(state.pending.contains(&PathBuf::from("bad.txt")));
        assert!(state.pending.contains(&PathBuf::from("small.txt")));
    }

    // ── Save + Load round-trip ────────────────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip_v2() {
        let dir = tempfile::tempdir().unwrap();
        let cp  = dir.path().join("test.checkpoint");

        let mut state = make_state(&["a.txt", "b.txt", "c.txt"]);
        state.mark_completed(PathBuf::from("a.txt"), Some("hash_a".into()), 1024);
        state.mark_failed(PathBuf::from("b.txt"), "io error".into());
        state.save(&cp).unwrap();

        let loaded = CheckpointState::load(&cp).unwrap();
        assert_eq!(loaded.completed.len(), 1);
        let entry = loaded.completed.get(&PathBuf::from("a.txt")).unwrap();
        assert_eq!(entry.hash.as_deref(), Some("hash_a"));
        assert_eq!(entry.size_bytes, 1024);
        assert_eq!(loaded.failed.len(), 1);
        assert_eq!(loaded.pending.len(), 1);
    }

    // ── FlowControl ───────────────────────────────────────────────────────────

    #[test]
    fn flow_control_starts_unpaused_uncancelled() {
        let fc = FlowControl::new();
        assert!(!fc.is_paused());
        assert!(!fc.is_cancelled());
        assert!(fc.check().is_ok());
    }

    #[test]
    fn flow_control_cancel_overrides_pause() {
        let fc = FlowControl::new();
        fc.pause();
        fc.cancel();
        match fc.check() {
            Err(CoreError::PipelineDisconnected) => {}
            other => panic!("esperaba PipelineDisconnected: {other:?}"),
        }
    }

    #[test]
    fn flow_control_clone_shares_state() {
        let fc1 = FlowControl::new();
        let fc2 = fc1.clone();
        fc1.pause();
        assert!(fc2.is_paused());
        fc2.resume();
        assert!(!fc1.is_paused());
    }
}
