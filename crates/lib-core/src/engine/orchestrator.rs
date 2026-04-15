//! # orchestrator
//!
//! Punto de entrada del motor. Escanea el árbol de archivos, clasifica
//! cada archivo (bloque vs enjambre), gestiona el checkpoint y coordina
//! ambos motores.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use walkdir::WalkDir;

use crate::buffer_pool::BufferPool;
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

/// Coordina el escaneo, clasificación y ejecución de la copia.
pub struct Orchestrator {
    config: Arc<EngineConfig>,
    flow:   FlowControl,
    os_ops: Arc<dyn OsOps>,               // ← NUEVO
}

impl Orchestrator {
    pub fn new(
        config: EngineConfig,
        flow:   FlowControl,
        os_ops: Arc<dyn OsOps>,           // ← NUEVO
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
        let all_files = self.scan_files(source_root, dest_root)?;
        let total_bytes: u64 = all_files.iter().map(|f| f.size).sum();
        let total_files = all_files.len();

        // ── 3. Checkpoint ─────────────────────────────────────────────────
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

        // ── 4. Filtrar completados ────────────────────────────────────────
        let pending: Vec<FileEntry> = all_files
            .into_iter()
            .filter(|f| !checkpoint.completed.contains_key(&f.relative))
            .collect();

        // ── 5. Triage ─────────────────────────────────────────────────────
        let (large_files, small_files): (Vec<FileEntry>, Vec<FileEntry>) =
            pending.into_iter().partition(|f| self.config.is_large_file(f.size));

        // ── 6. Telemetría ─────────────────────────────────────────────────
        let pending_count = large_files.len() + small_files.len();
        let telemetry = Arc::new(TelemetrySink::new(total_bytes, pending_count));

        let motor_done = Arc::new(AtomicBool::new(false));

        // ── 7. Motor de enjambre ──────────────────────────────────────────
        if !small_files.is_empty() {
            let swarm = SwarmEngine::new(
                Arc::clone(&self.config),
                self.flow.clone(),
                telemetry.handle(),
                Arc::clone(&self.os_ops),        // ← NUEVO
            );

            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_io()
                .build()
                .expect("No se pudo crear el runtime tokio");

            let cp_clone = checkpoint_path.clone();
            let swarm_results = rt.block_on(async {
                swarm.run(small_files, &cp_clone).await
            })?;

            for (relative, result) in swarm_results {
                match result {
                    Ok(hash) => checkpoint.mark_completed(relative, hash),
                    Err(e)  => checkpoint.mark_failed(relative, e.to_string()),
                }
            }

            let _ = checkpoint.save(&checkpoint_path);
        }

        // ── 8. Motor de bloques ───────────────────────────────────────────
        // Crear buffer pool solo para archivos grandes (optimización zero-alloc)
        let buffer_pool = Some(BufferPool::new(
            self.config.block_size_bytes,
            self.config.channel_capacity * 2, // Doble capacidad para reader + writer
        ));

        let block_engine = BlockEngine::new(
            Arc::clone(&self.config),
            self.flow.clone(),
            telemetry.handle(),
            Arc::clone(&self.os_ops),
            buffer_pool,
        );

        for entry in large_files {
            if self.flow.is_cancelled() {
                telemetry.fail_file();
                checkpoint.mark_failed(entry.relative.clone(), "Cancelado".into());
                let _ = checkpoint.save(&checkpoint_path);
                continue;
            }

            match block_engine.copy_file(&entry.source, &entry.dest, entry.size) {   // ← NUEVO
                Ok(hash) => {
                    telemetry.complete_file();
                    checkpoint.mark_completed(entry.relative.clone(), hash);
                }
                Err(e) => {
                    telemetry.fail_file();
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
