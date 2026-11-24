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
    use_xkb_stand_in(&dir);
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
fn use_xkb_stand_in(fallback: &Path) {
    const SYSTEM: &str = "/usr/share/X11/xkb";
    if std::env::var_os("XKB_CONFIG_ROOT").is_some() || Path::new(SYSTEM).is_dir() {
        return;
    }
    let Some(stand_in) = fallback.parent().and_then(Path::parent).map(|usr| usr.join("share/X11/xkb")) else {
        return;
    };
    if stand_in.is_dir() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("XKB_CONFIG_ROOT", &stand_in) };
        crate::log::info(&format!("no {SYSTEM} on this system; using the AppImage's empty stand-in"));
    }
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
    fn does_nothing_outside_an_appimage() {
        // The test binary has no lib/fallback next to it.
        assert!(fallback_dir().is_none());
        load_fallback_libraries();
    }
}
