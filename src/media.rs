//! Now-playing info and controls via Windows' GlobalSystemMediaTransportControls
//! (the same source as the Windows media flyout: Spotify, browsers, most players).
//!
//! Runs on its own thread. That thread sleeps on a channel (`rx.recv()`), which
//! costs no CPU; Windows wakes it through event handlers when something changes.
//! Results are sent to the UI thread with a callback.

use std::sync::mpsc::{Receiver, Sender, channel};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Imaging::*;
use windows::Media::Control::*;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::core::Interface;

pub enum Cmd {
    SessionChanged,
    Changed,
    /// One of the listed sessions changed (its id).
    Other(u64),
    PlayPause,
    Next,
    Prev,
    /// Play / pause a listed session.
    Toggle(u64),
    /// Bring a listed session's app (or browser tab) to the front.
    Open(u64),
    OpenCurrent,
    /// Set a listed session's app volume (0..1).
    Volume(u64, f32),
    /// Set the current session's app volume.
    CurrentVolume(f32),
    /// Read the apps' volumes again (the island opened; they may have changed
    /// in Windows' mixer meanwhile).
    Volumes,
}

/// Another media session than the current one, for the list under the player.
#[derive(Clone)]
pub struct Entry {
    pub id: u64,
    pub title: String,
    pub artist: String,
    pub app: String,
    pub playing: bool,
    /// Bumps whenever the thumbnail changed.
    pub art_id: u64,
    pub art: Option<Vec<u8>>, // THUMB x THUMB straight-alpha RGBA
    /// The app's own volume (0..1), when it has an audio session.
    pub volume: Option<f32>,
}

pub const THUMB: u32 = 48;

#[derive(Clone)]
pub struct Art {
    pub rgba: Vec<u8>, // 96x96 straight-alpha RGBA
    pub accent: [u8; 3],
}

#[derive(Clone)]
pub struct Info {
    pub title: String,
    pub artist: String,
    /// Friendly name of the playing app ("Spotify", "Edge", ...).
    pub app: String,
    pub album: String,
    pub playing: bool,
    /// Bumps whenever the artwork changed, so the UI only re-uploads it then.
    pub art_id: u64,
    pub art: Option<Art>,
    /// (position s, duration s, seconds since that position was measured)
    pub timeline: Option<(f64, f64, f64)>,
    /// The playing app's own volume (0..1), when it has an audio session.
    pub volume: Option<f32>,
}

pub const ART_SIZE: u32 = 96;

/// `on_update`: the current session (the one Windows' media flyout shows).
/// `on_list`: every other session, playing or paused.
pub fn start(on_update: impl Fn(Option<Info>) + Send + 'static, on_list: impl Fn(Vec<Entry>) + Send + 'static) -> Sender<Cmd> {
    let (tx, rx) = channel();
    let tx2 = tx.clone();
    std::thread::Builder::new()
        .name("islo-media".into())
        .stack_size(256 * 1024)
        .spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            if let Err(e) = run(tx2, rx, &on_update, &on_list) {
                eprintln!("media service stopped: {e}");
                on_update(None);
                on_list(Vec::new());
            }
        })
        .expect("spawn media thread");
    tx
}

struct Subscribed {
    session: GlobalSystemMediaTransportControlsSession,
    tokens: [i64; 3],
}

/// A session in the list. `key` is its COM identity, so the same session keeps
/// its id (and its subscriptions) when Windows hands out the list again, if it
/// hands out the same object. It doesn't always (GetCurrentSession never
/// matches GetSessions), so the current session is told apart by app and title.
struct Listed {
    id: u64,
    key: usize,
    session: GlobalSystemMediaTransportControlsSession,
    tokens: [i64; 2],
    entry: Option<Entry>,
    aumid: String,
    /// How to find its app in the Volume mixer (see audio::app_needles).
    needles: Vec<String>,
    volume: Option<f32>,
    /// The full title, for finding its browser tab.
    raw_title: String,
    art_key: String,
    dirty: bool,
}

