//! # detect
//!
//! Detección de tipo de hardware de almacenamiento.
//!
//! ## Windows
//!
//! Usa `IOCTL_STORAGE_QUERY_PROPERTY` con `StorageDeviceSeekPenaltyProperty`:
//!   - `IncursSeekPenalty = 0` → SSD / NVMe
//!   - `IncursSeekPenalty = 1` → HDD mecánico
//!
//! ## Linux
//!
//! Lee `/sys/block/<dev>/queue/rotational`: `0` → SSD, `1` → HDD.

use std::path::Path;

use crate::traits::DriveKind;

/// Detecta el tipo de unidad para `path` en la plataforma actual.
pub fn detect_drive_kind(path: &Path) -> DriveKind {
    #[cfg(windows)]
    return windows_detect(path);

    #[cfg(unix)]
    return linux_detect(path);

    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        DriveKind::Unknown
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Windows
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(windows)]
fn windows_detect(path: &Path) -> DriveKind {
    let root = match extract_drive_root(path) {
        Some(r) => r,
        None    => return DriveKind::Network,
    };

    let root_wide = encode_wide(&root);

    let drive_type = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root_wide.as_ptr())
    };

    match drive_type {
        4 => DriveKind::Network,
        3 => {
            // DRIVE_FIXED: consultar IOCTL para SSD vs HDD
            let letter = &root[..2]; // "C:"
            detect_seek_penalty(letter).unwrap_or(DriveKind::Hdd)
        }
        2 => DriveKind::Hdd,   // DRIVE_REMOVABLE
        _ => DriveKind::Unknown,
    }
}

/// Consulta IOCTL_STORAGE_QUERY_PROPERTY para determinar SSD vs HDD.
/// `drive_letter` = `"C:"` (sin barra final).
#[cfg(windows)]
fn detect_seek_penalty(drive_letter: &str) -> Option<DriveKind> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    // Abrir "\\.\C:" con acceso 0 (solo consulta de propiedades IOCTL).
    // No requiere permisos de administrador para volúmenes locales.
    let volume_path = format!("\\\\.\\{drive_letter}");
    let volume_wide = encode_wide(&volume_path);

    let handle = unsafe {
        CreateFileW(
            volume_wide.as_ptr(),
            0,                                  // dwDesiredAccess = 0 (consulta)
            FILE_SHARE_READ | FILE_SHARE_WRITE, // dwShareMode
            std::ptr::null(),                   // lpSecurityAttributes
            OPEN_EXISTING,                      // dwCreationDisposition
            0,                                  // dwFlagsAndAttributes
            std::ptr::null_mut(),               // hTemplateFile ← debe ser *mut c_void
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        tracing::debug!(
            "No se pudo abrir '{}' para IOCTL: {} (fallback=Hdd)",
            volume_path,
            std::io::Error::last_os_error()
        );
        return None;
    }

    let result = query_seek_penalty(handle);
    unsafe { CloseHandle(handle) };
    result
}

/// Envía IOCTL_STORAGE_QUERY_PROPERTY y devuelve SSD o HDD.
#[cfg(windows)]
fn query_seek_penalty(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<DriveKind> {
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const IOCTL_STORAGE_QUERY_PROPERTY:         u32 = 0x002D_1400;
    const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: i32 = 7;
    const PROPERTY_STANDARD_QUERY:              i32 = 0;

    #[repr(C)]
    struct StoragePropertyQuery {
        property_id:           i32,
        query_type:            i32,
        additional_parameters: [u8; 1],
    }

    #[repr(C)]
    struct DeviceSeekPenaltyDescriptor {
        version:             u32,
        size:                u32,
        incurs_seek_penalty: u8, // 0 = SSD, 1 = HDD
    }

    let query = StoragePropertyQuery {
        property_id:           STORAGE_DEVICE_SEEK_PENALTY_PROPERTY,
        query_type:            PROPERTY_STANDARD_QUERY,
        additional_parameters: [0u8],
    };

    let mut out   = DeviceSeekPenaltyDescriptor { version: 0, size: 0, incurs_seek_penalty: 0 };
    let mut bytes: u32 = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query  as *const _ as *const _,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            &mut out as *mut _   as *mut _,
            std::mem::size_of::<DeviceSeekPenaltyDescriptor>() as u32,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };

    if ok == 0 {
        tracing::debug!(
            "DeviceIoControl SEEK_PENALTY falló: {}",
            std::io::Error::last_os_error()
        );
        return None;
    }

    let kind = if out.incurs_seek_penalty == 0 {
        DriveKind::Ssd
    } else {
        DriveKind::Hdd
    };

    tracing::debug!("IOCTL seek_penalty={} → {:?}", out.incurs_seek_penalty, kind);
    Some(kind)
}

