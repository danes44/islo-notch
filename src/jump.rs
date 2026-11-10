//! Jump to the app playing a media session: its window, and in a browser the
//! very tab, found by the media's title through UI Automation (the tab strip
//! is read, tabs aren't switched by keyboard or mouse).
//!
//! Only runs on a click, on a short-lived thread, so the UI never waits on a
//! slow app and nothing runs in the background.

use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Shell::PropertiesSystem::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

/// `aumid`: the session's SourceAppUserModelId. `title`: the media's title.
pub fn to_media(aumid: String, title: String) {
    let _ = std::thread::Builder::new()
        .name("islo-jump".into())
        .stack_size(512 * 1024)
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            jump(&aumid, &title);
            CoUninitialize();
        });
}

/// Browsers whose tab strip we look through.
const BROWSERS: [&str; 7] = ["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vivaldi.exe", "arc.exe"];

unsafe fn jump(aumid: &str, title: &str) {
    unsafe {
        // Store apps: Windows brings the app forward (or starts it)
        if aumid.contains('!') {
            if let Ok(m) = CoCreateInstance::<_, IApplicationActivationManager>(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) {
                if m.ActivateApplication(&HSTRING::from(aumid), PCWSTR::null(), AO_NONE).is_ok() {
                    return;
                }
            }
        }
        let wins = windows_of(aumid);
        let Some(&(first, _)) = wins.first() else {
            // no window (closed to the tray): ask the shell to open the app
            let target = HSTRING::from(format!("shell:AppsFolder\\{aumid}"));
            ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL);
            return;
        };
        let want = title.trim().to_lowercase();
        if !want.is_empty() && wins.iter().any(|w| w.1) {
            if let Ok(uia) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                for &(h, browser) in &wins {
                    if browser && select_tab(&uia, h, &want) {
                        bring(h);
                        return;
                    }
                }
            }
        }
        // the window showing it (a browser's title is its tab), else the app's first
        let h = wins.iter().map(|w| w.0).find(|&h| !want.is_empty() && window_text(h).to_lowercase().contains(&want)).unwrap_or(first);
        bring(h);
    }
}

/// The app's top-level windows, most recently used first, with whether each is a browser.
fn windows_of(aumid: &str) -> Vec<(HWND, bool)> {
    struct Find {
        aumid: String,
        exe: String,
        found: Vec<(HWND, bool)>,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        unsafe {
            let f = &mut *(l.0 as *mut Find);
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            let owned = GetWindow(h, GW_OWNER).is_ok_and(|o| !o.is_invalid());
            if !IsWindowVisible(h).as_bool() || owned || ex & WS_EX_TOOLWINDOW.0 != 0 || GetWindowTextLengthW(h) == 0 {
                return TRUE;
            }
            let exe = exe_of(h);
            let ours = window_aumid(h).is_some_and(|a| a.eq_ignore_ascii_case(&f.aumid)) || (!f.exe.is_empty() && exe == f.exe);
            if ours {
                f.found.push((h, BROWSERS.contains(&exe.as_str())));
            }
            TRUE
        }
    }
    let mut f = Find { aumid: aumid.to_string(), exe: exe_hint(aumid), found: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize));
    }
    f.found
}

/// The exe a session's app id most likely belongs to ("MSEdge" -> msedge.exe).
pub fn exe_hint(aumid: &str) -> String {
    let lower = aumid.to_lowercase();
    let last = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
    if last.ends_with(".exe") {
        return last.to_string();
    }
    let known = [
        ("msedge", "msedge.exe"),
        ("chrome", "chrome.exe"),
        ("firefox", "firefox.exe"),
        ("308046b0af4a39cb", "firefox.exe"), // Firefox's default app id
        ("brave", "brave.exe"),
        ("opera", "opera.exe"),
        ("vivaldi", "vivaldi.exe"),
        ("spotify", "spotify.exe"),
    ];
    for (k, v) in known {
        if lower.contains(k) {
            return v.to_string();
        }
    }
    String::new()
}

fn exe_of(h: HWND) -> String {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let Ok(p) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return String::new() };
        let mut buf = [0u16; 520];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(p);
        if !ok {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();
        path.rsplit('\\').next().unwrap_or("").to_string()
    }
}

/// The app id a window says it belongs to (browsers set one per window).
fn window_aumid(h: HWND) -> Option<String> {
    // PKEY_AppUserModel_ID
    const KEY: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid: 5 };
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(h).ok()?;
        let v = store.GetValue(&KEY).ok()?;
        let s = BSTR::try_from(&v).ok()?.to_string();
        (!s.is_empty()).then_some(s)
    }
}

fn window_text(h: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Select the tab whose name has `want` (lowercase) in it. Only the tab strip
/// is searched (it comes before the page in the tree), never the page itself.
fn select_tab(uia: &IUIAutomation, h: HWND, want: &str) -> bool {
    unsafe {
        let Ok(root) = uia.ElementFromHandle(h) else { return false };
        let cond = |id: UIA_CONTROLTYPE_ID| uia.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(id.0));
        let (Ok(strip_c), Ok(tab_c)) = (cond(UIA_TabControlTypeId), cond(UIA_TabItemControlTypeId)) else { return false };
        let Ok(strip) = root.FindFirst(TreeScope_Descendants, &strip_c) else { return false };
        let Ok(tabs) = strip.FindAll(TreeScope_Descendants, &tab_c) else { return false };
        for k in 0..tabs.Length().unwrap_or(0) {
            let Ok(tab) = tabs.GetElement(k) else { continue };
            let name = tab.CurrentName().map(|n| n.to_string().to_lowercase()).unwrap_or_default();
            if !name.contains(want) {
                continue;
            }
            if let Ok(p) = tab.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId) {
                if p.Select().is_ok() {
                    return true;
                }
            }
            if let Ok(p) = tab.GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(UIA_LegacyIAccessiblePatternId) {
                if p.DoDefaultAction().is_ok() {
                    return true;
                }
            }
            if let Ok(p) = tab.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
                if p.Invoke().is_ok() {
                    return true;
                }
            }
        }
        false
    }
}

/// Restore and bring a window to the front. Islo just had your click, so
/// Windows lets it; if focus was taken meanwhile, borrow the foreground
/// thread's input state for the switch.
fn bring(h: HWND) {
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        }
        if SetForegroundWindow(h).as_bool() && GetForegroundWindow() == h {
            return;
        }
        let fg = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = fg != 0 && fg != me && AttachThreadInput(me, fg, true).as_bool();
        let _ = BringWindowToTop(h);
        let _ = SetForegroundWindow(h);
        if attached {
            let _ = AttachThreadInput(me, fg, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::exe_hint;

    #[test]
    fn app_ids_map_to_their_exe() {
        assert_eq!(exe_hint("MSEdge"), "msedge.exe");
        assert_eq!(exe_hint("Chrome"), "chrome.exe");
        assert_eq!(exe_hint("308046B0AF4A39CB"), "firefox.exe");
        assert_eq!(exe_hint("Spotify.exe"), "spotify.exe");
        assert_eq!(exe_hint(r"C:\Program Files\VideoLAN\VLC\vlc.exe"), "vlc.exe");
        assert_eq!(exe_hint("SomethingElse"), "");
    }
}
