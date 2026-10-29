//! Smaller Windows integrations: battery, full-screen detection, tray icon,
//! start-with-Windows, theme / animation settings, monitors, memory usage.
//! Everything here is event-driven: Windows sends messages to our hidden host
//! window; nothing is polled.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING, GUID_POWER_SAVING_STATUS};
use windows::core::{BOOL, PCWSTR, w};

pub const WM_APP_TRAY: u32 = WM_APP + 10;
pub const WM_APP_APPBAR: u32 = WM_APP + 11;
pub const WM_APP_FOREGROUND: u32 = WM_APP + 12;
pub const WM_APP_START_DRAG: u32 = WM_APP + 13;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------- color picker ----------------

/// The dialog's 16 "custom colors", kept while Islo runs.
static CUSTOM_COLORS: std::sync::Mutex<[u32; 16]> = std::sync::Mutex::new([0x00ff_ffff; 16]);

/// Windows' color dialog (any color), owned by `owner`. It runs on its own
/// thread so Islo's event loop isn't re-entered while it's open; `done` gets
/// the pick as 0xRRGGBB, or None when cancelled.
pub fn choose_color(owner: HWND, initial: u32, done: impl FnOnce(Option<u32>) + Send + 'static) {
    use windows::Win32::UI::Controls::Dialogs::{CC_ANYCOLOR, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
    // COLORREF is 0x00BBGGRR; swapping red and blue converts both ways
    let swap = |c: u32| ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff);
    let owner = owner.0 as isize;
    let _ = std::thread::Builder::new().name("islo-color".into()).spawn(move || {
        let mut custom = CUSTOM_COLORS.lock().map(|c| *c).unwrap_or([0x00ff_ffff; 16]).map(COLORREF);
        let mut cc = CHOOSECOLORW {
            lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: HWND(owner as _),
            rgbResult: COLORREF(swap(initial)),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_ANYCOLOR | CC_FULLOPEN | CC_RGBINIT,
            ..Default::default()
        };
        let ok = unsafe { ChooseColorW(&mut cc) }.as_bool();
        if let Ok(mut c) = CUSTOM_COLORS.lock() {
            *c = custom.map(|x| x.0);
        }
        done(ok.then(|| swap(cc.rgbResult.0)));
    });
}

// ---------------- battery ----------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Battery {
    pub present: bool,
    pub percent: u8,
    pub plugged: bool,
    /// Windows battery saver / energy saver is on.
    pub saver: bool,
}

pub fn battery() -> Battery {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe {
        let _ = GetSystemPowerStatus(&mut s);
    }
    let present = s.BatteryFlag != 128 && s.BatteryFlag != 255 && s.BatteryLifePercent <= 100;
    Battery { present, percent: s.BatteryLifePercent.min(100), plugged: s.ACLineStatus == 1, saver: s.SystemStatusFlag == 1 }
}

pub fn register_power_notifications(host: HWND) {
    unsafe {
        let _ = RegisterPowerSettingNotification(HANDLE(host.0), &GUID_BATTERY_PERCENTAGE_REMAINING, DEVICE_NOTIFY_WINDOW_HANDLE);
        let _ = RegisterPowerSettingNotification(HANDLE(host.0), &GUID_ACDC_POWER_SOURCE, DEVICE_NOTIFY_WINDOW_HANDLE);
        let _ = RegisterPowerSettingNotification(HANDLE(host.0), &GUID_POWER_SAVING_STATUS, DEVICE_NOTIFY_WINDOW_HANDLE);
    }
}

// ---------------- full-screen detection ----------------

/// Registers as an (invisible, zero-size) app bar: the shell then sends us
/// ABN_FULLSCREENAPP whenever an app enters/leaves full screen. We also listen
/// for foreground-window switches. Both are push notifications.
pub fn register_fullscreen_watch(host: HWND) {
    unsafe {
        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: host,
            uCallbackMessage: WM_APP_APPBAR,
            ..Default::default()
        };
        SHAppBarMessage(ABM_NEW, &mut abd);
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(foreground_hook), 0, 0, WINEVENT_OUTOFCONTEXT);
    }
}

pub fn unregister_fullscreen_watch(host: HWND) {
    unsafe {
        let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, hWnd: host, ..Default::default() };
        SHAppBarMessage(ABM_REMOVE, &mut abd);
    }
}

