//! # dry_run
//!
//! Simulación de copia sin escribir ningún byte al disco.
//!
//! ## Qué hace
//!
//! Recorre el mismo árbol de archivos que el Orchestrator real, aplica las mismas
//! reglas de triage, verifica permisos de lectura/escritura y espacio disponible,
//! y produce un informe detallado sin tocar el destino.
//!
//! ## Cuándo usarlo
//!
//! - Antes de mover datos críticos (`--move --dry-run`)
//! - Para estimar tiempo en copias grandes
//! - En scripts CI/CD para validar que los paths son correctos
//! - Para detectar archivos sin permiso de lectura antes de una copia larga
//!
//! ## Salida
//!
//! `DryRunReport` contiene:
//! - Lista completa de acciones que se ejecutarían
//! - Archivos que se saltarían (checkpoint / ya existen)
//! - Problemas detectados (sin permiso, disco lleno, path demasiado largo)
//! - Estimación de bytes y archivos

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::config::EngineConfig;

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

/// Acción que se ejecutaría sobre un archivo.
#[derive(Debug, Clone)]
pub enum PlannedAction {
    /// El archivo se copiaría al destino.
    Copy {
        source: PathBuf,
        dest:   PathBuf,
        size:   u64,
    },
    /// El archivo se movería (copiar + borrar origen).
    Move {
        source: PathBuf,
        dest:   PathBuf,
        size:   u64,
    },
    /// El archivo ya existe en el destino con el mismo tamaño — se saltaría.
    Skip {
        source: PathBuf,
        dest:   PathBuf,
        reason: SkipReason,
    },
    /// El archivo existe en destino pero con tamaño diferente — se sobreescribiría.
    Overwrite {
        source: PathBuf,
        dest:   PathBuf,
        size:   u64,
        existing_size: u64,
    },
}

/// Razón por la que un archivo se saltaría.
#[derive(Debug, Clone)]
pub enum SkipReason {
    /// Marcado como completado en el checkpoint de una reanudación.
    Checkpoint,
    /// Ya existe en destino con el mismo tamaño.
    AlreadyExists,
}

/// Problema detectado durante el dry-run.
#[derive(Debug, Clone)]
pub struct DryRunProblem {
    pub path:    PathBuf,
    pub kind:    ProblemKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemKind {
    /// No se puede leer el archivo origen.
    NoReadPermission,
    /// No se puede escribir en el directorio destino.
    NoWritePermission,
    /// El path destino supera la longitud máxima del SO (260 en Windows sin LFN).
    PathTooLong,
    /// Espacio insuficiente en el disco destino.
    InsufficientSpace,
    /// El archivo origen no existe (fue borrado entre el scan y la verificación).
    SourceGone,
}

/// Informe completo de lo que haría una operación.
#[derive(Debug)]
pub struct DryRunReport {
    pub actions:        Vec<PlannedAction>,
    pub problems:       Vec<DryRunProblem>,
    pub total_files:    usize,
    pub total_bytes:    u64,
    pub skipped_files:  usize,
    pub problem_files:  usize,
    /// `true` si la operación real puede ejecutarse sin problemas conocidos.
    pub is_safe:        bool,
}

impl DryRunReport {
    /// Imprime el informe en stdout de forma legible.
    pub fn print(&self, verbose: bool) {
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Dry-run — simulación (sin cambios en disco)");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Archivos a procesar: {}", self.total_files);
        println!("  Datos a transferir:  {}", format_bytes(self.total_bytes));
        println!("  Archivos a saltar:   {}", self.skipped_files);

        if self.problem_files > 0 {
            println!("  ⚠  Problemas detectados: {}", self.problem_files);
        }

