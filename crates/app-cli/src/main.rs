//! # filecopier CLI
//!
//! Interfaz de línea de comandos para FileCopier-Rust.
//!
//! ## Modos de operación
//!
//! ```
//! # Copiar (default)
//! filecopier C:\src D:\dst
//!
//! # Mover (copiar + borrar origen si exitoso)
//! filecopier --move C:\src D:\dst
//!
//! # Simular sin escribir
//! filecopier --dry-run C:\src D:\dst
//! filecopier --dry-run --move C:\src D:\dst   # muestra qué se borraría
//!
//! # Daemon: interceptar Ctrl+C / Ctrl+X del Explorer
//! filecopier --watch-clipboard --dest D:\dst
//! ```

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use lib_core::{
    checkpoint::{FlowControl, ResumePolicy},
    config::{EngineConfig, OperationMode},
    engine::Orchestrator,
    hash::Algorithm,
    telemetry::CopyProgress,
};
use lib_os::traits::DriveKind;

// ─────────────────────────────────────────────────────────────────────────────
// CLI args
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name       = "filecopier",
    version,
    about      = "Motor de copia/movimiento de alto rendimiento",
    after_help = "Ejemplos:\n\
                  filecopier C:\\src C:\\dst\n\
                  filecopier --move C:\\src C:\\dst\n\
                  filecopier --dry-run --move C:\\src C:\\dst\n\
                  filecopier --watch-clipboard --dest D:\\Backup"
)]
struct Cli {
    /// Ruta de origen (archivo o directorio). Omitir con --watch-clipboard.
    #[arg(value_name = "ORIGEN", required_unless_present = "watch_clipboard")]
    source: Option<PathBuf>,

    /// Ruta de destino. Omitir con --watch-clipboard (usar --dest en su lugar).
    #[arg(value_name = "DESTINO", required_unless_present = "watch_clipboard")]
    dest: Option<PathBuf>,

    // ── Operación ──────────────────────────────────────────────────────────

    /// Mover archivos: copiar → verificar → borrar origen.
    /// El origen nunca se borra si la copia falla.
    #[arg(long, short = 'm')]
    r#move: bool,

    /// Simular la operación sin escribir nada al disco.
    /// Muestra qué se copiaría/movería, problemas de permisos y espacio.
    #[arg(long)]
    dry_run: bool,

    // ── Daemon de portapapeles ─────────────────────────────────────────────

    /// [Solo Windows] Monitorear el portapapeles del sistema.
    /// Detecta Ctrl+C (copiar) y Ctrl+X (mover) sobre archivos en Explorer
    /// y ejecuta la operación automáticamente usando --dest como destino.
    #[arg(long)]
    watch_clipboard: bool,

    /// Directorio destino para el modo --watch-clipboard.
    #[arg(long, value_name = "DIR")]
    dest_dir: Option<PathBuf>,

    /// Intervalo de polling del portapapeles en milisegundos. Default: 500
    #[arg(long, default_value_t = 500, value_name = "MS")]
    clipboard_interval: u64,

    // ── Verificación ────────────────────────────────────────────────────────

    /// Habilita verificación de integridad post-copia (blake3 por default)
    #[arg(long)]
    verify: bool,

    /// Algoritmo de hashing: blake3, xxhash, sha2
    #[arg(long, default_value = "blake3", value_name = "ALGO")]
    hasher: String,

    // ── Rendimiento ─────────────────────────────────────────────────────────

    /// Tamaño de bloque en MB para archivos grandes
    #[arg(long, default_value_t = 4, value_name = "MB")]
    block_size: u64,

    /// Umbral en MB: archivos >= umbral usan motor de bloques
    #[arg(long, default_value_t = 16, value_name = "MB")]
    threshold: u64,

    /// Máximo de bloques en vuelo simultáneamente
    #[arg(long, default_value_t = 8, value_name = "N")]
    channel_cap: usize,

    /// Máximo de tareas concurrentes para archivos pequeños
    #[arg(long, default_value_t = 128, value_name = "N")]
    swarm_limit: usize,

    // ── Resiliencia ──────────────────────────────────────────────────────────

    /// Reanudar desde checkpoint existente
    #[arg(long, short = 'r')]
    resume: bool,

    /// Política de validación al reanudar (trust | size | hash)
    #[arg(long, default_value = "size", value_name = "POLICY", value_parser = parse_resume_policy)]
    resume_policy: ResumePolicy,

    #[arg(long, hide = true)]
    no_partial: bool,

    #[arg(long)]
    no_detect: bool,

    /// Nivel de verbosidad (-v info, -vv debug, -vvv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Mostrar solo errores y resumen final
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Con --dry-run: mostrar lista detallada de todas las acciones
    #[arg(long)]
    verbose_dry_run: bool,
}