unsafe extern "system" fn foreground_hook(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    // Out-of-context hooks run on our UI thread; just poke the host window.
    unsafe {
        let _ = PostMessageW(Some(crate::platform::host_hwnd()), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0));
    }
}

/// Is something covering the whole of `monitor` right now?
pub fn fullscreen_on(monitor: HMONITOR, ours: &[HWND]) -> bool {
    unsafe {
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                let fg = GetForegroundWindow();
                if fg.is_invalid() || MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) == monitor {
                    return true;
                }
            }
        }
        let fg = GetForegroundWindow();
        if fg.is_invalid() || ours.contains(&fg) {
            return false;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(fg, &mut class) as usize;
        let class = String::from_utf16_lossy(&class[..n]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" | "Windows.UI.Core.CoreWindow") {
            return false;
        }
        if MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) != monitor {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        let m = monitor_rect(monitor);
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

// ---------------- monitors ----------------

pub fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let v = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        v.push(m);
        BOOL(1)
    }
    let mut v: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

pub fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

/// 0 = primary, n = n-th enumerated display (falls back to primary).
pub fn monitor_by_index(i: i32) -> HMONITOR {
    if i <= 0 {
        return primary_monitor();
    }
    monitors().get(i as usize - 1).copied().unwrap_or_else(primary_monitor)
}

pub fn monitor_rect(m: HMONITOR) -> RECT {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(m, &mut mi);
    }
    mi.rcMonitor
}

/// The monitor minus the taskbar (used when docked to the bottom edge).
pub fn monitor_work_rect(m: HMONITOR) -> RECT {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    unsafe {
        let _ = GetMonitorInfoW(m, &mut mi);
    }
    mi.rcWork
}

/// The monitor under the mouse pointer.
pub fn monitor_at_cursor() -> HMONITOR {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
        MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY)
    }
}

pub fn monitor_scale(m: HMONITOR) -> f32 {
    let (mut x, mut y) = (96u32, 96u32);
    unsafe {
        let _ = GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut x, &mut y);
    }
    x.max(96) as f32 / 96.0
}


// ---------------- theme & motion ----------------

pub fn reg_dword(root: HKEY, path: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    let r = unsafe { RegGetValueW(root, path, name, RRF_RT_REG_DWORD, None, Some(&mut data as *mut _ as _), Some(&mut size)) };
    (r == ERROR_SUCCESS).then_some(data)
}

// ---------------- taskbar ----------------

const TASKBAR_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced");

/// Windows 11 taskbar alignment: 0 left, 1 center (the default when unset).
pub fn taskbar_alignment() -> u32 {
    reg_dword(HKEY_CURRENT_USER, TASKBAR_KEY, w!("TaskbarAl")).unwrap_or(1)
}

/// Move the taskbar apps left (0) or back to the center (1). Explorer picks it
/// up from the "TraySettings" broadcast, no restart needed.
pub fn set_taskbar_alignment(al: u32) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, TASKBAR_KEY, None, KEY_SET_VALUE, &mut key) == ERROR_SUCCESS {
            let _ = RegSetValueExW(key, w!("TaskbarAl"), None, REG_DWORD, Some(&al.to_le_bytes()));
            let _ = RegCloseKey(key);
        }
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("TraySettings").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            200,
            None,
        );
    }
}

/// The edge the (main) taskbar is on, when it's a top or bottom one; taskbar
/// mode follows it.
pub fn taskbar_edge() -> Option<crate::config::Edge> {
    use crate::config::Edge;
    let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return None;
    }
    match abd.uEdge {
        ABE_TOP => Some(Edge::Top),
        ABE_BOTTOM => Some(Edge::Bottom),
        _ => None, // left/right taskbars (Windows 10, patched shells): not supported
    }
}

/// How thick the (main) taskbar is, in physical px (0 if unknown).
pub fn taskbar_thickness() -> i32 {
    let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    if unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } == 0 {
        return 0;
    }
    let r = abd.rc;
    if abd.uEdge == ABE_TOP || abd.uEdge == ABE_BOTTOM { r.bottom - r.top } else { r.right - r.left }
}

/// Put an island back above the taskbar (clicking the taskbar raises it over
/// every other topmost window).
pub fn raise_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
}

pub fn apps_dark_mode() -> bool {
    reg_dword(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("AppsUseLightTheme")) == Some(0)
}

