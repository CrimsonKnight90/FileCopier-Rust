//! # telemetry
//!
//! Métricas diferenciadas en tiempo real para el motor de copia.
//!
//! ## Diseño de telemetría diferenciada
//!
//! - Motor de bloques → MB/s (throughput de bytes)
//! - Motor de enjambre → archivos/s (IOPS de metadatos)
//!
//! Ambas métricas se consolidan en `CopyProgress`.
//!
//! ## Thread-safety
//!
//! `TelemetrySink` usa contadores atómicos para actualizaciones lock-free
//! en el hot path. Las lecturas (snapshots) ocurren solo desde el thread de UI.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Snapshot inmutable del progreso en un instante dado.
#[derive(Debug, Clone)]
pub struct CopyProgress {
    pub total_bytes:              u64,
    pub copied_bytes:             u64,
    pub total_files:              usize,
    pub completed_files:          usize,
    pub failed_files:             usize,
    pub throughput_bytes_per_sec: f64,
    pub files_per_sec:            f64,
    pub percent:                  f64,
    pub elapsed_secs:             f64,
    pub eta_secs:                 Option<f64>,
    /// Nombre del archivo que se está copiando actualmente (si hay uno en progreso)
    pub current_file:             Option<String>,
    /// Progreso interno del archivo actual (0.0 - 1.0)
    pub current_file_progress:    f64,
}

/// Contador atómico compartido entre todos los threads del motor.
pub struct TelemetrySink {
    total_bytes:         u64,
    copied_bytes:        Arc<AtomicU64>,
    total_files:         usize,
    completed_files:     Arc<AtomicUsize>,
    failed_files:        Arc<AtomicUsize>,
    start:               Instant,
    last_snapshot_bytes: Arc<AtomicU64>,
    last_snapshot_time:  Arc<std::sync::Mutex<Instant>>,
    /// Archivo actual en proceso (para mostrar progreso interno)
    current_file:        Arc<std::sync::Mutex<Option<(String, f64)>>>,
}

impl TelemetrySink {
    pub fn new(total_bytes: u64, total_files: usize) -> Self {
        let now = Instant::now();
        Self {
            total_bytes,
            copied_bytes:        Arc::new(AtomicU64::new(0)),
            total_files,
            completed_files:     Arc::new(AtomicUsize::new(0)),
            failed_files:        Arc::new(AtomicUsize::new(0)),
            start:               now,
            last_snapshot_bytes: Arc::new(AtomicU64::new(0)),
            last_snapshot_time:  Arc::new(std::sync::Mutex::new(now)),
            current_file:        Arc::new(std::sync::Mutex::new(None)),
        }
    }

