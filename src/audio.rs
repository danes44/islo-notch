//! Master volume: push notifications from Windows (no polling) plus
//! set/mute commands. Follows the default output device when it changes
//! (e.g. plugging in headphones).

use std::sync::mpsc::{Sender, channel};
use windows::Win32::Media::Audio::Endpoints::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::core::{Interface, PCWSTR, implement};

pub enum Cmd {
    /// Change volume by this many percentage points.
    Step(f32),
    ToggleMute,
    Reacquire,
}

// ---------------- per-app volume (the Volume mixer) ----------------

/// What to look for in an audio session's id to tell it belongs to a media
/// session's app: the exe ("\chrome.exe") or a Store app's package name.
/// The id holds the process path, e.g. "...|\Device\HarddiskVolume3\Program
/// Files\Google\Chrome\Application\chrome.exe%b{...}".
pub fn app_needles(aumid: &str) -> Vec<String> {
    let lower = aumid.to_lowercase();
    let mut n = Vec::new();
    let exe = crate::jump::exe_hint(aumid);
    if !exe.is_empty() {
        n.push(format!("\\{exe}"));
    }
    if let Some((pkg, _)) = lower.split_once('!') {
        n.push(pkg.split('_').next().unwrap_or(pkg).to_string());
    }
    n
}

/// Every audio session of the default output whose id has one of `needles`.
fn app_sessions(needles: &[String]) -> Vec<ISimpleAudioVolume> {
    if needles.is_empty() {
        return Vec::new();
    }
    unsafe {
        let found = || -> Option<Vec<ISimpleAudioVolume>> {
            let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let dev = e.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let mgr: IAudioSessionManager2 = dev.Activate(CLSCTX_ALL, None).ok()?;
            let list = mgr.GetSessionEnumerator().ok()?;
            let mut out = Vec::new();
            for k in 0..list.GetCount().unwrap_or(0) {
                let Ok(c) = list.GetSession(k) else { continue };
                let Ok(c2) = c.cast::<IAudioSessionControl2>() else { continue };
                let Ok(id) = c2.GetSessionInstanceIdentifier() else { continue };
                let s = id.to_string().unwrap_or_default().to_lowercase();
                CoTaskMemFree(Some(id.0 as *const _));
                if needles.iter().any(|n| s.contains(n.as_str())) {
                    if let Ok(v) = c.cast::<ISimpleAudioVolume>() {
                        out.push(v);
                    }
                }
            }
            Some(out)
        };
        found().unwrap_or_default()
    }
}

/// The app's own volume (0..1), if it has an audio session.
pub fn app_volume(needles: &[String]) -> Option<f32> {
    app_sessions(needles).iter().filter_map(|v| unsafe { v.GetMasterVolume().ok() }).reduce(f32::max)
}

pub fn set_app_volume(needles: &[String], level: f32) {
    for v in app_sessions(needles) {
        unsafe {
            let _ = v.SetMasterVolume(level.clamp(0.0, 1.0), std::ptr::null());
            if level > 0.0 {
                let _ = v.SetMute(false, std::ptr::null());
            }
        }
    }
}

/// (volume 0..1, muted, is_initial_value)
pub type Callback = dyn Fn(f32, bool, bool) + Send + Sync;

// `#[implement]` generates the COM plumbing so Windows can call our struct.
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback(std::sync::Arc<Callback>);

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if let Some(d) = unsafe { data.as_ref() } {
            (self.0)(d.fMasterVolume, d.bMuted.as_bool(), false);
        }
        Ok(())
    }
}

#[implement(IMMNotificationClient)]
struct DeviceWatcher(Sender<Cmd>);

impl IMMNotificationClient_Impl for DeviceWatcher_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _: &PCWSTR) -> windows::core::Result<()> {
        if flow == eRender && role == eConsole {
            let _ = self.0.send(Cmd::Reacquire);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &windows::Win32::Foundation::PROPERTYKEY) -> windows::core::Result<()> {
        Ok(())
    }
}

pub fn start(cb: impl Fn(f32, bool, bool) + Send + Sync + 'static) -> Sender<Cmd> {
    let (tx, rx) = channel::<Cmd>();
    let tx2 = tx.clone();
    let cb: std::sync::Arc<Callback> = std::sync::Arc::new(cb);
    std::thread::Builder::new()
        .name("islo-audio".into())
        .stack_size(256 * 1024)
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return };
            let watcher: IMMNotificationClient = DeviceWatcher(tx2).into();
            let _ = enumerator.RegisterEndpointNotificationCallback(&watcher);
            let callback: IAudioEndpointVolumeCallback = VolumeCallback(cb.clone()).into();

            let acquire = || -> Option<IAudioEndpointVolume> {
                let dev = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
                let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None).ok()?;
                vol.RegisterControlChangeNotify(&callback).ok()?;
                let v = vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
                let m = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                cb(v, m, true);
                Some(vol)
            };
            let mut endpoint = acquire();

            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Cmd::Step(delta) => {
                        if let Some(vol) = &endpoint {
                            let cur = vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
                            let next = ((cur * 100.0 + delta).round() / 100.0).clamp(0.0, 1.0);
                            let _ = vol.SetMasterVolumeLevelScalar(next, std::ptr::null());
                            if next > 0.0 && vol.GetMute().map(|b| b.as_bool()).unwrap_or(false) {
                                let _ = vol.SetMute(false, std::ptr::null());
                            }
                        }
                    }
                    Cmd::ToggleMute => {
                        if let Some(vol) = &endpoint {
                            let m = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                            let _ = vol.SetMute(!m, std::ptr::null());
                        }
                    }
                    Cmd::Reacquire => {
                        if let Some(old) = endpoint.take() {
                            let _ = old.UnregisterControlChangeNotify(&callback);
                        }
                        endpoint = acquire();
                    }
                }
            }
        })
        .expect("spawn audio thread");
    tx
}