/// Windows "Animation effects" (Settings > Accessibility > Visual effects).
pub fn system_animations_enabled() -> bool {
    let mut on = BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    on.as_bool()
}

pub fn set_dark_title_bar(hwnd: HWND, dark: bool) {
    use windows::Win32::Graphics::Dwm::*;
    let v = BOOL(dark as i32);
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &v as *const _ as _, 4);
    }
}

// ---------------- start with Windows ----------------

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

pub fn autostart_enabled() -> bool {
    let mut size = 0u32;
    let r = unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Islo"), RRF_RT_REG_SZ, None, None, Some(&mut size)) };
    r == ERROR_SUCCESS
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value = wide(&format!("\"{}\"", exe.display()));
            let bytes = std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2);
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key) == ERROR_SUCCESS {
                let _ = RegSetValueExW(key, w!("Islo"), None, REG_SZ, Some(bytes));
                let _ = RegCloseKey(key);
            }
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("Islo"));
        }
    }
}

// ---------------- tray icon ----------------

pub fn taskbar_created_msg() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// The taskbar has its own theme ("Choose your default Windows mode").
fn taskbar_light() -> bool {
    reg_dword(HKEY_CURRENT_USER, w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("SystemUsesLightTheme")) == Some(1)
}

/// Brand tray glyph: resource 2 = white (dark taskbar), 3 = black (light taskbar).
/// LR_SHARED lets Windows cache it, so reloading on every theme change leaks nothing.
fn tray_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cx = GetSystemMetrics(SM_CXSMICON);
        let id = if taskbar_light() { 3 } else { 2 };
        LoadImageW(Some(hinst.into()), PCWSTR(id as _), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR | LR_SHARED)
            .or_else(|_| LoadImageW(Some(hinst.into()), PCWSTR(1 as _), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR | LR_SHARED))
            .map(|h| HICON(h.0))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap_or_default()
    }
}

pub fn tray_add(host: HWND) {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: host,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: tray_icon(),
            ..Default::default()
        };
        let tip = wide("Islo");
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

/// Swap the tray glyph when the taskbar flips between light and dark.
pub fn tray_refresh(host: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: host,
            uID: 1,
            uFlags: NIF_ICON,
            hIcon: tray_icon(),
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub fn tray_remove(host: HWND) {
    unsafe {
        let nid = NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: host, uID: 1, ..Default::default() };
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Sent (in the tray message's low word) when the balloon from `tray_balloon` is clicked.
pub const NIN_BALLOON_CLICK: u32 = WM_USER + 5;

/// A one-off Windows notification from the tray icon (title up to 63, text up to 255 chars).
/// Windows keeps it quiet during Focus / full screen by itself.
pub fn tray_balloon(host: HWND, title: &str, text: &str) {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: host,
            uID: 1,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_INFO | NIIF_RESPECT_QUIET_TIME,
            ..Default::default()
        };
        let t: Vec<u16> = title.encode_utf16().take(nid.szInfoTitle.len() - 1).collect();
        nid.szInfoTitle[..t.len()].copy_from_slice(&t);
        let b: Vec<u16> = text.encode_utf16().take(nid.szInfo.len() - 1).collect();
        nid.szInfo[..b.len()].copy_from_slice(&b);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub enum TrayChoice {
    Settings,
    ToggleHidden,
    Update,
    Quit,
    None,
}

/// `update`: the new version, when one is waiting to be installed.
pub fn tray_menu(host: HWND, hidden: bool, update: Option<&str>) -> TrayChoice {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return TrayChoice::None };
        if let Some(v) = update {
            let label = wide(&format!("Update to {v}…"));
            let _ = AppendMenuW(menu, MF_STRING, 4, PCWSTR(label.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        }
        let _ = AppendMenuW(menu, MF_STRING, 1, w!("Settings…"));
        let _ = AppendMenuW(menu, MF_STRING, 2, if hidden { w!("Show Islo") } else { w!("Hide Islo") });
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, 3, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(host);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, host, None);
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 {
            1 => TrayChoice::Settings,
            2 => TrayChoice::ToggleHidden,
            3 => TrayChoice::Quit,
            4 => TrayChoice::Update,
            _ => TrayChoice::None,
        }
    }
}

// ---------------- misc ----------------

/// (working set MB, private MB) — what Task Manager calls "Memory" is close to private.
pub fn memory_mb() -> (f64, f64) {
    use windows::Win32::System::ProcessStatus::*;
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, c.cb);
    }
    (c.WorkingSetSize as f64 / 1048576.0, c.PrivateUsage as f64 / 1048576.0)
}

