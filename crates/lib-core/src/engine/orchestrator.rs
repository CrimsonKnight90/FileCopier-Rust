//! # orchestrator
//!
//! Punto de entrada del motor. Escanea el árbol de archivos, clasifica
//! cada archivo (bloque vs enjambre), gestiona el checkpoint y coordina
//! ambos motores.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::WalkDir;

use crate::checkpoint::{CheckpointState, FlowControl};
use crate::config::EngineConfig;
use crate::engine::block::BlockEngine;
use crate::engine::swarm::SwarmEngine;
use crate::error::Result;
use crate::os_ops::OsOps;
use crate::telemetry::{CopyProgress, TelemetrySink};

#[derive(Debug)]
pub struct CopyResult {
    pub completed_files: usize,
    pub failed_files:    usize,
    pub total_bytes:     u64,
    pub copied_bytes:    u64,
    pub elapsed_secs:    f64,
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub source:   PathBuf,
    pub dest:     PathBuf,
    pub size:     u64,
    pub relative: PathBuf,
}

pub type ProgressCallback = Box<dyn Fn(CopyProgress) + Send + Sync>;

pub struct Orchestrator {
    config: Arc<EngineConfig>,
    flow:   FlowControl,
    os_ops: Arc<dyn OsOps>,
}

impl Orchestrator {
    pub fn new(
        config: EngineConfig,
        flow:   FlowControl,
        os_ops: Arc<dyn OsOps>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            flow,
            os_ops,
        }
    }

    pub fn run(
        &self,
        source_root: &Path,
        dest_root:   &Path,
        on_progress: Option<ProgressCallback>,
    ) -> Result<CopyResult> {

        // ── 1. Escanear origen ────────────────────────────────────────────
        let all_files    = self.scan_files(source_root, dest_root)?;
        let total_bytes: u64 = all_files.iter().map(|f| f.size).sum();

        // ── 2. Checkpoint ─────────────────────────────────────────────────
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

        // ── 3. Filtrar completados ────────────────────────────────────────
        let pending: Vec<FileEntry> = all_files
            .into_iter()
            .filter(|f| !checkpoint.completed.contains_key(&f.relative))
            .collect();

        // ── 4. Triage ─────────────────────────────────────────────────────
        let (large_files, small_files): (Vec<FileEntry>, Vec<FileEntry>) =
            pending.into_iter().partition(|f| self.config.is_large_file(f.size));

        // ── 5. Telemetría ─────────────────────────────────────────────────
        let pending_count = large_files.len() + small_files.len();
        let telemetry     = Arc::new(TelemetrySink::new(total_bytes, pending_count));
        let motor_done    = Arc::new(AtomicBool::new(false));

        // Reporter de progreso en thread separado (~2 Hz)
        if let Some(cb) = on_progress {
            let telemetry_ui = Arc::clone(&telemetry);
            let done_flag    = Arc::clone(&motor_done);
            std::thread::Builder::new()
                .name("progress-reporter".into())
                .spawn(move || {
                    while !done_flag.load(Ordering::Relaxed) {
                        cb(telemetry_ui.snapshot());
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    // Snapshot final
                    cb(telemetry_ui.snapshot());
                })
                .ok();
        }

        // ── 6. Motor de enjambre (archivos pequeños) ──────────────────────
        if !small_files.is_empty() {
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
                swarm.run(small_files, &cp_clone).await
            })?;

            for (relative, result) in swarm_results {
                match result {
                    Ok(hash) => checkpoint.mark_completed(relative, hash),
                    Err(e)   => checkpoint.mark_failed(relative, e.to_string()),
                }
            }
            let _ = checkpoint.save(&checkpoint_path);
        }

        // ── 7. Motor de bloques (archivos grandes) ────────────────────────
        //
        // BlockEngine crea su propio BufferPool internamente en copy_file().
        // Cada archivo grande tiene su propio pool — no hay estado compartido
        // entre archivos, lo que simplifica el ciclo de vida de los buffers.
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

            match block_engine.copy_file(&entry.source, &entry.dest, entry.size) {
                Ok(hash) => {
                    telemetry.complete_file();
                    checkpoint.mark_completed(entry.relative.clone(), hash);
                }
                Err(e) => {
                    telemetry.fail_file();
                    tracing::error!(
                        "Error copiando '{}': {}",
                        entry.source.display(),
                        e
                    );
                    checkpoint.mark_failed(entry.relative.clone(), e.to_string());
                }
            }

            let _ = checkpoint.save(&checkpoint_path);
        }

        motor_done.store(true, Ordering::Release);

        let snapshot = telemetry.snapshot();

        Ok(CopyResult {
            completed_files: snapshot.completed_files,
            failed_files:    snapshot.failed_files,
            total_bytes:     snapshot.total_bytes,
            copied_bytes:    snapshot.copied_bytes,
            elapsed_secs:    snapshot.elapsed_secs,
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
