//! # orchestrator
//!
//! Punto de entrada del motor. Soporta Copy, Move y Dry-run.
//!
//! ## Fix: dest incorrecto cuando source es un archivo individual
//!
//! Cuando `source_root` es un archivo (no un directorio), `strip_prefix` produce
//! un `relative` vacío, y `dest_root.join("")` == `dest_root` mismo.
//! El writer entonces construye `dest_root.partial` en lugar de
//! `dest_root/filename.partial`.
//!
//! Solución: si `source_root` es un archivo, `dest` se construye como
//! `dest_root / source_root.file_name()` en lugar de `dest_root / relative`.
//!
//! ## Fix: directorios vacíos tras --move
//!
//! Después de mover todos los archivos de un árbol, los directorios del origen
//! quedan vacíos. Se llama a `remove_empty_dirs_after_move(source_root)` al final
//! de la fase de bloques, solo si la operación es Move y no fue cancelada.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::WalkDir;

use crate::checkpoint::{CheckpointState, FlowControl, ResumePolicy};
use crate::config::{EngineConfig, OperationMode};
use crate::engine::block::BlockEngine;
use crate::engine::dry_run::{DryRunReport, DryRunner};
use crate::engine::move_op::{
    delete_source_after_copy, remove_empty_dirs_after_move,
    same_filesystem, try_atomic_move,
};
use crate::engine::swarm::SwarmEngine;
use crate::error::Result;
use crate::os_ops::OsOps;
use crate::telemetry::{CopyProgress, TelemetrySink};

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct CopyResult {
    pub completed_files:    usize,
    pub failed_files:       usize,
    pub total_bytes:        u64,
    pub copied_bytes:       u64,
    pub elapsed_secs:       f64,
    pub revalidated_files:  usize,
    pub moved_files:        usize,
    pub move_delete_failed: usize,
    /// Directorios vacíos eliminados del origen (modo Move).
    pub dirs_removed:       usize,
    pub dry_run_report:     Option<DryRunReport>,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub source:   PathBuf,
    pub dest:     PathBuf,
    pub size:     u64,
    pub relative: PathBuf,
}

pub type ProgressCallback = Box<dyn Fn(CopyProgress) + Send + Sync>;

// ─────────────────────────────────────────────────────────────────────────────
// Orchestrator
// ─────────────────────────────────────────────────────────────────────────────

pub struct Orchestrator {
    config: Arc<EngineConfig>,
    flow:   FlowControl,
    os_ops: Arc<dyn OsOps>,
}

impl Orchestrator {
    pub fn new(config: EngineConfig, flow: FlowControl, os_ops: Arc<dyn OsOps>) -> Self {
        Self { config: Arc::new(config), flow, os_ops }
    }