/// The message a running Islo quits on. Registered (not WM_CLOSE), so older
/// Islo builds just ignore it instead of losing their host window.
pub fn quit_msg() -> u32 {
    unsafe { RegisterWindowMessageW(w!("Islo.Quit")) }
}

/// Ask a running Islo to quit cleanly (tray icon removed, Windows' volume
/// flyout restored). With `force`, an Islo that hasn't quit after 3 s (an
/// older build that doesn't know the message) is ended. False when no other
/// Islo is running.
pub fn close_running(force: bool) -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject};
    unsafe {
        let h = match FindWindowW(w!("IsloHost"), w!("IsloHost")) {
            Ok(h) if !h.is_invalid() => h,
            _ => return false,
        };
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let _ = PostMessageW(Some(h), quit_msg(), WPARAM(0), LPARAM(0));
        if force && pid != 0 {
            if let Ok(p) = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, false, pid) {
                if WaitForSingleObject(p, 3000) == WAIT_TIMEOUT {
                    let _ = TerminateProcess(p, 0);
                }
                let _ = CloseHandle(p);
            }
        }
        true
    }
}

/// True if this is the only Islo running. With `wait` (right after an update),
/// give the previous Islo up to 10 s to exit first.
pub fn single_instance(wait: bool) -> bool {
    for _ in 0..if wait { 50 } else { 1 } {
        unsafe {
            // The handle is kept open on success: it's what marks Islo as running.
            let h = windows::Win32::System::Threading::CreateMutexW(None, true, w!("Local\\Islo.SingleInstance"));
            if GetLastError() != ERROR_ALREADY_EXISTS {
                return true;
            }
            // Close ours, or the mutex would outlive the other Islo and we'd never get it.
            if let Ok(h) = h {
                let _ = CloseHandle(h);
            }
        }
        if wait {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    false
}

pub fn open_path(path: &std::path::Path) {
    let p = wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(p.as_ptr()), None, None, SW_SHOWNORMAL);
    }
}


/// ("Charging" | "On battery" | "Fully charged", "Plugged in · about 40 min to full")
pub fn battery_detail() -> (String, String) {
    let b = battery();
    let fmt = |mins: i64| {
        if mins >= 60 {
            let (h, m) = (mins / 60, mins % 60);
            if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
        } else {
            format!("{} min", mins.max(1))
        }
    };
    // Charge rate / capacities come from the WinRT battery report (no polling: read on demand).
    let report = windows::Devices::Power::Battery::AggregateBattery().and_then(|bat| bat.GetReport()).ok();
    let read = |r: &windows::Devices::Power::BatteryReport| -> Option<(i32, i32, i32)> {
        let rate = r.ChargeRateInMilliwatts().ok()?.Value().ok()?;
        let rem = r.RemainingCapacityInMilliwattHours().ok()?.Value().ok()?;
        let full = r.FullChargeCapacityInMilliwattHours().ok()?.Value().ok()?;
        Some((rate, rem, full))
    };
    let vals = report.as_ref().and_then(read);
    if b.plugged {
        if b.percent >= 100 {
            return ("Fully charged".into(), "Plugged in".into());
        }
        let sub = match vals {
            Some((rate, rem, full)) if rate > 0 && full > rem => {
                let mins = ((full - rem) as f64 / rate as f64 * 60.0).round() as i64;
                format!("Plugged in · about {} to full", fmt(mins))
            }
            _ => "Plugged in".into(),
        };
        ("Charging".into(), sub)
    } else {
        let mut s = SYSTEM_POWER_STATUS::default();
        unsafe {
            let _ = GetSystemPowerStatus(&mut s);
        }
        let sub = if s.BatteryLifeTime != u32::MAX && s.BatteryLifeTime > 0 {
            format!("About {} left", fmt(s.BatteryLifeTime as i64 / 60))
        } else if let Some((rate, rem, _)) = vals.filter(|v| v.0 < 0) {
            format!("About {} left", fmt((rem as f64 / -(rate as f64) * 60.0).round() as i64))
        } else {
            "On battery".into()
        };
        (if b.percent <= 20 { "Battery low".into() } else { "On battery".into() }, sub)
    }
}

