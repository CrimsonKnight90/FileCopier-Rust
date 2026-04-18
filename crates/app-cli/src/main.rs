//! # filecopier CLI

use std::path::PathBuf;
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
    after_help  = "Ejemplos:\n\
                  filecopier C:\\src C:\\dst\n\
                  filecopier --move C:\\src C:\\dst\n\
                  filecopier --dry-run --move C:\\src C:\\dst\n\n\
                  # Daemon de portapapeles (intercepta Ctrl+C/Ctrl+X/Ctrl+V):\n\
                  filecopier --watch-clipboard\n\
                  filecopier --watch-clipboard --dest-dir D:\\Backup\n\
                  filecopier --watch-clipboard --verify"
)]
struct Cli {
    /// Ruta de origen. Omitir con --watch-clipboard.
    #[arg(value_name = "ORIGEN", required_unless_present = "watch_clipboard")]
    source: Option<PathBuf>,

    /// Ruta de destino. Omitir con --watch-clipboard (usar --dest-dir).
    #[arg(value_name = "DESTINO", required_unless_present = "watch_clipboard")]
    dest: Option<PathBuf>,

    // ── Operación ──────────────────────────────────────────────────────────

    /// Mover: copiar → verificar → borrar origen y carpetas vacías.
    #[arg(long, short = 'm')]
    r#move: bool,

    /// Simular sin escribir nada al disco.
    #[arg(long)]
    dry_run: bool,

    // ── Daemon de portapapeles ─────────────────────────────────────────────
    //
    // NUEVO: usa WM_CLIPBOARDUPDATE (event-driven, 0% CPU en idle).
    // Flujo:
    //   1. Ctrl+C / Ctrl+X  →  FileCopier guarda paths en cola
    //   2. Ctrl+V en Explorer  →  FileCopier detecta el pegado,
    //      resuelve la carpeta destino (IShellBrowser / UIAutomation)
    //      y ejecuta la operación

    /// [Windows] Daemon de portapapeles event-driven.
    /// Intercepta Ctrl+C, Ctrl+X y Ctrl+V sobre archivos en Explorer.
    /// No requiere especificar destino — lo detecta automáticamente.
    #[arg(long)]
    watch_clipboard: bool,

    /// Destino fijo para el daemon. Si se omite, se detecta la carpeta
    /// activa en Explorer al hacer Ctrl+V. Si no se puede detectar,
    /// muestra un diálogo de selección.
    #[arg(long, value_name = "DIR")]
    dest_dir: Option<PathBuf>,

    /// Con --watch-clipboard: mostrar diálogo si no se detecta destino.
    /// Default: true. Usar --no-fallback-dialog para deshabilitar.
    #[arg(long, default_value_t = true)]
    fallback_dialog: bool,

    // ── Verificación ────────────────────────────────────────────────────────

    #[arg(long)]
    verify: bool,

    #[arg(long, default_value = "blake3", value_name = "ALGO")]
    hasher: String,

    // ── Rendimiento ─────────────────────────────────────────────────────────

    #[arg(long, default_value_t = 4, value_name = "MB")]
    block_size: u64,

    #[arg(long, default_value_t = 16, value_name = "MB")]
    threshold: u64,

    #[arg(long, default_value_t = 8, value_name = "N")]
    channel_cap: usize,

    #[arg(long, default_value_t = 128, value_name = "N")]
    swarm_limit: usize,

    // ── Resiliencia ──────────────────────────────────────────────────────────

    #[arg(long, short = 'r')]
    resume: bool,

    #[arg(long, default_value = "size", value_name = "POLICY",
          value_parser = parse_resume_policy)]
    resume_policy: ResumePolicy,

    #[arg(long, hide = true)]
    no_partial: bool,

    #[arg(long)]
    no_detect: bool,

    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbosity: u8,

    #[arg(long, short = 'q')]
    quiet: bool,

    #[arg(long)]
    verbose_dry_run: bool,
}

