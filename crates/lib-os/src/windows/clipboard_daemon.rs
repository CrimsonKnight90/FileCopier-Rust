//! # clipboard_daemon
//!
//! Daemon profesional de interceptación del portapapeles de Windows.
//!
//! ## Arquitectura
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  Thread principal (UI thread — STA COM)                         │
//! │                                                                 │
//! │  CreateWindowEx("FileCopierDaemon", message-only HWND_MESSAGE)  │
//! │  AddClipboardFormatListener(hwnd)                               │
//! │                                                                 │
//! │  Message loop:                                                  │
//! │    WM_CLIPBOARDUPDATE ──────────────────────────────────────►   │
//! │        on_clipboard_update()                                    │
//! │            ┌─ CF_HDROP presente y distinto al que tenemos:      │
//! │            │    → guardar en PendingQueue (Copy o Move)         │
//! │            │                                                    │
//! │            └─ CF_HDROP ausente Y tenemos cola pendiente:        │
//! │                 → el usuario pegó (Explorer limpió el portap.)  │
//! │                 → resolver dest via IShellBrowser / UIAutomation│
//! │                 → lanzar FileCopier en thread separado          │
//! │                 → limpiar cola                                  │
//! └─────────────────────────────────────────────────────────────────┘
//!
//! ┌─────────────────────────────────────────────────────────────────┐
//! │  Thread worker (tokio runtime, creado UNA sola vez)             │
//! │  Ejecuta las copias/movimientos sin bloquear el UI thread.      │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Detección del pegado
//!
//! Cuando el usuario hace Ctrl+V en Explorer:
//! 1. Explorer lee `CF_HDROP` del portapapeles.
//! 2. Explorer copia los archivos.
//! 3. Explorer llama a `EmptyClipboard()` o reemplaza el contenido.
//! 4. Se dispara `WM_CLIPBOARDUPDATE` en todos los listeners.
//!
//! En ese momento, `IsClipboardFormatAvailable(CF_HDROP)` retorna FALSE
//! (o retorna TRUE con datos distintos a los que teníamos en la cola).
//! Eso nos indica que ocurrió un paste y debemos ejecutar la operación.
//!
//! ## Resolución del destino
//!
//! Se intenta en orden:
//! 1. `IShellBrowser` via `SHDocVw::ShellWindows` (COM STA) — path exacto
//! 2. `IUIAutomation` — leer AddressBar de la ventana foreground
//! 3. Título de ventana — heurística para carpetas estándar
//! 4. `FolderBrowserDialog` — diálogo manual si todo falla
//!
//! ## Por qué message-only window en lugar de `GetClipboardSequenceNumber`
//!
//! `WM_CLIPBOARDUPDATE` es la API oficial (Windows Vista+) para notificaciones
//! del portapapeles. Es event-driven — no hay polling. `GetClipboardSequenceNumber`
//! requiere polling activo cada N ms y tiene latencia. Con `WM_CLIPBOARDUPDATE`
//! la respuesta es inmediata y el CPU en idle es 0%.

use std::path::PathBuf;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

/// Operación pendiente en la cola.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingOperation {
    Copy,
    Move,
}

impl std::fmt::Display for PendingOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Copy => write!(f, "COPIAR"),
            Self::Move => write!(f, "MOVER"),
        }
    }
}

/// Archivos en espera de ser pegados.
#[derive(Debug, Clone)]
pub struct PendingItem {
    pub paths: Vec<PathBuf>,
    pub operation: PendingOperation,
    /// Número de secuencia del portapapeles cuando se capturó.
    /// Permite detectar si el portapapeles fue reemplazado antes de pegar.
    pub sequence: u32,
}