/// 0 for the primary display, n for the n-th enumerated one (matches `monitor_by_index`).
pub fn monitor_index(m: HMONITOR) -> i32 {
    if m == primary_monitor() {
        return 0;
    }
    monitors().iter().position(|x| *x == m).map(|i| i as i32 + 1).unwrap_or(0)
}

// ---------------- global shortcut ----------------

pub const HOTKEY_TOGGLE: i32 = 1;
pub const HOTKEY_ESC: i32 = 2;

/// Win + Alt + I opens / closes the island from any app.
pub fn register_toggle_hotkey(host: HWND) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe { RegisterHotKey(Some(host), HOTKEY_TOGGLE, MOD_WIN | MOD_ALT | MOD_NOREPEAT, b'I' as u32).is_ok() }
}

/// Esc is only claimed while the island was opened from the keyboard,
/// so other apps keep their Esc the rest of the time.
pub fn set_esc_hotkey(host: HWND, on: bool) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    unsafe {
        if on {
            let _ = RegisterHotKey(Some(host), HOTKEY_ESC, MOD_NOREPEAT, VK_ESCAPE.0 as u32);
        } else {
            let _ = UnregisterHotKey(Some(host), HOTKEY_ESC);
        }
    }
}

// ---------------- native volume / brightness flyout ----------------

/// Best effort: on Windows 10 and early Windows 11 the volume/brightness OSD
/// lives in a "NativeHWNDHost" window that can be minimized so only Islo's
/// HUD shows. Newer Windows 11 builds draw it elsewhere; there it stays.
/// Returns true if the flyout window was found.
pub fn set_native_osd_hidden(hidden: bool) -> bool {
    unsafe {
        let mut found = false;
        let mut prev: Option<HWND> = None;
        loop {
            let Ok(h) = FindWindowExW(None, prev, w!("NativeHWNDHost"), PCWSTR::null()) else { break };
            if h.is_invalid() {
                break;
            }
            // the OSD host has a DirectUIHWND child
            if FindWindowExW(Some(h), None, w!("DirectUIHWND"), PCWSTR::null()).is_ok_and(|c| !c.is_invalid()) {
                let _ = ShowWindow(h, if hidden { SW_MINIMIZE } else { SW_RESTORE });
                found = true;
            }
            prev = Some(h);
        }
        found
    }
}

/// Timer done chime (the system's notification sound).
pub fn chime() {
    unsafe {
        let _ = windows::Win32::System::Diagnostics::Debug::MessageBeep(MB_ICONASTERISK);
    }
}

/// Make outside text safe for a one-line label: line breaks and tabs become
/// spaces, runs of spaces collapse, and emoji / joiners are dropped (the CPU
/// renderer can't draw color emoji; they'd show as empty boxes).
pub fn display_text(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    let mut last_space = true;
    for ch in s.chars() {
        let c = ch as u32;
        let emoji = matches!(c,
            0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0x2B00..=0x2BFF
            | 0xFE00..=0xFE0F | 0x200D | 0x20E3 | 0xE000..=0xF8FF | 0xE0020..=0xE007F);
        if emoji {
            continue;
        }
        if ch.is_whitespace() || ch.is_control() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
            continue;
        }
        out.push(ch);
        last_space = false;
        if out.chars().count() >= max_chars {
            out.push('…');
            break;
        }
    }
    out.trim().to_string()
}

// ---------------- date ----------------

/// Today's date for the island ("Mon 26 Oct") and the milliseconds until the
/// next local midnight, so the caller can arm one timer instead of polling.
pub fn today() -> (String, u64) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (format_date(t.wDayOfWeek, t.wDay, t.wMonth), ms_to_midnight(t.wHour, t.wMinute, t.wSecond, t.wMilliseconds))
}

/// The time now in the user's own short format ("15:31" or "3:31 PM", as the
/// taskbar shows it), and the milliseconds until the next minute starts.
pub fn clock_now() -> (String, u64) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let pattern = reg_string(HKEY_CURRENT_USER, w!("Control Panel\\International"), w!("sShortTime")).unwrap_or_else(|| "HH:mm".into());
    let am = reg_string(HKEY_CURRENT_USER, w!("Control Panel\\International"), w!("s1159")).unwrap_or_else(|| "AM".into());
    let pm = reg_string(HKEY_CURRENT_USER, w!("Control Panel\\International"), w!("s2359")).unwrap_or_else(|| "PM".into());
    let next = 60_000 - (t.wSecond as u64 * 1000 + t.wMilliseconds as u64) + 150;
    (format_time(&pattern, t.wHour, t.wMinute, &am, &pm), next)
}

