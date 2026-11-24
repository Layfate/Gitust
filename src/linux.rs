//! Window system libraries, keyboard data and a font for the Linux AppImage.
//!
//! winit opens libX11, libxkbcommon-x11, libwayland-client and friends with
//! dlopen, so they never show up in `ldd` and AppImage tools don't bundle
//! them. On a system without one of them (often libxkbcommon-x11) Gitust used
//! to abort before showing a window.
//!
//! The AppImage carries copies in `usr/lib/fallback` (scripts/appimage/build.sh).
//! Before anything else runs, every library this session needs is looked for
//! on the system; if all are there nothing changes. Otherwise the bundled
//! copies of the missing ones are loaded, and glibc hands a library that's
//! already loaded to any later dlopen of the same soname, so winit ends up with
//! exactly these copies. The system wins wherever it has a library, except
//! that libraries built as a matched set (libxkbcommon and libxkbcommon-x11,
//! say) always come from the same side: mixing versions of those crashes.
//! Outside an AppImage there is no fallback folder and this does nothing.

use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `<session> <soname> [set]` lines, dependencies first. The build script reads the same file.
const LIBS: &str = include_str!("../assets/linux/appimage-libs.txt");

/// Runs before `main` and before other crates' constructors. tiny-xlib (under
/// softbuffer) loads libX11 in a constructor of its own, so on a system
/// without libX11 the bundled copy has to be loaded by then. Numbered
/// `.init_array` entries run before unnumbered ones (GNU ld and lld both sort
/// them), and 101–65535 is the range left to programs.
#[used]
#[unsafe(link_section = ".init_array.00200")]
static PREPARE: extern "C" fn() = prepare;

/// What `prepare` did, for the log, which isn't open yet when it runs: (error, message).
static NOTES: Mutex<Vec<(bool, String)>> = Mutex::new(Vec::new());

extern "C" fn prepare() {
    let Some(dir) = fallback_dir() else { return };
    let usr = dir.parent().and_then(Path::parent).unwrap_or(&dir).to_path_buf();
    use_xkb_stand_in(&usr);
    use_font_stand_in(&usr);

    let env = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    let wayland = env("WAYLAND_DISPLAY") || env("WAYLAND_SOCKET");
    // No display variable at all: winit tries X11, so that's what to prepare.
    let x11 = env("DISPLAY") || !wayland;
    let libs = needed(LIBS, x11, wayland);

    // Look without keeping anything loaded, so the choice below starts clean.
    let missing: Vec<&str> = libs.iter().map(|l| l.soname).filter(|s| !on_system(s)).collect();
    if missing.is_empty() {
        return;
    }
    let bundled = from_bundle(&libs, &missing);
    for lib in &libs {
        let name = if bundled.contains(&lib.soname) {
            dir.join(lib.soname).as_os_str().as_bytes().to_vec()
        } else {
            // The system's copy, loaded now so the bundled libraries link against it.
            lib.soname.as_bytes().to_vec()
        };
        if CString::new(name).ok().and_then(|n| open(&n)).is_none() {
            note(true, format!("couldn't load {}: {}", lib.soname, last_error()));
        }
    }
    note(false, format!("using bundled libraries: {}", bundled.join(", ")));
}

/// Writes what happened before `main` to the log.
pub fn log_startup_notes() {
    let notes = std::mem::take(&mut *NOTES.lock().unwrap_or_else(|e| e.into_inner()));
    for (error, msg) in notes {
        if error { crate::log::error(&msg) } else { crate::log::info(&msg) }
    }
}

fn note(error: bool, msg: String) {
    NOTES.lock().unwrap_or_else(|e| e.into_inner()).push((error, msg));
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
        // SAFETY: runs before main, while the program has a single thread.
        unsafe { std::env::set_var("XKB_CONFIG_ROOT", &stand_in) };
        note(false, format!("no {SYSTEM} on this system; using the AppImage's empty stand-in"));
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
        // SAFETY: runs before main, while the program has a single thread.
        unsafe { std::env::set_var("SLINT_DEFAULT_FONT", &font) };
        note(false, "no system fonts found; using the AppImage's built-in font".into());
    }
}