    pub fn run(
        &self,
        source_root: &Path,
        dest_root:   &Path,
        on_progress: Option<ProgressCallback>,
    ) -> Result<CopyResult> {

        // ── Dry-run ───────────────────────────────────────────────────────
        if self.config.dry_run {
            return self.run_dry(source_root, dest_root);
        }

        // ── Escanear origen ───────────────────────────────────────────────
        let all_files   = self.scan_files(source_root, dest_root)?;
        let total_bytes: u64 = all_files.iter().map(|f| f.size).sum();

        // ── Checkpoint ────────────────────────────────────────────────────
        let job_id          = generate_job_id(source_root, dest_root);
        let checkpoint_path = CheckpointState::default_path(dest_root, &job_id);

        let mut checkpoint = if self.config.resume && checkpoint_path.exists() {
            CheckpointState::load(&checkpoint_path)?
        } else {
            CheckpointState::new(
                &job_id,
                source_root.to_path_buf(),
                dest_root.to_path_buf(),
                all_files.iter().map(|f| f.relative.clone()),
            )
        };

        // ── Validación del checkpoint ─────────────────────────────────────
        let revalidated_files = if self.config.resume {
            checkpoint.validate_completed(dest_root, self.config.resume_policy)
        } else {
            0
        };
        if revalidated_files > 0 {
            let _ = checkpoint.save(&checkpoint_path);
        }

        // ── Filtrar completados ───────────────────────────────────────────
        let pending: Vec<FileEntry> = all_files
            .into_iter()
            .filter(|f| !checkpoint.completed.contains_key(&f.relative))
            .collect();

        // ── Triage ────────────────────────────────────────────────────────
        let (large_files, small_files): (Vec<FileEntry>, Vec<FileEntry>) =
            pending.into_iter().partition(|f| self.config.is_large_file(f.size));

        // ── Telemetría ────────────────────────────────────────────────────
        let pending_count = large_files.len() + small_files.len();
        let telemetry     = Arc::new(TelemetrySink::new(total_bytes, pending_count));
        let motor_done    = Arc::new(AtomicBool::new(false));

        if let Some(cb) = on_progress {
            let tel   = Arc::clone(&telemetry);
            let done  = Arc::clone(&motor_done);
            std::thread::Builder::new()
                .name("progress-reporter".into())
                .spawn(move || {
                    while !done.load(Ordering::Relaxed) {
                        cb(tel.snapshot());
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    cb(tel.snapshot());
                })
                .ok();
        }

        let is_move = self.config.operation_mode == OperationMode::Move;
        let mut moved_files        = 0usize;
        let mut move_delete_failed = 0usize;
        let mut any_cancelled      = false;

        // ── Motor de enjambre (archivos pequeños) ─────────────────────────
        if !small_files.is_empty() {
            // En modo Move, intentar rename atómico para archivos en mismo filesystem
            let (to_rename, to_copy): (Vec<FileEntry>, Vec<FileEntry>) = if is_move {
                small_files.into_iter()
                    .partition(|f| same_filesystem(&f.source, &f.dest))
            } else {
                (vec![], small_files)
            };

            for entry in to_rename {
                match try_atomic_move(&entry.source, &entry.dest) {
                    Some(result) => {
                        let size = result.bytes_moved;
                        telemetry.add_bytes(size);
                        telemetry.complete_file();
                        moved_files += 1;
                        checkpoint.mark_completed(entry.relative, None, size);
                    }
                    None => {
                        telemetry.fail_file();
                        checkpoint.mark_failed(entry.relative, "rename atómico falló".into());
                    }
                }
            }

            if !to_copy.is_empty() {
                let swarm = SwarmEngine::new(
                    Arc::clone(&self.config),
                    self.flow.clone(),
                    telemetry.handle(),
                    Arc::clone(&self.os_ops),
                );

                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_io()
                    .build()
                    .expect("No se pudo crear runtime tokio");

                let cp_clone     = checkpoint_path.clone();
                let swarm_results = rt.block_on(async {
                    swarm.run(to_copy, &cp_clone).await
                })?;

                for (relative, result) in swarm_results {
                    let dest_path = dest_root.join(&relative);
                    let size = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);

                    match result {
                        Ok(hash) => {
                            if is_move {
                                let source = source_root.join(&relative);
                                let mr = delete_source_after_copy(
                                    &source, &dest_path,
                                    hash.clone(), size, self.os_ops.as_ref(),
                                );
                                if mr.delete_failed { move_delete_failed += 1; }
                                else                { moved_files += 1; }
                            }
                            checkpoint.mark_completed(relative, hash, size);
                        }
                        Err(e) => checkpoint.mark_failed(relative, e.to_string()),
                    }
                }
            }

            let _ = checkpoint.save(&checkpoint_path);
        }

        // ── Motor de bloques (archivos grandes) ───────────────────────────
        let block_engine = BlockEngine::new(
            Arc::clone(&self.config),
            self.flow.clone(),
            telemetry.handle(),
            Arc::clone(&self.os_ops),
        );

        for entry in large_files {
            if self.flow.is_cancelled() {
                any_cancelled = true;
                telemetry.fail_file();
                checkpoint.mark_failed(entry.relative.clone(), "Cancelado".into());
                let _ = checkpoint.save(&checkpoint_path);
                continue;
            }

            // Rename atómico para archivos grandes en mismo filesystem
            if is_move && same_filesystem(&entry.source, &entry.dest) {
                match try_atomic_move(&entry.source, &entry.dest) {
                    Some(result) => {
                        let size = result.bytes_moved;
                        telemetry.add_bytes(size);
                        telemetry.complete_file();
                        moved_files += 1;
                        checkpoint.mark_completed(entry.relative.clone(), None, size);
                        let _ = checkpoint.save(&checkpoint_path);
                        continue;
                    }
                    None => {} // fallback copy+delete
                }
            }

            match block_engine.copy_file(&entry.source, &entry.dest, entry.size) {
                Ok(hash) => {
                    telemetry.complete_file();
                    let dest_size = std::fs::metadata(&entry.dest)
                        .map(|m| m.len())
                        .unwrap_or(entry.size);

                    if is_move {
                        let mr = delete_source_after_copy(
                            &entry.source, &entry.dest,
                            hash.clone(), dest_size, self.os_ops.as_ref(),
                        );
                        if mr.delete_failed { move_delete_failed += 1; }
                        else                { moved_files += 1; }
                    }

                    checkpoint.mark_completed(entry.relative.clone(), hash, dest_size);
                }
                Err(e) => {
                    telemetry.fail_file();
                    tracing::error!("Error copiando '{}': {}", entry.source.display(), e);
                    checkpoint.mark_failed(entry.relative.clone(), e.to_string());
                }
            }

            let _ = checkpoint.save(&checkpoint_path);
        }

        motor_done.store(true, Ordering::Release);

        // ── Limpiar directorios vacíos tras Move ──────────────────────────
        //
        // FIX: después de mover todos los archivos, los directorios del origen
        // quedan vacíos. Los eliminamos de hoja a raíz.
        // Solo si es Move y no fue totalmente cancelado.
        let dirs_removed = if is_move && !any_cancelled {
            let cleanup = remove_empty_dirs_after_move(source_root);
            cleanup.removed
        } else {
            0
        };

        let snapshot = telemetry.snapshot();

        Ok(CopyResult {
            completed_files:    snapshot.completed_files,
            failed_files:       snapshot.failed_files,
            total_bytes:        snapshot.total_bytes,
            copied_bytes:       snapshot.copied_bytes,
            elapsed_secs:       snapshot.elapsed_secs,
            revalidated_files,
            moved_files,
            move_delete_failed,
            dirs_removed,
            dry_run_report:     None,
        })
    }