/// Windows' short-time pattern: H/HH 24-hour, h/hh 12-hour, mm minutes, tt AM/PM.
pub fn format_time(pattern: &str, h: u16, m: u16, am: &str, pm: &str) -> String {
    let mut out = String::new();
    let cs: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let mut n = 1;
        while i + n < cs.len() && cs[i + n] == c {
            n += 1;
        }
        match c {
            'H' => out += &if n >= 2 { format!("{h:02}") } else { h.to_string() },
            'h' => {
                let h12 = if h % 12 == 0 { 12 } else { h % 12 };
                out += &if n >= 2 { format!("{h12:02}") } else { h12.to_string() };
            }
            'm' => out += &if n >= 2 { format!("{m:02}") } else { m.to_string() },
            't' => {
                let mark = if h < 12 { am } else { pm };
                if n >= 2 {
                    out += mark;
                } else {
                    out.extend(mark.chars().take(1)); // never split a multi-byte letter
                }
            }
            's' => {} // seconds: never in the island
            '\'' => {
                // quoted literal
                let mut j = i + 1;
                while j < cs.len() && cs[j] != '\'' {
                    out.push(cs[j]);
                    j += 1;
                }
                i = j + 1;
                continue;
            }
            _ => out.extend(std::iter::repeat_n(c, n)),
        }
        i += n;
    }
    out.trim().to_string()
}

fn reg_string(root: HKEY, path: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 64];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe { RegGetValueW(root, path, name, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut size)) };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = (size as usize / 2).saturating_sub(1).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn format_date(weekday: u16, day: u16, month: u16) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let wd = DAYS.get(weekday as usize).copied().unwrap_or("");
    let mo = MONTHS.get((month as usize).wrapping_sub(1)).copied().unwrap_or("");
    format!("{wd} {day} {mo}")
}

fn ms_to_midnight(h: u16, m: u16, s: u16, ms: u16) -> u64 {
    let now = ((h as u64 * 60 + m as u64) * 60 + s as u64) * 1000 + ms as u64;
    // +250 ms so the timer lands safely after midnight, never just before it.
    (24 * 3600 * 1000u64).saturating_sub(now).max(1000) + 250
}

// ---------------- pointer watch (Mochi's eyes) ----------------
//
// A low-level mouse hook: Windows calls us on every pointer move, so there is
// no polling. It is only installed while Mochi is on screen with "Eyes follow the
// pointer" on, and removed whenever the island hides (full-screen apps, games).
// The callback must stay tiny: it only stores the position.

thread_local! {
    static POINTER_CB: std::cell::RefCell<Option<Box<dyn Fn(i32, i32)>>> = const { std::cell::RefCell::new(None) };
    static POINTER_HOOK: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

unsafe extern "system" fn pointer_hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code >= 0 && wp.0 as u32 == WM_MOUSEMOVE {
        let info = unsafe { &*(lp.0 as *const MSLLHOOKSTRUCT) };
        let (x, y) = (info.pt.x, info.pt.y);
        POINTER_CB.with(|c| {
            if let Ok(c) = c.try_borrow() {
                if let Some(f) = c.as_ref() {
                    f(x, y);
                }
            }
        });
    }
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Start (Some) or stop (None) watching the pointer. Returns whether it is on.
pub fn watch_pointer(cb: Option<Box<dyn Fn(i32, i32)>>) -> bool {
    let on = cb.is_some();
    POINTER_CB.with(|c| *c.borrow_mut() = cb);
    let current = POINTER_HOOK.with(|h| h.get());
    unsafe {
        if on && current == 0 {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(pointer_hook), Some(hinst.into()), 0) {
                POINTER_HOOK.with(|c| c.set(h.0 as isize));
            }
        } else if !on && current != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(current as _));
            POINTER_HOOK.with(|c| c.set(0));
        }
    }
    POINTER_HOOK.with(|h| h.get()) != 0
}