/// Whether fontconfig is there and some scalable font is in a folder it reads.
fn system_has_fonts() -> bool {
    if !on_system("libfontconfig.so.1") {
        return false;
    }
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

#[derive(Debug, Clone, Copy, PartialEq)]
struct Lib<'a> {
    soname: &'a str,
    /// Libraries in the same set must come from the same build.
    set: Option<&'a str>,
}

/// The libraries to check for this session, in load order, without repeats.
fn needed(list: &str, x11: bool, wayland: bool) -> Vec<Lib<'_>> {
    let mut out: Vec<Lib> = Vec::new();
    for line in list.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut words = line.split_whitespace();
        let (Some(session), Some(soname)) = (words.next(), words.next()) else { continue };
        let wanted = match session {
            "x11" => x11,
            "wayland" => wayland,
            _ => true,
        };
        if wanted && !out.iter().any(|l| l.soname == soname) {
            out.push(Lib { soname, set: words.next() });
        }
    }
    out
}

/// The libraries to take from the bundle, in load order: the ones the system
/// lacks, plus the rest of any set one of them belongs to.
fn from_bundle<'a>(libs: &[Lib<'a>], missing: &[&str]) -> Vec<&'a str> {
    let sets: Vec<&str> = libs.iter().filter(|l| missing.contains(&l.soname)).filter_map(|l| l.set).collect();
    libs.iter()
        .filter(|l| missing.contains(&l.soname) || l.set.is_some_and(|s| sets.contains(&s)))
        .map(|l| l.soname)
        .collect()
}

/// Whether the system has `soname`, leaving nothing loaded.
fn on_system(soname: &str) -> bool {
    let Some(handle) = CString::new(soname).ok().and_then(|n| open(&n)) else { return false };
    // SAFETY: the handle came from a successful dlopen just above and is closed once.
    unsafe { libc::dlclose(handle) };
    true
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

    fn sonames<'a>(libs: &[Lib<'a>]) -> Vec<&'a str> {
        libs.iter().map(|l| l.soname).collect()
    }

    #[test]
    fn picks_the_libraries_for_the_session() {
        let x11 = sonames(&needed(LIBS, true, false));
        assert!(x11.contains(&"libxkbcommon-x11.so.0") && x11.contains(&"libxkbcommon.so.0"));
        assert!(!x11.contains(&"libwayland-client.so.0"));

        let wayland = sonames(&needed(LIBS, false, true));
        assert!(wayland.contains(&"libwayland-client.so.0") && wayland.contains(&"libxkbcommon.so.0"));
        assert!(!wayland.contains(&"libX11.so.6"));

        let both = sonames(&needed(LIBS, true, true));
        assert_eq!(both.iter().filter(|s| **s == "libxkbcommon.so.0").count(), 1);
        // Dependencies load first: libxkbcommon-x11 needs libxkbcommon and libxcb.
        let at = |s: &str| both.iter().position(|x| *x == s).unwrap();
        assert!(at("libxcb.so.1") < at("libxkbcommon-x11.so.0"));
        assert!(at("libxkbcommon.so.0") < at("libxkbcommon-x11.so.0"));
    }

    #[test]
    fn a_missing_library_brings_its_whole_set_from_the_bundle() {
        let libs = needed(LIBS, true, false);
        // The case that crashed: the system has libxkbcommon but not libxkbcommon-x11.
        assert_eq!(from_bundle(&libs, &["libxkbcommon-x11.so.0"]), ["libxkbcommon.so.0", "libxkbcommon-x11.so.0"]);
        assert_eq!(from_bundle(&libs, &["libXi.so.6"]), ["libXi.so.6"]);
        assert_eq!(from_bundle(&libs, &["libX11-xcb.so.1"]), ["libX11.so.6", "libX11-xcb.so.1"]);
        assert!(from_bundle(&libs, &[]).is_empty());
    }

    #[test]
    fn every_line_is_well_formed() {
        for line in LIBS.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let words: Vec<&str> = line.split_whitespace().collect();
            assert!((2..=3).contains(&words.len()), "{line}");
            assert!(["x11", "wayland", "any"].contains(&words[0]), "{line}");
            assert!(words[1].starts_with("lib") && words[1].contains(".so."), "{line}");
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
        prepare();
        assert!(NOTES.lock().unwrap().is_empty());
    }
}