fn identity(s: &GlobalSystemMediaTransportControlsSession) -> usize {
    s.cast::<windows::core::IUnknown>().map(|u| u.as_raw() as usize).unwrap_or(0)
}

/// Follow every session Windows has: keep the ones we know, subscribe to new ones.
fn sync_list(mgr: &GlobalSystemMediaTransportControlsSessionManager, list: &mut Vec<Listed>, next_id: &mut u64, tx: &Sender<Cmd>) {
    let mut old = std::mem::take(list);
    if let Ok(sessions) = mgr.GetSessions() {
        for session in sessions {
            let key = identity(&session);
            if let Some(p) = old.iter().position(|l| l.key == key) {
                list.push(old.swap_remove(p));
                continue;
            }
            let id = *next_id;
            *next_id += 1;
            macro_rules! notify {
                () => {{
                    let t = tx.clone();
                    TypedEventHandler::new(move |_, _| {
                        let _ = t.send(Cmd::Other(id));
                        Ok(())
                    })
                }};
            }
            let a = session.MediaPropertiesChanged(&notify!()).unwrap_or(0);
            let b = session.PlaybackInfoChanged(&notify!()).unwrap_or(0);
            list.push(Listed { id, key, session, tokens: [a, b], entry: None, aumid: String::new(), needles: Vec::new(), volume: None, raw_title: String::new(), art_key: String::new(), dirty: true });
        }
    }
    for l in old {
        let _ = l.session.RemoveMediaPropertiesChanged(l.tokens[0]);
        let _ = l.session.RemovePlaybackInfoChanged(l.tokens[1]);
    }
}

