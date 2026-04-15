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
//! 6. Operación final  → state.delete(checkpoint_path)
//! ```
//!
//! ## Formato en disco
//!
//! JSON legible: facilita inspección manual y evita problemas de
//! compatibilidad entre versiones. El overhead de serialización es
//! negligible (ocurre solo en pausa, no en el hot path).
//!
//! ## Control de flujo (Pausa/Reanudar)
//!
//! `FlowControl` expone un `AtomicBool` compartido que todos los threads
//! del motor leen en cada iteración del loop principal. Cuando se activa
//! `paused`, los threads terminan su bloque actual y se detienen limpiamente.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

// ─────────────────────────────────────────────────────────────────────────────
// FlowControl
// ─────────────────────────────────────────────────────────────────────────────

/// Control de flujo compartido entre el motor y el frontend (CLI/GUI).
///
/// Usa `AtomicBool` para comunicación lock-free entre threads.
/// El motor comprueba `is_paused()` al inicio de cada iteración;
/// cuando está pausado, espera en un spin-sleep ligero.
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

    /// Pausa el motor. Los threads en curso terminan su bloque actual.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        tracing::info!("FlowControl: motor pausado");
    }

    /// Reanuda el motor.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        tracing::info!("FlowControl: motor reanudado");
    }

    /// Cancela la operación de forma irrecuperable.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        tracing::info!("FlowControl: motor cancelado");
    }

    /// Los threads del motor llaman a esto en cada iteración de su loop.
    ///
    /// Retorna `Err(CoreError::Paused)` si debe detenerse.
    /// Retorna `Err(CoreError::PipelineDisconnected)` si fue cancelado.
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

    /// Versión bloqueante: espera hasta que se reanude o se cancele.
    ///
    /// Usa sleep de 50ms para no quemar CPU. Llamado después de detectar
    /// una pausa para que el thread espere sin polling agresivo.
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
// CheckpointState
// ─────────────────────────────────────────────────────────────────────────────

/// Estado serializable de una operación de copia.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointState {
    /// Identificador único del job (para evitar cargar checkpoints de otras ops).
    pub job_id: String,

    /// Momento en que se creó el checkpoint original.
    pub created_at: DateTime<Utc>,

    /// Momento del último save.
    pub updated_at: DateTime<Utc>,

    /// Ruta raíz del origen.
    pub source_root: PathBuf,

    /// Ruta raíz del destino.
    pub dest_root: PathBuf,

    /// Archivos completados exitosamente: path relativo → hash (si se verificó).
    pub completed: HashMap<PathBuf, Option<String>>,

    /// Archivos que fallaron: path relativo → mensaje de error.
    pub failed: HashMap<PathBuf, String>,

    /// Archivos pendientes: los que quedaban al momento del checkpoint.
    /// Se recalcula al cargar: todos los archivos del job - completed - failed.
    pub pending: HashSet<PathBuf>,

    /// Versión del formato para compatibilidad futura.
    pub format_version: u32,
}

impl CheckpointState {
    const FORMAT_VERSION: u32 = 1;

    /// Crea un nuevo estado de checkpoint para un job.
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

    /// Marca un archivo como completado.
    pub fn mark_completed(&mut self, relative_path: PathBuf, hash: Option<String>) {
        self.pending.remove(&relative_path);
        self.completed.insert(relative_path, hash);
        self.updated_at = Utc::now();
    }

    /// Marca un archivo como fallido (no bloqueante).
    pub fn mark_failed(&mut self, relative_path: PathBuf, error: String) {
        self.pending.remove(&relative_path);
        self.failed.insert(relative_path, error);
        self.updated_at = Utc::now();
    }

    /// Retorna `true` si la operación está completa (sin pendientes ni en progreso).
    pub fn is_complete(&self) -> bool {
        self.pending.is_empty()
    }

    /// Persistir el estado en disco como JSON.
    ///
    /// La escritura es atómica: se escribe en `.tmp` y luego se renombra.
    pub fn save(&self, checkpoint_path: &Path) -> Result<()> {
        let tmp_path = checkpoint_path.with_extension("tmp");

        let json = serde_json::to_string_pretty(self).map_err(|e| {
            CoreError::CheckpointSave {
                path: checkpoint_path.to_path_buf(),
                source: Box::new(e),
            }
        })?;

        std::fs::write(&tmp_path, &json).map_err(|e| CoreError::io(&tmp_path, e))?;

        std::fs::rename(&tmp_path, checkpoint_path).map_err(|e| {
            CoreError::rename(&tmp_path, checkpoint_path, e)
        })?;

        tracing::debug!(
            "Checkpoint guardado: {} completados, {} pendientes",
            self.completed.len(),
            self.pending.len()
        );

        Ok(())
    }

    /// Carga un checkpoint desde disco.
    pub fn load(checkpoint_path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(checkpoint_path)
            .map_err(|e| CoreError::io(checkpoint_path, e))?;

        let state: CheckpointState = serde_json::from_str(&json).map_err(|e| {
            CoreError::CheckpointLoad {
                path: checkpoint_path.to_path_buf(),
                source: Box::new(e),
            }
        })?;

        if state.format_version != Self::FORMAT_VERSION {
            tracing::warn!(
                "Checkpoint versión {} (esperada {}): puede haber incompatibilidades",
                state.format_version,
                Self::FORMAT_VERSION
            );
        }

        tracing::info!(
            "Checkpoint cargado: job_id={}, {} completados, {} pendientes, {} fallidos",
            state.job_id,
            state.completed.len(),
            state.pending.len(),
            state.failed.len()
        );

        Ok(state)
    }

    /// Elimina el checkpoint del disco (llamado al completar exitosamente).
    pub fn delete(checkpoint_path: &Path) -> Result<()> {
        if checkpoint_path.exists() {
            std::fs::remove_file(checkpoint_path)
                .map_err(|e| CoreError::io(checkpoint_path, e))?;
            tracing::info!("Checkpoint eliminado: {}", checkpoint_path.display());
        }
        Ok(())
    }

    /// Retorna el path canónico del archivo de checkpoint dado un destino.
    pub fn default_path(dest_root: &Path, job_id: &str) -> PathBuf {
        dest_root.join(format!(".filecopier_{job_id}.checkpoint"))
    }
}