fn parse_resume_policy(s: &str) -> std::result::Result<ResumePolicy, String> {
    match s.to_lowercase().as_str() {
        "trust" => Ok(ResumePolicy::TrustCheckpoint),
        "size"  => Ok(ResumePolicy::VerifySize),
        "hash"  => Ok(ResumePolicy::VerifyHash),
        other   => Err(format!(
            "Política desconocida: '{}'. Opciones: trust, size, hash", other
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbosity, cli.quiet);

    if cli.watch_clipboard {
        if let Err(e) = run_clipboard_daemon(&cli) {
            eprintln!("\n❌ Error en daemon: {e}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = run(cli) {
        eprintln!("\n❌ Error fatal: {e}");
        std::process::exit(1);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Operación normal
// ─────────────────────────────────────────────────────────────────────────────

fn run(cli: Cli) -> lib_core::error::Result<()> {
    let source = cli.source.as_ref().expect("ORIGEN requerido");
    let dest   = cli.dest.as_ref().expect("DESTINO requerido");

    if !cli.dry_run && !source.exists() {
        eprintln!("❌ El origen no existe: {}", source.display());
        std::process::exit(2);
    }

    let mut config = build_config(&cli);
    apply_hardware_detection(&mut config, source, dest, &cli);
    config.validate()?;

    if !cli.quiet {
        print_config_banner(&config, source, dest);
    }

    if cli.dry_run {
        let orch   = make_orchestrator(config, FlowControl::new(), cli.no_detect);
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

    let result = make_orchestrator(config, flow, cli.no_detect)
        .run(source, dest, Some(on_progress))?;

    if !cli.quiet { println!(); }
    print_summary(&result, start.elapsed());
    if result.failed_files > 0 { std::process::exit(3); }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Daemon de portapapeles (WM_CLIPBOARDUPDATE)
// ─────────────────────────────────────────────────────────────────────────────

fn run_clipboard_daemon(cli: &Cli) -> lib_core::error::Result<()> {
    #[cfg(not(windows))]
    {
        eprintln!("⚠  --watch-clipboard solo está disponible en Windows.");
        std::process::exit(1);
    }

    #[cfg(windows)]
    {
        use lib_os::windows::clipboard_daemon::{DaemonConfig, run_daemon};

        let config = DaemonConfig {
            fixed_dest:      cli.dest_dir.clone(),
            fallback_dialog: cli.fallback_dialog,
            verify:          cli.verify,
            block_size_mb:   cli.block_size,
            threshold_mb:    cli.threshold,
            channel_cap:     cli.channel_cap,
        };

        // Validar el destino fijo si se proporcionó
        if let Some(ref d) = config.fixed_dest {
            if !d.exists() {
                std::fs::create_dir_all(d).map_err(|e| {
                    lib_core::error::CoreError::io(d, e)
                })?;
            }
        }

        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("  FileCopier-Rust — Daemon de portapapeles");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        match &config.fixed_dest {
            Some(d) => println!("  Destino fijo:   {}", d.display()),
            None    => println!("  Destino:        [detectado automáticamente al pegar]"),
        }
        println!("  Verificación:   {}", if config.verify { "✓" } else { "✗" });
        println!("  Diálogo backup: {}", if config.fallback_dialog { "✓" } else { "✗" });
        println!();
        println!("  ┌─ Cómo usar ──────────────────────────────────────┐");
        println!("  │  1. Selecciona archivos en Explorer               │");
        println!("  │  2. Ctrl+C (copiar) o Ctrl+X (mover)             │");
        println!("  │     FileCopier guarda los archivos en cola        │");
        println!("  │  3. Ve a la carpeta destino en Explorer           │");
        println!("  │  4. Ctrl+V (pegar)                                │");
        println!("  │     FileCopier detecta el destino y opera         │");
        println!("  └──────────────────────────────────────────────────┘");
        println!();
        println!("  Presiona Ctrl+C en esta ventana para detener.");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        run_daemon(config)
            .map_err(|e| lib_core::error::CoreError::InvalidConfig {
                message: e.to_string(),
            })?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn build_config(cli: &Cli) -> EngineConfig {
    let algorithm = Algorithm::from_str(&cli.hasher).unwrap_or(Algorithm::Blake3);
    EngineConfig {
        triage_threshold_bytes: cli.threshold  * 1024 * 1024,
        block_size_bytes:       cli.block_size as usize * 1024 * 1024,
        channel_capacity:       cli.channel_cap,
        swarm_concurrency:      cli.swarm_limit,
        verify:                 cli.verify,
        hash_algorithm:         algorithm,
        operation_mode:         if cli.r#move { OperationMode::Move } else { OperationMode::Copy },
        dry_run:                cli.dry_run,
        resume:                 cli.resume,
        resume_policy:          cli.resume_policy,
        use_partial_files:      !cli.no_partial,
        bandwidth_limit_bytes_per_sec: 0,
        bandwidth_burst_bytes:  1 * 1024 * 1024,
    }
}

fn apply_hardware_detection(
    config:    &mut EngineConfig,
    source:    &std::path::Path,
    dest:      &std::path::Path,
    cli:       &Cli,
) {
    if cli.no_detect { return; }
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
        let lbl = |k: DriveKind| match k {
            DriveKind::Ssd     => "SSD/NVMe",
            DriveKind::Hdd     => "HDD",
            DriveKind::Network => "Red",
            DriveKind::Unknown => "?",
        };
        let c = if cli.verify {
            strategy.recommended_swarm_concurrency_verify
        } else {
            strategy.recommended_swarm_concurrency
        };
        println!(
            "  Hardware: {} → {}  |  enjambre={} bloque={}MB",
            lbl(strategy.source_kind), lbl(strategy.dest_kind),
            c, strategy.recommended_block_size / 1024 / 1024,
        );
    }
}

fn make_orchestrator(
    config:    EngineConfig,
    flow:      FlowControl,
    no_detect: bool,
) -> Orchestrator {
    let os_ops: Arc<dyn lib_core::os_ops::OsOps> = if !no_detect {
        lib_os::platform_adapter_os_ops().into()
    } else {
        Arc::new(lib_core::os_ops::NoOpOsOps)
    };
    Orchestrator::new(config, flow, os_ops)
}

// ─────────────────────────────────────────────────────────────────────────────
// Señales
// ─────────────────────────────────────────────────────────────────────────────

fn install_ctrlc_handler(flow: FlowControl, sc: Arc<AtomicU32>) {
    #[cfg(windows)]  install_windows(flow, sc);
    #[cfg(unix)]     install_unix(flow, sc);
}

#[cfg(windows)]
fn install_windows(flow: FlowControl, sc: Arc<AtomicU32>) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    WINDOWS_FLOW.set(flow).ok();
    WINDOWS_COUNT.set(sc).ok();
    unsafe { SetConsoleCtrlHandler(Some(win_handler), 1); }
}
#[cfg(windows)]
unsafe extern "system" fn win_handler(t: u32) -> i32 {
    if matches!(t, 0 | 1 | 2) { handle_win(); 1 } else { 0 }
}
#[cfg(windows)]
fn handle_win() {
    if let Some(c) = WINDOWS_COUNT.get() {
        let p = c.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = WINDOWS_FLOW.get() {
            if p == 0 { eprintln!("\n⏸  Pausa. Ctrl+C de nuevo para cancelar."); f.pause(); }
            else { eprintln!("\n⚠  Cancelando..."); f.cancel(); }
        }
    }
}
#[cfg(windows)] static WINDOWS_FLOW:  std::sync::OnceLock<FlowControl>    = std::sync::OnceLock::new();
#[cfg(windows)] static WINDOWS_COUNT: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();

#[cfg(unix)]
fn install_unix(flow: FlowControl, sc: Arc<AtomicU32>) {
    UNIX_FLOW.set(flow).ok();
    UNIX_COUNT.set(sc).ok();
    unsafe { libc::signal(libc::SIGINT, unix_handler as libc::sighandler_t); }
}
#[cfg(unix)]
extern "C" fn unix_handler(_: libc::c_int) {
    if let Some(c) = UNIX_COUNT.get() {
        let p = c.fetch_add(1, Ordering::SeqCst);
        if let Some(f) = UNIX_FLOW.get() {
            if p == 0 { eprintln!("\n⏸  Pausa."); f.pause(); }
            else { eprintln!("\n⚠  Cancelando..."); f.cancel(); }
        }
    }
}
#[cfg(unix)] static UNIX_FLOW:  std::sync::OnceLock<FlowControl>    = std::sync::OnceLock::new();
#[cfg(unix)] static UNIX_COUNT: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();

// ─────────────────────────────────────────────────────────────────────────────
// UI
// ─────────────────────────────────────────────────────────────────────────────

fn print_config_banner(config: &EngineConfig, source: &std::path::Path, dest: &std::path::Path) {
    let mode = match config.operation_mode {
        OperationMode::Copy => "copiar",
        OperationMode::Move => "MOVER (borra origen tras copia exitosa)",
    };
    let dry = if config.dry_run { " [DRY-RUN]" } else { "" };
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  FileCopier-Rust v{}{}", env!("CARGO_PKG_VERSION"), dry);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Origen:    {}", source.display());
    println!("  Destino:   {}", dest.display());
    println!("  Operación: {}", mode);
    println!("  Bloque:    {} MB", config.block_size_bytes / 1024 / 1024);
    println!("  Enjambre:  {} tareas", config.swarm_concurrency);
    println!(
        "  Verif.:    {}",
        if config.verify { format!("✓ ({})", config.hash_algorithm) }
        else             { "✗".into() }
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
}

fn print_progress(p: &CopyProgress) {
    let w = 30usize;
    let f = ((p.percent / 100.0) * w as f64) as usize;
    let bar = "█".repeat(f) + &"░".repeat(w - f);
    if let Some(ref name) = p.current_file {
        let n = std::path::Path::new(name).file_name()
            .and_then(|x| x.to_str()).unwrap_or(name);
        let fi = (p.current_file_progress * 10.0) as usize;
        let ib = "█".repeat(fi.min(10)) + &"░".repeat(10usize.saturating_sub(fi));
        print!("\r  [{bar}] {:.1}%  {}  {}/{}  ETA:{}  | {}: [{}]{:.0}%",
            p.percent, p.throughput_human(), p.completed_files, p.total_files,
            p.eta_human(), n, ib, p.current_file_progress * 100.0);
    } else {
        print!("\r  [{bar}] {:.1}%  {}  {}/{}  ETA:{}    ",
            p.percent, p.throughput_human(), p.completed_files, p.total_files,
            p.eta_human());
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn print_summary(r: &lib_core::engine::orchestrator::CopyResult, elapsed: std::time::Duration) {
    let mb  = r.copied_bytes as f64 / 1024.0 / 1024.0;
    let spd = if elapsed.as_secs_f64() > 0.0 { mb / elapsed.as_secs_f64() } else { 0.0 };
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Completados:  {} archivos", r.completed_files);
    if r.failed_files       > 0 { println!("  ⚠  Fallidos:           {}", r.failed_files); }
    if r.revalidated_files  > 0 { println!("  ↺  Revalidados:        {}", r.revalidated_files); }
    if r.moved_files        > 0 { println!("  ✂  Movidos:            {}", r.moved_files); }
    if r.move_delete_failed > 0 { println!("  ⚠  Origen no borrado:  {}", r.move_delete_failed); }
    if r.dirs_removed       > 0 { println!("  📁 Carpetas vacías:    {}", r.dirs_removed); }
    println!("  Datos:        {:.1} MB", mb);
    println!("  Tiempo:       {:.2}s", elapsed.as_secs_f64());
    println!("  Velocidad:    {:.1} MB/s", spd);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    if r.failed_files == 0 { println!("  ✓ Completado"); }
    else { println!("  ⚠  {} error(es)", r.failed_files); }
}

fn init_logging(verbosity: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let level = if quiet { "error" } else {
        match verbosity { 0 => "warn", 1 => "info", 2 => "debug", _ => "trace" }
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(level)))
        .with_target(false).with_thread_ids(false).compact().init();
}
