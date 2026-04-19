//! # explorer_path
//!
//! Obtiene el path de la carpeta actualmente activa en el Explorer de Windows.
//!
//! ## Estrategias (en orden de prioridad)
//!
//! 1. **IShellBrowser via SHDocVw** — consulta todas las ventanas de Explorer
//!    registradas en `ShellWindows` y encuentra cuál es la ventana foreground.
//!    Luego obtiene el path con `IShellBrowser::QueryActiveShellView` →
//!    `IFolderView::GetFolder` → `IPersistFolder2::GetCurFolder`.
//!
//! 2. **WM_USER+7 (INTERNAL)** — mensaje interno de Explorer para obtener
//!    el PIDL de la carpeta activa. Funciona en Windows 10/11.
//!
//! 3. **UI Automation** — usa `IUIAutomation` para encontrar el `AddressBar`
//!    de la ventana Explorer activa y leer su texto.
//!
//! 4. **Diálogo de selección** — fallback interactivo: muestra un
//!    `FolderBrowserDialog` para que el usuario elija manualmente.
//!
//! ## Por qué es complejo
//!
//! Explorer no expone el path actual directamente. La forma oficial es via COM
//! (IShellBrowser), pero requiere que el thread esté en un apartamento COM STA.
//! Por eso `get_active_explorer_path()` inicializa COM si es necesario.
//!
//! ## Nota sobre hooks de teclado
//!
//! Cuando se llama desde un hook WH_KEYBOARD_LL, `GetForegroundWindow()` puede
//! retornar un valor incorrecto porque el hook se ejecuta ANTES de que el
//! sistema actualice el foco. Por eso iteramos TODAS las ventanas Explorer
//! y usamos la más reciente como fallback.

use std::path::PathBuf;
use tracing::{debug, warn, info};

