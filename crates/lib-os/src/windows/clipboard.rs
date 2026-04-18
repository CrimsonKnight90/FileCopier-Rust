//! crates/lib-os/src/windows/clipboard.rs
//! # clipboard (Windows)
//!
//! Interceptación del portapapeles del sistema operativo para archivos.
//!
//! ## Protocolo del portapapeles de Windows para archivos
//!
//! 1. **`CF_HDROP`** — lista de paths. Estructura `DROPFILES` + paths UTF-16 null-separated.
//! 2. **`"Preferred DropEffect"`** — DWORD: `1` = Copy (Ctrl+C), `2` = Move (Ctrl+X).
//!
//! ## Fixes en esta versión
//!
//! ### Fix 1: destino configurable por evento
//!
//! El destino ya NO es un directorio fijo. El daemon acepta un destino en
//! tiempo de ejecución. `watch()` recibe un closure `dest_resolver` que
//! puede: preguntar al usuario, usar el directorio activo, leer de config, etc.
//!
//! ### Fix 2: runtime tokio compartido
//!
//! El runtime tokio se crea UNA sola vez antes del loop de polling y se
//! pasa al callback. Antes se creaba dentro del closure en cada evento,
//! y su `drop()` al salir del closure interrumpía el canal crossbeam del
//! pipeline → "Pipeline interrumpido: el canal fue cerrado prematuramente".
//!
//! ### Fix 3: dest incorrecto para archivo individual
//!
//! Cuando `source_path` es un archivo (no un directorio), el Orchestrator
//! ahora construye `dest = dest_dir/filename` correctamente.
//! Ver `orchestrator.rs::scan_files()`.

use std::path::PathBuf;

use anyhow::{bail, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

/// Operación detectada en el portapapeles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    /// `Ctrl+C` — copiar.
    Copy,
    /// `Ctrl+X` — mover (borrar origen tras copia exitosa).
    Move,
}

impl std::fmt::Display for ClipboardOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Copy => write!(f, "COPIAR"),
            Self::Move => write!(f, "MOVER"),
        }
    }
}

/// Evento completo del portapapeles.
#[derive(Debug, Clone)]
pub struct ClipboardEvent {
    pub paths:     Vec<PathBuf>,
    pub operation: ClipboardOperation,
}

// ─────────────────────────────────────────────────────────────────────────────
// ClipboardWatcher
// ─────────────────────────────────────────────────────────────────────────────

pub struct ClipboardWatcher {
    last_sequence: u32,
}

impl ClipboardWatcher {
    pub fn new() -> Self {
        Self { last_sequence: 0 }
    }

    /// Consulta el portapapeles una vez.
    /// Retorna `Some` si hay archivos nuevos, `None` si no hay cambios o no hay CF_HDROP.
    pub fn poll(&mut self) -> Result<Option<ClipboardEvent>> {
        unsafe { self.poll_impl() }
    }

