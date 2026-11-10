// Islo — a macOS-style notch / Dynamic Island for Windows.
//
// `windows_subsystem = "windows"` means no console window pops up.
#![cfg_attr(windows, windows_subsystem = "windows")]

// Brings in the Rust types generated from ui/*.slint by build.rs
// (Island, SettingsWindow, ShelfItem).
slint::include_modules!();

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod brightness;
#[cfg(windows)]
mod config;
#[cfg(windows)]
mod game;
#[cfg(windows)]
mod glass;
#[cfg(windows)]
mod http;
#[cfg(windows)]
mod live;
#[cfg(windows)]
mod lyrics;
#[cfg(windows)]
mod jump;
#[cfg(windows)]
mod media;
#[cfg(windows)]
mod notify;
#[cfg(windows)]
mod package;
#[cfg(windows)]
mod pet;
#[cfg(windows)]
mod platform;
#[cfg(windows)]
mod shelf;
#[cfg(windows)]
mod spring;
#[cfg(windows)]
mod system;
#[cfg(windows)]
mod update;

mod shadow;
#[cfg(not(windows))]
mod snapshot;

#[cfg(windows)]
fn main() {
    use windows::Win32::System::Ole::OleInitialize;
    use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

    install_crash_log();
    // `islo.exe --quit`: ask the running Islo to quit cleanly (dev.ps1 uses it)
    if std::env::args().any(|a| a == "--quit") {
        system::close_running(false);
        return;
    }
    let after_update = std::env::args().any(|a| a == update::AFTER_UPDATE_ARG);
    // Dev builds (`cargo run`, dev.ps1) replace a running Islo instead of
    // quitting, so every rebuild restarts it. Release builds never do this.
    let replace = cfg!(debug_assertions) && system::close_running(true);
    if !system::single_instance(after_update || replace) {
        return; // Islo is already running
    }
    update::clean_up_old();
    unsafe {
        // Crisp on every monitor: we handle DPI ourselves.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // OLE (single-threaded apartment) is needed for drag & drop on the UI thread.
        let _ = OleInitialize(None);
    }
    let platform = platform::Win32Platform::new().expect("create Win32 platform");
    slint::platform::set_platform(Box::new(platform)).expect("install platform");
    if let Err(e) = app::run() {
        let msg = system::wide(&format!("Islo could not start:\n{e}"));
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW};
            MessageBoxW(None, windows::core::PCWSTR(msg.as_ptr()), windows::core::w!("Islo"), MB_ICONERROR);
        }
    }
}

/// If Islo ever panics, write what happened to `islo-crash.log` next to the exe
/// (Rust aborts right after, so this is the only trace). Send that file when
/// reporting a crash.
#[cfg(windows)]
fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        let thread = std::thread::current().name().unwrap_or("main").to_string();
        let when = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let text = format!(
            "Islo {} crashed (unix time {when})\nthread: {thread}\nwhere: {place}\nwhat: {msg}\n\n",
            env!("CARGO_PKG_VERSION")
        );
        let path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("islo-crash.log")))
            .unwrap_or_else(|| "islo-crash.log".into());
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(text.as_bytes());
        }
    }));
}

#[cfg(not(windows))]
fn main() {
    snapshot::run();
}
