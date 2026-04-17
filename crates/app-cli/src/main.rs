//! # filecopier CLI
//!
//! Interfaz de línea de comandos para FileCopier-Rust.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;

use lib_core::{
    checkpoint::{FlowControl, ResumePolicy},
    config::EngineConfig,
    engine::Orchestrator,
    hash::Algorithm,
    telemetry::CopyProgress,
};
use lib_os::traits::DriveKind;

// ─────────────────────────────────────────────────────────────────────────────
// Argumentos CLI
// ─────────────────────────────────────────────────────────────────────────────

/// Motor de copia de alto rendimiento con verificación de integridad
#[derive(Parser, Debug)]
#[command(
    name       = "filecopier",
    version,
    about      = "Motor de copia de alto rendimiento con verificación de integridad",
    long_about = None,
    after_help = "Ejemplos:\n\
                  filecopier C:\\src C:\\dst\n\
                  filecopier --verify --hasher blake3 C:\\src C:\\dst\n\
                  filecopier --resume C:\\src C:\\dst\n\
                  filecopier --resume --resume-policy verify-hash C:\\src C:\\dst"
)]
struct Cli {
    #[arg(value_name = "ORIGEN")]
    source: PathBuf,

    #[arg(value_name = "DESTINO")]
    dest: PathBuf,

    /// Habilita verificación de integridad post-copia
    #[arg(long)]
    verify: bool,

    /// Algoritmo de hashing: blake3, xxhash, sha2
    #[arg(long, default_value = "blake3", value_name = "ALGO")]
    hasher: String,

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

    /// Reanudar desde checkpoint existente
    #[arg(long, short = 'r')]
    resume: bool,

    /// Política de validación al reanudar
    ///
    /// trust    - Confiar ciegamente en el checkpoint (no verificar disco)\n\
    /// size     - Verificar existencia y tamaño (default, 1 syscall por archivo)\n\
    /// hash     - Verificar existencia, tamaño y hash blake3 del contenido
    #[arg(
        long,
        default_value = "size",
        value_name    = "POLICY",
        value_parser  = parse_resume_policy,
        verbatim_doc_comment,
    )]
    resume_policy: ResumePolicy,

    /// Escribir directamente sin archivos .partial intermedios
    #[arg(long, hide = true)]
    no_partial: bool,

    /// Ignorar detección automática de hardware
    #[arg(long)]
    no_detect: bool,

    /// Nivel de verbosidad (-v info, -vv debug, -vvv trace)
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbosity: u8,

    /// Mostrar solo errores y resumen final
    #[arg(long, short = 'q')]
    quiet: bool,
}