    /// Loop de monitoreo.
    ///
    /// # Argumentos
    ///
    /// * `interval_ms`    — intervalo de polling en milisegundos.
    /// * `runtime`        — runtime tokio compartido. Se reutiliza en cada evento
    ///                      para evitar que su `drop` cierre el canal del pipeline.
    /// * `dest_resolver`  — closure llamado para cada evento que retorna el directorio
    ///                      destino. Retorna `None` para cancelar ese evento.
    /// * `on_event`       — closure llamado con el evento y el destino resuelto.
    ///                      Retorna `true` para continuar el loop, `false` para detener.
    pub fn watch<D, F>(
        &mut self,
        interval_ms:   u64,
        runtime:       &tokio::runtime::Runtime,
        mut dest_resolver: D,
        mut on_event:  F,
    ) where
        D: FnMut(&ClipboardEvent) -> Option<PathBuf>,
        F: FnMut(ClipboardEvent, PathBuf, &tokio::runtime::Runtime) -> bool,
    {
        tracing::info!("ClipboardWatcher: iniciado (intervalo={}ms)", interval_ms);

        loop {
            match self.poll() {
                Ok(Some(event)) => {
                    tracing::info!(
                        "ClipboardWatcher: {} archivo(s) — {:?}",
                        event.paths.len(), event.operation
                    );

                    if let Some(dest) = dest_resolver(&event) {
                        let should_continue = on_event(event, dest, runtime);
                        if !should_continue {
                            tracing::info!("ClipboardWatcher: detenido por callback");
                            return;
                        }
                        // Limpiar portapapeles para no re-procesar
                        let _ = self.clear_clipboard();
                    } else {
                        tracing::info!("ClipboardWatcher: evento cancelado por dest_resolver");
                    }
                }
                Ok(None)  => {}
                Err(e)    => tracing::warn!("ClipboardWatcher error: {}", e),
            }

            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    }

    /// Limpia el portapapeles después de procesar un evento.
    pub fn clear_clipboard(&self) -> Result<()> {
        unsafe {
            use windows_sys::Win32::System::DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard,
            };
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                bail!("OpenClipboard falló: {}", std::io::Error::last_os_error());
            }
            EmptyClipboard();
            CloseClipboard();
        }
        Ok(())
    }

    unsafe fn poll_impl(&mut self) -> Result<Option<ClipboardEvent>> {
        use windows_sys::Win32::System::DataExchange::{
            GetClipboardSequenceNumber, IsClipboardFormatAvailable,
            OpenClipboard, CloseClipboard,
        };
        use windows_sys::Win32::System::Ole::CF_HDROP;

        let seq = GetClipboardSequenceNumber();
        if seq == self.last_sequence {
            return Ok(None);
        }

        if IsClipboardFormatAvailable(CF_HDROP.into()) == 0 {
            self.last_sequence = seq;
            return Ok(None);
        }

        if OpenClipboard(std::ptr::null_mut()) == 0 {
            bail!("OpenClipboard falló: {}", std::io::Error::last_os_error());
        }

        let result = self.read_clipboard_data();
        CloseClipboard();
        self.last_sequence = seq;
        result
    }

    unsafe fn read_clipboard_data(&self) -> Result<Option<ClipboardEvent>> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use windows_sys::Win32::System::Ole::CF_HDROP;
        use windows_sys::Win32::UI::Shell::DragQueryFileW;

        let hdrop_handle = GetClipboardData(CF_HDROP.into());
        if hdrop_handle.is_null() {
            return Ok(None);
        }

        let hdrop = GlobalLock(hdrop_handle as _);
        if hdrop.is_null() {
            return Ok(None);
        }

        let file_count = DragQueryFileW(hdrop_handle as _, 0xFFFF_FFFF, std::ptr::null_mut(), 0);
        let mut paths  = Vec::with_capacity(file_count as usize);

        for i in 0..file_count {
            let len = DragQueryFileW(hdrop_handle as _, i, std::ptr::null_mut(), 0) as usize;
            if len == 0 { continue; }
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop_handle as _, i, buf.as_mut_ptr(), buf.len() as u32);
            let s = String::from_utf16_lossy(&buf[..len]);
            paths.push(PathBuf::from(s));
        }

        GlobalUnlock(hdrop_handle as _);

        if paths.is_empty() {
            return Ok(None);
        }

        let operation = self.read_drop_effect().unwrap_or(ClipboardOperation::Copy);
        Ok(Some(ClipboardEvent { paths, operation }))
    }

    unsafe fn read_drop_effect(&self) -> Option<ClipboardOperation> {
        use windows_sys::Win32::System::DataExchange::{
            GetClipboardData, RegisterClipboardFormatW,
        };
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

        let name: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
        let fmt  = RegisterClipboardFormatW(name.as_ptr());
        if fmt == 0 { return None; }

        let handle = GetClipboardData(fmt);
        if handle.is_null() { return None; }

        let ptr = GlobalLock(handle as _) as *const u32;
        if ptr.is_null() { return None; }

        let effect = *ptr;
        GlobalUnlock(handle as _);

        match effect {
            1 => Some(ClipboardOperation::Copy),
            2 => Some(ClipboardOperation::Move),
            _ => Some(ClipboardOperation::Copy),
        }
    }
}

impl Default for ClipboardWatcher {
    fn default() -> Self { Self::new() }
}

// ─────────────────────────────────────────────────────────────────────────────
// Utilidades de UI para el daemon
// ─────────────────────────────────────────────────────────────────────────────

/// Muestra un diálogo de selección de carpeta usando PowerShell (sin dependencias extra).
///
/// Se usa cuando el usuario no especifica `--dest-dir` para que elija
/// el destino en cada operación. En entornos no interactivos, retorna `None`.
#[cfg(windows)]
pub fn prompt_folder_dialog(title: &str) -> Option<PathBuf> {
    let script = format!(
        r#"Add-Type -AssemblyName System.Windows.Forms;
$d = New-Object System.Windows.Forms.FolderBrowserDialog;
$d.Description = '{title}';
$d.ShowNewFolderButton = $true;
if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}"#,
        title = title.replace('\'', "\\'")
    );

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(PathBuf::from(path)) }
}