//! Window system libraries for the Linux AppImage.
//!
//! winit opens libX11, libxkbcommon-x11, libwayland-client and friends with
//! dlopen when the window is created, so they never show up in `ldd` and
//! AppImage tools don't bundle them. On a system without one of them (often
//! libxkbcommon-x11) Gitust used to abort before showing a window.
//!
//! The AppImage carries copies in `usr/lib/fallback` (scripts/appimage/build.sh).
//! Before the first window, every library this session needs is tried from
//! the system; if all are there nothing changes. Otherwise the system's copies
//! stay loaded and the bundled ones fill the gaps. glibc hands out a library
//! that's already loaded to any later dlopen of the same soname, so winit ends
//! up with exactly these copies. The system always wins over the bundle, so
//! the AppImage keeps using the host's (newer) libraries wherever it can.
//! Outside an AppImage there is no fallback folder and this does nothing.

use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `<session> <soname>` lines, dependencies first. The build script reads the same file.
const LIBS: &str = include_str!("../assets/linux/appimage-libs.txt");

/// Must run before any other thread starts (it may set an environment variable).
pub fn load_fallback_libraries() {
    let Some(dir) = fallback_dir() else { return };
    let usr = dir.parent().and_then(Path::parent).unwrap_or(&dir).to_path_buf();
    use_xkb_stand_in(&usr);
    use_font_stand_in(&usr);
    let env = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let wayland = env("WAYLAND_DISPLAY") || env("WAYLAND_SOCKET");
    // No display variable at all: winit tries X11, so that's what to prepare.
    let x11 = env("DISPLAY") || !wayland;

    let mut system = Vec::new();
    let mut missing = Vec::new();
    for soname in needed(LIBS, x11, wayland) {
        match open(&CString::new(soname).expect("no NUL in sonames")) {
            Some(handle) => system.push(handle),
            None => missing.push(soname),
        }
    }
    if missing.is_empty() {
        // Everything is on the system: let winit load it as it would anyway.
        for handle in system {
            // SAFETY: each handle came from a successful dlopen above and is closed once.
            unsafe { libc::dlclose(handle) };
        }
        return;
    }

    // The system's copies stay loaded, so the bundled libraries link against them.
    let mut bundled = Vec::new();
    for soname in missing {
        let path = dir.join(soname);
        match CString::new(path.as_os_str().as_bytes()).ok().and_then(|p| open(&p)) {
            Some(_) => bundled.push(soname),
            None => crate::log::error(&format!("couldn't load the bundled {soname}: {}", last_error())),
        }
    }
    crate::log::info(&format!("using bundled libraries the system lacks: {}", bundled.join(", ")));
}

/// libxkbcommon won't create a context unless its data folder exists, even
/// though X11 and Wayland both hand it the keymap ready-made; winit then
/// panics. Desktops always have the folder (xkb-data / xkeyboard-config), bare
/// systems don't, so the AppImage carries an empty stand-in for them.
fn use_xkb_stand_in(usr: &Path) {
    const SYSTEM: &str = "/usr/share/X11/xkb";
    if std::env::var_os("XKB_CONFIG_ROOT").is_some() || Path::new(SYSTEM).is_dir() {
        return;
    }
    let stand_in = usr.join("share/X11/xkb");
    if stand_in.is_dir() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("XKB_CONFIG_ROOT", &stand_in) };
        crate::log::info(&format!("no {SYSTEM} on this system; using the AppImage's empty stand-in"));
    }
}

/// Slint's software renderer needs at least one font and only embeds one on
/// the web, so a system with no fonts (or no fontconfig to find them) would
/// panic at the first text. Desktops always have fonts; for the rest, the
/// AppImage carries Inter, the font Slint itself falls back to elsewhere.
fn use_font_stand_in(usr: &Path) {
    if std::env::var_os("SLINT_DEFAULT_FONT").is_some() || system_has_fonts() {
        return;
    }
    let font = usr.join("share/fonts/gitust/Inter-VariableFont.ttf");
    if font.is_file() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("SLINT_DEFAULT_FONT", &font) };
        crate::log::info("no system fonts found; using the AppImage's built-in font");
    }
}