    #[inline]
    pub fn add_bytes(&self, bytes: u64) {
        self.copied_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn complete_file(&self) {
        self.completed_files.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn fail_file(&self) {
        self.failed_files.fetch_add(1, Ordering::Relaxed);
    }

    /// Actualiza el archivo actual en proceso y su progreso interno (0.0 - 1.0)
    pub fn set_current_file(&self, path: &std::path::Path, progress: f64) {
        let mut current = self.current_file.lock().unwrap();
        *current = Some((path.display().to_string(), progress.clamp(0.0, 1.0)));
    }

    /// Limpia el archivo actual cuando se completa
    pub fn clear_current_file(&self) {
        let mut current = self.current_file.lock().unwrap();
        *current = None;
    }

    pub fn handle(&self) -> TelemetryHandle {
        TelemetryHandle {
            copied_bytes:    Arc::clone(&self.copied_bytes),
            completed_files: Arc::clone(&self.completed_files),
            failed_files:    Arc::clone(&self.failed_files),
            current_file:    Arc::clone(&self.current_file),
        }
    }

    /// Calcula un snapshot inmutable del progreso actual.
    ///
    /// Llamado ~2 Hz desde el thread de UI. Calcula throughput incremental
    /// usando una ventana deslizante para evitar lecturas falsas de 0 B/s
    /// cuando el enjambre termina antes del primer tick del reporter.
    pub fn snapshot(&self) -> CopyProgress {
        let copied    = self.copied_bytes.load(Ordering::Acquire);
        let completed = self.completed_files.load(Ordering::Acquire);
        let failed    = self.failed_files.load(Ordering::Acquire);

        let elapsed      = self.start.elapsed();
        let elapsed_secs = elapsed.as_secs_f64();

        // Velocidad global desde el inicio (siempre disponible si hay bytes)
        let global_bps = if elapsed_secs > 0.0 {
            copied as f64 / elapsed_secs
        } else {
            0.0
        };

        // Velocidad incremental (ventana deslizante para suavizar picos)
        let throughput_bytes_per_sec = {
            let mut last_time  = self.last_snapshot_time.lock().unwrap();
            let last_bytes     = self.last_snapshot_bytes.load(Ordering::Relaxed);
            let delta_bytes    = copied.saturating_sub(last_bytes);
            let delta_secs     = last_time.elapsed().as_secs_f64();

            // Usar velocidad incremental si el delta es significativo.
            // Si el delta es muy pequeño (motor terminó entre ticks) o cero,
            // caer al global para evitar mostrar 0 B/s o 46 MB/s fijo.
            let rate = if delta_secs > 0.01 && delta_bytes > 0 {
                delta_bytes as f64 / delta_secs
            } else if elapsed_secs > 0.1 {
                // Fallback: velocidad global acumulada desde el inicio.
                // Evita el bug de "46.2 MB/s" fijo o "0 B/s" cuando el
                // enjambre completa archivos fuera del tick del reporter.
                global_bps
            } else {
                0.0
            };

            // Actualizar ventana deslizante
            self.last_snapshot_bytes.store(copied, Ordering::Relaxed);
            *last_time = Instant::now();

            rate
        };

        // Archivos/s
        let files_per_sec = if elapsed_secs > 0.0 {
            completed as f64 / elapsed_secs
        } else {
            0.0
        };

        // Porcentaje global
        let percent = if self.total_bytes > 0 {
            (copied as f64 / self.total_bytes as f64 * 100.0).min(100.0)
        } else {
            100.0
        };

        // ETA basada en velocidad incremental
        let eta_secs = if throughput_bytes_per_sec > 0.0 && copied < self.total_bytes {
            let remaining = self.total_bytes - copied;
            Some(remaining as f64 / throughput_bytes_per_sec)
        } else {
            None
        };

        // Archivo actual en proceso
        let (current_file, current_file_progress) = {
            let current = self.current_file.lock().unwrap();
            match &*current {
                Some((path, progress)) => (Some(path.clone()), *progress),
                None => (None, 0.0),
            }
        };

        CopyProgress {
            total_bytes: self.total_bytes,
            copied_bytes: copied,
            total_files: self.total_files,
            completed_files: completed,
            failed_files: failed,
            throughput_bytes_per_sec,
            files_per_sec,
            percent,
            elapsed_secs,
            eta_secs,
            current_file,
            current_file_progress,
        }
    }
}

/// Handle ligero cloneable para threads del motor.
/// Solo expone operaciones de escritura — el sink original lee los snapshots.
#[derive(Clone)]
pub struct TelemetryHandle {
    copied_bytes:    Arc<AtomicU64>,
    completed_files: Arc<AtomicUsize>,
    failed_files:    Arc<AtomicUsize>,
    current_file:    Arc<std::sync::Mutex<Option<(String, f64)>>>,
}

impl TelemetryHandle {
    #[inline]
    pub fn add_bytes(&self, bytes: u64) {
        self.copied_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn complete_file(&self) {
        self.completed_files.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn fail_file(&self) {
        self.failed_files.fetch_add(1, Ordering::Relaxed);
    }

    /// Actualiza el archivo actual en proceso y su progreso interno (0.0 - 1.0)
    pub fn set_current_file(&self, path: &std::path::Path, progress: f64) {
        let mut current = self.current_file.lock().unwrap();
        *current = Some((path.display().to_string(), progress.clamp(0.0, 1.0)));
    }

    /// Limpia el archivo actual cuando se completa
    pub fn clear_current_file(&self) {
        let mut current = self.current_file.lock().unwrap();
        *current = None;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Formatters
// ─────────────────────────────────────────────────────────────────────────────

impl CopyProgress {
    pub fn throughput_human(&self) -> String {
        format_bytes_per_sec(self.throughput_bytes_per_sec)
    }

    pub fn eta_human(&self) -> String {
        match self.eta_secs {
            None    => "—".into(),
            Some(s) => format_duration(s),
        }
    }
}

fn format_bytes_per_sec(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if bps >= GB      { format!("{:.2} GB/s", bps / GB) }
    else if bps >= MB { format!("{:.1} MB/s", bps / MB) }
    else if bps >= KB { format!("{:.0} KB/s", bps / KB) }
    else              { format!("{:.0} B/s",  bps)      }
}

fn format_duration(secs: f64) -> String {
    let s = secs as u64;
    if s < 60        { format!("{s}s") }
    else if s < 3600 { format!("{}m {}s", s / 60, s % 60) }
    else             { format!("{}h {}m", s / 3600, (s % 3600) / 60) }
}