/// Configuración del daemon.
#[derive(Clone)]
pub struct DaemonConfig {
    /// Directorio destino fijo. Si `None`, se resuelve en tiempo de pegado.
    pub fixed_dest: Option<PathBuf>,
    /// Mostrar diálogo si no se puede resolver automáticamente.
    pub fallback_dialog: bool,
    /// Verificación de integridad post-copia.
    pub verify: bool,
    pub block_size_mb: u64,
    pub threshold_mb: u64,
    pub channel_cap: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            fixed_dest: None,
            fallback_dialog: true,
            verify: false,
            block_size_mb: 4,
            threshold_mb: 16,
            channel_cap: 8,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Estado global del daemon (accedido desde WndProc y keyboard hook)
// ─────────────────────────────────────────────────────────────────────────────

struct DaemonState {
    queue: Option<PendingItem>,
    config: DaemonConfig,
    runtime: Arc<tokio::runtime::Runtime>,
}

static DAEMON_STATE: std::sync::OnceLock<Arc<Mutex<DaemonState>>> = std::sync::OnceLock::new();

// Handle del hook de teclado global — guardado para poder desinstalarlo.
static KEYBOARD_HOOK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

// ─────────────────────────────────────────────────────────────────────────────
// Daemon principal
// ─────────────────────────────────────────────────────────────────────────────

/// Inicia el daemon. Bloquea el thread actual hasta que se detenga.
///
/// Debe llamarse desde el thread principal (STA COM).
pub fn run_daemon(config: DaemonConfig) -> Result<()> {
    unsafe { run_daemon_impl(config) }
}

unsafe fn run_daemon_impl(config: DaemonConfig) -> Result<()> {
    use windows_sys::Win32::System::DataExchange::AddClipboardFormatListener;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassExW,
        TranslateMessage, HWND_MESSAGE, MSG, WNDCLASSEXW,
    };