/// Whether fontconfig is there and some scalable font is in a folder it reads.
fn system_has_fonts() -> bool {
    let Some(fontconfig) = open(c"libfontconfig.so.1") else { return false };
    // SAFETY: the handle came from a successful dlopen just above.
    unsafe { libc::dlclose(fontconfig) };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data_home =
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    [
        Some(PathBuf::from("/usr/share/fonts")),
        Some(PathBuf::from("/usr/local/share/fonts")),
        data_home.map(|d| d.join("fonts")),
        home.map(|h| h.join(".fonts")),
    ]
    .into_iter()
    .flatten()
    .any(|dir| has_font_file(&dir, 4))
}

/// Stops at the first .ttf/.otf/.ttc found, `depth` folders down at most.
fn has_font_file(dir: &Path, depth: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            depth > 0 && has_font_file(&path, depth - 1)
        } else {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| ["ttf", "otf", "ttc"].contains(&e.to_ascii_lowercase().as_str()))
        }
    })
}

/// `usr/lib/fallback` next to `usr/bin/gitust`, when it exists.
fn fallback_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("lib/fallback");
    dir.is_dir().then_some(dir)
}

/// The sonames to check for this session, in load order, without repeats.
fn needed(list: &str, x11: bool, wayland: bool) -> Vec<&str> {
    let mut out = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((session, soname)) = line.split_once(char::is_whitespace) else { continue };
        let soname = soname.trim();
        let wanted = match session {
            "x11" => x11,
            "wayland" => wayland,
            _ => true,
        };
        if wanted && !out.contains(&soname) {
            out.push(soname);
        }
    }
    out
}

fn open(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: `name` is a valid C string. The libraries are the standard window
    // system ones winit itself loads; their initialisers have no side effects.
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    (!handle.is_null()).then_some(handle)
}

fn last_error() -> String {
    // SAFETY: dlerror returns null or a C string valid until the next dl* call.
    let err = unsafe { libc::dlerror() };
    if err.is_null() { "unknown error".into() } else { unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_libraries_for_the_session() {
        let x11 = needed(LIBS, true, false);
        assert!(x11.contains(&"libxkbcommon-x11.so.0") && x11.contains(&"libxkbcommon.so.0"));
        assert!(!x11.contains(&"libwayland-client.so.0"));

        let wayland = needed(LIBS, false, true);
        assert!(wayland.contains(&"libwayland-client.so.0") && wayland.contains(&"libxkbcommon.so.0"));
        assert!(!wayland.contains(&"libX11.so.6"));

        let both = needed(LIBS, true, true);
        assert_eq!(both.iter().filter(|s| **s == "libxkbcommon.so.0").count(), 1);
        // Dependencies load first: libxkbcommon-x11 needs libxkbcommon and libxcb.
        let at = |s: &str| both.iter().position(|x| *x == s).unwrap();
        assert!(at("libxcb.so.1") < at("libxkbcommon-x11.so.0"));
        assert!(at("libxkbcommon.so.0") < at("libxkbcommon-x11.so.0"));
    }

    #[test]
    fn every_line_is_well_formed() {
        for line in LIBS.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (session, soname) = line.split_once(' ').unwrap();
            assert!(["x11", "wayland", "any"].contains(&session), "{line}");
            assert!(soname.starts_with("lib") && soname.contains(".so."), "{line}");
        }
    }

    #[test]
    fn finds_font_files_in_nested_folders() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!has_font_file(dir.path(), 4));
        let deep = dir.path().join("truetype/dejavu");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("README"), "").unwrap();
        assert!(!has_font_file(dir.path(), 4));
        std::fs::write(deep.join("DejaVuSans.TTF"), "").unwrap();
        assert!(has_font_file(dir.path(), 4));
        assert!(!has_font_file(dir.path(), 1), "too deep for depth 1");
    }

    #[test]
    fn does_nothing_outside_an_appimage() {
        // The test binary has no lib/fallback next to it.
        assert!(fallback_dir().is_none());
        load_fallback_libraries();
    }
}
