//! Windows autostart via `HKCU\...\Run`.
//!
//! Sets the `AgendTerminal` value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Same mechanism
//! Ollama uses on Windows. `HKCU` needs no admin rights.

use std::{os::windows::ffi::OsStrExt, path::PathBuf, ptr};

use windows_sys::Win32::{
    Foundation::ERROR_FILE_NOT_FOUND,
    System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
    },
};

use super::Autostart;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "AgendTerminal";

pub struct WindowsAutostart;

impl WindowsAutostart {
    pub fn new(_home: PathBuf) -> Self {
        Self
    }
}

/// NUL-terminated UTF-16 buffer as required by the -W registry APIs.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

impl Autostart for WindowsAutostart {
    fn enable(&self) -> anyhow::Result<()> {
        let exe = std::env::current_exe()?.canonicalize()?;
        // Quoting the exe path handles "Program Files" and friends; the
        // Run key treats the whole value as a command line.
        let value = format!("\"{}\" tray", exe.display());
        let key_w = wide(RUN_KEY);
        let name_w = wide(VALUE_NAME);
        let value_w = wide(&value);
        let byte_len: u32 = (value_w.len() * std::mem::size_of::<u16>())
            .try_into()
            .map_err(|_| anyhow::anyhow!("registry value too large"))?;

        // SAFETY: all pointers are into stack-owned buffers that outlive
        // each call; hkey is closed before return on every path.
        unsafe {
            let mut hkey: HKEY = ptr::null_mut();
            let rc = RegOpenKeyExW(HKEY_CURRENT_USER, key_w.as_ptr(), 0, KEY_WRITE, &mut hkey);
            if rc != 0 {
                anyhow::bail!("RegOpenKeyExW(HKCU\\{RUN_KEY}) failed: {rc}");
            }
            let rc = RegSetValueExW(
                hkey,
                name_w.as_ptr(),
                0,
                REG_SZ,
                value_w.as_ptr().cast::<u8>(),
                byte_len,
            );
            RegCloseKey(hkey);
            if rc != 0 {
                anyhow::bail!("RegSetValueExW({VALUE_NAME}) failed: {rc}");
            }
        }
        Ok(())
    }

    fn disable(&self) -> anyhow::Result<()> {
        let key_w = wide(RUN_KEY);
        let name_w = wide(VALUE_NAME);

        // SAFETY: same invariants as enable().
        unsafe {
            let mut hkey: HKEY = ptr::null_mut();
            let rc = RegOpenKeyExW(HKEY_CURRENT_USER, key_w.as_ptr(), 0, KEY_WRITE, &mut hkey);
            if rc != 0 {
                // Run key itself absent — treat as already disabled.
                return Ok(());
            }
            let rc = RegDeleteValueW(hkey, name_w.as_ptr());
            RegCloseKey(hkey);
            if rc != 0 && rc != ERROR_FILE_NOT_FOUND {
                anyhow::bail!("RegDeleteValueW({VALUE_NAME}) failed: {rc}");
            }
        }
        Ok(())
    }

    fn is_enabled(&self) -> anyhow::Result<bool> {
        let key_w = wide(RUN_KEY);
        let name_w = wide(VALUE_NAME);

        // SAFETY: same invariants as enable().
        unsafe {
            let mut hkey: HKEY = ptr::null_mut();
            let rc = RegOpenKeyExW(HKEY_CURRENT_USER, key_w.as_ptr(), 0, KEY_READ, &mut hkey);
            if rc != 0 {
                return Ok(false);
            }
            let rc = RegQueryValueExW(
                hkey,
                name_w.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            RegCloseKey(hkey);
            Ok(rc == 0)
        }
    }
}