        if verbose || !self.problems.is_empty() {
            println!();
            if !self.problems.is_empty() {
                println!("  Problemas:");
                for p in &self.problems {
                    let kind_label = match p.kind {
                        ProblemKind::NoReadPermission  => "SIN LECTURA",
                        ProblemKind::NoWritePermission => "SIN ESCRITURA",
                        ProblemKind::PathTooLong       => "PATH LARGO",
                        ProblemKind::InsufficientSpace => "DISCO LLENO",
                        ProblemKind::SourceGone        => "ORIGEN DESAPARECIDO",
                    };
                    println!("    [{kind_label}] {} — {}", p.path.display(), p.message);
                }
            }

            if verbose {
                println!();
                println!("  Acciones planificadas:");
                for action in &self.actions {
                    match action {
                        PlannedAction::Copy { source, dest, size } => {
                            println!(
                                "    COPIAR  {} → {} ({})",
                                source.display(), dest.display(), format_bytes(*size)
                            );
                        }
                        PlannedAction::Move { source, dest, size } => {
                            println!(
                                "    MOVER   {} → {} ({})",
                                source.display(), dest.display(), format_bytes(*size)
                            );
                        }
                        PlannedAction::Skip { source, reason, .. } => {
                            let r = match reason {
                                SkipReason::Checkpoint    => "checkpoint",
                                SkipReason::AlreadyExists => "ya existe",
                            };
                            println!("    SALTAR  {} [{r}]", source.display());
                        }
                        PlannedAction::Overwrite { source, dest, size, existing_size } => {
                            println!(
                                "    SOBRE   {} → {} ({} → {})",
                                source.display(), dest.display(),
                                format_bytes(*existing_size), format_bytes(*size)
                            );
                        }
                    }
                }
            }
        }

        println!();
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        if self.is_safe {
            println!("  ✓ Sin problemas detectados — la operación puede ejecutarse");
        } else {
            println!("  ✗ HAY PROBLEMAS — revisa los errores antes de ejecutar");
        }
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// DryRunner
// ─────────────────────────────────────────────────────────────────────────────

/// Ejecuta el análisis dry-run.
pub struct DryRunner<'a> {
    config:    &'a EngineConfig,
    is_move:   bool,
    /// Paths relativos ya completados (del checkpoint).
    completed: std::collections::HashSet<PathBuf>,
}

impl<'a> DryRunner<'a> {
    pub fn new(
        config:    &'a EngineConfig,
        is_move:   bool,
        completed: std::collections::HashSet<PathBuf>,
    ) -> Self {
        Self { config, is_move, completed }
    }