/// Intenta obtener la ruta de la carpeta activa en el Explorer foreground.
///
/// Prueba las estrategias disponibles en orden y retorna la primera que funcione.
/// Si ninguna funciona, retorna `None`.
pub fn get_active_explorer_path() -> Option<PathBuf> {
    // Estrategia 1: IShellBrowser via COM (más confiable)
    if let Some(path) = try_ishellbrowser() {
        debug!("ExplorerPath: obtenido via IShellBrowser — {}", path.display());
        return Some(path);
    }

    // Estrategia 2: UI Automation (AddressBar)
    if let Some(path) = try_ui_automation() {
        debug!("ExplorerPath: obtenido via UIAutomation — {}", path.display());
        return Some(path);
    }

    // Estrategia 3: Leer la barra de título de la ventana Explorer
    if let Some(path) = try_window_title() {
        debug!("ExplorerPath: obtenido via window title — {}", path.display());
        return Some(path);
    }

    warn!("ExplorerPath: no se pudo determinar la carpeta activa");
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Estrategia 1 — IShellBrowser via SHDocVw COM
// ─────────────────────────────────────────────────────────────────────────────

fn try_ishellbrowser() -> Option<PathBuf> {
    unsafe { try_ishellbrowser_impl() }
}

unsafe fn try_ishellbrowser_impl() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{
        IShellBrowser, IShellView, IShellWindows, ShellWindows,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx,
        CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::core::{Interface, VARIANT};

    // Inicializar COM (puede ya estar inicializado — ignorar resultado)
    let com_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    debug!("IShellBrowser: CoInitializeEx hr={:?}", com_hr);

    let foreground = windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
    debug!("IShellBrowser: foreground HWND={:?}", foreground);

    if foreground.is_null() {
        warn!("IShellBrowser: foreground HWND es null");
        return None;
    }

    // Crear instancia de ShellWindows (colección de ventanas Explorer)
    let shell_windows_result: windows::core::Result<IShellWindows> = CoCreateInstance(
        &ShellWindows,
        None,
        CLSCTX_LOCAL_SERVER,
    );

    let shell_windows = match shell_windows_result {
        Ok(sw) => sw,
        Err(e) => {
            warn!("IShellBrowser: CoCreateInstance(ShellWindows) falló: {:?}", e);
            return None;
        }
    };

    let count = match shell_windows.Count() {
        Ok(c) => c,
        Err(e) => {
            warn!("IShellBrowser: ShellWindows.Count() falló: {:?}", e);
            return None;
        }
    };

    debug!("IShellBrowser: {} ventanas Shell encontradas", count);

    // Iterar TODAS las ventanas Explorer y tomar la que coincida con foreground
    // o la última válida si hay múltiples (fallback para hook de teclado)
    let mut last_valid_path: Option<PathBuf> = None;

    for i in 0..count {
        let item = match shell_windows.Item(&VARIANT::from(i as i32)) {
            Ok(it) => it,
            Err(e) => {
                debug!("IShellBrowser: Item({}) falló: {:?}", i, e);
                continue;
            }
        };

        use windows::Win32::System::Com::IServiceProvider;
        use windows::Win32::UI::Shell::IWebBrowserApp;

        let browser_app: IWebBrowserApp = match item.cast() {
            Ok(b) => b,
            Err(e) => {
                debug!("IShellBrowser: cast IWebBrowserApp({}) falló: {:?}", i, e);
                continue;
            }
        };

        let hwnd_val = match browser_app.HWND() {
            Ok(h) => h,
            Err(e) => {
                debug!("IShellBrowser: HWND({}) falló: {:?}", i, e);
                continue;
            }
        };

        let hwnd = hwnd_val.0 as isize;
        let is_foreground = hwnd == foreground as isize;
        let is_explorer = is_explorer_window(hwnd_val.0 as isize as _);
        
        debug!(
            "IShellBrowser: ventana {} hwnd={} foreground={} is_foreground={} is_explorer={}",
            i, hwnd, foreground as isize, is_foreground, is_explorer
        );

        if !is_explorer {
            continue;
        }

        // Si es la foreground, usarla inmediatamente
        if is_foreground {
            debug!("IShellBrowser: ventana {} es foreground, procesando...", i);
            return process_explorer_window(&browser_app, i);
        }

        // Guardar como fallback si es Explorer válido
        if last_valid_path.is_none() {
            last_valid_path = process_explorer_window(&browser_app, i);
            if last_valid_path.is_some() {
                debug!("IShellBrowser: guardado path de ventana {} como fallback", i);
            }
        }
    }

    // Si no encontramos la foreground pero tenemos al menos una ventana Explorer
    if let Some(path) = last_valid_path {
        info!("IShellBrowser: usando ventana Explorer no-foreground como fallback");
        return Some(path);
    }

    warn!("IShellBrowser: ninguna ventana Explorer válida encontrada");
    None
}

/// Procesa una ventana Explorer específica para obtener su path
/// Usa un enfoque simplificado que evita IFolderView/IPersistFolder2
fn process_explorer_window(
    browser_app: &windows::Win32::UI::Shell::IWebBrowserApp,
    index: i32,
) -> Option<PathBuf> {
    use windows::Win32::System::Com::IServiceProvider;
    use windows::Win32::UI::Shell::{IShellBrowser, IShellView, SHGetPathFromIDListW};
    use windows::core::Interface;

    let service_provider: IServiceProvider = browser_app.cast().ok()?;
    let shell_browser: IShellBrowser = service_provider
        .QueryService(&windows::Win32::UI::Shell::SID_STopLevelBrowser)
        .ok()?;

    let shell_view: IShellView = shell_browser.QueryActiveShellView().ok()?;
    
    // Obtener el PIDL usando IShellView::GetItemObject
    // Esto es más compatible que IFolderView
    use windows::Win32::UI::Shell::SGIO_OPENITEM;
    let pidl_result = shell_view.GetItemObject(SGIO_OPENITEM);
    let pidl = pidl_result.ok()?;
    
    // Convertir a IPersistFolder para obtener el path
    use windows::Win32::System::Com::IPersist;
    let persist: windows::Win32::UI::Shell::IPersistFolder = shell_view.cast().ok()?;
    let pidl = persist.GetCurFolder().ok()?;

    let mut buf = [0u16; 32768];
    if SHGetPathFromIDListW(pidl, &mut buf).as_bool() {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        debug!("IShellBrowser: path obtenido de ventana {} = '{}'", index, s);
        if !s.is_empty() {
            return Some(PathBuf::from(s));
        }
    } else {
        debug!("IShellBrowser: SHGetPathFromIDListW({}) falló", index);
    }

    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Estrategia 2 — UI Automation: leer AddressBar de Explorer
// ─────────────────────────────────────────────────────────────────────────────

fn try_ui_automation() -> Option<PathBuf> {
    unsafe { try_ui_automation_impl() }
}

unsafe fn try_ui_automation_impl() -> Option<PathBuf> {
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement,
        UIA_NamePropertyId, UIA_ControlTypePropertyId, UIA_EditControlTypeId,
        IUIAutomationCondition, TreeScope_Descendants, UIA_ValuePatternId,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::core::{Interface, BSTR, VARIANT};

    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let foreground_hwnd = windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
    if foreground_hwnd.is_null() {
        return None;
    }

    // Verificar que es una ventana Explorer
    if !is_explorer_window(foreground_hwnd) {
        return None;
    }

    let automation: IUIAutomation = CoCreateInstance(
        &CUIAutomation,
        None,
        CLSCTX_INPROC_SERVER,
    ).ok()?;

    // Obtener elemento raíz de la ventana foreground
    let root: IUIAutomationElement = automation
        .ElementFromHandle(windows::Win32::Foundation::HWND(foreground_hwnd))
        .ok()?;

    // Crear condición: ControlType == Edit
    let condition: IUIAutomationCondition = automation
        .CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_EditControlTypeId.0 as i32),
        )
        .ok()?;

    let elements = root
        .FindAll(TreeScope_Descendants, &condition)
        .ok()?;

    let count = elements.Length().ok()?;

    for i in 0..count {
        let elem: IUIAutomationElement = elements.GetElement(i).ok()?;

        // Leer el Name para identificar la barra de dirección
        let name_val = elem.GetCurrentPropertyValue(UIA_NamePropertyId).ok()?;
        let name_str: String = name_val.to_string();

        if !name_str.contains("Address") && !name_str.contains("Dirección") {
            continue;
        }

        // Obtener el valor (path actual)
        let pattern: windows::Win32::UI::Accessibility::IUIAutomationValuePattern = elem
            .GetCurrentPattern(UIA_ValuePatternId)
            .ok()?
            .cast()
            .ok()?;

        let value: BSTR = pattern.CurrentValue().ok()?;
        let path_str = value.to_string();

        if !path_str.is_empty() && std::path::Path::new(&path_str).is_dir() {
            return Some(PathBuf::from(path_str));
        }
    }

    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Estrategia 3 — Título de ventana Explorer
// ─────────────────────────────────────────────────────────────────────────────
//
// En Windows 11, el título de la ventana Explorer es exactamente el nombre
// de la carpeta (no el path completo). Esta estrategia es un fallback débil
// pero funciona para carpetas en ubicaciones estándar.

fn try_window_title() -> Option<PathBuf> {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowTextW, GetWindowTextLengthW,
        };

        let hwnd = GetForegroundWindow();
        if hwnd.is_null() { return None; }
        if !is_explorer_window(hwnd) { return None; }

        let len = GetWindowTextLengthW(hwnd) as usize;
        if len == 0 { return None; }

        let mut buf = vec![0u16; len + 1];
        GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);

        let title = String::from_utf16_lossy(&buf[..len]);
        if title.is_empty() { return None; }

        // El título puede ser el path completo o solo el nombre de carpeta.
        // Si es un path válido, usarlo directamente.
        let candidate = PathBuf::from(&title);
        if candidate.is_dir() {
            return Some(candidate);
        }

        // Intentar resolver como subcarpeta de lugares estándar
        for base in &[
            std::env::var("USERPROFILE").unwrap_or_default(),
            "C:\\Users".to_string(),
            "C:\\".to_string(),
        ] {
            if base.is_empty() { continue; }
            let candidate = PathBuf::from(base).join(&title);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }

        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Verifica si una ventana es una ventana de Explorer (CabinetWClass o ExploreWClass).
pub fn is_explorer_window(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let mut buf = [0u16; 256];
        let len = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len == 0 { return false; }
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        // CabinetWClass = carpeta normal, ExploreWClass = vista de árbol
        class == "CabinetWClass" || class == "ExploreWClass"
    }
}

/// Diálogo de selección de carpeta (fallback).
pub fn prompt_folder_dialog(title: &str) -> Option<PathBuf> {
    let script = format!(
        concat!(
            "Add-Type -AssemblyName System.Windows.Forms;",
            "$d = New-Object System.Windows.Forms.FolderBrowserDialog;",
            "$d.Description = '{}';",
            "$d.ShowNewFolderButton = $true;",
            "if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}"
        ),
        title.replace('\'', "\\'")
    );

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(PathBuf::from(path)) }
}