fn parse_resume_policy(s: &str) -> std::result::Result<ResumePolicy, String> {
    match s.to_lowercase().as_str() {
        "trust" => Ok(ResumePolicy::TrustCheckpoint),
        "size"  => Ok(ResumePolicy::VerifySize),
        "hash"  => Ok(ResumePolicy::VerifyHash),
        other   => Err(format!("Política desconocida: '{}'. Opciones: trust, size, hash", other)),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbosity, cli.quiet);

    // Modo daemon de portapapeles
    if cli.watch_clipboard {
        if let Err(e) = run_clipboard_daemon(&cli) {
            eprintln!("\n❌ Error en daemon de portapapeles: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run(cli) {
        eprintln!("\n❌ Error fatal: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> lib_core::error::Result<()> {
    let source = cli.source.as_ref().expect("ORIGEN requerido");
    let dest   = cli.dest.as_ref().expect("DESTINO requerido");

    if !cli.dry_run && !source.exists() {
        eprintln!("❌ El origen no existe: {}", source.display());
        std::process::exit(2);
    }

    let algorithm = Algorithm::from_str(&cli.hasher).unwrap_or_else(|e| {
        eprintln!("⚠  {e}. Usando blake3 por defecto.");
        Algorithm::Blake3
    });

    let mut config = EngineConfig {
        triage_threshold_bytes: cli.threshold * 1024 * 1024,
        block_size_bytes:        cli.block_size as usize * 1024 * 1024,
        channel_capacity:        cli.channel_cap,
        swarm_concurrency:       cli.swarm_limit,
        verify:                  cli.verify,
        hash_algorithm:          algorithm,
        operation_mode:          if cli.r#move { OperationMode::Move } else { OperationMode::Copy },
        dry_run:                 cli.dry_run,
        resume:                  cli.resume,
        resume_policy:           cli.resume_policy,
        use_partial_files:       !cli.no_partial,
        bandwidth_limit_bytes_per_sec: 0,
        bandwidth_burst_bytes:   1 * 1024 * 1024,
    };

    if !cli.no_detect {
        let adapter  = lib_os::platform_adapter();
        let strategy = adapter.compute_strategy(source, dest);

        if cli.swarm_limit == 128 {
            config.swarm_concurrency = if cli.verify {
                strategy.recommended_swarm_concurrency_verify
            } else {
                strategy.recommended_swarm_concurrency
            };
        }
        if cli.block_size == 4 {
            config.block_size_bytes = strategy.recommended_block_size;
        }

        if !cli.quiet {
            print_hardware_info(&strategy, cli.verify);
        }
    }

    config.validate()?;

    if !cli.quiet {
        print_config_banner(&config, source, dest);
    }

    // Dry-run: no necesita señales ni progreso
    if cli.dry_run {
        let flow   = FlowControl::new();
        let os_ops = Arc::new(lib_core::os_ops::NoOpOsOps);
        let orch   = Orchestrator::new(config, flow, os_ops);
        let result = orch.run(source, dest, None)?;
        if let Some(report) = result.dry_run_report {
            report.print(cli.verbose_dry_run || cli.verbosity > 0);
        }
        return Ok(());
    }

    let flow         = FlowControl::new();
    let signal_count = Arc::new(AtomicU32::new(0));
    install_ctrlc_handler(flow.clone(), Arc::clone(&signal_count));

    let start = Instant::now();
    let quiet = cli.quiet;

    let on_progress: lib_core::engine::orchestrator::ProgressCallback =
        Box::new(move |p: CopyProgress| { if !quiet { print_progress(&p); } });

    let os_ops: Arc<dyn lib_core::os_ops::OsOps> = if !cli.no_detect {
        lib_os::platform_adapter_os_ops().into()
    } else {
        Arc::new(lib_core::os_ops::NoOpOsOps)
    };

    let result = Orchestrator::new(config, flow, os_ops)
        .run(source, dest, Some(on_progress))?;

    if !cli.quiet { println!(); }
    print_summary(&result, start.elapsed());

    if result.failed_files > 0 { std::process::exit(3); }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon de portapapeles
// ─────────────────────────────────────────────────────────────────────────────

fn run_clipboard_daemon(cli: &Cli) -> lib_core::error::Result<()> {
    #[cfg(not(windows))]
    {
        eprintln!("⚠  --watch-clipboard solo está disponible en Windows.");
        eprintln!("   En Linux/macOS, usar la integración con el gestor de archivos nativo.");
        std::process::exit(1);
    }

    #[cfg(windows)]
    {
        use lib_os::windows::clipboard::{ClipboardOperation, ClipboardWatcher};

        let dest_dir = match &cli.dest_dir {
            Some(d) => d.clone(),
            None => {
                eprintln!("❌ --watch-clipboard requiere --dest-dir <DIR>");
                std::process::exit(1);
            }
        };

        if !dest_dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dest_dir) {
                eprintln!("❌ No se pudo crear el directorio destino '{}': {}", dest_dir.display(), e);
                std::process::exit(1);
            }
        }

        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  FileCopier-Rust — Daemon de portapapeles");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  Destino:   {}", dest_dir.display());
        println!("  Intervalo: {} ms", cli.clipboard_interval);
        println!();
        println!("  Esperando Ctrl+C o Ctrl+X sobre archivos en Explorer...");
        println!("  (Presiona Ctrl+C en esta ventana para detener el daemon)");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        let verify    = cli.verify;
        let block_size = cli.block_size;
        let threshold  = cli.threshold;
        let is_quiet   = cli.quiet;

        let mut watcher = ClipboardWatcher::new();

        watcher.watch(cli.clipboard_interval, move |event| {
            let operation_str = match event.operation {
                ClipboardOperation::Copy => "COPIAR",
                ClipboardOperation::Move => "MOVER",
            };

            println!();
            println!("  ▶ {} {} archivo(s) → {}",
                operation_str,
                event.paths.len(),
                dest_dir.display()
            );

            for source_path in &event.paths {
                println!("    {}", source_path.display());

                if !source_path.exists() {
                    println!("    ⚠  No existe, saltando");
                    continue;
                }

                let config = EngineConfig {
                    triage_threshold_bytes: threshold * 1024 * 1024,
                    block_size_bytes:        block_size as usize * 1024 * 1024,
                    channel_capacity:        8,
                    swarm_concurrency:       128,
                    verify,
                    operation_mode: match event.operation {
                        ClipboardOperation::Copy => OperationMode::Copy,
                        ClipboardOperation::Move => OperationMode::Move,
                    },
                    dry_run: false,
                    ..EngineConfig::default()
                };

                let flow   = FlowControl::new();
                let os_ops: Arc<dyn lib_core::os_ops::OsOps> =
                    lib_os::platform_adapter_os_ops().into();

                let start = Instant::now();
                match Orchestrator::new(config, flow, os_ops)
                    .run(source_path, &dest_dir, None)
                {
                    Ok(result) => {
                        println!(
                            "    ✓ {} archivo(s), {:.1} MB en {:.1}s",
                            result.completed_files,
                            result.copied_bytes as f64 / 1024.0 / 1024.0,
                            start.elapsed().as_secs_f64(),
                        );
                        if result.move_delete_failed > 0 {
                            println!(
                                "    ⚠  {} origen(es) no se pudieron borrar",
                                result.move_delete_failed
                            );
                        }
                    }
                    Err(e) => {
                        println!("    ✗ Error: {}", e);
                    }
                }
            }

            true // continuar el loop
        });
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Señales
// ─────────────────────────────────────────────────────────────────────────────

fn install_ctrlc_handler(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    #[cfg(windows)]  install_windows(flow, signal_count);
    #[cfg(unix)]     install_unix(flow, signal_count);
}

#[cfg(windows)]
fn install_windows(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    WINDOWS_FLOW.set(flow).ok();
    WINDOWS_COUNT.set(signal_count).ok();
    unsafe { SetConsoleCtrlHandler(Some(win_handler), 1); }
}
#[cfg(windows)]
unsafe extern "system" fn win_handler(t: u32) -> i32 {
    if matches!(t, 0 | 1 | 2) { handle_win(); 1 } else { 0 }
}
#[cfg(windows)]
fn handle_win() {
    if let Some(c) = WINDOWS_COUNT.get() {
        let prev = c.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = WINDOWS_FLOW.get() {
            if prev == 0 { eprintln!("\n⏸  Pausa. Ctrl+C de nuevo para cancelar."); f.pause(); }
            else { eprintln!("\n⚠  Cancelando..."); f.cancel(); }
        }
    }
}
#[cfg(windows)] static WINDOWS_FLOW:  std::sync::OnceLock<FlowControl>    = std::sync::OnceLock::new();
#[cfg(windows)] static WINDOWS_COUNT: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();

#[cfg(unix)]
fn install_unix(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    UNIX_FLOW.set(flow).ok();
    UNIX_COUNT.set(signal_count).ok();
    unsafe { libc::signal(libc::SIGINT, unix_handler as libc::sighandler_t); }
}
#[cfg(unix)]
extern "C" fn unix_handler(_: libc::c_int) {
    if let Some(c) = UNIX_COUNT.get() {
        let prev = c.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = UNIX_FLOW.get() {
            if prev == 0 { eprintln!("\n⏸  Pausa."); f.pause(); }
            else { eprintln!("\n⚠  Cancelando..."); f.cancel(); }
        }
    }
}
#[cfg(unix)] static UNIX_FLOW:  std::sync::OnceLock<FlowControl>    = std::sync::OnceLock::new();
#[cfg(unix)] static UNIX_COUNT: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();

// ─────────────────────────────────────────────────────────────────────────────
// UI helpers
// ─────────────────────────────────────────────────────────────────────────────

fn print_config_banner(config: &EngineConfig, source: &Path, dest: &Path) {
    let mode_label = match config.operation_mode {
        OperationMode::Copy => "copiar",
        OperationMode::Move => "MOVER (borrar origen tras copia exitosa)",
    };
    let dry_label = if config.dry_run { " [DRY-RUN — sin cambios en disco]" } else { "" };

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  FileCopier-Rust v{}{}", env!("CARGO_PKG_VERSION"), dry_label);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Origen:       {}", source.display());
    println!("  Destino:      {}", dest.display());
    println!("  Operación:    {}", mode_label);
    println!("  Bloque:       {} MB", config.block_size_bytes / 1024 / 1024);
    println!("  Enjambre:     {} tareas", config.swarm_concurrency);
    println!(
        "  Verificación: {}",
        if config.verify { format!("✓ ({})", config.hash_algorithm) }
        else             { "✗  (usa --verify para activar)".into() }
    );
    if config.resume {
        let pol = match config.resume_policy {
            ResumePolicy::TrustCheckpoint => "trust",
            ResumePolicy::VerifySize      => "size",
            ResumePolicy::VerifyHash      => "hash",
        };
        println!("  Checkpoint:   reanudar [política: {}]", pol);
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
}

fn print_hardware_info(strategy: &lib_os::traits::CopyStrategy, verify: bool) {
    let label = |k: DriveKind| match k {
        DriveKind::Ssd     => "SSD/NVMe",
        DriveKind::Hdd     => "HDD",
        DriveKind::Network => "Red",
        DriveKind::Unknown => "Desconocido",
    };
    let c = if verify { strategy.recommended_swarm_concurrency_verify }
            else       { strategy.recommended_swarm_concurrency };
    println!(
        "  Hardware: {} → {}  |  enjambre={} bloque={}MB",
        label(strategy.source_kind), label(strategy.dest_kind),
        c, strategy.recommended_block_size / 1024 / 1024,
    );
}

fn print_progress(p: &CopyProgress) {
    let w = 30usize;
    let f = ((p.percent / 100.0) * w as f64) as usize;
    let bar = "█".repeat(f) + &"░".repeat(w - f);

    if let Some(ref name) = p.current_file {
        let n = std::path::Path::new(name)
            .file_name().and_then(|x| x.to_str()).unwrap_or(name);
        let ib = 10usize;
        let fi = (p.current_file_progress * ib as f64) as usize;
        let ibar = "█".repeat(fi.min(ib)) + &"░".repeat(ib.saturating_sub(fi));
        print!("\r  [{bar}] {:.1}%  {}  {}/{}  ETA:{}  | {}: [{}]{:.0}%",
            p.percent, p.throughput_human(), p.completed_files, p.total_files,
            p.eta_human(), n, ibar, p.current_file_progress * 100.0);
    } else {
        print!("\r  [{bar}] {:.1}%  {}  {}/{}  ETA:{}    ",
            p.percent, p.throughput_human(), p.completed_files, p.total_files, p.eta_human());
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn print_summary(result: &lib_core::engine::orchestrator::CopyResult, elapsed: std::time::Duration) {
    let mb  = result.copied_bytes as f64 / 1024.0 / 1024.0;
    let spd = if elapsed.as_secs_f64() > 0.0 { mb / elapsed.as_secs_f64() } else { 0.0 };

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Resumen");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Completados:  {} archivos", result.completed_files);
    if result.failed_files         > 0 { println!("  ⚠  Fallidos:           {}", result.failed_files); }
    if result.revalidated_files    > 0 { println!("  ↺  Revalidados:        {}", result.revalidated_files); }
    if result.moved_files          > 0 { println!("  ✂  Movidos:            {}", result.moved_files); }
    if result.move_delete_failed   > 0 { println!("  ⚠  Origen no borrado:  {}", result.move_delete_failed); }
    println!("  Datos:        {:.1} MB", mb);
    println!("  Tiempo:       {:.2}s", elapsed.as_secs_f64());
    println!("  Velocidad:    {:.1} MB/s", spd);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    if result.failed_files == 0 { println!("  ✓ Completado exitosamente"); }
    else { println!("  ⚠  Completado con {} error(es)", result.failed_files); }
}

fn init_logging(verbosity: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let level = if quiet { "error" } else {
        match verbosity { 0 => "warn", 1 => "info", 2 => "debug", _ => "trace" }
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level)))
        .with_target(false).with_thread_ids(false).compact().init();
}