fn read_listed(l: &mut Listed) {
    l.dirty = false;
    let Some(props) = l.session.TryGetMediaPropertiesAsync().ok().and_then(|op| op.join().ok()) else {
        l.entry = None;
        return;
    };
    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let mut artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    if artist.is_empty() {
        artist = props.AlbumArtist().map(|s| s.to_string()).unwrap_or_default();
    }
    if title.is_empty() && artist.is_empty() {
        l.entry = None;
        return;
    }
    let playing = l
        .session
        .GetPlaybackInfo()
        .and_then(|p| p.PlaybackStatus())
        .map(|s| s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
        .unwrap_or(false);
    let (mut art_id, mut art) = l.entry.take().map(|e| (e.art_id, e.art)).unwrap_or((0, None));
    let key = format!("{title}\u{1}{artist}");
    if l.art_key != key || art.is_none() {
        let fresh = decode_art(&props, THUMB).map(|a| a.rgba);
        if l.art_key != key || fresh.is_some() {
            art = fresh;
            art_id += 1;
        }
        l.art_key = key;
    }
    let id = aumid(&l.session);
    if l.needles.is_empty() || id != l.aumid {
        l.needles = crate::audio::app_needles(&id);
        l.volume = crate::audio::app_volume(&l.needles);
    }
    l.aumid = id;
    let app = friendly_app(&l.aumid);
    l.raw_title = title.clone();
    l.entry = Some(Entry {
        id: l.id,
        title: crate::system::display_text(&title, 120),
        artist: crate::system::display_text(&artist, 80),
        app,
        playing,
        art_id,
        art,
        volume: l.volume,
    });
}

fn aumid(s: &GlobalSystemMediaTransportControlsSession) -> String {
    s.SourceAppUserModelId().map(|s| s.to_string()).unwrap_or_default()
}

fn run(tx: Sender<Cmd>, rx: Receiver<Cmd>, on_update: &dyn Fn(Option<Info>), on_list: &dyn Fn(Vec<Entry>)) -> windows::core::Result<()> {
    let mgr = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.join()?;
    {
        let t = tx.clone();
        mgr.CurrentSessionChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Cmd::SessionChanged);
            Ok(())
        }))?;
        let t = tx.clone();
        mgr.SessionsChanged(&TypedEventHandler::new(move |_, _| {
            let _ = t.send(Cmd::SessionChanged);
            Ok(())
        }))?;
    }

    let mut sub: Option<Subscribed> = None;
    let mut art_key = String::new();
    let mut art_id = 0u64;
    let mut art: Option<Art> = None;
    let mut last_sent_none = false;
    let mut cur_title = String::new();
    let mut list: Vec<Listed> = Vec::new();
    let mut next_id = 1u64;
    // what the UI was last sent: (id, art_id, playing, title, artist) per row
    let mut sent: Vec<(u64, u64, bool, String, String, i32)> = Vec::new();
    let mut cur_needles: Vec<String> = Vec::new();
    let mut cur_volume: Option<f32> = None;

    let _ = tx.send(Cmd::SessionChanged);
    while let Ok(first) = rx.recv() {
        // Coalesce bursts (players often fire 3-4 events at once).
        let mut cmds = vec![first];
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        let mut refresh = false;
        let mut relist = false;
        for cmd in cmds {
            match cmd {
                Cmd::Other(id) => {
                    if let Some(l) = list.iter_mut().find(|l| l.id == id) {
                        l.dirty = true;
                        relist = true;
                    }
                }
                Cmd::Toggle(id) => {
                    if let Some(l) = list.iter().find(|l| l.id == id) {
                        if let Ok(op) = l.session.TryTogglePlayPauseAsync() {
                            let _ = op.join();
                        }
                    }
                }
                Cmd::Open(id) => {
                    if let Some(l) = list.iter().find(|l| l.id == id) {
                        crate::jump::to_media(aumid(&l.session), l.raw_title.clone());
                    }
                }
                Cmd::OpenCurrent => {
                    if let Some(s) = &sub {
                        crate::jump::to_media(aumid(&s.session), cur_title.clone());
                    }
                }
                Cmd::Volume(id, v) => {
                    if let Some(l) = list.iter_mut().find(|l| l.id == id) {
                        crate::audio::set_app_volume(&l.needles, v);
                        l.volume = Some(v);
                        if let Some(e) = &mut l.entry {
                            e.volume = Some(v);
                        }
                        relist = true;
                    }
                }
                Cmd::CurrentVolume(v) => {
                    crate::audio::set_app_volume(&cur_needles, v);
                    cur_volume = Some(v);
                }
                Cmd::Volumes => {
                    cur_volume = crate::audio::app_volume(&cur_needles);
                    for l in list.iter_mut() {
                        l.volume = crate::audio::app_volume(&l.needles);
                        if let Some(e) = &mut l.entry {
                            e.volume = l.volume;
                        }
                    }
                    refresh = true;
                    relist = true;
                }
                Cmd::SessionChanged => {
                    sync_list(&mgr, &mut list, &mut next_id, &tx);
                    relist = true;
                    if let Some(old) = sub.take() {
                        let _ = old.session.RemoveMediaPropertiesChanged(old.tokens[0]);
                        let _ = old.session.RemovePlaybackInfoChanged(old.tokens[1]);
                        let _ = old.session.RemoveTimelinePropertiesChanged(old.tokens[2]);
                    }
                    if let Ok(session) = mgr.GetCurrentSession() {
                        // Each event has its own argument type, so a small macro
                        // stamps out one handler per event.
                        macro_rules! notify {
                            () => {{
                                let t = tx.clone();
                                TypedEventHandler::new(move |_, _| {
                                    let _ = t.send(Cmd::Changed);
                                    Ok(())
                                })
                            }};
                        }
                        let a = session.MediaPropertiesChanged(&notify!()).unwrap_or(0);
                        let b = session.PlaybackInfoChanged(&notify!()).unwrap_or(0);
                        let c = session.TimelinePropertiesChanged(&notify!()).unwrap_or(0);
                        sub = Some(Subscribed { session, tokens: [a, b, c] });
                    }
                    cur_needles = sub.as_ref().map(|s| crate::audio::app_needles(&aumid(&s.session))).unwrap_or_default();
                    cur_volume = crate::audio::app_volume(&cur_needles);
                    refresh = true;
                }
                Cmd::Changed => refresh = true,
                Cmd::PlayPause | Cmd::Next | Cmd::Prev => {
                    if let Some(s) = &sub {
                        let r = match cmd {
                            Cmd::PlayPause => s.session.TryTogglePlayPauseAsync(),
                            Cmd::Next => s.session.TrySkipNextAsync(),
                            _ => s.session.TrySkipPreviousAsync(),
                        };
                        if let Ok(op) = r {
                            let _ = op.join();
                        }
                    }
                }
            }
        }
        if refresh {
            match &sub {
                None => {
                    cur_title.clear();
                    if !last_sent_none {
                        on_update(None);
                        last_sent_none = true;
                    }
                }
                Some(s) => {
                    let mut info = read(&s.session, &mut art_key, &mut art_id, &mut art);
                    if let Some(i) = &mut info {
                        i.volume = cur_volume;
                    }
                    cur_title = info.as_ref().map(|i| i.title.clone()).unwrap_or_default();
                    last_sent_none = info.is_none();
                    on_update(info);
                }
            }
        }

        if relist || refresh {
            for l in list.iter_mut().filter(|l| l.dirty) {
                read_listed(l);
            }
            // the current one is the big player above the list; playing ones first
            let cur_app = sub.as_ref().map(|s| aumid(&s.session)).unwrap_or_default();
            let is_current = |l: &Listed| l.aumid == cur_app && l.entry.as_ref().is_some_and(|e| e.title == cur_title);
            let mut rows: Vec<Entry> = list.iter().filter(|l| !is_current(l)).filter_map(|l| l.entry.clone()).collect();
            rows.sort_by_key(|e| !e.playing);
            let now: Vec<_> = rows
                .iter()
                .map(|e| (e.id, e.art_id, e.playing, e.title.clone(), e.artist.clone(), e.volume.map_or(-1, |v| (v * 100.0).round() as i32)))
                .collect();
            if now != sent {
                sent = now;
                on_list(rows);
            }
        }
    }
    Ok(())
}