    // Inicializar COM para este thread (STA — necesario para IShellBrowser)
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);

    // Crear runtime tokio compartido para todos los eventos
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_io()
            .enable_time()
            .thread_name("filecopier-worker")
            .build()
            .expect("No se pudo crear runtime tokio"),
    );

    // Inicializar estado global
    let state = Arc::new(Mutex::new(DaemonState {
        queue: None,
        config: config.clone(),
        runtime: Arc::clone(&runtime),
    }));
    DAEMON_STATE.set(Arc::clone(&state)).unwrap_or(()); // puede fallar si el daemon se reinicia — ignorar

    // Registrar clase de ventana
    let class_name: Vec<u16> = "FileCopierDaemon\0".encode_utf16().collect();
    let hinstance = GetModuleHandleW(std::ptr::null());

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(daemon_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name.as_ptr(),
        // Todos los demás campos = 0/null
        style: 0,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        hIconSm: std::ptr::null_mut(),
    };

    let atom = RegisterClassExW(&wc);
    if atom == 0 {
        // Puede fallar si ya está registrada (segunda ejecución del daemon)
        // — no es un error fatal.
        tracing::debug!("RegisterClassExW: clase ya registrada o error");
    }

    // Crear ventana message-only (HWND_MESSAGE = no visible, solo recibe mensajes)
    let title: Vec<u16> = "FileCopier Clipboard Daemon\0".encode_utf16().collect();
    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        title.as_ptr(),
        0, // sin estilo (message-only no necesita)
        0,
        0,
        0,
        0,
        HWND_MESSAGE, // ventana padre = HWND_MESSAGE → no visible
        std::ptr::null_mut(),
        hinstance,
        std::ptr::null(),
    );

    if hwnd.is_null() {
        bail!("CreateWindowExW falló: {}", std::io::Error::last_os_error());
    }

   // Suscribirse a notificaciones del portapapeles
    if AddClipboardFormatListener(hwnd) == 0 {
        bail!(
            "AddClipboardFormatListener falló: {}",
            std::io::Error::last_os_error()
        );
    }

    // Instalar hook de teclado global (WH_KEYBOARD_LL).
    // Este hook se ejecuta en el mismo thread que tiene el message loop,
    // por lo que no necesita un thread separado. Intercepta Ctrl+V en
    // CUALQUIER aplicación, incluyendo Explorer, ANTES de que Explorer
    // procese la pulsación.
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowsHookExW, WH_KEYBOARD_LL,
    };
    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), std::ptr::null_mut(), 0);
    if hook.is_null() {
        bail!(
            "SetWindowsHookExW falló: {}",
            std::io::Error::last_os_error()
        );
    }
    KEYBOARD_HOOK.store(hook, Ordering::Relaxed);

    tracing::info!("Daemon iniciado — hook de teclado activo, esperando Ctrl+V...");

    // Message loop — bloquea hasta WM_QUIT
    let mut msg: MSG = std::mem::zeroed();
    loop {
        let ret = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
        if ret == 0 || ret == -1 {
            break;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    Ok(())
}

/// Hook de teclado de bajo nivel (WH_KEYBOARD_LL).
///
/// Se llama ANTES de que la aplicación destino (Explorer) reciba la tecla.
/// Detecta Ctrl+V y, si hay archivos en cola, cancela el evento de teclado
/// (retorna 1) y despacha la operación de FileCopier.
///
/// # Safety
/// Llamado por Windows desde el message loop del thread que instaló el hook.
unsafe extern "system" fn keyboard_hook_proc(
    code: i32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, KBDLLHOOKSTRUCT,
        WM_KEYDOWN, WM_SYSKEYDOWN,        
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_V};

    // Solo procesar si code == HC_ACTION y es key-down
    if code == HC_ACTION as i32
        && (wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN)
    {
        let kb = &*(lparam as *const KBDLLHOOKSTRUCT);

        // VK_V con Ctrl presionado = Ctrl+V
        let ctrl_pressed = GetKeyState(VK_CONTROL as i32) as u16 & 0x8000 != 0;

        if kb.vkCode == VK_V as u32 && ctrl_pressed {
            // Verificar si tenemos archivos en cola ANTES de que Explorer actúe
            if let Some(state_arc) = DAEMON_STATE.get() {
                if let Ok(mut state) = state_arc.try_lock() {
                    if state.queue.is_some() {
                        let pending = state.queue.take().unwrap();

                        // Verificar que los paths siguen existiendo (aún no los tocó nadie)
                        let valid_paths: Vec<std::path::PathBuf> = pending
                            .paths
                            .iter()
                            .filter(|p| p.exists())
                            .cloned()
                            .collect();

                        if !valid_paths.is_empty() {
                            // Resolver destino AHORA (antes de que Explorer consuma el Ctrl+V)
                            let dest = resolve_dest_static(&state.config);

                            if let Some(dest) = dest {
                                println!();
                                println!(
                                    "  ▶ {} {} elemento(s) → {}",
                                    pending.operation,
                                    valid_paths.len(),
                                    dest.display()
                                );

                                let runtime = Arc::clone(&state.runtime);
                                let config  = state.config.clone();
                                let op      = pending.operation;

                                std::thread::spawn(move || {
                                    execute_operation(valid_paths, dest, op, &config, &runtime);
                                });

                                // Consumir el Ctrl+V — no dejar que Explorer lo procese
                                return 1;
                            } else {
                                // No se pudo resolver destino — devolver archivos a la cola
                                // y dejar que Explorer maneje el Ctrl+V normalmente.
                                state.queue = Some(PendingItem {
                                    paths: pending.paths,
                                    operation: pending.operation,
                                    sequence: pending.sequence,
                                });
                            }
                        } else {
                            println!("  ⚠  Los archivos ya no existen en el origen");
                        }
                    }
                }
            }
        }
    }

    let hook = KEYBOARD_HOOK.load(Ordering::Relaxed);
    CallNextHookEx(hook, code, wparam, lparam)
}

/// Versión de `resolve_dest` que no toma `&DaemonState` (para usar desde el hook).
fn resolve_dest_static(config: &DaemonConfig) -> Option<std::path::PathBuf> {
    if let Some(ref d) = config.fixed_dest {
        return Some(d.clone());
    }
    if let Some(path) = super::explorer_path::get_active_explorer_path() {
        return Some(path);
    }
    if config.fallback_dialog {
        println!("  🗂  Selecciona la carpeta de destino...");
        return super::explorer_path::prompt_folder_dialog("Selecciona dónde pegar — FileCopier");
    }
    None
}