fn parse_resume_policy(s: &str) -> std::result::Result<ResumePolicy, String> {
    match s.to_lowercase().as_str() {
        "trust"      => Ok(ResumePolicy::TrustCheckpoint),
        "size"       => Ok(ResumePolicy::VerifySize),
        "hash"       => Ok(ResumePolicy::VerifyHash),
        other => Err(format!(
            "Política desconocida: '{}'. Opciones: trust, size, hash",
            other
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbosity, cli.quiet);

    if let Err(e) = run(cli) {
        eprintln!("\n❌ Error fatal: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> lib_core::error::Result<()> {
    if !cli.source.exists() {
        eprintln!("❌ El origen no existe: {}", cli.source.display());
        std::process::exit(2);
    }

    let algorithm = Algorithm::from_str(&cli.hasher).unwrap_or_else(|e| {
        eprintln!("⚠  {e}. Usando blake3 por defecto.");
        Algorithm::Blake3
    });

    let mut config = EngineConfig {
        triage_threshold_bytes: cli.threshold  * 1024 * 1024,
        block_size_bytes:        cli.block_size as usize * 1024 * 1024,
        channel_capacity:        cli.channel_cap,
        swarm_concurrency:       cli.swarm_limit,
        verify:                  cli.verify,
        hash_algorithm:          algorithm,
        resume:                  cli.resume,
        resume_policy:           cli.resume_policy,
        use_partial_files:       !cli.no_partial,
        bandwidth_limit_bytes_per_sec: 0,
        bandwidth_burst_bytes:   1 * 1024 * 1024,
    };

    if !cli.no_detect {
        let adapter  = lib_os::platform_adapter();
        let strategy = adapter.compute_strategy(&cli.source, &cli.dest);

        tracing::info!(
            "Hardware: origen={:?}, destino={:?}",
            strategy.source_kind,
            strategy.dest_kind
        );

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
        print_config_banner(&config, &cli.source, &cli.dest);
    }

    let flow         = FlowControl::new();
    let signal_count = Arc::new(AtomicU32::new(0));
    install_ctrlc_handler(flow.clone(), Arc::clone(&signal_count));

    let start = Instant::now();
    let quiet = cli.quiet;

    let on_progress: lib_core::engine::orchestrator::ProgressCallback =
        Box::new(move |progress: CopyProgress| {
            if !quiet { print_progress(&progress); }
        });

    let os_ops: Arc<dyn lib_core::os_ops::OsOps> = if !cli.no_detect {
        lib_os::platform_adapter_os_ops().into()
    } else {
        Arc::new(lib_core::os_ops::NoOpOsOps)
    };

    let orchestrator = Orchestrator::new(config, flow, os_ops);
    let result = orchestrator.run(&cli.source, &cli.dest, Some(on_progress))?;

    if !cli.quiet {
        println!();
    }

    print_summary(&result, start.elapsed());

    if result.failed_files > 0 {
        std::process::exit(3);
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler de señales
// ─────────────────────────────────────────────────────────────────────────────

fn install_ctrlc_handler(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    #[cfg(windows)]  install_ctrlc_handler_windows(flow, signal_count);
    #[cfg(unix)]     install_ctrlc_handler_unix(flow, signal_count);
}

#[cfg(windows)]
fn install_ctrlc_handler_windows(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    WINDOWS_FLOW.set(flow).expect("handler ya instalado");
    WINDOWS_SIGNAL_COUNT.set(signal_count).expect("handler ya instalado");
    unsafe { SetConsoleCtrlHandler(Some(windows_ctrl_handler), 1); }
}

#[cfg(windows)]
unsafe extern "system" fn windows_ctrl_handler(ctrl_type: u32) -> i32 {
    match ctrl_type { 0 | 1 | 2 => { handle_signal_windows(); 1 } _ => 0 }
}

#[cfg(windows)]
fn handle_signal_windows() {
    if let Some(count) = WINDOWS_SIGNAL_COUNT.get() {
        let prev = count.fetch_add(1, Ordering::SeqCst);
        if let Some(flow) = WINDOWS_FLOW.get() {
            if prev == 0 {
                eprintln!("\n⏸  Pausa solicitada. Presiona Ctrl+C de nuevo para cancelar.");
                flow.pause();
            } else if prev == 1 {
                eprintln!("\n⚠  Cancelando y guardando checkpoint...");
                flow.cancel();
            } else {
                flow.cancel();
            }
        }
    }
}

#[cfg(windows)]
static WINDOWS_FLOW:         std::sync::OnceLock<FlowControl>        = std::sync::OnceLock::new();
#[cfg(windows)]
static WINDOWS_SIGNAL_COUNT: std::sync::OnceLock<Arc<AtomicU32>>     = std::sync::OnceLock::new();

#[cfg(unix)]
fn install_ctrlc_handler_unix(flow: FlowControl, signal_count: Arc<AtomicU32>) {
    UNIX_FLOW.set(flow).expect("handler ya instalado");
    UNIX_SIGNAL_COUNT.set(signal_count).expect("handler ya instalado");
    unsafe { libc::signal(libc::SIGINT, unix_sigint_handler as libc::sighandler_t); }
}

#[cfg(unix)]
extern "C" fn unix_sigint_handler(_sig: libc::c_int) {
    if let Some(count) = UNIX_SIGNAL_COUNT.get() {
        let prev = count.fetch_add(1, Ordering::SeqCst);
        if let Some(flow) = UNIX_FLOW.get() {
            if prev == 0 {
                eprintln!("\n⏸  Pausa solicitada. Presiona Ctrl+C de nuevo para cancelar.");
                flow.pause();
            } else if prev == 1 {
                eprintln!("\n⚠  Cancelando y guardando checkpoint...");
                flow.cancel();
            } else {
                flow.cancel();
            }
        }
    }
}

#[cfg(unix)]
static UNIX_FLOW:         std::sync::OnceLock<FlowControl>    = std::sync::OnceLock::new();
#[cfg(unix)]
static UNIX_SIGNAL_COUNT: std::sync::OnceLock<Arc<AtomicU32>> = std::sync::OnceLock::new();

// ─────────────────────────────────────────────────────────────────────────────
// UI helpers
// ─────────────────────────────────────────────────────────────────────────────

fn print_config_banner(config: &EngineConfig, source: &std::path::Path, dest: &std::path::Path) {
    let policy_label = match config.resume_policy {
        ResumePolicy::TrustCheckpoint => "trust (sin validación)",
        ResumePolicy::VerifySize      => "size  (existencia + tamaño)",
        ResumePolicy::VerifyHash      => "hash  (existencia + tamaño + blake3)",
    };

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  FileCopier-Rust v{}", env!("CARGO_PKG_VERSION"));
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Origen:       {}", source.display());
    println!("  Destino:      {}", dest.display());
    println!("  Bloque:       {} MB", config.block_size_bytes / 1024 / 1024);
    println!("  Umbral:       {} MB", config.triage_threshold_bytes / 1024 / 1024);
    println!("  Enjambre:     {} tareas", config.swarm_concurrency);
    println!(
        "  Verificación: {}",
        if config.verify {
            format!("✓ ({})", config.hash_algorithm)
        } else {
            "✗  (usa --verify para activar)".into()
        }
    );
    println!(
        "  Checkpoint:   {}",
        if config.resume {
            format!("reanudar [política: {}]", policy_label)
        } else {
            "nuevo".into()
        }
    );
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
    let concurrency = if verify {
        strategy.recommended_swarm_concurrency_verify
    } else {
        strategy.recommended_swarm_concurrency
    };
    println!(
        "  Hardware: {} → {}  |  enjambre={} bloque={}MB{}",
        label(strategy.source_kind),
        label(strategy.dest_kind),
        concurrency,
        strategy.recommended_block_size / 1024 / 1024,
        if verify { "  [verify: concurrencia reducida]" } else { "" },
    );
}

fn print_progress(p: &CopyProgress) {
    let bar_width = 30usize;
    let filled    = ((p.percent / 100.0) * bar_width as f64) as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_width - filled);

    if let Some(ref current_file) = p.current_file {
        let file_name = std::path::Path::new(current_file)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(current_file.as_str());
        let inner = 10usize;
        let fi    = ((p.current_file_progress) * inner as f64) as usize;
        let ib: String = "█".repeat(fi.min(inner)) + &"░".repeat(inner.saturating_sub(fi));
        print!(
            "\r  [{bar}] {:.1}%  {}  {}/{}  ETA: {}  |  {}: [{}] {:.0}%",
            p.percent, p.throughput_human(), p.completed_files, p.total_files,
            p.eta_human(), file_name, ib, p.current_file_progress * 100.0,
        );
    } else {
        print!(
            "\r  [{bar}] {:.1}%  {}  {}/{}  ETA: {}    ",
            p.percent, p.throughput_human(), p.completed_files, p.total_files,
            p.eta_human(),
        );
    }

    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn print_summary(
    result:  &lib_core::engine::orchestrator::CopyResult,
    elapsed: std::time::Duration,
) {
    let mb      = result.copied_bytes as f64 / 1024.0 / 1024.0;
    let avg_spd = if elapsed.as_secs_f64() > 0.0 {
        mb / elapsed.as_secs_f64()
    } else {
        0.0
    };

    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Resumen de copia");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Completados:  {} archivos", result.completed_files);
    if result.failed_files > 0 {
        println!("  ⚠  Fallidos: {} archivos", result.failed_files);
    }
    if result.revalidated_files > 0 {
        println!(
            "  ↺  Recopiados por validación: {} archivos",
            result.revalidated_files
        );
    }
    println!("  Copiados:     {:.1} MB", mb);
    println!("  Tiempo:       {:.2}s", elapsed.as_secs_f64());
    println!("  Velocidad:    {:.1} MB/s (promedio)", avg_spd);
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    if result.failed_files == 0 {
        println!("  ✓ Copia completada exitosamente");
    } else {
        println!("  ⚠  Copia completada con {} error(es)", result.failed_files);
        println!("    Revisa el checkpoint para detalles.");
    }
}

fn init_logging(verbosity: u8, quiet: bool) {
    use tracing_subscriber::EnvFilter;
    let level = if quiet { "error" } else {
        match verbosity { 0 => "warn", 1 => "info", 2 => "debug", _ => "trace" }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(level)),
        )
        .with_target(false)
        .with_thread_ids(false)
        .compact()
        .init();
}
