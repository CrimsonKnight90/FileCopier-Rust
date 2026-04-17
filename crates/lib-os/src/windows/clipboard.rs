//! crates/lib-os/src/windows/clipboard.rs
//! # clipboard (Windows)
//!
//! Interceptación del portapapeles del sistema operativo para archivos.
//!
//! ## Cómo funciona en Windows Explorer
//!
//! Cuando el usuario selecciona archivos y presiona `Ctrl+C` o `Ctrl+X`,
//! Explorer escribe en el portapapeles dos formatos simultáneamente:
//!
//! 1. **`CF_HDROP`** — lista de paths de los archivos seleccionados.
//!    Es un bloque de memoria con estructura `DROPFILES` seguida de paths
//!    null-separated en UTF-16, terminado en doble null.
//!
//! 2. **`"Preferred DropEffect"`** — DWORD que indica la operación:
//!    - `DROPEFFECT_COPY (1)` → Ctrl+C (copiar)
//!    - `DROPEFFECT_MOVE (2)` → Ctrl+X (mover)
//!
//! ## Flujo de intercepción
//!
//! ```text
//! Usuario: Ctrl+C / Ctrl+X en Explorer
//!       ↓
//! ClipboardWatcher::poll() llama a GetClipboardData(CF_HDROP)
//!       ↓
//! Lee "Preferred DropEffect" para determinar Copy vs Move
//!       ↓
//! Retorna ClipboardEvent { paths, operation }
//!       ↓
//! El daemon lanza FileCopier con esos paths como origen
//! ```
//!
//! ## Modo daemon
//!
//! `ClipboardWatcher` puede usarse de dos formas:
//! - `poll()` — llamada única, retorna `None` si no hay archivos en el portapapeles
//! - `watch(dest, callback)` — loop bloqueante que escucha cambios y lanza la operación
//!
//! ## Notas de seguridad
//!
//! - El watcher solo actúa cuando el portapapeles contiene `CF_HDROP`.
//! - No captura texto, imágenes ni otros formatos — solo archivos.
//! - Después de lanzar la operación, limpia el portapapeles para evitar
//!   re-ejecuciones accidentales.

use std::path::PathBuf;

use anyhow::{bail, Result};

// ─────────────────────────────────────────────────────────────────────────────
// Tipos públicos
// ─────────────────────────────────────────────────────────────────────────────

/// Operación solicitada por el usuario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    /// `Ctrl+C` en Explorer — copiar archivos.
    Copy,
    /// `Ctrl+X` en Explorer — mover archivos (borrar origen tras copia exitosa).
    Move,
}

/// Evento del portapapeles con archivos y operación.
#[derive(Debug, Clone)]
pub struct ClipboardEvent {
    /// Paths de los archivos/directorios seleccionados.
    pub paths:     Vec<PathBuf>,
    /// Operación solicitada (Copy o Move).
    pub operation: ClipboardOperation,
}

// ─────────────────────────────────────────────────────────────────────────────
// ClipboardWatcher
// ─────────────────────────────────────────────────────────────────────────────

/// Lee y monitorea el portapapeles de Windows para archivos copiados/cortados.
pub struct ClipboardWatcher {
    /// Número de secuencia del último portapapeles procesado.
    /// Permite detectar cambios sin polling agresivo.
    last_sequence: u32,
}

impl ClipboardWatcher {
    pub fn new() -> Self {
        Self { last_sequence: 0 }
    }

    /// Consulta el portapapeles una vez.
    ///
    /// Retorna `Some(ClipboardEvent)` si hay archivos en el portapapeles.
    /// Retorna `None` si el portapapeles no contiene `CF_HDROP`.
    pub fn poll(&mut self) -> Result<Option<ClipboardEvent>> {
        unsafe { self.poll_impl() }
    }