fn read(session: &GlobalSystemMediaTransportControlsSession, art_key: &mut String, art_id: &mut u64, art: &mut Option<Art>) -> Option<Info> {
    let props = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let mut artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    if artist.is_empty() {
        artist = props.AlbumArtist().map(|s| s.to_string()).unwrap_or_default();
    }
    if title.is_empty() && artist.is_empty() {
        return None;
    }
    let (title, artist) = (crate::system::display_text(&title, 120), crate::system::display_text(&artist, 80));
    let playing = session
        .GetPlaybackInfo()
        .and_then(|p| p.PlaybackStatus())
        .map(|s| s == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
        .unwrap_or(false);

    // Only decode artwork when the track changed.
    let key = format!("{title}\u{1}{artist}");
    if *art_key != key {
        *art_key = key;
        *art = decode_art(&props, ART_SIZE);
        *art_id += 1;
    } else if art.is_none() {
        // Some players publish the thumbnail a moment after the title.
        if let Some(a) = decode_art(&props, ART_SIZE) {
            *art = Some(a);
            *art_id += 1;
        }
    }

    let timeline = session.GetTimelineProperties().ok().and_then(|t| {
        let pos = t.Position().ok()?.Duration as f64 / 1e7;
        let start = t.StartTime().ok()?.Duration as f64 / 1e7;
        let end = t.EndTime().ok()?.Duration as f64 / 1e7;
        let updated = t.LastUpdatedTime().ok()?.UniversalTime;
        let dur = end - start;
        if dur <= 1.0 {
            return None;
        }
        let age = (now_filetime() - updated) as f64 / 1e7;
        Some((pos - start, dur, age.clamp(0.0, 24.0 * 3600.0)))
    });

    let app = session.SourceAppUserModelId().map(|s| friendly_app(&s.to_string())).unwrap_or_default();
    let album = props.AlbumTitle().map(|s| s.to_string()).unwrap_or_default();
    Some(Info { title, artist, app, album, playing, art_id: *art_id, art: art.clone(), timeline, volume: None })
}

fn now_filetime() -> i64 {
    // WinRT DateTime and FILETIME share the same epoch (1601) and 100 ns units.
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

/// The artwork as a `size` x `size` square.
fn decode_art(props: &GlobalSystemMediaTransportControlsSessionMediaProperties, size: u32) -> Option<Art> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.join().ok()?;
    let decoder = BitmapDecoder::CreateAsync(&stream).ok()?.join().ok()?;
    let (w, h) = (decoder.PixelWidth().ok()? as f64, decoder.PixelHeight().ok()? as f64);
    if w < 1.0 || h < 1.0 {
        return None;
    }
    // Scale so the short side is `size`, then center-crop a square
    // (video thumbnails are 16:9).
    let scale = size as f64 / w.min(h);
    let (sw, sh) = ((w * scale).round() as u32, (h * scale).round() as u32);
    let t = BitmapTransform::new().ok()?;
    t.SetScaledWidth(sw).ok()?;
    t.SetScaledHeight(sh).ok()?;
    t.SetInterpolationMode(BitmapInterpolationMode::Fant).ok()?;
    t.SetBounds(BitmapBounds {
        X: sw.saturating_sub(size) / 2,
        Y: sh.saturating_sub(size) / 2,
        Width: size.min(sw),
        Height: size.min(sh),
    })
    .ok()?;
    let data = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Rgba8,
            BitmapAlphaMode::Straight,
            &t,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
        .ok()?
        .join()
        .ok()?;
    let bytes = data.DetachPixelData().ok()?;
    let rgba: Vec<u8> = bytes.to_vec();
    if rgba.len() != (size * size * 4) as usize {
        return None;
    }
    Some(Art { accent: accent_of(&rgba), rgba })
}

/// A lively accent color from the artwork: average of the more saturated pixels,
/// then pushed to a readable brightness on black.
fn accent_of(rgba: &[u8]) -> [u8; 3] {
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for p in rgba.chunks_exact(4).step_by(3) {
        let (pr, pg, pb) = (p[0] as i32, p[1] as i32, p[2] as i32);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        if max - min > 40 && max > 60 {
            r += pr as u64;
            g += pg as u64;
            b += pb as u64;
            n += 1;
        }
    }
    if n == 0 {
        return [0x8a, 0xb4, 0xff];
    }
    let (r, g, b) = ((r / n) as f32, (g / n) as f32, (b / n) as f32);
    let max = r.max(g).max(b).max(1.0);
    let k = 230.0 / max;
    [(r * k).min(255.0) as u8, (g * k).min(255.0) as u8, (b * k).min(255.0) as u8]
}

/// "Spotify.exe" -> "Spotify", "SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify" -> "Spotify",
/// "MSEdge" -> "Edge", "Chrome" -> "Chrome".
fn friendly_app(aumid: &str) -> String {
    let mut s = aumid.rsplit('!').next().unwrap_or(aumid);
    s = s.rsplit('\\').next().unwrap_or(s);
    let s = s.trim_end_matches(".exe").trim_end_matches(".EXE");
    let lower = s.to_ascii_lowercase();
    let known = [
        ("spotify", "Spotify"),
        ("msedge", "Edge"),
        ("chrome", "Chrome"),
        ("firefox", "Firefox"),
        ("brave", "Brave"),
        ("opera", "Opera"),
        ("vlc", "VLC"),
        ("zunemusic", "Media Player"),
        ("microsoft.media.player", "Media Player"),
        ("applemusic", "Apple Music"),
        ("itunes", "iTunes"),
        ("tidal", "TIDAL"),
        ("deezer", "Deezer"),
        ("youtube", "YouTube Music"),
    ];
    for (k, v) in known {
        if lower.contains(k) {
            return v.to_string();
        }
    }
    let last = s.rsplit('.').next().unwrap_or(s);
    let mut c = last.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
