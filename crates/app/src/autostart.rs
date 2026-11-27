//! Starting HasteClip with the computer: the user's startup list on Windows
//! (the registry's Run key, no admin needed), a login item (LaunchAgent) on
//! macOS, an autostart entry on Linux. It starts in the tray (`--background`),
//! or with its window open if Settings' Start in the tray is off: that's read
//! at launch (`RecordSettings::start_in_tray`), so the entry is the same
//! either way. The switch reads and writes that entry itself, so it always
//! shows what's really set, and nothing changes unless it's flipped.

/// The launch argument for starting with the computer: in the tray, unless
/// it's set to open its window then.
pub const BACKGROUND: &str = "--background";

/// What the switch is called here.
pub fn label() -> &'static str {
    if cfg!(windows) {
        "Start with Windows"
    } else if cfg!(target_os = "macos") {
        "Open at login"
    } else {
        "Start when you log in"
    }
}

/// Whether HasteClip starts with the computer now.
pub fn is_on() -> bool {
    imp::is_on()
}

/// Start with the computer, or not.
pub fn set(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Only turning it off is allowed from a copy that isn't the installed app.
    if on && let Some(why) = not_installed(&exe) {
        return Err(why.into());
    }
    imp::set(on, &exe)
}

/// Why `exe` can't be what starts with the computer, if it can't. A build
/// from the source folder isn't the installed app: starting it with the
/// computer would launch the developer's build. A Mac app opened straight
/// from its disk image, or from where it was downloaded (macOS runs that
/// from a copy of its own), won't be there when you next log in.
fn not_installed(exe: &std::path::Path) -> Option<&'static str> {
    if exe.components().any(|c| c.as_os_str() == "target") {
        return Some("only the installed app can start with the computer");
    }
    if cfg!(target_os = "macos") && (exe.starts_with("/Volumes") || exe.components().any(|c| c.as_os_str() == "AppTranslocation")) {
        return Some("move HasteClip to Applications first");
    }
    None
}

/// A new install's first launch (see `RecordSettings::autostart_pending`):
/// HasteClip starts with the computer from now on, in the tray, as a clipper
/// only saves what happens while it runs. It's the one time the entry changes
/// without the switch being flipped, besides uninstalling on Windows (the
/// installer's hook, in `main`, takes it away). Not on a Linux desktop without a
/// tray: started there, it would open its window every time you log in.
/// Whether that's settled: not while this copy isn't the installed app (a
/// build of your own, a Mac app opened from its disk image), so the
/// installed one does it the first time it's opened.
pub fn new_install() -> bool {
    let Ok(exe) = std::env::current_exe() else { return true };
    if let Some(why) = not_installed(&exe) {
        crate::logfile::tell(&format!("Not starting with the computer yet: {why}"));
        return false;
    }
    if !crate::tray::available() {
        crate::logfile::tell("Not starting with the computer: there's no tray here to start in");
        return true;
    }
    match imp::set(true, &exe) {
        Ok(()) => eprintln!("A new install: \"{}\" is on", label()),
        Err(e) => crate::logfile::tell(&format!("Couldn't start with the computer: {e}")),
    }
    true
}

#[cfg(windows)]
mod imp {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
    use windows::core::HSTRING;

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "HasteClip";

    pub fn is_on() -> bool {
        let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
        // SAFETY: only asks whether the value exists (no buffer).
        unsafe { RegGetValueW(HKEY_CURRENT_USER, &key, &name, RRF_RT_REG_SZ, None, None, None) }.is_ok()
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let (key, name) = (HSTRING::from(KEY), HSTRING::from(NAME));
        if !on {
            // SAFETY: plain strings.
            let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
            return if r.is_ok() || !is_on() { Ok(()) } else { Err(format!("couldn't remove it ({r:?})")) };
        }
        let command = format!("\"{}\" {}", exe.display(), super::BACKGROUND);
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the data is a NUL-terminated UTF-16 string, its size in bytes.
        let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, &key, &name, REG_SZ.0, Some(wide.as_ptr().cast()), (wide.len() * 2) as u32) };
        if r.is_ok() { Ok(()) } else { Err(format!("couldn't add it ({r:?})")) }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    fn plist() -> Option<std::path::PathBuf> {
        Some(dirs::home_dir()?.join("Library").join("LaunchAgents").join("com.hasteclip.app.plist"))
    }

    pub fn is_on() -> bool {
        plist().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let path = plist().ok_or("no home folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>com.hasteclip.app</string>\n<key>ProgramArguments</key><array><string>{}</string><string>{}</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
            exe.display(),
            super::BACKGROUND
        );
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(path, xml).map_err(|e| e.to_string())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    fn entry() -> Option<std::path::PathBuf> {
        Some(dirs::config_dir()?.join("autostart").join("hasteclip.desktop"))
    }

    pub fn is_on() -> bool {
        entry().is_some_and(|p| p.exists())
    }

    pub fn set(on: bool, exe: &std::path::Path) -> Result<(), String> {
        let path = entry().ok_or("no config folder")?;
        if !on {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            };
        }
        // KDE gives its screencast and window list to the first entry it finds
        // whose Exec is us, so this one asks for them too, like the app's own.
        let desktop = format!(
            "[Desktop Entry]\nType=Application\nName=HasteClip\nExec=\"{}\" {}\nX-GNOME-Autostart-enabled=true\nX-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_plasma_window_management\n",
            exe.display(),
            super::BACKGROUND
        );
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(path, desktop).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Only the installed app starts with the computer: not a build of your
    /// own, nor a Mac app opened from its disk image or from Downloads.
    #[test]
    fn only_the_installed_app_starts_with_the_computer() {
        assert!(not_installed(Path::new("/home/me/hasteclip/target/release/hasteclip")).is_some());
        assert!(not_installed(Path::new(r"C:\Users\me\AppData\Local\HasteClip\current\hasteclip.exe")).is_none());
        assert!(not_installed(Path::new("/Applications/HasteClip.app/Contents/MacOS/hasteclip")).is_none());
        assert!(not_installed(Path::new("/home/me/Apps/HasteClip/hasteclip")).is_none());
        let mac = cfg!(target_os = "macos");
        assert_eq!(not_installed(Path::new("/Volumes/HasteClip/HasteClip.app/Contents/MacOS/hasteclip")).is_some(), mac);
        let downloaded = "/private/var/folders/x1/abc/T/AppTranslocation/0F1E/d/HasteClip.app/Contents/MacOS/hasteclip";
        assert_eq!(not_installed(Path::new(downloaded)).is_some(), mac);
    }
}