    /// Ejecuta el análisis y produce el informe.
    pub fn run(&self, source_root: &Path, dest_root: &Path) -> DryRunReport {
        let mut actions       = Vec::new();
        let mut problems      = Vec::new();
        let mut total_bytes   = 0u64;
        let mut total_files   = 0usize;
        let mut skipped_files = 0usize;

        // Verificar que el destino es escribible
        if let Some(dest_parent) = dest_root.parent() {
            if dest_parent.exists() {
                if let Err(e) = check_write_permission(dest_root) {
                    problems.push(DryRunProblem {
                        path:    dest_root.to_path_buf(),
                        kind:    ProblemKind::NoWritePermission,
                        message: e,
                    });
                }
            }
        }

        // Espacio disponible en destino
        let available_space = get_available_space(dest_root);

        // Escanear origen
        for entry in WalkDir::new(source_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let source   = entry.path().to_path_buf();
            let size     = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let relative = match source.strip_prefix(source_root) {
                Ok(r)  => r.to_path_buf(),
                Err(_) => continue,
            };
            let dest = dest_root.join(&relative);

            // ── Archivos ya en checkpoint ─────────────────────────────────
            if self.completed.contains(&relative) {
                skipped_files += 1;
                actions.push(PlannedAction::Skip {
                    source: source.clone(),
                    dest:   dest.clone(),
                    reason: SkipReason::Checkpoint,
                });
                continue;
            }

            // ── Verificar permiso de lectura ──────────────────────────────
            if !source.exists() {
                problems.push(DryRunProblem {
                    path:    source.clone(),
                    kind:    ProblemKind::SourceGone,
                    message: "el archivo desapareció durante el escaneo".into(),
                });
                continue;
            }

            if let Err(msg) = check_read_permission(&source) {
                problems.push(DryRunProblem {
                    path:    source.clone(),
                    kind:    ProblemKind::NoReadPermission,
                    message: msg,
                });
                continue;
            }

            // ── Path demasiado largo (Windows MAX_PATH = 260) ─────────────
            #[cfg(windows)]
            if dest.to_string_lossy().len() > 260 {
                problems.push(DryRunProblem {
                    path:    dest.clone(),
                    kind:    ProblemKind::PathTooLong,
                    message: format!("path destino tiene {} chars (máx 260 en Windows sin LFN)", dest.to_string_lossy().len()),
                });
            }

            // ── Ya existe en destino ──────────────────────────────────────
            if dest.exists() {
                let dest_size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                if dest_size == size {
                    skipped_files += 1;
                    actions.push(PlannedAction::Skip {
                        source: source.clone(),
                        dest:   dest.clone(),
                        reason: SkipReason::AlreadyExists,
                    });
                    continue;
                } else {
                    // Tamaño diferente → sobreescribir
                    total_files += 1;
                    total_bytes += size;
                    actions.push(PlannedAction::Overwrite {
                        source, dest, size, existing_size: dest_size,
                    });
                    continue;
                }
            }

            // ── Acción normal ─────────────────────────────────────────────
            total_files += 1;
            total_bytes += size;

            if self.is_move {
                actions.push(PlannedAction::Move { source, dest, size });
            } else {
                actions.push(PlannedAction::Copy { source, dest, size });
            }
        }

        // ── Verificar espacio disponible ──────────────────────────────────
        if let Some(avail) = available_space {
            if total_bytes > avail {
                problems.push(DryRunProblem {
                    path:    dest_root.to_path_buf(),
                    kind:    ProblemKind::InsufficientSpace,
                    message: format!(
                        "necesario: {}, disponible: {}",
                        format_bytes(total_bytes),
                        format_bytes(avail)
                    ),
                });
            }
        }

        let problem_files = problems.len();
        let is_safe       = problems.is_empty();

        DryRunReport {
            actions,
            problems,
            total_files,
            total_bytes,
            skipped_files,
            problem_files,
            is_safe,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers de plataforma
// ─────────────────────────────────────────────────────────────────────────────

fn check_read_permission(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn check_write_permission(path: &Path) -> Result<(), String> {
    // Intentar crear un archivo temporal para verificar escritura
    let test_path = if path.is_dir() {
        path.join(".filecopier_write_test")
    } else {
        path.parent()
            .unwrap_or(Path::new("."))
            .join(".filecopier_write_test")
    };

    let result = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&test_path)
        .map(|_| ())
        .map_err(|e| e.to_string());

    let _ = std::fs::remove_file(&test_path);
    result
}

fn get_available_space(path: &Path) -> Option<u64> {
    let check_path = if path.exists() { path } else {
        path.parent().unwrap_or(Path::new("."))
    };

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = std::ffi::OsStr::new(check_path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free_bytes: u64 = 0;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_bytes,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 0 { Some(free_bytes) } else { None }
    }

    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        if let Ok(cpath) = CString::new(check_path.as_os_str().as_bytes()) {
            let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
            let ret = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
            if ret == 0 {
                return Some(stat.f_bavail * stat.f_bsize as u64);
            }
        }
        None
    }

    #[cfg(not(any(windows, unix)))]
    None
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB      { format!("{:.2} GB", bytes as f64 / GB as f64) }
    else if bytes >= MB { format!("{:.1} MB", bytes as f64 / MB as f64) }
    else if bytes >= KB { format!("{:.0} KB", bytes as f64 / KB as f64) }
    else                { format!("{} B",  bytes) }
}