    fn run_dry(&self, source_root: &Path, dest_root: &Path) -> Result<CopyResult> {
        let job_id          = generate_job_id(source_root, dest_root);
        let checkpoint_path = CheckpointState::default_path(dest_root, &job_id);

        let completed = if self.config.resume && checkpoint_path.exists() {
            CheckpointState::load(&checkpoint_path)
                .map(|s| s.completed.into_keys().collect())
                .unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };

        let is_move = self.config.operation_mode == OperationMode::Move;
        let runner  = DryRunner::new(&self.config, is_move, completed);
        let report  = runner.run(source_root, dest_root);

        Ok(CopyResult {
            completed_files:    0,
            failed_files:       report.problem_files,
            total_bytes:        report.total_bytes,
            copied_bytes:       0,
            elapsed_secs:       0.0,
            revalidated_files:  0,
            moved_files:        0,
            move_delete_failed: 0,
            dirs_removed:       0,
            dry_run_report:     Some(report),
        })
    }

    /// Escanea el árbol de origen y construye la lista de `FileEntry`.
    ///
    /// ## Fix: source es archivo individual
    ///
    /// Si `source_root` es un archivo (no un directorio), `strip_prefix`
    /// produce `relative = ""` y `dest_root.join("") == dest_root`.
    /// El writer entonces genera `dest_root.partial` — incorrecto.
    ///
    /// Solución: cuando `source_root` es un archivo, `dest` se construye como
    /// `dest_root / source_root.file_name()`.
    fn scan_files(&self, source_root: &Path, dest_root: &Path) -> Result<Vec<FileEntry>> {
        let mut files = Vec::new();

        // Caso especial: source_root es un archivo individual
        if source_root.is_file() {
            let size     = std::fs::metadata(source_root).map(|m| m.len()).unwrap_or(0);
            let filename = source_root.file_name()
                .expect("source_root debe tener nombre de archivo");
            // dest = dest_root/filename  (no dest_root sola)
            let dest     = if dest_root.is_dir() {
                dest_root.join(filename)
            } else {
                dest_root.to_path_buf() // el usuario especificó el path destino completo
            };
            let relative = PathBuf::from(filename);
            files.push(FileEntry { source: source_root.to_path_buf(), dest, size, relative });
            return Ok(files);
        }

        // Caso normal: source_root es un directorio
        for entry in WalkDir::new(source_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let source   = entry.path().to_path_buf();
            let size     = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let relative = source.strip_prefix(source_root).unwrap().to_path_buf();
            let dest     = dest_root.join(&relative);
            files.push(FileEntry { source, dest, size, relative });
        }

        Ok(files)
    }
}

fn generate_job_id(source: &Path, dest: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    dest.hash(&mut h);
    format!("{:016x}", h.finish())
}