/// WndProc de la ventana message-only.
///
/// # Safety
/// Llamado por Windows desde el message loop — thread único, sin reentrancia.
unsafe extern "system" fn daemon_wndproc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, PostQuitMessage, WM_CLIPBOARDUPDATE, WM_DESTROY,
    };

    match msg {
        WM_CLIPBOARDUPDATE => {
            on_clipboard_update();
            0
        }
        WM_DESTROY => {
            use windows_sys::Win32::System::DataExchange::RemoveClipboardFormatListener;
            use windows_sys::Win32::UI::WindowsAndMessaging::UnhookWindowsHookEx;
            RemoveClipboardFormatListener(hwnd);
            let hook = KEYBOARD_HOOK.load(Ordering::Relaxed);
            if !hook.is_null() {
                UnhookWindowsHookEx(hook as _);
            }
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Lógica principal del portapapeles
// ─────────────────────────────────────────────────────────────────────────────

/// Llamada cada vez que el portapapeles cambia (WM_CLIPBOARDUPDATE).
fn on_clipboard_update() {
    let has_hdrop = clipboard_has_hdrop();

    let state_arc = match DAEMON_STATE.get() {
        Some(s) => s,
        None => return,
    };

    let mut state = match state_arc.lock() {
        Ok(s) => s,
        Err(_) => return,
    };

    if has_hdrop {
        match read_clipboard_files_and_op() {
            Some(item) => {
                // Solo actualizar la cola — NO ejecutar aquí.
                // La ejecución la dispara el hook de teclado (Ctrl+V),
                // ANTES de que Explorer tenga oportunidad de actuar.
                let op = item.operation;
                let count = item.paths.len();
                state.queue = Some(item);
                println!();
                println!(
                    "  📋 {} {} archivo(s) en cola — ve a la carpeta destino y pega (Ctrl+V)",
                    op, count
                );
            }
            None => {
                tracing::debug!("WM_CLIPBOARDUPDATE: CF_HDROP presente pero no legible");
            }
        }
    }
    // Ya NO actuamos cuando CF_HDROP desaparece — eso ocurre DESPUÉS
    // de que Explorer ya copió. El hook de teclado intercepta antes.
}

// execute_pending_item y resolve_dest eliminados —
// reemplazados por la lógica en keyboard_hook_proc y resolve_dest_static.

// ─────────────────────────────────────────────────────────────────────────────
// Ejecución de la operación
// ─────────────────────────────────────────────────────────────────────────────

/// Ejecuta la copia/movimiento de todos los paths hacia `dest`.
fn execute_operation(
    sources: Vec<PathBuf>,
    dest: PathBuf,
    op: PendingOperation,
    config: &DaemonConfig,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    use lib_core::{
        checkpoint::FlowControl,
        config::{EngineConfig, OperationMode},
        engine::Orchestrator,
    };

    // Asegurar que el directorio destino existe
    if !dest.exists() {
        if let Err(e) = std::fs::create_dir_all(&dest) {
            println!("  ✗ No se pudo crear destino '{}': {}", dest.display(), e);
            return;
        }
    }

    let engine_config = EngineConfig {
        triage_threshold_bytes: config.threshold_mb * 1024 * 1024,
        block_size_bytes: config.block_size_mb as usize * 1024 * 1024,
        channel_capacity: config.channel_cap,
        swarm_concurrency: 64,
        verify: config.verify,
        operation_mode: match op {
            PendingOperation::Copy => OperationMode::Copy,
            PendingOperation::Move => OperationMode::Move,
        },
        dry_run: false,
        ..EngineConfig::default()
    };

    for source in &sources {
        if !source.exists() {
            println!("  ⚠  Ya no existe: {}", source.display());
            continue;
        }

        let flow = FlowControl::new();
        let os_ops = std::sync::Arc::new(crate::windows::WindowsAdapter::new())
            as std::sync::Arc<dyn lib_core::os_ops::OsOps>;

        let start = std::time::Instant::now();
        let orch = Orchestrator::new(engine_config.clone(), flow, os_ops);

        match orch.run(source, &dest, None) {
            Ok(result) => {
                let elapsed = start.elapsed().as_secs_f64();
                let mb = result.copied_bytes as f64 / 1024.0 / 1024.0;
                println!(
                    "  ✓ {} → {} archivo(s), {:.1} MB en {:.1}s ({:.0} MB/s)",
                    source.file_name().unwrap_or_default().to_string_lossy(),
                    result.completed_files,
                    mb,
                    elapsed,
                    if elapsed > 0.0 { mb / elapsed } else { 0.0 }
                );
                if result.failed_files > 0 {
                    println!("    ⚠  {} error(es)", result.failed_files);
                }
                if result.dirs_removed > 0 {
                    println!(
                        "    ✓  {} carpeta(s) vacía(s) eliminadas",
                        result.dirs_removed
                    );
                }
            }
            Err(e) => {
                println!("  ✗ {}: {}", source.display(), e);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers de portapapeles
// ─────────────────────────────────────────────────────────────────────────────

/// Retorna `true` si el portapapeles contiene `CF_HDROP` (archivos).
fn clipboard_has_hdrop() -> bool {
    unsafe {
        use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
        use windows_sys::Win32::System::Ole::CF_HDROP;
        IsClipboardFormatAvailable(CF_HDROP as u32) != 0
    }
}

/// Número de secuencia actual del portapapeles.
fn get_clipboard_sequence() -> u32 {
    unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

/// Lee los paths y la operación (Copy/Move) del portapapeles actual.
fn read_clipboard_files_and_op() -> Option<PendingItem> {
    unsafe { read_clipboard_impl() }
}

unsafe fn read_clipboard_impl() -> Option<PendingItem> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_HDROP;
    use windows_sys::Win32::UI::Shell::DragQueryFileW;

    if OpenClipboard(std::ptr::null_mut()) == 0 {
        return None;
    }

    let result = (|| {
        let seq = GetClipboardSequenceNumber();

        // Leer CF_HDROP
        let hdrop = GetClipboardData(CF_HDROP as u32);
        if hdrop.is_null() {
            return None;
        }

        let ptr = GlobalLock(hdrop as _);
        if ptr.is_null() {
            return None;
        }

        let count = DragQueryFileW(hdrop as _, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
        let mut paths = Vec::with_capacity(count as usize);

        for i in 0..count {
            let len = DragQueryFileW(hdrop as _, i, std::ptr::null_mut(), 0) as usize;
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop as _, i, buf.as_mut_ptr(), buf.len() as u32);
            let s = String::from_utf16_lossy(&buf[..len]);
            paths.push(PathBuf::from(s));
        }

        GlobalUnlock(hdrop as _);

        if paths.is_empty() {
            return None;
        }

        // Leer "Preferred DropEffect"
        let op = read_drop_effect_inner().unwrap_or(PendingOperation::Copy);

        Some(PendingItem {
            paths,
            operation: op,
            sequence: seq,
        })
    })();

    CloseClipboard();
    result
}

unsafe fn read_drop_effect_inner() -> Option<PendingOperation> {
    use windows_sys::Win32::System::DataExchange::{GetClipboardData, RegisterClipboardFormatW};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
    let fmt = RegisterClipboardFormatW(name.as_ptr());
    if fmt == 0 {
        return None;
    }

    let handle = GetClipboardData(fmt);
    if handle.is_null() {
        return None;
    }

    let ptr = GlobalLock(handle as _) as *const u32;
    if ptr.is_null() {
        return None;
    }
    let effect = *ptr;
    GlobalUnlock(handle as _);

    match effect {
        1 => Some(PendingOperation::Copy),
        2 => Some(PendingOperation::Move),
        _ => Some(PendingOperation::Copy),
    }
}

unsafe fn read_performed_drop_effect_inner() -> Option<u32> {
    use windows_sys::Win32::System::DataExchange::{GetClipboardData, RegisterClipboardFormatW};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    let name: Vec<u16> = "Performed DropEffect\0".encode_utf16().collect();
    let fmt = RegisterClipboardFormatW(name.as_ptr());
    if fmt == 0 {
        return None;
    }

    let handle = GetClipboardData(fmt);
    if handle.is_null() {
        return None;
    }

    let ptr = GlobalLock(handle as _) as *const u32;
    if ptr.is_null() {
        return None;
    }
    let effect = *ptr;
    GlobalUnlock(handle as _);

    Some(effect)
}