/// Extrae `"C:\\"` de cualquier path Windows sin llamar a `canonicalize()`.
#[cfg(windows)]
fn extract_drive_root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    if s.starts_with("\\\\") || s.starts_with("//") {
        return None; // UNC → red
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic() {
        return Some(format!("{}:\\", (b[0] as char).to_ascii_uppercase()));
    }
    None
}

/// Convierte `&str` a `Vec<u16>` terminado en null para WinAPI.
#[cfg(windows)]
fn encode_wide(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Linux / Unix
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn linux_detect(path: &Path) -> DriveKind {
    match find_block_device(path) {
        Some(dev) => read_rotational(&dev),
        None      => DriveKind::Unknown,
    }
}

#[cfg(unix)]
fn find_block_device(path: &Path) -> Option<String> {
    let mounts   = std::fs::read_to_string("/proc/mounts").ok()?;
    let abs_path = path.canonicalize().ok()?;
    let abs_str  = abs_path.to_string_lossy();

    let mut best_mount = "";
    let mut best_dev   = "";

    for line in mounts.lines() {
        let mut parts  = line.split_whitespace();
        let device     = parts.next()?;
        let mountpoint = parts.next()?;
        if abs_str.starts_with(mountpoint) && mountpoint.len() > best_mount.len() {
            best_mount = mountpoint;
            best_dev   = device;
        }
    }

    if best_dev.is_empty() { return None; }
    let dev_name = Path::new(best_dev).file_name()?.to_str()?;
    Some(strip_partition_number(dev_name).to_string())
}

#[cfg(unix)]
fn strip_partition_number(dev: &str) -> &str {
    if dev.contains('p') {
        if let Some(pos) = dev.rfind('p') {
            if dev[pos + 1..].chars().all(|c| c.is_ascii_digit()) {
                return &dev[..pos];
            }
        }
    }
    let end = dev.trim_end_matches(|c: char| c.is_ascii_digit());
    if end.len() < dev.len() { end } else { dev }
}

#[cfg(unix)]
fn read_rotational(dev_name: &str) -> DriveKind {
    match std::fs::read_to_string(format!("/sys/block/{dev_name}/queue/rotational")) {
        Ok(s) => match s.trim() { "0" => DriveKind::Ssd, "1" => DriveKind::Hdd, _ => DriveKind::Unknown },
        Err(_) => DriveKind::Unknown,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_current_dir_does_not_panic() {
        let kind = detect_drive_kind(Path::new("."));
        println!("DriveKind para '.': {kind:?}");
    }

    #[cfg(windows)]
    #[test]
    fn extract_drive_root_standard() {
        assert_eq!(
            extract_drive_root(Path::new(r"C:\Users\herna\Documents")),
            Some("C:\\".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_drive_root_forward_slashes() {
        assert_eq!(
            extract_drive_root(Path::new("D:/Games")),
            Some("D:\\".to_string())
        );
    }

    #[cfg(windows)]
    #[test]
    fn extract_drive_root_unc_is_none() {
        assert_eq!(
            extract_drive_root(Path::new(r"\\server\share\file")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn strip_partition_sda() {
        assert_eq!(strip_partition_number("sda1"), "sda");
        assert_eq!(strip_partition_number("sda"),  "sda");
    }

    #[cfg(unix)]
    #[test]
    fn strip_partition_nvme() {
        assert_eq!(strip_partition_number("nvme0n1p1"), "nvme0n1");
        assert_eq!(strip_partition_number("nvme0n1"),   "nvme0n1");
    }
}