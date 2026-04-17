//! # orchestrator
//!
//! Punto de entrada del motor. Soporta Copy, Move y Dry-run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::WalkDir;

use crate::checkpoint::{CheckpointState, FlowControl};
use crate::config::{EngineConfig, OperationMode};
use crate::engine::block::BlockEngine;
use crate::engine::dry_run::{DryRunReport, DryRunner};
use crate::engine::move_op::{delete_source_after_copy, same_filesystem, try_atomic_move};
use crate::engine::swarm::SwarmEngine;
use crate::error::Result;
use crate::os_ops::OsOps;
use crate::telemetry::{CopyProgress, TelemetrySink};

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct CopyResult {
    pub completed_files:   usize,
    pub failed_files:      usize,
    pub total_bytes:       u64,
    pub copied_bytes:      u64,
    pub elapsed_secs:      f64,
    /// Archivos revertidos en validación de checkpoint y recopiados.
    pub revalidated_files: usize,
    /// Archivos movidos exitosamente (modo Move).
    pub moved_files:       usize,
    /// Archivos cuyo origen no pudo borrarse después de la copia (modo Move).
    pub move_delete_failed: usize,
    /// `Some(report)` si se ejecutó en modo dry-run.
    pub dry_run_report:    Option<DryRunReport>,
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

        // ── Dry-run: analizar sin ejecutar ────────────────────────────────
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
            let tel_ui    = Arc::clone(&telemetry);
            let done_flag = Arc::clone(&motor_done);
            std::thread::Builder::new()
                .name("progress-reporter".into())
                .spawn(move || {
                    while !done_flag.load(Ordering::Relaxed) {
                        cb(tel_ui.snapshot());
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    cb(tel_ui.snapshot());
                })
                .ok();
        }

        let mut moved_files        = 0usize;
        let mut move_delete_failed = 0usize;
        let is_move = self.config.operation_mode == OperationMode::Move;

        // ── Motor de enjambre (archivos pequeños) ─────────────────────────
        if !small_files.is_empty() {
            // En modo Move, intentar rename atómico primero para archivos en
            // el mismo filesystem (O(1), sin I/O de datos)
            let (to_rename, to_copy): (Vec<FileEntry>, Vec<FileEntry>) = if is_move {
                small_files.into_iter().partition(|f| same_filesystem(&f.source, &f.dest))
            } else {
                (vec![], small_files)
            };

            // Rename atómico para archivos en mismo filesystem
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
                        // Fallback a copy+delete
                        telemetry.fail_file();
                        checkpoint.mark_failed(entry.relative, "rename atómico falló".into());
                    }
                }
            }

            // Copia normal (enjambre) para archivos cross-device o modo Copy
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

                let cp_clone = checkpoint_path.clone();
                let swarm_results = rt.block_on(async {
                    swarm.run(to_copy, &cp_clone).await
                })?;

                for (relative, result) in swarm_results {
                    let size = std::fs::metadata(dest_root.join(&relative))
                        .map(|m| m.len())
                        .unwrap_or(0);

                    match result {
                        Ok(hash) => {
                            // Modo Move: borrar origen después de copia exitosa
                            if is_move {
                                let source = source_root.join(&relative);
                                let dest   = dest_root.join(&relative);
                                let mr = delete_source_after_copy(
                                    &source, &dest, hash.clone(), size, self.os_ops.as_ref()
                                );
                                if mr.delete_failed {
                                    move_delete_failed += 1;
                                    tracing::warn!(
                                        "Move: no se pudo borrar origen '{}': {}",
                                        source.display(),
                                        mr.delete_error.unwrap_or_default()
                                    );
                                } else {
                                    moved_files += 1;
                                }
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
                telemetry.fail_file();
                checkpoint.mark_failed(entry.relative.clone(), "Cancelado".into());
                let _ = checkpoint.save(&checkpoint_path);
                continue;
            }

            // Modo Move mismo filesystem: rename atómico O(1)
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
                    None => { /* fallback a copy+delete */ }
                }
            }

            match block_engine.copy_file(&entry.source, &entry.dest, entry.size) {
                Ok(hash) => {
                    telemetry.complete_file();
                    let dest_size = std::fs::metadata(&entry.dest)
                        .map(|m| m.len())
                        .unwrap_or(entry.size);

                    // Modo Move: borrar origen
                    if is_move {
                        let mr = delete_source_after_copy(
                            &entry.source, &entry.dest,
                            hash.clone(), dest_size, self.os_ops.as_ref()
                        );
                        if mr.delete_failed {
                            move_delete_failed += 1;
                        } else {
                            moved_files += 1;
                        }
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
        let snapshot = telemetry.snapshot();

        Ok(CopyResult {
            completed_files:   snapshot.completed_files,
            failed_files:      snapshot.failed_files,
            total_bytes:       snapshot.total_bytes,
            copied_bytes:      snapshot.copied_bytes,
            elapsed_secs:      snapshot.elapsed_secs,
            revalidated_files,
            moved_files,
            move_delete_failed,
            dry_run_report:    None,
        })
    }

    /// Ejecuta en modo dry-run: análisis sin escritura.
    fn run_dry(&self, source_root: &Path, dest_root: &Path) -> Result<CopyResult> {
        // Cargar checkpoint si existe (para mostrar qué se saltaría)
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

        let total_files  = report.total_files;
        let total_bytes  = report.total_bytes;
        let skipped      = report.skipped_files;

        Ok(CopyResult {
            completed_files:    0,
            failed_files:       report.problem_files,
            total_bytes,
            copied_bytes:       0, // nada se copió
            elapsed_secs:       0.0,
            revalidated_files:  0,
            moved_files:        0,
            move_delete_failed: 0,
            dry_run_report:     Some(report),
        })
    }

    fn scan_files(&self, source_root: &Path, dest_root: &Path) -> Result<Vec<FileEntry>> {
        let mut files = Vec::new();
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
