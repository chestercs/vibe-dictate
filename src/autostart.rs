use anyhow::{Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APP_NAME: &str = "vibe-dictate";

pub fn set_enabled(enabled: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = hkcu
        .open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_WRITE)
        .context("open Run key")?;
    if enabled {
        let exe = std::env::current_exe().context("current_exe")?;
        let exe_str = format!("\"{}\"", exe.display());
        run.set_value(APP_NAME, &exe_str)
            .context("set Run value")?;
    } else {
        // delete_value returns error if missing — ignore
        let _ = run.delete_value(APP_NAME);
    }
    Ok(())
}

pub fn is_enabled() -> Result<bool> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = hkcu
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .context("open Run key")?;
    Ok(run.get_value::<String, _>(APP_NAME).is_ok())
}