    /// Loop de monitoreo con intervalo de polling configurable.
    ///
    /// Llama a `callback(event)` cada vez que detecta un nuevo evento de
    /// archivos en el portapapeles. El callback recibe el evento y debe
    /// retornar `true` para continuar el loop o `false` para detenerse.
    ///
    /// `interval_ms` — intervalo de polling en milisegundos (recomendado: 500)
    pub fn watch<F>(
        &mut self,
        interval_ms: u64,
        mut callback: F,
    ) where
        F: FnMut(ClipboardEvent) -> bool,
    {
        tracing::info!(
            "ClipboardWatcher: iniciado (intervalo={}ms)",
            interval_ms
        );

        loop {
            match self.poll() {
                Ok(Some(event)) => {
                    tracing::info!(
                        "ClipboardWatcher: {} archivo(s) detectados ({:?})",
                        event.paths.len(),
                        event.operation
                    );
                    let should_continue = callback(event);
                    if !should_continue {
                        tracing::info!("ClipboardWatcher: detenido por callback");
                        return;
                    }
                    // Limpiar portapapeles para evitar re-procesamiento
                    let _ = self.clear_clipboard();
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("ClipboardWatcher: error leyendo portapapeles: {}", e);
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    }

    /// Limpia el portapapeles después de procesar un evento.
    pub fn clear_clipboard(&self) -> Result<()> {
        unsafe {
            use windows_sys::Win32::System::DataExchange::{OpenClipboard, EmptyClipboard, CloseClipboard};
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                bail!("OpenClipboard falló: {}", std::io::Error::last_os_error());
            }
            EmptyClipboard();
            CloseClipboard();
        }
        Ok(())
    }

    /// Implementación unsafe de poll usando WinAPI.
    unsafe fn poll_impl(&mut self) -> Result<Option<ClipboardEvent>> {
        use windows_sys::Win32::System::DataExchange::{
            GetClipboardSequenceNumber, OpenClipboard, CloseClipboard,
            IsClipboardFormatAvailable,
        };
        use windows_sys::Win32::System::Ole::CF_HDROP;

        // ── Verificar si el portapapeles cambió desde la última consulta ──
        let seq = GetClipboardSequenceNumber();
        if seq == self.last_sequence {
            return Ok(None);
        }

        // ── Verificar que hay CF_HDROP disponible ─────────────────────────
        if IsClipboardFormatAvailable(CF_HDROP as u32) == 0 {
            self.last_sequence = seq;
            return Ok(None);
        }

        // ── Abrir portapapeles ─────────────────────────────────────────────
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            bail!("OpenClipboard falló: {}", std::io::Error::last_os_error());
        }

        let result = self.read_clipboard_data();
        CloseClipboard();
        self.last_sequence = seq;
        result
    }

    /// Lee los datos del portapapeles (debe llamarse con el portapapeles abierto).
    unsafe fn read_clipboard_data(&self) -> Result<Option<ClipboardEvent>> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use windows_sys::Win32::System::Ole::CF_HDROP;
        use windows_sys::Win32::UI::Shell::DragQueryFileW;

        // ── Leer CF_HDROP — lista de paths ────────────────────────────────
        let hdrop_handle = GetClipboardData(CF_HDROP as u32);
        if hdrop_handle.is_null() {
            return Ok(None);
        }

        let hdrop = GlobalLock(hdrop_handle as _);
        if hdrop.is_null() {
            return Ok(None);
        }

        // DragQueryFileW(hDrop, 0xFFFFFFFF, null, 0) retorna el número de archivos
        let file_count = DragQueryFileW(
            hdrop_handle as _,
            0xFFFF_FFFF,
            std::ptr::null_mut(),
            0,
        );

        let mut paths = Vec::with_capacity(file_count as usize);

        for i in 0..file_count {
            // Primera llamada: obtener longitud necesaria
            let len = DragQueryFileW(hdrop_handle as _, i, std::ptr::null_mut(), 0) as usize;
            if len == 0 { continue; }

            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop_handle as _, i, buf.as_mut_ptr(), buf.len() as u32);

            // Convertir UTF-16 a PathBuf
            let path_str = String::from_utf16_lossy(&buf[..len]);
            paths.push(PathBuf::from(path_str));
        }

        GlobalUnlock(hdrop_handle as _);

        if paths.is_empty() {
            return Ok(None);
        }

        // ── Leer "Preferred DropEffect" ───────────────────────────────────
        let operation = self.read_drop_effect().unwrap_or(ClipboardOperation::Copy);

        Ok(Some(ClipboardEvent { paths, operation }))
    }

    /// Lee el formato "Preferred DropEffect" para determinar Copy vs Move.
    unsafe fn read_drop_effect(&self) -> Option<ClipboardOperation> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

        // Registrar el formato "Preferred DropEffect" (nombre estándar de Shell)
        let format_name: Vec<u16> = "Preferred DropEffect\0"
            .encode_utf16()
            .collect();
        let format_id = windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW(format_name.as_ptr());
        if format_id == 0 {
            return None;
        }

        let handle = GetClipboardData(format_id);
        if handle.is_null() {
            return None;
        }

        let ptr = GlobalLock(handle as _) as *const u32;
        if ptr.is_null() {
            return None;
        }

        let drop_effect = *ptr;
        GlobalUnlock(handle as _);

        // DROPEFFECT_COPY = 1, DROPEFFECT_MOVE = 2
        match drop_effect {
            1 => Some(ClipboardOperation::Copy),
            2 => Some(ClipboardOperation::Move),
            _ => Some(ClipboardOperation::Copy), // default conservador
        }
    }
}

impl Default for ClipboardWatcher {
    fn default() -> Self {
        Self::new()
    }
}