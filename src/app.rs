//! The controller: owns the island views (one per display when "All displays"
//! is chosen), their springs and state machines, the settings window, and
//! wires system services to the UI.
//!
//! Threading model (important in Rust!): Slint objects live on the UI thread
//! only. Service threads never touch them; they send plain data with
//! `slint::invoke_from_event_loop`, and that closure runs on the UI thread,
//! where it can reach `App` through `with_app`.
//!
//! Ownership note: each `View` owns its `Island` component. Callbacks can't
//! hold a `&mut View` (that would be a second mutable borrow), so they capture
//! the view's *index* and look the view up again through `with_app`.

use crate::config::{Config, Edge};
use crate::platform::{self, Kind, WinWindow};
use crate::shelf::{self, DropEvent, Entry};
use crate::spring::Spring;
use crate::system::{self, Battery};
use crate::{Island, NearbyItem, NoteItem, PetSkin, SettingsWindow, ShelfItem};
use crate::{audio, brightness, game, live, lyrics, media, notify, package, pet, update};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::ABN_FULLSCREENAPP;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---- geometry from the design tokens (logical px) ----
/// Wait after the pointer leaves before collapsing.
const COLLAPSE_GRACE_MS: u64 = 100;
const COMPACT_H: f32 = 36.0;
const COMPACT_MAX: f32 = 300.0;
const EXPANDED_W: f32 = 380.0;
const HUD_W: f32 = 260.0;
const NOTCH_R: f32 = 14.0; // radius-tile: compact corners
const ISLAND_R: f32 = 30.0; // radius-island: expanded corners
const PAD: f32 = 16.0;
const HEADER: f32 = 34.0; // switcher row + gap
const SHOULDER: f32 = 10.0;
/// Taskbar mode: a live pill is the notch (about 160) plus a wing each side.
const TB_WINGS_W: f32 = 300.0;
const TAB_W: f32 = 36.0;
const SLIVER_W: f32 = 8.0;
const GLYPH: f32 = 36.0; // one glyph slot in the side tab

/// Island windows are a little larger than the largest shape so the shoulders
/// and spring overshoot fit; the extra pixels are click-through.
const WIN_W: f32 = EXPANDED_W + 60.0; // top / bottom
const WIN_H: f32 = 330.0; // tall enough for the lyrics panel and its soft shadow
const SIDE_W: f32 = EXPANDED_W + 40.0; // left / right
const SIDE_H: f32 = 560.0;

// compact kinds (must match island.slint)
const C_IDLE: i32 = 0;
const C_MEDIA: i32 = 1;
const C_SHELF: i32 = 2;
const C_DROP: i32 = 3;
const C_KEPT: i32 = 4;
const C_CHARGING: i32 = 5;
const C_LOW: i32 = 6;
const C_ERROR: i32 = 7;
const C_TIMER: i32 = 8;
const C_MIC: i32 = 9;
const C_CAMERA: i32 = 10;
const C_FOCUS: i32 = 11;
const C_TIMER_DONE: i32 = 12;
const C_LYRICS: i32 = 13;
const C_PET: i32 = 14;
const C_NEARBY: i32 = 15;
const C_NOTIFY: i32 = 16;
const C_CALL: i32 = 17;
const C_NOTE_NEW: i32 = 18;
/// Quiet pill: featured feature on the left, date + battery on the right.
const C_GLANCE: i32 = 19;

// panels
const P_MEDIA: i32 = 0;
const P_SHELF: i32 = 1;
const P_POWER: i32 = 2;
const P_TIMER: i32 = 3;
const P_LYRICS: i32 = 4;
const P_PET: i32 = 5;
const P_NOTIF: i32 = 6;
const P_GAME: i32 = 7;

// side-tab glyphs
const G_MEDIA: i32 = 0;
const G_CHARGING: i32 = 1;
const G_SHELF: i32 = 2;
const G_LOW: i32 = 3;
const G_TIMER: i32 = 4;
const G_MIC: i32 = 5;
const G_CAMERA: i32 = 6;
const G_FOCUS: i32 = 7;
const G_PET: i32 = 8;
const G_NOTIFY: i32 = 9;
const G_CALL: i32 = 10;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Run `f` with the app. If the app is already borrowed (a re-entrant call),
/// the call is skipped instead of panicking.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

/// Run on the UI thread from any thread.
fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with_app(f);
    });
}

fn cursor() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

/// A running (or paused) countdown.
struct TimerRun {
    total: Duration,
    end: Instant,
    paused_left: Option<Duration>,
}

impl TimerRun {
    fn left(&self) -> Duration {
        self.paused_left.unwrap_or_else(|| self.end.saturating_duration_since(Instant::now()))
    }
}

struct MediaState {
    info: media::Info,
    received: Instant,
}

/// One island on one display.
struct View {
    island: Island,
    win: Rc<WinWindow>,
    monitor: HMONITOR,
    w: Spring,
    h: Spring,
    r: Spring,
    /// Left/right edges: where the shape is centered along the edge (window px).
    a: Spring,
    last_frame: Option<Instant>,
    shape_dirty: bool,
    hovering: bool,
    pressed: bool,
    expanded: bool,
    drag_over: bool,
    panel: i32,
    /// Left/right edges: the tab's center along the edge (window px).
    tab_center: f32,
    /// Left/right edges: which glyph the pointer rests on.
    glyph_slot: Option<usize>,
    press_at: Option<POINT>,
    dragging: bool,
    suppress_click: bool,
    hidden_fullscreen: bool,
    shown: bool,
    hover_timer: Timer,
    leave_timer: Timer,
    /// Mochi's gaze as last sent to this island (quantized), to skip no-op updates.
    look: (f32, f32),
    /// Scrolling for volume over the closed island: don't let hover open it.
    scroll_hold: bool,
}

struct App {
    views: Vec<View>,
    settings: Option<SettingsWindow>,
    cfg: Config,

    hud_active: bool,
    /// One-shot compact states (kept, charging, low battery, error).
    transient: Option<i32>,
    /// Width "flash" when plugging in: briefly widen, then settle.
    flash_wide: bool,
    hidden_user: bool,
    sys_anim: bool,
    /// Taskbar thickness in logical px while in taskbar mode (0 otherwise):
    /// the quiet island is exactly as tall, like a notch in the menu bar.
    taskbar_h: f32,

    media: Option<MediaState>,
    art_id: u64,
    art: slint::Image,
    accent: slint::Color,
    media_tx: Option<Sender<media::Cmd>>,
    audio_tx: Option<Sender<audio::Cmd>>,
    battery: Battery,
    hud: (i32, f32, bool),

    shelf: Vec<Entry>,
    shelf_model: Rc<VecModel<ShelfItem>>,
    pending_drag: Option<usize>,

    hud_timer: Timer,
    transient_timer: Timer,
    flash_timer: Timer,
    progress_timer: Timer,
    wave_timer: Timer,
    wave_start: Instant,
    mem_timer: Timer,
    saved_timer: Timer,
    click_timer: Timer,

    timer: Option<TimerRun>,
    timer_tick: Timer,
    privacy: live::Privacy,
    focus_on: bool,
    /// Esc is registered as a hotkey only while the island was opened from the keyboard.
    esc_armed: bool,
    osd_hidden: bool,

    lyrics: lyrics::Lines,
    /// Timed lines (false: plain lyrics, shown whole without following the song).
    lyrics_synced: bool,
    /// What the lookup found for this song (L_* below).
    lyric_state: i32,
    /// Players that report no timeline: our own clock for the song
    /// (seconds played before `since`, and since when it's been playing).
    clock: (f64, Option<Instant>),
    lyrics_key: String,
    lyric_idx: usize,
    lyrics_model: Rc<VecModel<slint::SharedString>>,
    lyrics_tx: Option<Sender<lyrics::Request>>,
    lyric_timer: Timer,

    // ---- pet ----
    owner: String,
    beacon_id: [u8; 3], // new every run (pet::session_id)
    ble_tx: Option<Sender<pet::Cmd>>,
    ble_ok: bool,
    peers: Vec<pet::Peer>,
    greeted: Vec<[u8; 3]>,
    pet_list_open: bool,
    /// Our pet is out: (host id, host pet name, acknowledged)
    away: Option<([u8; 3], String, bool)>,
    /// A friend's pet staying here
    visitor: Option<pet::Peer>,
    /// We said "go home" to this pet; keep saying it for a little while
    sending_home: Option<[u8; 3]>,
    pet_timer: Timer,
    /// "Mon 26 Oct", refreshed by a single-shot timer at local midnight.
    date_text: String,
    date_timer: Timer,
    /// "15:31" in the user's format, refreshed on the minute while a clock shows.
    time_text: String,
    /// Music paused for 5 s: the pill rests (back to the quiet view).
    media_resting: bool,
    rest_timer: Timer,
    /// Tab switching: page glide, and the sideways-scroll gesture.
    page_timer: Timer,
    xacc: f32,
    xacc_at: Instant,
    xgo_at: Instant,
    /// Petting and carrying Mochi.
    strokes: u32,
    stroke_at: Instant,
    squish_timer: Timer,
    hearts_timer: Timer,
    carrying: bool,
    carry_watch: Timer, // while held: follow the real pointer, drop on release
    /// Tic-tac-toe.
    game_board: game::Board,
    game_over: Option<(u8, Option<[usize; 3]>)>,
    game_thinking: bool,
    game_timer: Timer,
    game_score: (u32, u32, u32),
    game_mode: i32,
    game_net: i32,
    game_code: String,
    game_net_text: String,
    game_room: Option<game::Room>,
    game_tx: Option<Sender<game::Cmd>>,
    // ---- Mochi: gaze, naps, blinks, peeks ----
    pointer_on: bool,      // the pointer hook is installed
    pointer_at: (i32, i32),
    pointer_awake: bool,   // the pointer moved recently, so Mochi is up
    nap_reset_at: Instant,
    eye_timer: Timer,      // coalesces pointer moves to <= 60 updates/s
    nap_timer: Timer,
    blink_timer: Timer,
    peek: Option<usize>,   // the island Mochi is peeking from
    peek_timer: Timer,     // waits between peeks
    peek_step: Timer,      // stay out, then tidy up after ducking
    rng: u64,
    pet_bob: bool,
    visit_timer: Timer,
    home_timer: Timer,
    // ---- Mochi: moods, eating, yanks ----
    mood: i32,             // pet::face, shown until mood_timer ends it
    mood_lively: bool,     // its motion and marks play (the first few seconds)
    mood_timer: Timer,
    lively_timer: Timer,
    act: i32,              // a short scene over the mood (eating, a yank), or -1
    act_lively: bool,
    act_timer: Timer,
    eating: bool,
    feeds: Vec<u64>,       // unix seconds of treats in the last hour (full after 3)
    pokes: Vec<Instant>,   // recent clicks on the peeking Mochi
    excited_at: Option<Instant>,

    // ---- notifications ----
    notify_tx: Option<Sender<notify::Cmd>>,
    /// 0 unknown, 1 unavailable (no package), 2 needs access, 3 denied, 4 working
    notify_state: i32,
    /// Show Windows' access prompt as soon as the listener says it needs it
    /// (the user just turned notifications on, or Set up just restarted Islo).
    notify_ask: bool,
    /// Set up (registering Islo with Windows): running, and its last error.
    notify_setting_up: bool,
    notify_setup_error: String,
    notes: Vec<notify::Note>,
    known_notes: Vec<u32>,
    dismissed: Vec<u32>,
    unread: usize,
    call: Option<notify::Note>,
    call_timer: Timer,

    // ---- updates ----
    /// A newer release found on GitHub, waiting for "Install".
    update: Option<update::Release>,
    /// Same numbers as `update-state` in settings.slint.
    update_state: i32,
    update_desc: String,
    /// Daily check while "Check automatically" is on.
    update_timer: Timer,
    /// The version we already told the user about (one notification per version).
    update_told: String,
}

// update states (must match settings.slint)
const U_IDLE: i32 = 0;
const U_CHECKING: i32 = 1;
const U_AVAILABLE: i32 = 2;
const U_INSTALLING: i32 = 3;
const U_CURRENT: i32 = 4;
const U_ERROR: i32 = 5;

// lyric states (must match `lyric-state` in island.slint)
const L_HIDDEN: i32 = 0;
const L_SEARCHING: i32 = 1;
const L_SYNCED: i32 = 2;
const L_PLAIN: i32 = 3;
const L_MISSING: i32 = 4;
const L_INSTRUMENTAL: i32 = 5;
const L_OFFLINE: i32 = 6;

impl App {
    fn reduce_motion(&self) -> bool {
        self.cfg.reduce_motion || !self.sys_anim
    }

    fn media_visible(&self) -> bool {
        self.cfg.media && self.media.is_some()
    }

    /// Media worth showing in the closed island: playing, or paused under 5 s
    /// (with "Rest after music pauses" on; otherwise whenever there is media).
    fn media_live(&self) -> bool {
        self.media_visible() && !(self.cfg.media_rest && self.media_resting)
    }

    /// Lyrics to show for this song (synced or plain).
    fn lyrics_on(&self) -> bool {
        self.cfg.lyrics && self.media_visible() && !self.lyrics.is_empty()
    }

    /// Timed lyrics that follow the song (the compact lyric views need these).
    fn synced_on(&self) -> bool {
        self.lyrics_on() && self.lyrics_synced
    }

    fn playing(&self) -> bool {
        self.media.as_ref().is_some_and(|m| m.info.playing)
    }

    fn any_shown(&self) -> bool {
        self.views.iter().any(|v| v.shown)
    }

    /// Set something on every island.
    fn each(&self, f: impl Fn(&Island)) {
        for v in &self.views {
            f(&v.island);
        }
    }

    // ---------------- per-view state ----------------

    fn mode(&self, v: &View) -> i32 {
        if v.expanded {
            2
        } else if self.hud_active && !v.dragging {
            1
        } else {
            0
        }
    }

    fn edge_of(&self, v: &View) -> i32 {
        if v.dragging { 4 } else { self.cfg.edge.index() }
    }

    fn vertical(&self, v: &View) -> bool {
        !v.dragging && self.cfg.edge.vertical()
    }

    /// Left/right edges at rest show the glyph tab instead of the compact pill.
    fn tab(&self, v: &View) -> bool {
        self.vertical(v) && self.mode(v) == 0 && self.transient.is_none() && !v.drag_over
    }

    /// What the compact pill shows, in priority order.
    fn compact_kind(&self, v: &View) -> i32 {
        if v.drag_over {
            return C_DROP;
        }
        if let Some(k) = self.transient {
            return k;
        }
        if self.call.is_some() {
            return C_CALL;
        }
        if self.cfg.privacy && self.privacy.mic.is_some() {
            return C_MIC;
        }
        if self.cfg.privacy && self.privacy.camera.is_some() {
            return C_CAMERA;
        }
        if self.cfg.timer && self.timer.is_some() {
            return C_TIMER;
        }
        // (unread notifications don't take over the pill: they show as a bell badge)
        if self.media_live() {
            return if self.synced_on() && self.cfg.lyric_lines >= 2 { C_LYRICS } else { C_MEDIA };
        }
        if self.low_battery(10) {
            return C_LOW;
        }
        if self.cfg.focus && self.focus_on {
            return C_FOCUS;
        }
        if self.cfg.pet && self.visitor.is_some() {
            return C_PET;
        }
        if self.cfg.shelf && !self.shelf.is_empty() {
            return C_SHELF;
        }
        // (in the taskbar the clock, date and battery are already beside it)
        if self.cfg.pill_glance && !self.cfg.taskbar {
            return C_GLANCE; // date + battery, with the featured feature (Mochi naps here too)
        }
        if self.cfg.pet {
            return C_PET; // Mochi naps in the island when nothing else is going on
        }
        C_IDLE
    }

    /// The feature shown on the left of the quiet pill, if its module is on.
    fn featured(&self) -> i32 {
        let c = &self.cfg;
        match c.featured {
            1 if c.media => 1,
            2 if c.timer => 2,
            3 if c.pet => 3,
            4 if c.focus => 4,
            _ => 0,
        }
    }

    /// Recompute the date and arm one timer for the next local midnight.
    fn refresh_date(&mut self) {
        let (date, to_midnight) = system::today();
        let (time, to_minute) = system::clock_now();
        let changed = date != self.date_text || time != self.time_text;
        self.date_text = date;
        self.time_text = time;
        if changed {
            self.push_clock();
        }
        // wake once a minute only while a clock is on show; else at midnight
        let clock_shown = self.cfg.clock != 2 && (self.cfg.pill_glance || self.cfg.header_date) && self.any_shown();
        let wait = if clock_shown { to_minute } else { to_midnight };
        self.date_timer.start(TimerMode::SingleShot, Duration::from_millis(wait), || {
            with_app(|a| a.refresh_date());
        });
    }

    /// What the quiet pill shows on the right: clock, clock and date, or date.
    fn clock_text(&self) -> String {
        match self.cfg.clock {
            0 => self.time_text.clone(),
            2 => self.date_text.clone(),
            _ => format!("{} · {}", self.time_text, self.date_text),
        }
    }

    /// The same for the open island's header, shortened when the tabs need the room.
    fn header_clock(&self, v: &View) -> String {
        if !self.cfg.header_date {
            return String::new();
        }
        let n = self.panels(v).len();
        let tabs = if n > 1 { n as f32 * 28.0 } else { 0.0 };
        let ring = if self.cfg.battery && self.battery.present && v.panel != P_POWER { 26.0 } else { 0.0 };
        let room = EXPANDED_W - 2.0 * PAD - tabs - ring - 2.0 * 28.0 - 8.0;
        fit_clock(self.cfg.clock, &self.time_text, &self.date_text, room)
    }

    fn push_clock(&self) {
        let (pill, date) = (self.clock_text(), self.date_text.clone());
        for v in &self.views {
            v.island.set_clock_text(pill.clone().into());
            v.island.set_date_text(date.clone().into());
            v.island.set_header_clock(self.header_clock(v).into());
        }
    }

    /// Rest mode: 5 s after the music pauses, the pill goes back to the quiet view.
    fn update_rest(&mut self, had_media: bool, was_playing: bool) {
        if self.media.is_none() || self.playing() {
            self.rest_timer.stop();
            self.media_resting = false;
            return;
        }
        if !had_media {
            self.media_resting = true; // already paused when we first saw it
        } else if was_playing {
            self.media_resting = false;
            self.rest_timer.start(TimerMode::SingleShot, Duration::from_secs(5), || {
                with_app(|a| {
                    a.media_resting = true;
                    a.retarget();
                });
            });
        }
    }

    // ---------------- Mochi: gaze, naps, blinks, peeks ----------------

    fn rand(&mut self) -> u32 {
        // xorshift64*: plenty for picking spots and delays
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }

    fn rand_range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.rand() as u64 % (hi - lo).max(1)
    }

    /// Mochi is on screen somewhere (so its eyes are worth moving).
    fn mochi_visible(&self) -> bool {
        if !self.cfg.pet || self.away.is_some() {
            return false;
        }
        self.peek.is_some()
            || self.views.iter().any(|v| {
                if !v.shown {
                    return false;
                }
                if v.expanded {
                    return v.panel == P_PET;
                }
                let k = self.compact_kind(v);
                self.mode(v) == 0 && (k == C_PET || (k == C_GLANCE && self.featured() == 3))
            })
    }

    /// Install the pointer hook only while it's useful; drop it the moment it isn't.
    fn update_pointer_watch(&mut self) {
        let want = self.cfg.pet_track && self.mochi_visible();
        if want == self.pointer_on {
            return;
        }
        let cb: Option<Box<dyn Fn(i32, i32)>> = if want {
            Some(Box::new(|x, y| {
                with_app(|a| a.on_pointer_move(x, y));
            }))
        } else {
            None
        };
        self.pointer_on = system::watch_pointer(cb);
        if self.pointer_on {
            let p = cursor();
            self.pointer_at = (p.x, p.y);
            self.pointer_awake = true;
            self.restart_nap();
            self.update_eyes();
            self.schedule_blink();
        } else {
            self.pointer_awake = false;
            self.eye_timer.stop();
            self.nap_timer.stop();
            self.blink_timer.stop();
            for v in &mut self.views {
                v.look = (0.0, 0.0);
                let g = v.island.global::<PetSkin>();
                g.set_look_x(0.0);
                g.set_look_y(0.0);
                g.set_blink(0.0);
            }
        }
    }

    /// Runs inside the mouse hook: keep it to a store and (at most) one timer start.
    fn on_pointer_move(&mut self, x: i32, y: i32) {
        self.pointer_at = (x, y);
        if !self.pointer_awake {
            self.pointer_awake = true;
            self.update_pet_anim();
            self.schedule_blink();
        }
        if self.nap_reset_at.elapsed() > Duration::from_secs(1) {
            self.restart_nap();
        }
        if !self.eye_timer.running() {
            self.eye_timer.start(TimerMode::SingleShot, Duration::from_millis(16), || {
                with_app(|a| a.update_eyes());
            });
        }
    }

    /// Mochi naps after a minute without pointer movement (getting sleepy first,
    /// if it's out where you'd see it).
    fn restart_nap(&mut self) {
        self.nap_reset_at = Instant::now();
        self.nap_timer.start(TimerMode::SingleShot, Duration::from_secs(60), || {
            with_app(|a| {
                if a.mochi_visible() && !a.reduce_motion() && a.mood == pet::face::NEUTRAL && a.act < 0 {
                    a.set_mood(pet::face::SLEEPY, 3000);
                    a.nap_timer.start(TimerMode::SingleShot, Duration::from_millis(3000), || {
                        with_app(|a| a.fall_asleep());
                    });
                } else {
                    a.fall_asleep();
                }
            });
        });
    }

    fn fall_asleep(&mut self) {
        self.pointer_awake = false;
        self.blink_timer.stop();
        self.update_pet_anim();
    }

    fn schedule_blink(&mut self) {
        if !self.pointer_on {
            return;
        }
        let ms = self.rand_range(2800, 6500);
        self.blink_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), || {
            with_app(|a| {
                if !a.pointer_awake && a.peek.is_none() {
                    return;
                }
                a.each(|isl| isl.global::<PetSkin>().set_blink(1.0));
                a.blink_timer.start(TimerMode::SingleShot, Duration::from_millis(110), || {
                    with_app(|a| {
                        a.each(|isl| isl.global::<PetSkin>().set_blink(0.0));
                        a.schedule_blink();
                    });
                });
            });
        });
    }

    /// Point every Mochi's eyes at the pointer.
    fn update_eyes(&mut self) {
        let (px, py) = self.pointer_at;
        let peek = self.peek;
        for i in 0..self.views.len() {
            let v = &self.views[i];
            if !v.shown {
                continue;
            }
            let (x, y, w, h) = self.pill_rect(v);
            // roughly where Mochi sits on this island
            let (cx, cy) = if peek == Some(i) {
                (x + w / 2.0, y + h)
            } else if v.expanded {
                (x + 16.0 + 24.0, y + 16.0 + 34.0 + 24.0)
            } else {
                (x + 22.0, y + h / 2.0)
            };
            let (wx, wy) = v.win.pos();
            let s = v.win.scale();
            let (dx, dy) = (px as f32 - (wx as f32 + cx * s), py as f32 - (wy as f32 + cy * s));
            let dist = (dx * dx + dy * dy).sqrt().max(1.0);
            let k = (dist / (140.0 * s)).min(1.0) / dist;
            let q = |f: f32| (f * 20.0).round() / 20.0;
            let look = (q(dx * k), q(dy * k));
            if look != v.look {
                let g = v.island.global::<PetSkin>();
                g.set_look_x(look.0);
                g.set_look_y(look.1);
                self.views[i].look = look;
            }
        }
    }

    /// Can Mochi peek from this island right now? It stays out while you hover
    /// and while the island is open (riding its edge), except on its own panel
    /// or when the open island leaves no room for it in the window.
    fn peek_ok(&self, v: &View) -> bool {
        let c = &self.cfg;
        let size = pet::SIZES[c.pet_size.clamp(0, 2) as usize];
        c.pet && c.pet_peek && !self.reduce_motion() && self.away.is_none() && self.visitor.is_none()
            && v.shown && !v.dragging && !v.drag_over && !v.pressed
            && !(v.expanded && v.panel == P_PET)
            && v.h.target.max(v.h.value) + size * 0.8 <= WIN_H
            && matches!(self.mode(v), 0 | 2) && !self.tab(v)
            && matches!(self.edge_of(v), 0 | 1 | 4)
            // anything calm, music included; not over calls, drops or alerts
            && matches!(self.compact_kind(v), C_GLANCE | C_PET | C_IDLE | C_MEDIA | C_LYRICS | C_SHELF | C_FOCUS | C_TIMER | C_LOW)
    }

    /// Keep the peek schedule in step with what the island is doing.
    fn update_peek(&mut self) {
        if let Some(i) = self.peek {
            // never out of your hand: it lands first, then may hide
            if !self.carrying && self.views.get(i).is_none_or(|v| !self.peek_ok(v)) {
                self.duck_peek();
            }
            return;
        }
        let any = self.views.iter().any(|v| self.peek_ok(v));
        if !any {
            self.peek_timer.stop();
        } else if !self.peek_timer.running() {
            let (lo, hi) = pet::OFTEN[self.cfg.pet_often.clamp(0, 2) as usize];
            let ms = self.rand_range(lo, hi);
            self.peek_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), || {
                with_app(|a| a.start_peek());
            });
        }
    }

    fn start_peek(&mut self) {
        let ok: Vec<usize> = (0..self.views.len()).filter(|&i| self.peek_ok(&self.views[i])).collect();
        if ok.is_empty() || self.peek.is_some() {
            return;
        }
        let i = ok[self.rand() as usize % ok.len()];
        let walk = match self.cfg.pet_moves {
            0 => false,
            1 => true,
            _ => self.rand() % 2 == 0,
        };
        self.peek = Some(i);
        self.greet_mood();
        if walk {
            let from_left = self.rand() % 2 == 0;
            self.walk_step(i, from_left, 0);
        } else {
            let edge = self.edge_of(&self.views[i]);
            // below the pill is the classic peek; sides now and then
            let sides: &[i32] = match edge {
                1 => &[4, 4, 2, 3],
                4 => &[1, 4, 2, 3],
                _ => &[1, 1, 2, 3],
            };
            let side = sides[self.rand() as usize % sides.len()];
            let pos = 0.15 + (self.rand() % 700) as f32 / 1000.0;
            let isl = &self.views[i].island;
            isl.set_peek_glide(0);
            isl.set_peek_walking(false);
            isl.set_peek_pos(pos);
            isl.set_peek_side(side);
            isl.set_peek_out(true);
            let stay = self.rand_range(2400, 4200);
            self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(stay), || {
                with_app(|a| a.duck_peek());
            });
        }
        self.update_pointer_watch();
        self.update_eyes();
        self.update_pet_anim();
    }

    /// Mochi walks around the island: out from one side, round the corner,
    /// along the free edge (with a pause to look around), round the other
    /// corner, and back in. Each step is one Slint animation plus one
    /// single-shot timer, so nothing ticks between steps.
    fn walk_step(&mut self, i: usize, from_left: bool, step: u32) {
        if self.peek != Some(i) {
            return; // ducked meanwhile (poked, hovered, island woke up)
        }
        let Some(v) = self.views.get(i) else { return };
        let edge = self.edge_of(v);
        // the long free edge: below the pill, or above it on the bottom edge
        let far = if edge == 1 { 4 } else { 1 };
        let (start, end) = if from_left { (2, 3) } else { (3, 2) };
        let (p0, p1) = if from_left { (0.0, 1.0) } else { (1.0, 0.0) };
        // walking speed: ~45 px/s along the pill
        let span = (v.w.value - 2.0 * v.r.value - pet::SIZES[self.cfg.pet_size.clamp(0, 2) as usize]).max(20.0);
        let isl = v.island.clone_strong();
        let (wait, next) = match step {
            // pop out of the starting side
            0 => {
                isl.set_peek_glide(0);
                isl.set_peek_walking(false);
                isl.set_peek_turning(false);
                isl.set_peek_pos(0.5);
                isl.set_peek_side(start);
                isl.set_peek_out(true);
                (900, 1)
            }
            // round the corner onto the free edge
            1 => {
                isl.set_peek_glide(320);
                isl.set_peek_turning(true);
                isl.set_peek_side(far);
                isl.set_peek_pos(p0);
                (360, 2)
            }
            // walk to somewhere in the middle
            2 => {
                let mid = 0.35 + (self.rand() % 300) as f32 / 1000.0;
                let ms = (span * (mid - p0).abs() / 45.0 * 1000.0) as u64;
                isl.set_peek_turning(false);
                isl.set_peek_walking(true);
                isl.set_peek_glide(ms as i64);
                isl.set_peek_pos(mid);
                (ms, 3)
            }
            // stop and look around for a moment
            3 => {
                isl.set_peek_walking(false);
                (self.rand_range(900, 1800), 4)
            }
            // on to the far end
            4 => {
                let ms = (span * (p1 - isl.get_peek_pos()).abs() / 45.0 * 1000.0) as u64;
                isl.set_peek_walking(true);
                isl.set_peek_glide(ms as i64);
                isl.set_peek_pos(p1);
                (ms, 5)
            }
            // round the last corner
            5 => {
                isl.set_peek_walking(false);
                isl.set_peek_glide(320);
                isl.set_peek_turning(true);
                isl.set_peek_side(end);
                isl.set_peek_pos(0.5);
                (1100, 6)
            }
            _ => {
                self.duck_peek();
                return;
            }
        };
        self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(wait), move || {
            with_app(|a| a.walk_step(i, from_left, next));
        });
    }

    /// Mochi hides behind the island again (also when poked or the island wakes up).
    fn duck_peek(&mut self) {
        let Some(i) = self.peek else { return };
        if self.carrying {
            return; // held: it hides once you let go
        }
        if let Some(v) = self.views.get(i) {
            v.island.set_peek_walking(false);
            v.island.set_peek_turning(false);
            v.island.set_peek_out(false);
        }
        self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(260), move || {
            with_app(|a| {
                if let Some(v) = a.views.get(i) {
                    v.island.set_peek_side(0);
                    v.island.set_pet_out(false); // back in the pill / Pet tab
                }
                a.peek = None;
                a.update_peek();
                a.update_pointer_watch();
                a.update_pet_anim();
            });
        });
    }


    // ---------------- 0.3.12: scroll, tabs, pin, shelf, petting ----------------

    /// Up/down over the island: volume. The HUD shows at once, and hover doesn't
    /// open the island under the wheel.
    fn on_scroll(&mut self, i: usize, dy: f32) {
        if !self.cfg.hud || dy == 0.0 {
            return;
        }
        if let Some(v) = self.views.get_mut(i) {
            if !v.expanded {
                v.hover_timer.stop();
                v.scroll_hold = true;
            }
        }
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(audio::Cmd::Step(if dy > 0.0 { 2.0 } else { -2.0 }));
        }
    }

    /// Sideways scroll (touchpad, or Shift + wheel) while open: one tab per gesture.
    fn on_scroll_x(&mut self, i: usize, dx: f32) {
        let now = Instant::now();
        if now - self.xacc_at > Duration::from_millis(250) {
            self.xacc = 0.0;
        }
        self.xacc += dx;
        self.xacc_at = now;
        if self.xacc.abs() > 40.0 && now - self.xgo_at > Duration::from_millis(320) {
            self.xgo_at = now;
            let dir = if self.xacc < 0.0 { 1 } else { -1 };
            self.xacc = 0.0;
            self.tab_step(i, dir);
        }
    }

    fn tab_step(&mut self, i: usize, dir: i32) {
        let Some(v) = self.views.get(i) else { return };
        if !v.expanded {
            return;
        }
        let panels = self.panels(v);
        let cur = panels.iter().position(|p| *p == v.panel).unwrap_or(0) as i32;
        let next = cur + dir;
        if next < 0 || next >= panels.len() as i32 {
            self.page_bump(i, dir);
            return;
        }
        self.open_panel(i, panels[next as usize]);
    }

    /// Switch the open island to panel `p`, sliding the content in from the
    /// side we're heading to.
    fn open_panel(&mut self, i: usize, p: i32) {
        let Some(v) = self.views.get(i) else { return };
        let panels = self.panels(v);
        let from = panels.iter().position(|x| *x == v.panel).unwrap_or(0) as i32;
        let to = panels.iter().position(|x| *x == p).unwrap_or(0) as i32;
        if let Some(v) = self.views.get_mut(i) {
            v.panel = p;
        }
        if p == P_POWER {
            self.refresh_power();
        }
        if p == P_NOTIF {
            self.unread = 0;
            self.call = None;
            self.update_notes_ui();
        }
        if p == P_GAME {
            self.update_game_ui();
        }
        if p == P_PET {
            self.greet_mood();
        }
        self.retarget();
        if to != from {
            self.page_slide(i, if to > from { 56.0 } else { -56.0 });
        }
    }

    /// The new content starts `from` px aside and glides in (with a little overshoot).
    fn page_slide(&mut self, i: usize, from: f32) {
        let Some(v) = self.views.get(i) else { return };
        if self.reduce_motion() {
            v.island.set_page_x(0.0);
            return;
        }
        v.island.set_page_anim(false);
        v.island.set_page_x(from);
        self.page_timer.start(TimerMode::SingleShot, Duration::from_millis(16), move || {
            with_app(|a| {
                if let Some(v) = a.views.get(i) {
                    v.island.set_page_anim(true);
                    v.island.set_page_x(0.0);
                }
            });
        });
    }

    /// Pushing past the first or last tab: a small bounce instead of looping.
    fn page_bump(&mut self, i: usize, dir: i32) {
        let Some(v) = self.views.get(i) else { return };
        if self.reduce_motion() {
            return;
        }
        v.island.set_page_anim(true);
        v.island.set_page_x(-dir as f32 * 14.0);
        self.page_timer.start(TimerMode::SingleShot, Duration::from_millis(140), move || {
            with_app(|a| {
                if let Some(v) = a.views.get(i) {
                    v.island.set_page_x(0.0);
                }
            });
        });
    }

    /// Dragging the content sideways: it follows the finger (a third of the way
    /// past the ends); let go past 60 px and the next tab slides in.
    fn on_swipe(&mut self, i: usize, dx: f32, done: bool) {
        let Some(v) = self.views.get(i) else { return };
        let panels = self.panels(v);
        let cur = panels.iter().position(|p| *p == v.panel).unwrap_or(0);
        let at_end = (cur == 0 && dx > 0.0) || (cur + 1 >= panels.len() && dx < 0.0);
        let x = if at_end { dx * 0.35 } else { dx };
        if !done {
            v.island.set_page_anim(false);
            v.island.set_page_x(x);
            return;
        }
        if !at_end && dx.abs() > 60.0 {
            let next = if dx < 0.0 { cur + 1 } else { cur - 1 };
            self.open_panel(i, panels[next]);
        } else {
            v.island.set_page_anim(true);
            v.island.set_page_x(0.0);
        }
    }

    /// The pin in the header (and Settings): keep the island open.
    fn toggle_pin(&mut self, i: usize) {
        self.cfg.keep_open = !self.cfg.keep_open;
        self.cfg.save();
        self.push_island_settings();
        if self.cfg.keep_open {
            if self.views.get(i).is_some_and(|v| !v.expanded) {
                self.expand(i);
            }
        } else {
            for k in 0..self.views.len() {
                if !self.cursor_over_pill(k) {
                    self.collapse(k);
                }
            }
        }
        self.retarget();
    }

    fn unpin(&mut self) {
        if self.cfg.keep_open {
            self.cfg.keep_open = false;
            self.cfg.save();
            self.push_island_settings();
        }
    }

    /// Files into the shelf (drop, "Add files…", "Paste"). Returns how many.
    fn add_to_shelf(&mut self, files: Vec<std::path::PathBuf>) -> usize {
        let mut added = 0;
        for f in files {
            if !f.exists() {
                continue;
            }
            if let Some(k) = self.shelf.iter().position(|e| e.path == f) {
                self.shelf.remove(k);
            }
            self.shelf.insert(0, shelf::make_entry(f)); // newest first
            added += 1;
        }
        self.shelf.truncate(12);
        self.rebuild_shelf();
        self.retarget();
        added
    }

    /// Stroking Mochi: a squish with each stroke, then happy eyes and a heart.
    fn pet_stroke(&mut self) {
        if !self.cfg.pet || !self.cfg.pet_touch || self.eating || self.carrying {
            return;
        }
        let now = Instant::now();
        if now - self.stroke_at > Duration::from_millis(1500) {
            self.strokes = 0;
        }
        self.strokes += 1;
        self.stroke_at = now;
        self.pointer_awake = true;
        if !self.reduce_motion() {
            self.each(|isl| isl.global::<PetSkin>().set_squish(-0.12));
            self.squish_timer.start(TimerMode::SingleShot, Duration::from_millis(90), || {
                with_app(|a| a.each(|isl| isl.global::<PetSkin>().set_squish(0.0)));
            });
        }
        if self.strokes >= 2 {
            self.set_mood(pet::face::HAPPY, 2500);
            if self.strokes % 3 == 2 {
                self.each(|isl| isl.global::<PetSkin>().set_hearts(true));
                self.hearts_timer.start(TimerMode::SingleShot, Duration::from_millis(1900), || {
                    with_app(|a| a.each(|isl| isl.global::<PetSkin>().set_hearts(false)));
                });
            }
        }
        // a petted Mochi stays out a little longer
        if let Some(i) = self.peek {
            self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(3500), move || {
                with_app(|a| {
                    if a.peek == Some(i) && !a.carrying {
                        a.duck_peek()
                    }
                });
            });
        }
        self.update_pet_anim();
    }

    /// Mochi picked up (it hangs from the pointer, eyes wide). Works on the
    /// peeking Mochi and on the one in the pill or the Pet tab, which comes out.
    fn pet_carry(&mut self, i: usize, x: f32, y: f32) {
        if !self.cfg.pet_touch || self.eating {
            return;
        }
        if self.peek.is_none() && !self.lift_pet(i) {
            return;
        }
        if self.peek != Some(i) {
            return; // out on another display
        }
        let Some(v) = self.views.get(i) else { return };
        let (ww, wh) = self.win_size(v);
        let size = pet::SIZES[self.cfg.pet_size.clamp(0, 2) as usize];
        let isl = v.island.clone_strong();
        if !self.carrying {
            self.carrying = true;
            self.peek_step.stop(); // no walking or ducking while held
            isl.set_peek_walking(false);
            isl.set_peek_turning(false);
            self.set_act(pet::face::SURPRISED, true);
            isl.global::<PetSkin>().set_jolt(0.14); // dangling: a little stretched
            // The press can belong to the pill or the Pet tab, which may close
            // mid-drag and miss the release. So while held (only then), follow
            // the real pointer each frame and let go when the button is up.
            self.carry_watch.start(TimerMode::Repeated, Duration::from_millis(16), move || {
                with_app(|a| a.carry_follow(i));
            });
        }
        isl.set_peek_free(true);
        isl.set_peek_fx(clamp_safe(x, size / 2.0, ww - size / 2.0));
        isl.set_peek_fy(clamp_safe(y, size / 2.0, wh - size / 2.0));
    }

    fn carry_follow(&mut self, i: usize) {
        if !self.carrying || i >= self.views.len() {
            self.carry_watch.stop();
            return;
        }
        let v = &self.views[i];
        let pt = cursor();
        let (wx, wy) = v.win.pos();
        let s = v.win.scale();
        let (x, y) = ((pt.x - wx) as f32 / s, (pt.y - wy) as f32 / s);
        if unsafe { GetKeyState(VK_LBUTTON.0 as i32) } < 0 {
            let (ww, wh) = self.win_size(v);
            let size = pet::SIZES[self.cfg.pet_size.clamp(0, 2) as usize];
            v.island.set_peek_fx(clamp_safe(x, size / 2.0, ww - size / 2.0));
            v.island.set_peek_fy(clamp_safe(y, size / 2.0, wh - size / 2.0));
        } else {
            self.pet_drop(i, x, y);
        }
    }

    /// Mochi out of the pill or the Pet tab and into your pointer: it becomes the
    /// peeking Mochi, held. The open island closes so there's room to put it down.
    fn lift_pet(&mut self, i: usize) -> bool {
        let c = &self.cfg;
        let Some(v) = self.views.get(i) else { return false };
        let ok = c.pet && c.pet_touch && self.away.is_none() && self.visitor.is_none()
            && v.shown && !v.dragging && !v.drag_over && matches!(self.edge_of(v), 0 | 1 | 4);
        if !ok {
            return false;
        }
        self.peek_timer.stop();
        self.peek = Some(i);
        let isl = v.island.clone_strong();
        isl.set_peek_glide(0);
        isl.set_peek_walking(false);
        isl.set_peek_turning(false);
        isl.set_peek_side(1);
        isl.set_peek_pos(0.5);
        isl.set_peek_out(true);
        isl.set_peek_free(true);
        isl.set_pet_out(true);
        if self.views[i].expanded && !self.cfg.keep_open {
            self.collapse(i);
        }
        self.update_pointer_watch();
        self.update_eyes();
        self.update_pet_anim();
        true
    }

    /// Let go: Mochi lands on the nearest spot around the island with a squash.
    fn pet_drop(&mut self, i: usize, x: f32, y: f32) {
        if !self.carrying {
            return;
        }
        self.carrying = false;
        self.carry_watch.stop();
        let Some(v) = self.views.get(i) else { return };
        let size = pet::SIZES[self.cfg.pet_size.clamp(0, 2) as usize];
        let (px, py, w, h) = self.pill_rect(v);
        let r = v.r.value.min(h / 2.0);
        let span = (w - 2.0 * r - size).max(1.0);
        let t = ((x - px - r - size / 2.0) / span).clamp(0.0, 1.0);
        let along = px + r + span * t + size / 2.0;
        let mid_y = py + (h - size).max(0.0) / 2.0 + size / 2.0;
        // the spots Mochi can sit at, as (side, center x, center y)
        let mut spots = vec![(2, px - 0.22 * size, mid_y), (3, px + w + 0.22 * size, mid_y)];
        match self.edge_of(v) {
            1 => spots.push((4, along, py - 0.22 * size)),
            4 => {
                spots.push((1, along, py + h + 0.22 * size));
                spots.push((4, along, py - 0.22 * size));
            }
            _ => spots.push((1, along, py + h + 0.22 * size)),
        }
        let dist = |s: &(i32, f32, f32)| (s.1 - x).powi(2) + (s.2 - y).powi(2);
        let side = spots.iter().min_by(|a, b| dist(a).total_cmp(&dist(b))).map(|s| s.0).unwrap_or(1);
        let isl = v.island.clone_strong();
        isl.set_peek_glide(300);
        isl.set_peek_landing(true);
        isl.set_peek_turning(true);
        isl.set_peek_side(side);
        isl.set_peek_pos(if side == 2 || side == 3 { 0.5 } else { t });
        isl.set_peek_out(true);
        isl.set_peek_free(false);
        self.set_act(-1, false);
        // land: squash, then wobble back; happy to be put down
        Timer::single_shot(Duration::from_millis(300), move || {
            with_app(|a| {
                if let Some(v) = a.views.get(i) {
                    v.island.set_peek_turning(false);
                    v.island.set_peek_landing(false);
                    v.island.global::<PetSkin>().set_jolt(-0.16);
                }
                Timer::single_shot(Duration::from_millis(110), move || {
                    with_app(|a| {
                        if let Some(v) = a.views.get(i) {
                            v.island.global::<PetSkin>().set_jolt(0.0);
                        }
                    });
                });
                a.set_mood(pet::face::HAPPY, 2000);
            });
        });
        // it stays where you put it for a while, then hides
        self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(5000), move || {
            with_app(|a| {
                if a.peek == Some(i) && !a.carrying {
                    a.duck_peek()
                }
            });
        });
    }

    // ---------------- tic-tac-toe ----------------

    fn update_game_ui(&self) {
        let online = self.game_mode == 1;
        let (board, over, mine) = match (&self.game_room, online) {
            (Some(r), true) if self.game_net == 2 => {
                // from my side: my cells are x (1), theirs o (2)
                let b: game::Board = std::array::from_fn(|k| match r.board[k] {
                    0 => 0,
                    s if s == r.seat => 1,
                    _ => 2,
                });
                (b, game::outcome(&b), r.turn == r.seat)
            }
            _ => (self.game_board, self.game_over, !self.game_thinking),
        };
        let win: Vec<bool> = (0..9).map(|k| over.and_then(|o| o.1).is_some_and(|l| l.contains(&k))).collect();
        let (status, face) = match over {
            Some((1, _)) => ("You win", pet::face::SAD),
            Some((2, _)) => (if online { "They win" } else { "Mochi wins" }, if online { 0 } else { pet::face::HAPPY }),
            Some(_) => ("Draw", 0),
            None if online && !mine => ("Their turn", 0),
            None if self.game_thinking => ("Mochi is thinking", 0),
            None => ("Your turn", 0),
        };
        let sub = if online {
            format!("Online · room {}", self.game_room.as_ref().map(|r| r.code.as_str()).unwrap_or(""))
        } else {
            let (y, m, d) = self.game_score;
            format!("You {y} · Mochi {m} · Draws {d}")
        };
        let btn = if !online {
            "New game"
        } else if over.is_some() {
            "Play again"
        } else {
            "Leave"
        };
        let cells: Vec<i32> = board.iter().map(|&c| c as i32).collect();
        let level = self.cfg.game_level;
        let ok = self.cfg.games_online && !game::RELAY.is_empty();
        self.each(|isl| {
            isl.set_game_cells(ModelRc::new(VecModel::from(cells.clone())));
            isl.set_game_win(ModelRc::new(VecModel::from(win.clone())));
            isl.set_game_status(status.into());
            isl.set_game_sub(sub.clone().into());
            isl.set_game_btn(btn.into());
            isl.set_game_face(face);
            isl.set_game_thinking(self.game_thinking);
            isl.set_game_mode(self.game_mode);
            isl.set_game_level(level);
            isl.set_game_net(self.game_net);
            isl.set_game_code(self.game_code.clone().into());
            isl.set_game_net_text(self.game_net_text.clone().into());
            isl.set_game_online_ok(ok);
        });
    }

    fn game_play(&mut self, k: usize) {
        if k >= 9 {
            return;
        }
        if self.game_mode == 1 {
            if let (Some(r), Some(tx)) = (&self.game_room, &self.game_tx) {
                if self.game_net == 2 && r.turn == r.seat && r.board[k] == 0 && game::outcome(&r.board).is_none() {
                    let _ = tx.send(game::Cmd::Move(k));
                }
            }
            return;
        }
        if self.game_over.is_some() || self.game_thinking || self.game_board[k] != 0 {
            return;
        }
        self.game_board[k] = 1;
        if self.game_settle() {
            return;
        }
        self.game_thinking = true;
        self.update_game_ui();
        self.game_timer.start(TimerMode::SingleShot, Duration::from_millis(560), || {
            with_app(|a| {
                let level = a.cfg.game_level;
                let board = a.game_board;
                let mut rng = a.rng;
                let m = game::pick(&board, level, |n| {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    (rng % n.max(1) as u64) as u32
                });
                a.rng = rng;
                a.game_thinking = false;
                if let Some(m) = m {
                    a.game_board[m] = 2;
                }
                a.game_settle();
            });
        });
    }

    /// After a move: record a finished game. True when it's over.
    fn game_settle(&mut self) -> bool {
        self.game_over = game::outcome(&self.game_board);
        match self.game_over {
            Some((1, _)) => {
                self.game_score.0 += 1;
                self.set_mood(pet::face::SAD, 2500);
            }
            Some((2, _)) => {
                self.game_score.1 += 1;
                self.set_mood(pet::face::HAPPY, 2500);
            }
            Some(_) => self.game_score.2 += 1,
            None => {}
        }
        self.update_game_ui();
        self.game_over.is_some()
    }

    fn game_new(&mut self) {
        if self.game_mode == 1 {
            if let Some(tx) = &self.game_tx {
                let over = self.game_room.as_ref().is_some_and(|r| game::outcome(&r.board).is_some());
                let _ = tx.send(if over { game::Cmd::Again } else { game::Cmd::Leave });
            }
            return;
        }
        self.game_timer.stop();
        self.game_board = [0; 9];
        self.game_over = None;
        self.game_thinking = false;
        self.update_game_ui();
    }

    fn game_mode_set(&mut self, mode: i32) {
        if mode == self.game_mode {
            return;
        }
        if self.game_mode == 1 && self.game_room.is_some() {
            if let Some(tx) = &self.game_tx {
                let _ = tx.send(game::Cmd::Leave);
            }
        }
        self.game_mode = mode;
        self.game_room = None;
        if mode == 1 {
            (self.game_net, self.game_net_text) = if !self.cfg.games_online {
                (4, "Online games are off. Turn them on in Settings › Modules › Games.".into())
            } else if game::RELAY.is_empty() {
                (4, "Online play isn't set up in this build of Islo.".into())
            } else {
                (0, "Play a friend anywhere: host a game and send them the code, or copy theirs and paste it here.".into())
            };
        }
        self.update_game_ui();
    }

    fn game_sender(&mut self) -> Option<Sender<game::Cmd>> {
        if self.game_tx.is_none() {
            self.game_tx = Some(game::start(|ev| post(move |a| a.on_game(ev))));
        }
        self.game_tx.clone()
    }

    fn game_host(&mut self) {
        if !self.cfg.games_online {
            return;
        }
        if let Some(tx) = self.game_sender() {
            self.game_net = 5;
            self.game_net_text = "Making a room…".into();
            let _ = tx.send(game::Cmd::Host);
            self.update_game_ui();
        }
    }

    /// "Paste a code": the island can't take typing, so the code comes from the clipboard.
    fn game_join(&mut self) {
        if !self.cfg.games_online {
            return;
        }
        match platform::clipboard_get().as_deref().and_then(game::clean_code) {
            Some(code) => {
                if let Some(tx) = self.game_sender() {
                    self.game_net = 5;
                    self.game_net_text = format!("Joining {code}…");
                    let _ = tx.send(game::Cmd::Join(code));
                }
            }
            None => {
                self.game_net = 4;
                self.game_net_text = "Copy your friend's 4-letter code first, then press Paste a code.".into();
            }
        }
        self.update_game_ui();
    }

    fn game_leave(&mut self) {
        if let Some(tx) = &self.game_tx {
            let _ = tx.send(game::Cmd::Leave);
        }
        self.game_room = None;
        self.game_net = 0;
        self.game_net_text = "Play a friend anywhere: host a game and send them the code, or copy theirs and paste it here.".into();
        self.update_game_ui();
    }

    fn on_game(&mut self, ev: game::Event) {
        match ev {
            game::Event::Hosting(code) => {
                self.game_net = 1;
                self.game_net_text = "Send this code to a friend. Waiting for them…".into();
                self.game_code = code;
            }
            game::Event::State(r) => {
                if r.gone {
                    self.game_net = 4;
                    self.game_net_text = "Your friend left the game.".into();
                    self.game_room = None;
                } else {
                    self.game_net = if r.players < 2 { 1 } else { 2 };
                    if r.players < 2 {
                        self.game_net_text = "Send this code to a friend. Waiting for them…".into();
                    }
                    self.game_code = r.code.clone();
                    self.game_room = Some(r);
                }
            }
            game::Event::Limit => {
                self.game_net = 3;
                self.game_room = None;
                self.game_net_text = "Sorry! I'm a solo developer (with a little help from AI), and Islo is free, so online games run on a free server. Today's free limit ran out. It resets tomorrow, and Mochi's still up for a game.".into();
            }
            game::Event::Problem(msg) => {
                self.game_net = 4;
                self.game_net_text = msg;
            }
            game::Event::Closed => {
                if self.game_net != 3 && self.game_net != 4 {
                    self.game_net = 0;
                    self.game_net_text = "Play a friend anywhere: host a game and send them the code, or copy theirs and paste it here.".into();
                }
                self.game_room = None;
            }
        }
        if self.game_mode == 1 {
            self.update_game_ui();
        }
    }

    fn low_battery(&self, at: u8) -> bool {
        let b = self.battery;
        self.cfg.battery && b.present && !b.plugged && b.percent <= at
    }

    fn charging_live(&self) -> bool {
        let b = self.battery;
        self.cfg.battery && b.present && b.plugged && b.percent < 100
    }

    /// Live features as side-tab glyphs (left/right edges).
    fn glyphs(&self) -> Vec<i32> {
        let mut g = Vec::new();
        if self.call.is_some() {
            g.push(G_CALL);
        }
        if self.notes_on() && self.unread > 0 {
            g.push(G_NOTIFY);
        }
        if self.cfg.privacy && self.privacy.mic.is_some() {
            g.push(G_MIC);
        }
        if self.cfg.privacy && self.privacy.camera.is_some() {
            g.push(G_CAMERA);
        }
        if self.cfg.timer && self.timer.is_some() {
            g.push(G_TIMER);
        }
        if self.media_live() {
            g.push(G_MEDIA);
        }
        if self.charging_live() {
            g.push(G_CHARGING);
        }
        if self.low_battery(20) {
            g.push(G_LOW);
        }
        if self.cfg.shelf && !self.shelf.is_empty() {
            g.push(G_SHELF);
        }
        if self.cfg.focus && self.focus_on {
            g.push(G_FOCUS);
        }
        if self.cfg.pet && (self.visitor.is_some() || self.away.is_some()) {
            g.push(G_PET);
        }
        g
    }

    fn glyph_panel(g: i32) -> i32 {
        match g {
            G_MEDIA => P_MEDIA, // (swapped for lyrics in expand when available)
            G_SHELF => P_SHELF,
            G_TIMER => P_TIMER,
            G_PET => P_PET,
            G_NOTIFY | G_CALL => P_NOTIF,
            G_CHARGING | G_LOW => P_POWER,
            _ => P_MEDIA,
        }
    }

    fn tab_height(&self) -> f32 {
        let n = self.glyphs().len();
        if self.cfg.side_style == 2 {
            return 160.0;
        }
        if n == 0 {
            return 36.0;
        }
        let shown = n.min(3) as f32;
        12.0 + shown * GLYPH + if n > 3 { 28.0 } else { 0.0 }
    }

    /// Panels the header switcher offers.
    fn panels(&self, v: &View) -> Vec<i32> {
        let mut p = Vec::new();
        // synced lyrics lead; plain lyrics sit after the player
        if self.synced_on() {
            p.push(P_LYRICS);
        }
        if self.cfg.media {
            p.push(P_MEDIA);
        }
        if self.lyrics_on() && !self.lyrics_synced {
            p.push(P_LYRICS);
        }
        // the shelf is always a tab away (not only while dragging a file in)
        if self.cfg.shelf {
            p.push(P_SHELF);
        }
        if self.cfg.timer {
            p.push(P_TIMER);
        }
        if self.notes_on() {
            p.push(P_NOTIF);
        }
        if self.cfg.pet {
            p.push(P_PET);
        }
        if self.cfg.games {
            p.push(P_GAME);
        }
        if self.charging_live() || self.low_battery(20) || v.panel == P_POWER {
            p.push(P_POWER);
        }
        if p.is_empty() {
            p.push(P_MEDIA);
        }
        p
    }

    fn panel_height(&self, v: &View) -> f32 {
        let header = if self.panels(v).len() > 1 || self.cfg.header_date { HEADER } else { 0.0 };
        let content = match v.panel {
            P_SHELF if v.drag_over => 120.0,
            P_SHELF if self.shelf.is_empty() => 164.0,
            P_SHELF => 144.0,
            P_GAME => 204.0,
            P_POWER => 56.0,
            P_TIMER => 96.0,
            P_LYRICS => 228.0,
            P_NOTIF => self.visible_notes().len().clamp(1, 3) as f32 * 64.0 + 40.0,
            P_PET => 92.0 + if self.pet_list_open { 12.0 + self.nearby().len().clamp(1, 2) as f32 * 40.0 } else { 0.0 },
            _ if self.media_visible() => 130.0,
            _ => 80.0,
        };
        PAD + header + content + PAD
    }

    fn win_size(&self, v: &View) -> (f32, f32) {
        if self.vertical(v) { (SIDE_W, SIDE_H) } else { (WIN_W, WIN_H) }
    }

    /// Target (w, h, radius, anchor) for a view, in logical px.
    fn targets(&self, v: &View) -> (f32, f32, f32, f32) {
        let base = self.cfg.width;
        if v.dragging {
            // lifted off: a plain pill
            return (base, COMPACT_H, COMPACT_H / 2.0, 0.0);
        }
        let mode = self.mode(v);
        let (mut w, mut h, r) = match mode {
            2 => (EXPANDED_W, self.panel_height(v), ISLAND_R),
            1 => (HUD_W, self.compact_h(), self.compact_r()),
            _ if self.tab(v) => {
                let w = if self.cfg.side_style == 1 { SLIVER_W } else { TAB_W };
                (w, self.tab_height(), if self.cfg.side_style == 1 { 4.0 } else { NOTCH_R })
            }
            _ => {
                let kind = self.compact_kind(v);
                if kind == C_LYRICS {
                    // current line in white, the next ones in grey
                    let n = self.cfg.lyric_lines.clamp(2, 10) as f32;
                    let h = 16.0 + n * 20.0;
                    return (EXPANDED_W, h, (h / 2.0).min(ISLAND_R), v.tab_center);
                }
                let w = match kind {
                    // one lyric line: stays a pill, widened so lines fit
                    C_MEDIA if self.synced_on() => EXPANDED_W,
                    C_MEDIA | C_DROP | C_ERROR => (base + 60.0).clamp(260.0, COMPACT_MAX),
                    C_CHARGING | C_LOW | C_MIC | C_CAMERA | C_FOCUS | C_PET => base.max(220.0),
                    C_NEARBY | C_CALL | C_NOTE_NEW => (base + 60.0).clamp(260.0, COMPACT_MAX),
                    C_NOTIFY => base.max(220.0),
                    C_GLANCE => {
                        // featured glyph, note icons, clock/date text, battery ring
                        let text = self.clock_text().chars().count() as f32 * 6.6;
                        let featured = if self.featured() == 0 { 0.0 } else { 32.0 };
                        let notes = if self.notes_on() && self.unread > 0 { 56.0 } else { 0.0 };
                        let ring = if self.cfg.battery && self.battery.present { 30.0 } else { 0.0 };
                        base.max(24.0 + featured + notes + text + ring + 12.0).min(COMPACT_MAX)
                    }
                    _ => base,
                };
                // taskbar mode: live things sit in wings either side of an
                // empty notch, like the notch apps on a MacBook
                let w = if self.cfg.taskbar && !matches!(kind, C_IDLE | C_PET | C_GLANCE) { w.max(TB_WINGS_W) } else { w };
                (w, self.compact_h(), self.compact_r())
            }
        };
        if mode == 0 && !self.tab(v) {
            if self.flash_wide {
                w = COMPACT_MAX;
            }
            if v.pressed {
                // press: scale ~0.97 toward the edge
                w *= 0.97;
                h *= 0.97;
            }
        }
        // anchor along a side edge: the hovered glyph for panels, else the tab
        let anchor = match (mode, v.glyph_slot) {
            (2, Some(slot)) => self.tab_top(v) + 6.0 + slot as f32 * GLYPH + GLYPH / 2.0,
            _ => v.tab_center,
        };
        (w, h, r, anchor)
    }

    /// The music color: the album art's, or plain white when that's turned off.
    fn shown_accent(&self) -> slint::Color {
        if self.cfg.art_tint { self.accent } else { slint::Color::from_rgb_u8(0xff, 0xff, 0xff) }
    }

    /// Height of the compact pill: the taskbar's in taskbar mode.
    fn compact_h(&self) -> f32 {
        if self.cfg.taskbar && self.taskbar_h > 0.0 { self.taskbar_h } else { COMPACT_H }
    }

    /// Corner radius of the compact pill: in the taskbar a softer, rounder
    /// bottom (half the height, up to 18).
    fn compact_r(&self) -> f32 {
        if self.cfg.taskbar { (self.compact_h() / 2.0).min(18.0).max(NOTCH_R) } else { NOTCH_R }
    }

    fn tab_top(&self, v: &View) -> f32 {
        let h = self.tab_height();
        clamp_safe(v.tab_center - h / 2.0, SHOULDER, SIDE_H - h - SHOULDER)
    }

    /// State changed: point every view's springs at its new shape and update content.
    fn retarget(&mut self) {
        for i in 0..self.views.len() {
            self.retarget_view(i);
        }
        self.update_progress_timer();
        self.update_wave();
        self.update_peek();
        self.update_pointer_watch();
        self.update_pet_anim();
    }

    fn retarget_view(&mut self, i: usize) {
        let (w, h, r, a) = self.targets(&self.views[i]);
        let reduce = self.reduce_motion();
        let mode = self.mode(&self.views[i]);
        let edge = self.edge_of(&self.views[i]);
        let tab = self.tab(&self.views[i]);
        let kind = self.compact_kind(&self.views[i]);
        let panels = self.panels(&self.views[i]);
        let glyphs = self.glyphs();
        let media_visible = self.media_visible();
        let side_style = self.cfg.side_style;
        let featured = self.featured();
        let header_date = self.cfg.header_date;
        let focus_on = self.focus_on;
        let header_clock = self.header_clock(&self.views[i]);
        let panel_index = panels.iter().position(|p| *p == self.views[i].panel).unwrap_or(0) as i32;
        let (keep_open, soft, align, touch) = (self.cfg.keep_open, self.cfg.soft_shadow, self.cfg.lyric_align, self.cfg.pet_touch && self.cfg.pet);
        let (in_taskbar, compact_h) = (self.cfg.taskbar && !self.views[i].dragging, self.compact_h());
        let lyric_line = kind == C_MEDIA && self.synced_on();
        let v = &mut self.views[i];
        let growing = w > v.w.target + 0.5 || h > v.h.target + 0.5;
        for s in [&mut v.w, &mut v.h, &mut v.r, &mut v.a] {
            // Design: grow ~380 ms with a small overshoot (ease-spring),
            // collapse ~150 ms with none (ease-exit). With the leave grace that
            // keeps "pointer out -> compact" around 250 ms.
            if growing { s.configure(330.0, 0.62) } else { s.configure(1100.0, 1.0) }
        }
        if reduce || v.dragging {
            v.w.snap(w);
            v.h.snap(h);
            v.r.snap(r);
            v.a.snap(a);
        } else {
            v.w.target = w;
            v.h.target = h;
            v.r.target = r;
            v.a.target = a;
        }
        v.shape_dirty = true;
        let isl = &v.island;
        isl.set_edge(edge);
        isl.set_tab(tab);
        isl.set_side_style(side_style);
        isl.set_tab_glyphs(ModelRc::new(VecModel::from(glyphs.iter().copied().take(3).collect::<Vec<_>>())));
        isl.set_tab_extra(glyphs.len().saturating_sub(3) as i32);
        isl.set_mode(mode);
        isl.set_compact_kind(kind);
        isl.set_panel(v.panel);
        isl.set_panels(ModelRc::new(VecModel::from(panels)));
        isl.set_drag_hover(v.drag_over);
        isl.set_reduce_motion(reduce);
        isl.set_has_media(media_visible);
        isl.set_featured(featured);
        isl.set_header_date(header_date);
        isl.set_focus_on(focus_on);
        isl.set_header_clock(header_clock.into());
        isl.set_panel_index(panel_index);
        isl.set_hovering(v.hovering);
        isl.set_keep_open(keep_open);
        isl.set_soft_shadow(soft);
        isl.set_lyric_align(align);
        isl.set_pet_touch(touch);
        isl.set_in_taskbar(in_taskbar);
        isl.set_compact_h(compact_h);
        isl.set_lyric_line(lyric_line);
    }

    /// Called every loop iteration. Returns true while springs are moving.
    fn frame(&mut self) -> bool {
        let now = Instant::now();
        let mut any = false;
        for v in &mut self.views {
            let dt = v.last_frame.map(|t| (now - t).as_secs_f32()).unwrap_or(1.0 / 120.0);
            let mut moving = false;
            moving |= v.w.step(dt);
            moving |= v.h.step(dt);
            moving |= v.r.step(dt);
            moving |= v.a.step(dt);
            if moving || v.shape_dirty {
                v.shape_dirty = false;
                let h = v.h.value.max(4.0);
                v.island.set_pill_w(v.w.value.max(4.0));
                v.island.set_pill_h(h);
                v.island.set_radius(clamp_safe(v.r.value, 0.0, h / 2.0));
                v.island.set_anchor(v.a.value);
            }
            v.last_frame = if moving { Some(now) } else { None };
            any |= moving;
        }
        any
    }

    // ---------------- views & placement ----------------

    fn wanted_monitors(&self) -> Vec<HMONITOR> {
        match self.cfg.show_on {
            1 => system::monitors(),
            2 => vec![system::monitor_at_cursor()],
            _ => vec![system::monitor_by_index(self.cfg.monitor)],
        }
    }

    /// Make sure there is exactly one island per wanted display.
    fn sync_views(&mut self) {
        let wanted = self.wanted_monitors();
        if wanted.len() == 1 && self.views.len() == 1 {
            self.views[0].monitor = wanted[0];
            self.place_all();
            return;
        }
        let same = wanted.len() == self.views.len() && wanted.iter().zip(&self.views).all(|(m, v)| *m == v.monitor);
        if !same {
            for v in self.views.drain(..) {
                let _ = v.island.hide();
                shelf::revoke_drop_target(v.win.hwnd());
                platform::forget(v.island.window());
            }
            for (i, m) in wanted.into_iter().enumerate() {
                if let Ok(v) = self.create_view(i, m) {
                    self.views.push(v);
                }
            }
            for i in 0..self.views.len() {
                self.sync_island(i);
            }
        }
        self.place_all();
        for i in 0..self.views.len() {
            self.check_fullscreen_view(i);
        }
        self.apply_visibility();
        self.retarget();
    }

    fn create_view(&self, idx: usize, monitor: HMONITOR) -> Result<View, slint::PlatformError> {
        platform::set_next_kind(Kind::Island);
        let island = Island::new()?;
        let win = platform::native(island.window()).expect("island window");
        island.set_shelf_items(self.shelf_model.clone().into());
        let (compact, open) = crate::shadow::images();
        island.set_shadow_compact(compact);
        island.set_shadow_open(open);
        wire_island(&island, idx);
        shelf::register_drop_target(win.hwnd(), move |ev| {
            with_app(|a| a.on_drop(idx, ev));
        });
        let w0 = self.cfg.width;
        Ok(View {
            island,
            win,
            monitor,
            w: Spring::new(w0),
            h: Spring::new(COMPACT_H),
            r: Spring::new(NOTCH_R),
            a: Spring::new(SIDE_H / 2.0),
            last_frame: None,
            shape_dirty: true,
            hovering: false,
            pressed: false,
            expanded: false,
            drag_over: false,
            panel: P_MEDIA,
            tab_center: SIDE_H / 2.0,
            glyph_slot: None,
            press_at: None,
            dragging: false,
            suppress_click: false,
            hidden_fullscreen: false,
            shown: false,
            hover_timer: Timer::default(),
            leave_timer: Timer::default(),
            look: (0.0, 0.0),
            scroll_hold: false,
        })
    }

    /// Copy all shared state into one island (used for new islands).
    fn sync_island(&self, i: usize) {
        let isl = &self.views[i].island;
        isl.set_clock_text(self.clock_text().into());
        isl.set_header_clock(self.header_clock(&self.views[i]).into());
        isl.set_art(self.art.clone());
        isl.set_has_art(self.media.as_ref().is_some_and(|m| m.info.art.is_some()));
        isl.set_accent(self.shown_accent());
        if let Some(m) = &self.media {
            isl.set_track_title(m.info.title.clone().into());
            isl.set_subtitle(subtitle(&m.info).into());
            isl.set_playing(m.info.playing);
            isl.set_has_timeline(m.info.timeline.is_some());
        }
        isl.set_date_text(self.date_text.clone().into());
        apply_skin(isl, &self.cfg);
        let b = self.battery;
        isl.set_has_battery(self.cfg.battery && b.present);
        isl.set_battery_level(b.percent as i32);
        isl.set_charging(b.plugged);
        let (pt, ps) = system::battery_detail();
        isl.set_power_title(pt.into());
        isl.set_power_sub(ps.into());
        let (k, val, muted) = self.hud;
        isl.set_hud_kind(k);
        isl.set_hud_value(val);
        isl.set_hud_muted(muted);
        self.update_shelf_summary_on(isl);
        isl.set_lyrics(self.lyrics_model.clone().into());
        isl.set_lyric_state(if self.cfg.lyrics && self.media_visible() { self.lyric_state } else { L_HIDDEN });
        isl.set_mic_app(self.privacy.mic.clone().unwrap_or_default().into());
        isl.set_camera_app(self.privacy.camera.clone().unwrap_or_default().into());
    }

    /// Taskbar mode moves the taskbar apps to the left so the island has the
    /// middle; leaving it puts them back where they were.
    /// It follows the taskbar to the top or bottom of the screen.
    fn sync_taskbar(&mut self) {
        let before = (self.cfg.taskbar, self.cfg.taskbar_al, self.cfg.edge);
        if self.cfg.taskbar {
            match system::taskbar_edge() {
                Some(e) => self.cfg.edge = e,
                None => self.cfg.taskbar = false,
            }
        }
        if self.cfg.taskbar {
            let now = system::taskbar_alignment();
            if self.cfg.taskbar_al < 0 {
                self.cfg.taskbar_al = now.min(1) as i32;
            }
            if now != 0 {
                system::set_taskbar_alignment(0);
            }
        } else if self.cfg.taskbar_al >= 0 {
            system::set_taskbar_alignment(self.cfg.taskbar_al as u32);
            self.cfg.taskbar_al = -1;
        }
        self.taskbar_h = if self.cfg.taskbar {
            system::taskbar_thickness() as f32 / system::monitor_scale(system::primary_monitor())
        } else {
            0.0
        };
        if before != (self.cfg.taskbar, self.cfg.taskbar_al, self.cfg.edge) {
            self.cfg.save();
        }
    }

    fn raise_islands(&self) {
        for v in self.views.iter().filter(|v| v.shown) {
            system::raise_topmost(v.win.hwnd());
        }
    }

    fn place_all(&mut self) {
        for i in 0..self.views.len() {
            self.place(i);
        }
    }

    fn place(&mut self, i: usize) {
        let vertical = self.vertical(&self.views[i]);
        let (ww, wh) = self.win_size(&self.views[i]);
        let edge = self.cfg.edge;
        let edge_pos = self.cfg.edge_pos;
        let v = &mut self.views[i];
        if v.dragging {
            return; // follow_cursor places it
        }
        let m = v.monitor;
        let dpi = system::monitor_scale(m);
        v.win.set_scales(dpi, 1.0);
        let s = v.win.scale();
        let (pw, ph) = ((ww * s).ceil() as i32, (wh * s).ceil() as i32);
        let rc = system::monitor_rect(m);
        if vertical {
            let x = if edge == Edge::Left { rc.left } else { rc.right - pw };
            let center = rc.top as f32 + edge_pos * (rc.bottom - rc.top) as f32;
            let y = ((center - ph as f32 / 2.0) as i32).clamp(rc.top, (rc.bottom - ph).max(rc.top));
            v.tab_center = ((center - y as f32) / s).clamp(SHOULDER + 40.0, SIDE_H - SHOULDER - 40.0);
            v.win.place(x, y, pw as u32, ph as u32);
        } else {
            let span = (rc.right - rc.left - pw).max(0);
            let x = rc.left + (span as f32 * edge_pos) as i32;
            // In taskbar mode it sits inside the taskbar, flush with the screen
            // edge; otherwise next to the taskbar, never under it.
            let y = match (edge, self.cfg.taskbar) {
                (Edge::Bottom, true) => rc.bottom - ph,
                (Edge::Bottom, false) => system::monitor_work_rect(m).bottom - ph,
                (_, true) => rc.top,
                _ => system::monitor_work_rect(m).top,
            };
            v.win.place(x, y, pw as u32, ph as u32);
        }
    }

    fn apply_visibility(&mut self) {
        for v in &mut self.views {
            let want = !self.hidden_user && !v.hidden_fullscreen;
            if want != v.shown {
                v.shown = want;
                let _ = if want { v.island.show() } else { v.island.hide() };
            }
        }
        self.update_wave();
        // hidden (full screen, games): no peeks and no pointer hook
        self.update_peek();
        self.update_pointer_watch();
    }

    fn check_fullscreen(&mut self) {
        let mut changed = false;
        for i in 0..self.views.len() {
            changed |= self.check_fullscreen_view(i);
        }
        if changed {
            self.retarget();
            self.apply_visibility();
        }
    }

    fn check_fullscreen_view(&mut self, i: usize) -> bool {
        let mut ours: Vec<HWND> = self.views.iter().map(|v| v.win.hwnd()).collect();
        if let Some(s) = &self.settings {
            if let Some(w) = platform::native(s.window()) {
                ours.push(w.hwnd());
            }
        }
        let fs = self.cfg.hide_fullscreen && system::fullscreen_on(self.views[i].monitor, &ours);
        let v = &mut self.views[i];
        if fs != v.hidden_fullscreen {
            v.hidden_fullscreen = fs;
            if fs {
                v.expanded = false;
            } else if self.cfg.keep_open {
                // back from full screen: a pinned island opens again
                self.expand(i);
            }
            return true;
        }
        false
    }

    // ---------------- hover, press & drag ----------------

    /// Hit rectangle of the visible shape (window logical px). Uses the larger of
    /// the current and target size so a growing/shrinking pill doesn't flicker hover.
    fn pill_rect(&self, v: &View) -> (f32, f32, f32, f32) {
        let (ww, wh) = self.win_size(v);
        let mut w = v.w.value.max(v.w.target);
        let h = v.h.value.max(v.h.target);
        if self.tab(v) && self.cfg.side_style == 1 {
            w = w.max(TAB_W); // the sliver's hover target stays 36 px
        }
        let a = v.a.value;
        // (only meaningful on left/right edges; a tall top/bottom panel can leave
        // no room between the shoulders, so this must not panic)
        let side_y = clamp_safe(a - h / 2.0, SHOULDER, wh - h - SHOULDER);
        match self.edge_of(v) {
            1 => ((ww - w) / 2.0, wh - h, w, h),
            2 => (0.0, side_y, w, h),
            3 => (ww - w, side_y, w, h),
            4 => ((ww - w) / 2.0, (wh - h) / 2.0, w, h),
            _ => ((ww - w) / 2.0, 0.0, w, h),
        }
    }

    fn over_pill(&self, v: &View, x: f32, y: f32) -> bool {
        let (px, py, w, h) = self.pill_rect(v);
        x >= px - 2.0 && x <= px + w + 2.0 && y >= py - 2.0 && y <= py + h + 2.0
    }

    /// Ask Windows where the cursor really is (used to double-check before collapsing).
    fn cursor_over_pill(&self, i: usize) -> bool {
        let v = &self.views[i];
        if !v.shown {
            return false;
        }
        let pt = cursor();
        let (wx, wy) = v.win.pos();
        let s = v.win.scale();
        self.over_pill(v, (pt.x - wx) as f32 / s, (pt.y - wy) as f32 / s)
    }

    fn on_pointer(&mut self, i: usize, p: Option<slint::LogicalPosition>) {
        if i >= self.views.len() {
            return;
        }
        // drag: begins after a 6 px move with the button held on the compact pill
        if let Some(p0) = self.views[i].press_at {
            let down = unsafe { GetKeyState(VK_LBUTTON.0 as i32) } < 0;
            let c = cursor();
            let s = self.views[i].win.scale();
            if !self.views[i].dragging && down && ((c.x - p0.x).abs() + (c.y - p0.y).abs()) as f32 > 6.0 * s {
                let v = &mut self.views[i];
                v.dragging = true;
                v.pressed = false;
                v.expanded = false;
                v.hover_timer.stop();
                v.leave_timer.stop();
                self.retarget();
            }
        }
        if self.views[i].dragging {
            self.follow_cursor(i);
            return;
        }
        let inside = p.is_some_and(|p| self.over_pill(&self.views[i], p.x, p.y));
        // which side-tab glyph is under the pointer
        if inside && self.tab(&self.views[i]) {
            if let Some(p) = p {
                let n = self.glyphs().len().min(3).max(1);
                let slot = ((p.y - self.tab_top(&self.views[i]) - 6.0) / GLYPH).floor().clamp(0.0, n as f32 - 1.0) as usize;
                self.views[i].glyph_slot = Some(slot);
            }
        }
        if inside != self.views[i].hovering {
            self.on_hover(i, inside);
        }
    }

    fn follow_cursor(&mut self, i: usize) {
        let c = cursor();
        let m = unsafe { MonitorFromPoint(c, MONITOR_DEFAULTTONEAREST) };
        let v = &mut self.views[i];
        v.win.set_scales(system::monitor_scale(m), 1.0);
        let s = v.win.scale();
        let (pw, ph) = ((WIN_W * s).ceil() as i32, (WIN_H * s).ceil() as i32);
        v.win.place(c.x - pw / 2, c.y - ph / 2, pw as u32, ph as u32);
    }

    /// Release after dragging: snap to the nearest edge of that display and
    /// stay where it was dropped along that edge.
    fn drop_island(&mut self, i: usize) {
        let c = cursor();
        let m = unsafe { MonitorFromPoint(c, MONITOR_DEFAULTTONEAREST) };
        let rc = system::monitor_rect(m);
        let d = [
            (Edge::Top, c.y - rc.top),
            (Edge::Bottom, rc.bottom - c.y),
            (Edge::Left, c.x - rc.left),
            (Edge::Right, rc.right - c.x),
        ];
        let edge = d.iter().min_by_key(|e| e.1).map(|e| e.0).unwrap_or(Edge::Top);
        let s = system::monitor_scale(m);
        let pos = if edge.vertical() {
            (c.y - rc.top) as f32 / (rc.bottom - rc.top).max(1) as f32
        } else {
            let pw = WIN_W * s;
            let span = ((rc.right - rc.left) as f32 - pw).max(1.0);
            (c.x as f32 - pw / 2.0 - rc.left as f32) / span
        };
        self.cfg.edge = edge;
        self.cfg.edge_pos = pos.clamp(0.0, 1.0);
        if self.cfg.taskbar && system::taskbar_edge() != Some(edge) {
            self.cfg.taskbar = false; // dropped away from the taskbar: leave it
        }
        self.sync_taskbar();
        if self.cfg.show_on == 0 {
            self.cfg.monitor = system::monitor_index(m);
            self.views[i].monitor = m;
        }
        self.cfg.save();
        let v = &mut self.views[i];
        v.dragging = false;
        v.suppress_click = true;
        v.hovering = false;
        v.glyph_slot = None;
        // if the release never produces a click, don't eat the next real one
        self.click_timer.start(TimerMode::SingleShot, Duration::from_millis(400), || {
            with_app(|a| a.views.iter_mut().for_each(|v| v.suppress_click = false));
        });
        self.push_island_settings();
        self.place_all();
        self.retarget();
        // The island shrinks to the pill while it's carried; pinned, it opens
        // again where it lands.
        if self.cfg.keep_open {
            self.expand(i);
        }
    }

    fn on_press(&mut self, i: usize, pressed: bool) {
        if i >= self.views.len() {
            return;
        }
        if pressed {
            // Any press on the island's background can start a drag, open or not
            // (hover opens it almost immediately, so the closed pill is rarely
            // under the pointer). Buttons, tiles and the lyrics list keep their
            // own presses, so they don't reach this.
            self.views[i].press_at = Some(cursor());
            self.views[i].pressed = !self.tab(&self.views[i]);
        } else {
            self.views[i].press_at = None;
            self.views[i].pressed = false;
            if self.views[i].dragging {
                self.drop_island(i);
                return;
            }
        }
        self.retarget();
    }

    fn on_hover(&mut self, i: usize, h: bool) {
        let delay = self.cfg.hover_delay as u64;
        let hover_expand = self.cfg.hover_expand;
        let keep_open = self.cfg.keep_open;
        let carrying = self.carrying;
        let v = &mut self.views[i];
        v.hovering = h;
        if h {
            v.leave_timer.stop();
            // carrying Mochi across the pill doesn't open it
            if hover_expand && !v.expanded && !v.scroll_hold && !carrying {
                // Hover intent: open only after the pointer rests a moment.
                v.hover_timer.start(TimerMode::SingleShot, Duration::from_millis(delay), move || {
                    with_app(|a| {
                        if !a.carrying && a.views.get(i).is_some_and(|v| v.hovering && !v.dragging && !v.scroll_hold) {
                            a.expand(i);
                        }
                    });
                });
            }
        } else {
            v.hover_timer.stop();
            v.pressed = false;
            v.scroll_hold = false;
            // pinned: it stays open when the pointer leaves
            if v.expanded && !keep_open {
                // Short grace period so a pointer that slips past the edge for a
                // moment doesn't snap it shut; the collapse spring then takes ~150 ms.
                self.schedule_collapse(i, COLLAPSE_GRACE_MS);
            }
        }
        self.retarget();
    }

    fn schedule_collapse(&mut self, i: usize, ms: u64) {
        self.views[i].leave_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), move || {
            with_app(|a| {
                if i >= a.views.len() {
                    return;
                }
                if a.cursor_over_pill(i) {
                    a.views[i].hovering = true; // a missed event: we're still inside
                } else {
                    a.collapse(i);
                }
            });
        });
    }

    fn expand(&mut self, i: usize) {
        let media_visible = self.media_live() || self.shelf.is_empty();
        let synced = self.synced_on();
        let tab = self.tab(&self.views[i]);
        let glyphs = self.glyphs();
        let panels = self.panels(&self.views[i]);
        let v = &mut self.views[i];
        v.hover_timer.stop();
        if v.expanded {
            return;
        }
        if tab {
            // side tab: open the panel of the glyph under the pointer
            let slot = v.glyph_slot.unwrap_or(0);
            v.panel = glyphs.get(slot).map(|g| Self::glyph_panel(*g)).unwrap_or(P_MEDIA);
            if v.panel == P_MEDIA && synced {
                v.panel = P_LYRICS;
            }
        } else {
            v.glyph_slot = None;
            if !panels.contains(&v.panel) {
                v.panel = panels[0];
            }
            // Open where the action is: the whole song's lyrics when we have
            // them; files waiting and nothing playing -> shelf.
            if synced && (v.panel == P_MEDIA || v.panel == P_LYRICS) {
                v.panel = P_LYRICS;
            }
            if !media_visible && panels.contains(&P_SHELF) {
                v.panel = P_SHELF;
            }
        }
        if v.panel == P_NOTIF {
            self.unread = 0;
            self.call = None;
        }
        v.expanded = true;
        let pet_panel = v.panel == P_PET;
        self.hud_active = false;
        self.hud_timer.stop();
        self.pet_list_open = false;
        self.refresh_power();
        self.update_pet_ui();
        self.retarget();
        self.pet_jolt(i, true);
        if pet_panel {
            self.greet_mood();
        }
    }

    fn collapse(&mut self, i: usize) {
        let v = &mut self.views[i];
        if v.expanded && !v.drag_over {
            v.expanded = false;
            v.pressed = false;
            if self.esc_armed && !self.views.iter().any(|v| v.expanded) {
                self.esc_armed = false;
                system::set_esc_hotkey(platform::host_hwnd(), false);
            }
            self.retarget();
            self.pet_jolt(i, false);
        }
    }

    // ---------------- pet ----------------

    fn nearby(&self) -> Vec<pet::Peer> {
        let visitor = self.visitor.as_ref().map(|v| v.id);
        self.peers.iter().filter(|p| Some(p.id) != visitor).cloned().collect()
    }

    fn my_id(&self) -> [u8; 3] {
        self.beacon_id
    }

    /// Mochi is awake (and bobs gently) only while someone looks at it.
    fn update_pet_anim(&mut self) {
        let watching = self.views.iter().any(|v| v.shown && v.expanded && v.panel == P_PET);
        let tracking = self.pointer_on && self.pointer_awake;
        let awake = watching || tracking || self.peek.is_some() || self.views.iter().any(|v| v.hovering);
        self.each(|isl| isl.set_pet_awake(awake));
        if watching && !self.reduce_motion() {
            if !self.pet_timer.running() {
                self.pet_timer.start(TimerMode::Repeated, Duration::from_millis(700), || {
                    with_app(|a| {
                        a.pet_bob = !a.pet_bob;
                        let y = if a.pet_bob { -2.0 } else { 0.0 };
                        a.each(|isl| isl.set_pet_bob(y));
                    });
                });
            }
        } else if self.pet_timer.running() {
            self.pet_timer.stop();
            self.each(|isl| isl.set_pet_bob(0.0));
        }
    }

    fn update_pet_ui(&self) {
        let name = self.cfg.pet_name.clone();
        let status = format!("{} · {}", pet::mood(self.cfg.pet_fed), pet::fed_ago(self.cfg.pet_fed));
        let (visitor, vowner) = self.visitor.as_ref().map(|v| (v.pet.clone(), pet::shown_owner(&v.owner))).unwrap_or_default();
        let (away, away_to) = self.away.as_ref().map(|a| (true, a.1.clone())).unwrap_or((false, String::new()));
        let items: Vec<NearbyItem> =
            self.nearby().into_iter().map(|p| NearbyItem { owner: pet::shown_owner(&p.owner).into(), pet: p.pet.into() }).collect();
        let (open, ok) = (self.pet_list_open, self.ble_ok && self.cfg.pet_nearby);
        self.each(|isl| {
            isl.set_pet_name(name.clone().into());
            isl.set_pet_status(status.clone().into());
            isl.set_visitor(visitor.clone().into());
            isl.set_visitor_owner(vowner.clone().into());
            isl.set_pet_away(away);
            isl.set_pet_away_to(away_to.clone().into());
            isl.set_pet_list_open(open);
            isl.set_ble_ok(ok);
            isl.set_nearby(ModelRc::new(VecModel::from(items.clone())));
        });
    }

    /// Start or stop Bluetooth visits to match the settings.
    fn update_ble(&mut self) {
        let want = self.cfg.pet && self.cfg.pet_nearby;
        if want && self.ble_tx.is_none() {
            self.ble_ok = true;
            self.ble_tx = Some(pet::start(self.my_id(), |ev| post(move |a| a.on_ble(ev))));
            self.beacon();
        } else if !want {
            if let Some(tx) = self.ble_tx.take() {
                let _ = tx.send(pet::Cmd::Stop);
            }
            self.peers.clear();
            self.away = None;
            self.visitor = None;
        }
    }

    /// Tell nearby islands what we're up to.
    fn beacon(&self) {
        let Some(tx) = &self.ble_tx else { return };
        let (kind, target) = if let Some(id) = self.sending_home {
            (3, id)
        } else if let Some(a) = &self.away {
            (1, a.0)
        } else if let Some(v) = &self.visitor {
            (2, v.id)
        } else {
            (0, [0; 3])
        };
        // any device in range can read the beacon: the name only goes out when the user opted in
        let owner = if self.cfg.pet_share_name { self.owner.clone() } else { String::new() };
        let _ = tx.send(pet::Cmd::Beacon { pet: self.cfg.pet_name.clone(), owner, kind, target });
    }

    fn on_ble(&mut self, ev: pet::Event) {
        let me = self.my_id();
        match ev {
            pet::Event::Unavailable => self.ble_ok = false,
            pet::Event::Peers(peers) => {
                self.ble_ok = true;
                // someone new nearby: a quiet heads-up, once per session
                if let Some(p) = peers.iter().find(|p| !self.greeted.contains(&p.id)) {
                    self.greeted.push(p.id);
                    let t = format!("{} is nearby", pet::shown_owner(&p.owner));
                    self.each(|isl| isl.set_nearby_text(t.clone().into()));
                    if self.away.is_none() && self.visitor.is_none() {
                        self.show_transient(C_NEARBY, 4000);
                    }
                }
                // a friend's pet asks to visit us
                if self.visitor.is_none() && self.away.is_none() && self.sending_home.is_none() {
                    if let Some(p) = peers.iter().find(|p| p.kind == 1 && p.target == me) {
                        self.visitor = Some(p.clone());
                        self.set_mood(pet::face::LOVE, 8000);
                    }
                }
                // our visitor went home (its owner called it back, or walked away)
                if let Some(v) = &self.visitor {
                    if !peers.iter().any(|p| p.id == v.id && p.kind == 1 && p.target == me) {
                        self.visitor = None;
                    }
                }
                // our pet: arrived, sent home, or the host vanished
                if let Some((host, _, acked)) = self.away.clone() {
                    match peers.iter().find(|p| p.id == host) {
                        Some(p) if p.kind == 2 && p.target == me => {
                            if let Some(a) = &mut self.away {
                                a.2 = true;
                            }
                        }
                        Some(p) if p.kind == 3 && p.target == me => self.away = None,
                        None if acked => self.away = None,
                        _ => {}
                    }
                }
                self.peers = peers;
                self.beacon();
            }
        }
        self.update_pet_ui();
        self.retarget();
    }

    // ---------------- Mochi's moods ----------------

    /// Show a mood for `ms`. Its motion and marks play for the first few seconds
    /// (they redraw every frame), then the face stays still until it ends.
    fn set_mood(&mut self, face: i32, ms: u64) {
        self.mood = face;
        self.mood_lively = face != pet::face::NEUTRAL && !self.reduce_motion();
        self.push_face();
        if self.mood_lively {
            self.lively_timer.start(TimerMode::SingleShot, Duration::from_millis(ms.min(4000)), || {
                with_app(|a| {
                    a.mood_lively = false;
                    a.push_face();
                });
            });
        }
        self.mood_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), || {
            with_app(|a| {
                a.mood = pet::face::NEUTRAL;
                a.mood_lively = false;
                a.push_face();
            });
        });
    }

    /// A short scene over the mood (eating, a yank); `face` -1 ends it.
    fn set_act(&mut self, face: i32, lively: bool) {
        self.act = face;
        self.act_lively = lively && !self.reduce_motion();
        self.push_face();
    }

    fn push_face(&self) {
        let (face, lively) = if self.act >= 0 { (self.act, self.act_lively) } else { (self.mood, self.mood_lively) };
        self.each(|isl| {
            let g = isl.global::<PetSkin>();
            g.set_face(face);
            g.set_lively(lively);
        });
    }

    /// How Mochi feels when it comes out (or you open its panel), from what's going on.
    fn greet_mood(&mut self) {
        if self.mood != pet::face::NEUTRAL || self.act >= 0 {
            return;
        }
        let hungry = self.cfg.pet_fed != 0 && pet::now_unix().saturating_sub(self.cfg.pet_fed) > 24 * 3600;
        let face = if self.low_battery(10) {
            pet::face::SCARED
        } else if hungry {
            pet::face::SAD
        } else if self.playing() && self.lyric_state == L_MISSING && self.rand() % 2 == 0 {
            pet::face::CONFUSED
        } else if self.media.is_none() && self.rand() % 5 == 0 {
            pet::face::BORED
        } else {
            return;
        };
        self.set_mood(face, 6000);
    }

    /// Music started: Mochi gets excited (at most every 5 minutes) and comes out to show it.
    fn music_started(&mut self) {
        if self.excited_at.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) || self.act >= 0 {
            return;
        }
        self.excited_at = Some(Instant::now());
        self.set_mood(pet::face::EXCITED, 4000);
        if self.peek.is_none() && self.views.iter().any(|v| self.peek_ok(v)) {
            self.start_peek();
        }
    }

    /// Clicked the peeking Mochi: it hides, but the third poke within a minute makes it angry.
    fn poke_peek(&mut self) {
        let now = Instant::now();
        self.pokes.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.pokes.push(now);
        if self.pokes.len() >= 3 && self.act < 0 && !self.reduce_motion() {
            self.pokes.clear();
            self.set_mood(pet::face::ANGRY, 2600);
            // it stays out to make its point, then hides
            self.peek_step.start(TimerMode::SingleShot, Duration::from_millis(2400), || {
                with_app(|a| a.duck_peek());
            });
            return;
        }
        self.duck_peek();
    }

    /// The island sprang open (or shut) under the peeking Mochi: its edge yanks
    /// Mochi along. Wide eyes and a stretch, then (opening) a squeezed-eye wobble.
    fn pet_jolt(&mut self, i: usize, opening: bool) {
        if self.peek != Some(i) || self.reduce_motion() || self.eating {
            return;
        }
        if let Some(v) = self.views.get(i) {
            v.island.global::<PetSkin>().set_jolt(if opening { 0.3 } else { 0.15 });
        }
        Timer::single_shot(Duration::from_millis(140), move || {
            with_app(|a| {
                if let Some(v) = a.views.get(i) {
                    v.island.global::<PetSkin>().set_jolt(0.0); // eases back with a wobble
                }
            });
        });
        self.set_act(pet::face::SURPRISED, opening);
        self.act_timer.start(TimerMode::SingleShot, Duration::from_millis(if opening { 520 } else { 320 }), move || {
            with_app(|a| {
                if !opening {
                    a.set_act(-1, false);
                    return;
                }
                a.set_act(pet::face::SQUEEZED, false);
                a.act_timer.start(TimerMode::SingleShot, Duration::from_millis(420), || {
                    with_app(|a| a.set_act(-1, false));
                });
            });
        });
    }

    fn pet_feed(&mut self) {
        if self.eating {
            return;
        }
        let now = pet::now_unix();
        self.feeds.retain(|t| now.saturating_sub(*t) < 3600);
        let full = self.feeds.len() >= 3;
        if self.reduce_motion() {
            if full {
                self.set_mood(pet::face::DISGUSTED, 1500);
            } else {
                self.fed(now);
            }
            return;
        }
        self.eating = true;
        self.eat_step(0, full);
    }

    /// A treat went down: Mochi is happy for a minute.
    fn fed(&mut self, now: u64) {
        self.feeds.push(now);
        self.cfg.pet_fed = now;
        self.cfg.save();
        self.set_mood(pet::face::HAPPY, 60_000);
        self.update_pet_ui();
    }

    /// Feeding, one step per timer: the treat drops, Mochi opens wide, chomps
    /// three times, chews, then hops with a heart. When full, it turns away.
    fn eat_step(&mut self, step: u32, full: bool) {
        let mouth = |a: &Self, m: f32| a.each(|isl| isl.global::<PetSkin>().set_mouth(m));
        let (ms, next) = match step {
            0 => {
                self.each(|isl| {
                    isl.set_pet_treat(true);
                    let g = isl.global::<PetSkin>();
                    g.set_look_x(0.0);
                    g.set_look_y(-1.0); // watches it fall
                });
                self.set_act(pet::face::NEUTRAL, false);
                (380, 1)
            }
            1 if full => {
                self.each(|isl| isl.set_pet_treat(false));
                self.set_act(pet::face::DISGUSTED, true);
                (1400, 99)
            }
            1 => {
                mouth(self, 1.0);
                (170, 2)
            }
            2 => {
                self.each(|isl| {
                    isl.set_pet_treat(false);
                    isl.global::<PetSkin>().set_crumbs(true);
                });
                self.set_act(pet::face::SQUEEZED, true);
                mouth(self, 0.2);
                (190, 3)
            }
            3 | 5 => {
                mouth(self, 0.8);
                (190, step + 1)
            }
            4 | 6 => {
                mouth(self, 0.2);
                (190, step + 1)
            }
            7 => {
                mouth(self, 0.0);
                self.each(|isl| isl.global::<PetSkin>().set_crumbs(false));
                self.set_act(pet::face::SLEEPY, false); // chewing, eyes shut
                (450, 8)
            }
            8 => {
                self.set_act(-1, false);
                self.fed(pet::now_unix());
                self.each(|isl| isl.global::<PetSkin>().set_hearts(true));
                (1800, 100)
            }
            _ => {
                self.each(|isl| {
                    isl.set_pet_treat(false);
                    let g = isl.global::<PetSkin>();
                    g.set_mouth(0.0);
                    g.set_crumbs(false);
                    g.set_hearts(false);
                });
                if step == 99 {
                    self.set_act(-1, false);
                }
                self.eating = false;
                self.update_eyes();
                return;
            }
        };
        self.act_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), move || {
            with_app(|a| a.eat_step(next, full));
        });
    }

    fn pet_send(&mut self, k: usize) {
        let Some(host) = self.nearby().get(k).cloned() else { return };
        self.away = Some((host.id, host.pet.clone(), false));
        self.pet_list_open = false;
        self.beacon();
        // no answer within 20 s: the visit failed
        self.visit_timer.start(TimerMode::SingleShot, Duration::from_secs(20), || {
            with_app(|a| {
                if a.away.as_ref().is_some_and(|x| !x.2) {
                    a.away = None;
                    a.beacon();
                    a.each(|isl| {
                        isl.set_error_title("Couldn't connect".into());
                        isl.set_error_text("Retry".into());
                    });
                    a.update_pet_ui();
                    a.show_transient(C_ERROR, 3000);
                }
            });
        });
        // visits end on their own after 10 minutes
        self.home_timer.start(TimerMode::SingleShot, Duration::from_secs(600), || {
            with_app(|a| a.pet_call_home());
        });
        self.update_pet_ui();
        self.retarget();
    }

    fn pet_call_home(&mut self) {
        self.away = None;
        self.visit_timer.stop();
        self.home_timer.stop();
        self.beacon();
        self.update_pet_ui();
        self.retarget();
    }

    fn pet_send_home(&mut self) {
        let Some(v) = self.visitor.take() else { return };
        self.sending_home = Some(v.id);
        self.beacon();
        self.visit_timer.start(TimerMode::SingleShot, Duration::from_secs(20), || {
            with_app(|a| {
                a.sending_home = None;
                a.beacon();
            });
        });
        self.update_pet_ui();
        self.retarget();
    }

    // ---------------- notifications ----------------

    fn notes_on(&self) -> bool {
        self.cfg.notifications && self.notify_state == 4
    }

    fn visible_notes(&self) -> Vec<&notify::Note> {
        self.notes.iter().filter(|n| !self.dismissed.contains(&n.id)).collect()
    }

    fn update_notify(&mut self) {
        let want = self.cfg.notifications;
        if want && self.notify_tx.is_none() {
            self.notify_tx = Some(notify::start(|ev| post(move |a| a.on_notify(ev))));
        } else if !want {
            if let Some(tx) = self.notify_tx.take() {
                let _ = tx.send(notify::Cmd::Stop);
            }
            self.unread = 0;
            self.call = None;
        }
    }

    fn on_notify(&mut self, ev: notify::Event) {
        match ev {
            notify::Event::Unavailable => {
                self.notify_state = 1;
                self.notify_tx = None;
            }
            notify::Event::NeedsAccess => {
                self.notify_state = 2;
                self.notify_tx = None;
                if std::mem::take(&mut self.notify_ask) {
                    self.ask_notify_access();
                }
            }
            notify::Event::Denied => {
                self.notify_state = 3;
                self.notify_tx = None;
            }
            notify::Event::List(list) => {
                self.notify_state = 4;
                let first_load = self.known_notes.is_empty() && self.notes.is_empty();
                let fresh: Vec<notify::Note> = list.iter().filter(|n| !self.known_notes.contains(&n.id)).cloned().collect();
                self.known_notes = list.iter().map(|n| n.id).collect();
                self.notes = list;
                // don't announce what was already in Action Center when Islo started
                if !first_load && !fresh.is_empty() {
                    self.unread += fresh.len();
                    let newest = &fresh[0];
                    if newest.is_call() {
                        let name = if newest.title.to_lowercase().contains("incoming") { newest.body.clone() } else { newest.title.clone() };
                        self.each(|isl| isl.set_call_name(name.clone().into()));
                        self.call = Some(newest.clone());
                        // calls ring for ~30 s
                        self.call_timer.start(TimerMode::SingleShot, Duration::from_secs(30), || {
                            with_app(|a| {
                                a.call = None;
                                a.retarget();
                            });
                        });
                    }
                    // Other notifications only bump the bell badge; no popup.
                }
            }
        }
        self.update_notes_ui();
        self.push_settings();
        self.retarget();
    }

    fn update_notes_ui(&self) {
        let now = notify_now();
        let icon = |n: &notify::Note| {
            n.icon.as_ref().map(|px| {
                slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px, notify::ICON, notify::ICON))
            })
        };
        let notes = self.visible_notes();
        let items: Vec<NoteItem> = notes
            .iter()
            .map(|n| {
                let mins = (now - n.time).max(0) / 600_000_000;
                let time = match mins {
                    0 => "now".to_string(),
                    1..=59 => format!("{mins} min"),
                    60..=1439 => format!("{} h", mins / 60),
                    _ => format!("{} d", mins / 1440),
                };
                let img = icon(n);
                NoteItem {
                    app: n.app.clone().into(),
                    initial: n.app.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default().into(),
                    title: n.title.clone().into(),
                    body: n.body.clone().into(),
                    time: time.into(),
                    call: n.is_call(),
                    has_icon: img.is_some(),
                    icon: img.unwrap_or_default(),
                }
            })
            .collect();
        let count = self.unread as i32;
        // the closed pill: icons of the newest unread apps (two different apps at most)
        let mut seen: Vec<&str> = Vec::new();
        let mut stack: Vec<slint::Image> = Vec::new();
        for n in notes.iter().take(self.unread.max(0)) {
            if stack.len() == 2 {
                break;
            }
            if seen.contains(&n.app.as_str()) {
                continue;
            }
            seen.push(&n.app);
            if let Some(img) = icon(n) {
                stack.push(img);
            }
        }
        let first_app = notes.first().map(|n| n.app.clone()).unwrap_or_default();
        self.each(|isl| {
            isl.set_notes(ModelRc::new(VecModel::from(items.clone())));
            isl.set_note_count(count);
            isl.set_note_icons(ModelRc::new(VecModel::from(stack.clone())));
            if count > 0 && self.transient != Some(C_NOTE_NEW) {
                isl.set_note_app(first_app.clone().into());
            }
        });
    }

    fn note_open(&mut self, k: usize) {
        let Some(n) = self.visible_notes().get(k).map(|n| (*n).clone()) else { return };
        if !n.aumid.is_empty() {
            system::open_path(std::path::Path::new(&format!("shell:AppsFolder\\{}", n.aumid)));
        }
        if self.call.as_ref().is_some_and(|c| c.id == n.id) {
            self.call = None;
        }
        self.retarget();
    }

    fn notes_clear(&mut self) {
        let ids: Vec<u32> = self.notes.iter().map(|n| n.id).collect();
        self.dismissed.extend(ids);
        self.unread = 0;
        self.call = None;
        self.update_notes_ui();
        self.retarget();
    }

    // ---------------- keyboard ----------------

    /// Win + Alt + I: open (or close) the island on the first display.
    fn toggle_from_keyboard(&mut self) {
        if self.views.is_empty() {
            return;
        }
        if self.views[0].expanded {
            self.unpin();
            self.collapse(0);
        } else {
            self.views[0].glyph_slot = None;
            self.expand(0);
            self.esc_armed = true;
            system::set_esc_hotkey(platform::host_hwnd(), true);
        }
    }

    // ---------------- timer ----------------

    fn timer_start(&mut self, minutes: i32) {
        let total = Duration::from_secs(minutes.max(1) as u64 * 60);
        self.timer = Some(TimerRun { total, end: Instant::now() + total, paused_left: None });
        let label = if minutes >= 60 { format!("{} h", minutes / 60) } else { format!("{minutes} min") };
        self.each(|isl| isl.set_timer_label(label.clone().into()));
        self.timer_tick.start(TimerMode::Repeated, Duration::from_millis(1000), || {
            with_app(|a| a.timer_step());
        });
        self.update_timer_ui();
        self.retarget();
    }

    fn timer_pause(&mut self) {
        let Some(t) = &mut self.timer else { return };
        match t.paused_left.take() {
            Some(left) => t.end = Instant::now() + left,
            None => t.paused_left = Some(t.end.saturating_duration_since(Instant::now())),
        }
        self.update_timer_ui();
        self.retarget();
    }

    fn timer_cancel(&mut self) {
        self.timer = None;
        self.timer_tick.stop();
        self.update_timer_ui();
        self.retarget();
    }

    fn timer_step(&mut self) {
        let done = self.timer.as_ref().is_some_and(|t| t.paused_left.is_none() && t.left().is_zero());
        if done {
            self.timer = None;
            self.timer_tick.stop();
            system::chime();
            self.update_timer_ui();
            self.show_transient(C_TIMER_DONE, 5000);
        } else {
            self.update_timer_ui();
        }
    }

    fn update_timer_ui(&self) {
        let (text, left, running, paused) = match &self.timer {
            Some(t) => {
                let l = t.left();
                let secs = l.as_secs() + if l.subsec_millis() > 0 { 1 } else { 0 };
                let text = if secs >= 3600 {
                    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
                } else {
                    format!("{}:{:02}", secs / 60, secs % 60)
                };
                (text, l.as_secs_f32() / t.total.as_secs_f32().max(1.0), true, t.paused_left.is_some())
            }
            None => (String::new(), 1.0, false, false),
        };
        self.each(|isl| {
            isl.set_timer_text(text.clone().into());
            isl.set_timer_left(left);
            isl.set_timer_running(running);
            isl.set_timer_paused(paused);
        });
    }

    // ---------------- privacy & focus ----------------

    fn on_privacy(&mut self, p: live::Privacy) {
        let (mic, cam) = (p.mic.clone().unwrap_or_default(), p.camera.clone().unwrap_or_default());
        self.each(|isl| {
            isl.set_mic_app(mic.clone().into());
            isl.set_camera_app(cam.clone().into());
        });
        self.privacy = p;
        self.retarget();
    }

    fn on_focus(&mut self, on: bool) {
        self.focus_on = on;
        self.retarget();
    }

    fn on_click(&mut self, i: usize) {
        let Some(v) = self.views.get_mut(i) else { return };
        if v.suppress_click || v.dragging {
            v.suppress_click = false;
            return;
        }
        if !v.expanded {
            self.expand(i);
        }
    }

    // ---------------- HUD & transient states ----------------

    fn show_hud(&mut self, kind: i32, value: f32, muted: bool) {
        if !self.any_shown() {
            return;
        }
        self.hud = (kind, value, muted);
        // Replace the Windows flyout where Windows still lets us (see system.rs).
        if self.cfg.hide_flyout && !self.osd_hidden {
            self.osd_hidden = system::set_native_osd_hidden(true);
        }
        self.each(|isl| {
            isl.set_hud_kind(kind);
            isl.set_hud_value(value);
            isl.set_hud_muted(muted);
        });
        // open islands show a small level bar inside; closed ones become the HUD
        for v in &self.views {
            if v.expanded {
                v.island.set_vol_toast(true);
            }
        }
        let was = self.hud_active;
        self.hud_active = true;
        self.hud_timer.start(TimerMode::SingleShot, Duration::from_millis(1200), || {
            with_app(|a| {
                a.hud_active = false;
                a.each(|isl| isl.set_vol_toast(false));
                a.retarget();
            });
        });
        if !was {
            self.retarget();
        }
    }

    fn show_transient(&mut self, kind: i32, ms: u64) {
        self.transient = Some(kind);
        self.transient_timer.start(TimerMode::SingleShot, Duration::from_millis(ms), || {
            with_app(|a| {
                a.transient = None;
                a.retarget();
            });
        });
        self.retarget();
    }

    // ---------------- media ----------------

    fn on_media(&mut self, info: Option<media::Info>) {
        let was_playing = self.playing();
        let had_media = self.media.is_some();
        match info {
            None => {
                self.media = None;
                self.lyrics_key.clear();
                self.lyric_state = L_HIDDEN;
            }
            Some(info) => {
                if info.art_id != self.art_id {
                    self.art_id = info.art_id;
                    match &info.art {
                        Some(art) => {
                            let px = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&art.rgba, media::ART_SIZE, media::ART_SIZE);
                            self.art = slint::Image::from_rgba8(px);
                            let [r, g, b] = art.accent;
                            self.accent = slint::Color::from_rgb_u8(r, g, b);
                        }
                        None => {
                            self.art = slint::Image::default();
                            self.accent = slint::Color::from_rgb_u8(0xfc, 0x90, 0xbc); // island-rose
                        }
                    }
                    let (art, accent, has) = (self.art.clone(), self.shown_accent(), info.art.is_some());
                    self.each(|isl| {
                        isl.set_art(art.clone());
                        isl.set_has_art(has);
                        isl.set_accent(accent);
                    });
                }
                let (title, sub) = (info.title.clone(), subtitle(&info));
                let (playing, timeline) = (info.playing, info.timeline.is_some());
                self.each(|isl| {
                    isl.set_track_title(title.clone().into());
                    isl.set_subtitle(sub.clone().into());
                    isl.set_playing(playing);
                    isl.set_has_timeline(timeline);
                });
                let key = format!("{}\u{1}{}", info.artist, info.title);
                let req = lyrics::Request {
                    key: key.clone(),
                    artist: info.artist.clone(),
                    title: info.title.clone(),
                    album: info.album.clone(),
                    duration: info.timeline.map(|t| t.1).unwrap_or(0.0),
                };
                // our own clock for players without a timeline
                if key != self.lyrics_key {
                    self.clock = (0.0, playing.then(Instant::now));
                } else if playing != was_playing {
                    match self.clock.1.take() {
                        Some(since) if !playing => self.clock.0 += since.elapsed().as_secs_f64(),
                        _ if playing => self.clock.1 = Some(Instant::now()),
                        _ => {}
                    }
                }
                self.media = Some(MediaState { info, received: Instant::now() });
                self.update_progress();
                if key != self.lyrics_key {
                    self.lyrics_key = key;
                    self.lyrics.clear();
                    self.lyrics_synced = false;
                    self.lyrics_model.set_vec(Vec::new());
                    self.lyric_state = L_SEARCHING;
                    if self.cfg.lyrics {
                        if let Some(tx) = &self.lyrics_tx {
                            let _ = tx.send(req);
                        }
                    }
                }
            }
        }
        self.update_rest(had_media, was_playing);
        self.push_lyric_state();
        self.lyrics_sync();
        self.retarget();
        if self.playing() && !was_playing {
            self.music_started();
        }
    }

    // ---------------- lyrics ----------------

    fn on_lyrics(&mut self, key: String, found: lyrics::Found) {
        if key != self.lyrics_key {
            return; // the song changed while we were fetching
        }
        self.lyric_state = match () {
            _ if found.lines.is_empty() && found.offline => L_OFFLINE,
            _ if found.lines.is_empty() && found.instrumental => L_INSTRUMENTAL,
            _ if found.lines.is_empty() => L_MISSING,
            _ if found.synced => L_SYNCED,
            _ => L_PLAIN,
        };
        self.lyrics_model.set_vec(found.lines.iter().map(|l| slint::SharedString::from(l.1.as_str())).collect::<Vec<_>>());
        self.lyrics = found.lines;
        self.lyrics_synced = found.synced;
        self.lyric_idx = usize::MAX;
        self.push_lyric_state();
        self.lyrics_sync();
        self.retarget();
        // Mochi looks puzzled when there are no lyrics (if it's out to see it)
        if self.lyric_state == L_MISSING && self.mochi_visible() && self.mood == pet::face::NEUTRAL {
            self.set_mood(pet::face::CONFUSED, 3500);
        }
    }

    /// Tell the islands whether this song has lyrics (shown as a chip and a glyph).
    fn push_lyric_state(&self) {
        let state = if self.cfg.lyrics && self.media_visible() { self.lyric_state } else { L_HIDDEN };
        self.each(|isl| isl.set_lyric_state(state));
    }

    /// Where the song is right now (seconds): from the player's timeline, or
    /// our own clock for players that don't report one (or report nonsense).
    fn position_now(&self) -> Option<f64> {
        let m = self.media.as_ref()?;
        if let Some((pos, dur, age)) = m.info.timeline {
            if age < dur + 30.0 {
                let mut t = pos;
                if m.info.playing {
                    t += age + m.received.elapsed().as_secs_f64();
                }
                return Some(t.clamp(0.0, dur));
            }
        }
        Some(self.clock.0 + self.clock.1.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0))
    }

    /// Show the line for "now", then sleep exactly until the next line starts.
    /// (No polling: one single-shot timer per lyric line.)
    fn lyrics_sync(&mut self) {
        let title = self.media.as_ref().map(|m| m.info.title.clone()).unwrap_or_default();
        if !self.synced_on() {
            self.lyric_timer.stop();
            if self.lyrics_on() && self.lyric_idx != usize::MAX - 1 {
                // plain lyrics: the whole text, nothing highlighted, from the top
                self.lyric_idx = usize::MAX - 1;
                self.each(|isl| {
                    isl.set_lyric_index(-1);
                    isl.set_lyric_scroll(0.0);
                });
            }
            self.each(|isl| isl.set_compact_title(title.clone().into()));
            return;
        }
        // small lead so the line appears as it's sung, not after
        let t = self.position_now().unwrap_or(0.0) + 0.15;
        let idx = self.lyrics.iter().rposition(|l| l.0 <= t).unwrap_or(0);
        if idx != self.lyric_idx {
            self.lyric_idx = idx;
            let n = self.cfg.lyric_lines.clamp(1, 10) as usize;
            let window: Vec<slint::SharedString> = self.lyrics.iter().skip(idx).take(n).map(|l| l.1.as_str().into()).collect();
            let current: slint::SharedString = self.lyrics[idx].1.as_str().into();
            // keep the current line on the 3rd row of the full view
            let scroll = (idx as f32 - 2.0).max(0.0) * 22.0;
            self.each(|isl| {
                isl.set_lyric_index(idx as i32);
                isl.set_lyric_window(ModelRc::new(VecModel::from(window.clone())));
                isl.set_lyric_scroll(scroll);
                isl.set_compact_title(current.clone());
            });
        }
        let playing = self.playing();
        match self.lyrics.get(idx + 1) {
            Some(next) if playing && (next.0 - t).is_finite() => {
                let wait = (next.0 - t).clamp(0.05, 600.0);
                self.lyric_timer.start(TimerMode::SingleShot, Duration::from_secs_f64(wait), || {
                    with_app(|a| a.lyrics_sync());
                });
            }
            _ => self.lyric_timer.stop(),
        }
    }

    fn update_progress(&self) {
        let Some(m) = &self.media else { return };
        let Some((pos, dur, age)) = m.info.timeline else { return };
        let mut t = pos;
        if m.info.playing {
            t += age + m.received.elapsed().as_secs_f64();
        }
        let t = t.clamp(0.0, dur);
        let fmt = |s: f64| {
            let s = s as u64;
            if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
        };
        let (p, e, d) = ((t / dur) as f32, fmt(t), fmt(dur));
        self.each(|isl| {
            isl.set_progress(p);
            isl.set_elapsed(e.clone().into());
            isl.set_duration(d.clone().into());
        });
    }

    /// The progress bar ticks once per second, and only while you can see it.
    fn update_progress_timer(&mut self) {
        let need = self.views.iter().any(|v| self.mode(v) == 2 && v.panel == P_MEDIA)
            && self.media.as_ref().is_some_and(|m| m.info.playing && m.info.timeline.is_some());
        if need && !self.progress_timer.running() {
            self.update_progress();
            self.progress_timer.start(TimerMode::Repeated, Duration::from_secs(1), || {
                with_app(|a| a.update_progress());
            });
        } else if !need {
            self.progress_timer.stop();
        }
    }

    /// The waveform is Islo's only loop: it runs while audio plays and a wave
    /// is on screen, and stops when hidden, in full screen, with reduced
    /// motion, or on battery saver (if that setting is on).
    fn update_wave(&mut self) {
        let visible = self.views.iter().any(|v| {
            v.shown
                && ((self.mode(v) == 0 && !self.tab(v) && self.compact_kind(v) == C_MEDIA)
                    || (self.mode(v) == 2 && (v.panel == P_MEDIA || v.panel == P_LYRICS)))
        });
        let saver = self.cfg.saver_pause && self.battery.saver;
        let want = self.playing() && visible && self.media_visible() && !self.reduce_motion() && !saver;
        if want {
            if !self.wave_timer.running() {
                self.wave_step();
                self.wave_timer.start(TimerMode::Repeated, Duration::from_millis(150), || {
                    with_app(|a| a.wave_step());
                });
            }
        } else {
            self.wave_timer.stop();
            let lv = if self.playing() { vec![0.5, 0.8, 0.6, 0.9, 0.55] } else { vec![0.25; 5] };
            self.each(|isl| isl.set_wave(ModelRc::new(VecModel::from(lv.clone()))));
        }
    }

    fn wave_step(&mut self) {
        // Each bar follows a 1.2 s cycle with its own phase (like the design's
        // CSS animation delays), sampled every 150 ms and tweened by Slint.
        let t = self.wave_start.elapsed().as_secs_f32();
        let phase = [0.0, -0.3, -0.6, -0.9, -0.45];
        let lv: Vec<f32> = phase
            .iter()
            .map(|p| {
                let x = ((t + p) / 1.2) * std::f32::consts::TAU;
                0.3 + 0.7 * (0.5 - 0.5 * x.cos())
            })
            .collect();
        self.each(|isl| isl.set_wave(ModelRc::new(VecModel::from(lv.clone()))));
    }

    fn media_cmd(&self, cmd: media::Cmd) {
        if let Some(tx) = &self.media_tx {
            let _ = tx.send(cmd);
        }
    }

    // ---------------- battery ----------------

    fn refresh_power(&self) {
        let (t, s) = system::battery_detail();
        self.each(|isl| {
            isl.set_power_title(t.clone().into());
            isl.set_power_sub(s.clone().into());
        });
    }

    fn on_power(&mut self, initial: bool) {
        let b = system::battery();
        let old = self.battery;
        self.battery = b;
        let show = self.cfg.battery && b.present;
        self.each(|isl| {
            isl.set_has_battery(show);
            isl.set_battery_level(b.percent as i32);
            isl.set_charging(b.plugged);
        });
        if self.views.iter().any(|v| v.expanded && v.panel == P_POWER) {
            self.refresh_power();
        }
        if b.saver != old.saver {
            self.update_wave();
        }
        if !show || initial {
            self.retarget();
            return;
        }
        if b.plugged && !old.plugged {
            // Plug-in: widen the pill once, then settle into "Charging".
            self.flash_wide = !self.reduce_motion();
            self.flash_timer.start(TimerMode::SingleShot, Duration::from_millis(260), || {
                with_app(|a| {
                    a.flash_wide = false;
                    a.retarget();
                });
            });
            self.show_transient(C_CHARGING, 3000);
        } else {
            let crossed = |lim: u8| !b.plugged && old.percent > lim && b.percent <= lim;
            if crossed(20) || crossed(10) || crossed(5) {
                self.show_transient(C_LOW, 4000);
            } else {
                self.retarget();
            }
        }
    }

    // ---------------- shelf ----------------

    fn rebuild_shelf(&self) {
        let items: Vec<ShelfItem> = self
            .shelf
            .iter()
            .map(|e| {
                let (icon, has_icon) = match &e.icon {
                    Some((size, px)) => (
                        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px, *size, *size)),
                        true,
                    ),
                    None => (slint::Image::default(), false),
                };
                let size = if e.bytes > 0 { shelf::human_size(e.bytes) } else { "Folder".into() };
                ShelfItem { name: e.name.clone().into(), size: size.into(), icon, has_icon }
            })
            .collect();
        // one model shared by every island
        self.shelf_model.set_vec(items);
        for v in &self.views {
            self.update_shelf_summary_on(&v.island);
        }
    }

    fn update_shelf_summary_on(&self, isl: &Island) {
        let total: u64 = self.shelf.iter().map(|e| e.bytes).sum();
        let n = self.shelf.len();
        let files = if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        let summary = if total > 0 { format!("{files} · {}", shelf::human_size(total)) } else { files };
        isl.set_shelf_summary(summary.into());
    }

    fn on_drop(&mut self, i: usize, ev: DropEvent) {
        if i >= self.views.len() {
            return;
        }
        let delay = self.cfg.hover_delay as u64;
        match ev {
            DropEvent::Enter => {
                let v = &mut self.views[i];
                v.drag_over = true;
                v.leave_timer.stop();
                // The pill widens and names the target; resting on it opens the drop zone.
                v.hover_timer.start(TimerMode::SingleShot, Duration::from_millis(delay), move || {
                    with_app(|a| {
                        if a.views.get(i).is_some_and(|v| v.drag_over) {
                            let v = &mut a.views[i];
                            v.panel = P_SHELF;
                            v.glyph_slot = None;
                            v.expanded = true;
                            a.hud_active = false;
                            a.retarget();
                        }
                    });
                });
            }
            DropEvent::Leave => {
                let v = &mut self.views[i];
                v.drag_over = false;
                v.hover_timer.stop();
                if v.expanded && !v.hovering && !self.cfg.keep_open {
                    v.expanded = false;
                }
            }
            DropEvent::Drop(files) => {
                let pinned = self.cfg.keep_open;
                {
                    let v = &mut self.views[i];
                    v.drag_over = false;
                    v.hover_timer.stop();
                    if pinned {
                        // stays open on the shelf, showing what you dropped
                        v.panel = P_SHELF;
                    } else {
                        v.expanded = false;
                    }
                }
                let mut added = 0;
                for f in files {
                    if !f.exists() {
                        continue;
                    }
                    if let Some(k) = self.shelf.iter().position(|e| e.path == f) {
                        self.shelf.remove(k);
                    }
                    self.shelf.insert(0, shelf::make_entry(f)); // newest first
                    added += 1;
                }
                self.shelf.truncate(12);
                self.rebuild_shelf();
                if added == 0 {
                    self.each(|isl| {
                        isl.set_error_title("Couldn't keep it".into());
                        isl.set_error_text("Retry".into());
                    });
                    self.show_transient(C_ERROR, 2500);
                } else {
                    let t = if added == 1 { "1 file".to_string() } else { format!("{added} files") };
                    self.each(|isl| isl.set_kept_text(t.clone().into()));
                    self.show_transient(C_KEPT, 1600);
                }
                return;
            }
        }
        self.retarget();
    }

    // ---------------- settings ----------------

    fn settings_dark(&self) -> bool {
        // the window previews its own theme before Save, so go by what it shows
        let theme = self.settings.as_ref().map_or(self.cfg.theme, |s| s.get_theme());
        match theme {
            1 => false,
            2 => true,
            _ => system::apps_dark_mode(),
        }
    }

    fn open_settings(&mut self) {
        if self.settings.is_none() {
            platform::set_next_kind(Kind::Normal);
            let Ok(s) = SettingsWindow::new() else { return };
            s.on_changed(|key| {
                with_app(|a| a.settings_edit(key.as_str()));
            });
            s.on_save_changes(|| {
                with_app(|a| a.save_settings());
            });
            s.on_discard_changes(|| {
                with_app(|a| a.discard_settings());
            });
            s.on_reset_position(|| {
                with_app(|a| {
                    a.cfg.edge = Edge::Top;
                    a.cfg.edge_pos = 0.5;
                    a.cfg.save();
                    a.push_island_settings();
                    a.flash_saved("edge");
                    a.apply_config();
                });
            });
            s.on_restore_defaults(|| {
                with_app(|a| {
                    // into the window only: Save applies them, Discard keeps yours
                    let Some(s) = &a.settings else { return };
                    let defaults = Config { pet_fed: a.cfg.pet_fed, ..Config::default() };
                    fill_settings(s, &defaults);
                    a.settings_edit("defaults");
                });
            });
            s.on_quit_app(quit);
            s.on_check_update(|| {
                with_app(|a| a.check_update(true));
            });
            s.on_install_update(|| {
                with_app(|a| a.install_update());
            });
            s.on_open_releases(|| system::open_path(std::path::Path::new(&update::releases_page())));
            s.on_set_up_notifications(|| {
                with_app(|a| a.set_up_notifications());
            });
            s.on_remove_notifications(|| {
                with_app(|a| a.remove_notifications());
            });
            s.on_set_pet_color(|which, c| {
                with_app(|a| a.set_pet_color(which, rgb_u32(c)));
            });
            s.on_pick_pet_color(|which| {
                with_app(|a| a.pick_pet_color(which));
            });
            s.window().on_close_requested(|| {
                let unsaved = with_app(|a| {
                    let s = a.settings.as_ref()?;
                    s.get_dirty().then(|| s.set_close_warn(true))
                })
                .flatten()
                .is_some();
                if unsaved {
                    // the save bar asks: Save or Discard, and either one closes
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
                with_app(|a| a.close_settings());
                slint::CloseRequestResponse::HideWindow
            });
            self.settings = Some(s);
            self.push_settings();
        }
        let Some(s) = &self.settings else { return };
        update_memory_text(s);
        let _ = s.show();
        if let Some(w) = platform::native(s.window()) {
            system::set_dark_title_bar(w.hwnd(), self.settings_dark());
        }
        self.mem_timer.start(TimerMode::Repeated, Duration::from_secs(2), || {
            with_app(|a| {
                if let Some(s) = &a.settings {
                    update_memory_text(s);
                }
            });
        });
    }

    /// Free the settings UI once closed (it's rebuilt on next open).
    fn close_settings(&mut self) {
        self.mem_timer.stop();
        Timer::single_shot(Duration::from_millis(0), || {
            with_app(|a| {
                if let Some(s) = a.settings.take() {
                    let _ = s.hide();
                    platform::forget(s.window());
                }
            });
        });
    }

    /// Copy the config into the settings window. With unsaved changes there,
    /// only the status rows are updated, so a notification never wipes what
    /// you picked.
    fn push_settings(&self) {
        let Some(s) = &self.settings else { return };
        let c = &self.cfg;
        if !s.get_dirty() {
            fill_settings(s, c);
        }
        s.set_dark(self.settings_dark());
        s.set_reduce_ui(self.reduce_motion());
        s.set_system_reduce_motion(!self.sys_anim);
        s.set_online_built(!game::RELAY.is_empty());
        s.set_owner_name(self.owner.clone().into());
        s.set_version(env!("CARGO_PKG_VERSION").into());
        s.set_has_live(true);
        s.set_has_lyrics(true);
        s.set_has_pet(true);
        // 0 toggle, 1 "Set up" button, 2 setting up
        let setup = if notify::has_identity() { 0 } else if self.notify_setting_up { 2 } else { 1 };
        let desc = match (setup, self.notify_state) {
            (2, _) => "Setting up. Islo restarts by itself when it's done.".to_string(),
            _ if !self.notify_setup_error.is_empty() => self.notify_setup_error.clone(),
            (1, _) if package::developer_mode() => {
                "Windows only shares notifications with registered apps. Set up registers Islo and restarts it.".to_string()
            }
            (1, _) => "Windows only shares notifications with registered apps. Set up turns on Developer Mode (Windows asks once) and registers Islo.".to_string(),
            (_, 2) => "Turn this on, then allow Islo in the Windows prompt.".to_string(),
            (_, 3) => "Windows blocked Islo. Allow it under Settings > Privacy & security > Notifications.".to_string(),
            _ => "Show new notifications inside the island. Calls from Teams, WhatsApp or Phone Link ring in green.".to_string(),
        };
        s.set_notifications_setup(setup);
        s.set_notifications_desc(desc.into());
        self.push_update_ui();
    }

    /// The island itself changed the pin or the edge (header pin, Esc, a drag,
    /// Reset position): show that in Settings even with unsaved changes there.
    fn push_island_settings(&self) {
        self.push_settings();
        if let Some(s) = &self.settings {
            s.set_keep_open(self.cfg.keep_open);
            s.set_edge(self.cfg.edge.index());
            s.set_taskbar(self.cfg.taskbar);
        }
    }

    /// A control in Settings changed: it waits in the window until Save.
    fn settings_edit(&mut self, key: &str) {
        let Some(s) = &self.settings else { return };
        s.set_dirty(true);
        s.set_save_state(0);
        if key == "skin" && s.get_pet_skin() >= 0 {
            // the color rows follow the preset
            let (_, body, eyes) = pet::SKINS[(s.get_pet_skin() as usize).min(pet::SKINS.len() - 1)];
            push_pet_colors(s, body, eyes);
        }
        if key == "theme" || key == "defaults" {
            // this window previews its theme right away
            let dark = self.settings_dark();
            s.set_dark(dark);
            if let Some(w) = platform::native(s.window()) {
                system::set_dark_title_bar(w.hwnd(), dark);
            }
        }
    }

    /// Save: take everything from the window, write islo.ini, check it, apply.
    fn save_settings(&mut self) {
        let Some(s) = &self.settings else { return };
        let old = self.cfg.clone();
        let mut c = old.clone();
        read_settings(s, &mut c);
        c.hide_flyout = c.hud;
        if s.get_autostart() != system::autostart_enabled() {
            system::set_autostart(s.get_autostart());
        }
        let closing = s.get_close_warn();
        self.cfg = c;
        let result = self.cfg.try_save();
        let Some(s) = &self.settings else { return };
        match &result {
            Ok(path) => {
                s.set_dirty(false);
                s.set_close_warn(false);
                s.set_save_state(1);
                s.set_save_msg(format!("Saved to {}", path.display()).into());
            }
            Err(e) => {
                // still applied for now; the bar says why it isn't on disk
                s.set_save_state(2);
                s.set_save_msg(e.as_str().into());
            }
        }
        self.apply_changes(&old);
        if closing && result.is_ok() {
            self.close_settings();
        }
    }

    /// Discard: put the saved settings back into the window.
    fn discard_settings(&mut self) {
        let Some(s) = &self.settings else { return };
        let closing = s.get_close_warn();
        s.set_dirty(false);
        s.set_close_warn(false);
        s.set_save_state(0);
        s.set_save_msg("".into());
        self.push_settings();
        if let Some(w) = self.settings.as_ref().and_then(|s| platform::native(s.window())) {
            system::set_dark_title_bar(w.hwnd(), self.settings_dark());
        }
        if closing {
            self.close_settings();
        }
    }

    /// After a save: do what each changed setting needs, then refresh everything.
    fn apply_changes(&mut self, old: &Config) {
        let c = self.cfg.clone();
        if c.theme != old.theme || c.reduce_motion != old.reduce_motion {
            if let Some(s) = &self.settings {
                let dark = self.settings_dark();
                s.set_dark(dark);
                s.set_reduce_ui(self.reduce_motion());
                if let Some(w) = platform::native(s.window()) {
                    system::set_dark_title_bar(w.hwnd(), dark);
                }
            }
        }
        if c.lyrics && !old.lyrics && self.lyrics.is_empty() {
            // refetch for the current song
            self.lyrics_key.clear();
            if let Some(m) = &self.media {
                let info = m.info.clone();
                self.on_media(Some(info));
            }
        }
        if c.lyric_lines != old.lyric_lines {
            self.lyric_idx = usize::MAX;
        }
        if c.update_check != old.update_check {
            self.schedule_update_checks();
        }
        if c.art_tint != old.art_tint {
            let accent = self.shown_accent();
            self.each(|isl| isl.set_accent(accent));
        }
        let looks = (c.pet_skin, c.pet_body, c.pet_eyes, c.pet_size) != (old.pet_skin, old.pet_body, old.pet_eyes, old.pet_size);
        if looks {
            for v in &self.views {
                apply_skin(&v.island, &self.cfg);
            }
        }
        if looks || (c.pet_peek, c.pet_moves, c.pet_often) != (old.pet_peek, old.pet_moves, old.pet_often) {
            // show the change: Mochi comes out in a moment (if the island is quiet)
            self.peek_timer.start(TimerMode::SingleShot, Duration::from_millis(1200), || {
                with_app(|a| a.start_peek());
            });
        }
        if c.notifications && !old.notifications {
            if self.notify_state == 2 {
                self.ask_notify_access();
            } else {
                // not checked yet: ask once the listener reports it needs access
                self.notify_ask = true;
            }
        }
        if (c.clock, c.pill_glance, c.header_date) != (old.clock, old.pill_glance, old.header_date) {
            self.refresh_date(); // whether a minute tick is needed
            // refresh_date only pushes when the time or date itself changed; the
            // format did, so the pill and the header need the new text now
            self.push_clock();
        }
        if c.keep_open != old.keep_open {
            if c.keep_open {
                if self.views.first().is_some_and(|v| !v.expanded) {
                    self.expand(0);
                }
            } else {
                for k in 0..self.views.len() {
                    if !self.cursor_over_pill(k) {
                        self.collapse(k);
                    }
                }
            }
        }
        if (!c.games || !c.games_online) && self.game_mode == 1 {
            self.game_leave();
            self.game_mode_set(0);
        }
        if !c.games {
            for v in &mut self.views {
                if v.panel == P_GAME {
                    v.panel = P_MEDIA;
                }
            }
        }
        self.push_lyric_state();
        self.lyrics_sync();
        self.apply_config();
        self.update_game_ui();
        self.push_settings();
        self.retarget();
    }

    /// Show Windows' consent prompt, then start listening.
    fn ask_notify_access(&mut self) {
        notify::request_access(|ok| {
            post(move |a| {
                if ok {
                    a.notify_state = 0;
                    a.notify_tx = None;
                    a.update_notify();
                } else {
                    a.notify_state = 3;
                    a.push_settings();
                }
            })
        });
    }

    /// Settings > Notifications > Set up: register Islo with Windows, then
    /// restart with notifications on (only a new process gets the identity).
    fn set_up_notifications(&mut self) {
        if self.notify_setting_up {
            return;
        }
        self.notify_setting_up = true;
        self.notify_setup_error.clear();
        self.push_settings();
        package::set_up(|result| {
            post(move |a| {
                a.notify_setting_up = false;
                match result {
                    Ok(()) => {
                        a.cfg.notifications = true;
                        a.cfg.save();
                        if package::relaunch() {
                            quit();
                            return;
                        }
                        a.notify_setup_error = "Islo is registered. Quit Islo and start it again to see notifications.".into();
                    }
                    Err(msg) => a.notify_setup_error = msg,
                }
                a.push_settings();
            })
        });
    }

    /// Settings > Pet > Body color (0) or Eye color (1): into the window, like
    /// every other setting (Save applies it).
    fn set_pet_color(&mut self, which: i32, c: u32) {
        let Some(s) = &self.settings else { return };
        let (mut body, mut eyes) = (rgb_u32(s.get_pet_body()), rgb_u32(s.get_pet_eyes()));
        if which == 0 {
            body = c;
        } else {
            eyes = c;
        }
        push_pet_colors(s, body, eyes);
        s.set_pet_skin(pet::preset_of(body, eyes));
        self.settings_edit(if which == 0 { "petbody" } else { "peteyes" });
    }

    /// "Custom…": Windows' color picker, starting from the current color.
    fn pick_pet_color(&self, which: i32) {
        let Some(s) = &self.settings else { return };
        let Some(hwnd) = platform::native(s.window()).map(|w| w.hwnd()) else { return };
        let initial = rgb_u32(if which == 0 { s.get_pet_body() } else { s.get_pet_eyes() });
        system::choose_color(hwnd, initial, move |picked| {
            if let Some(c) = picked {
                post(move |a| a.set_pet_color(which, c));
            }
        });
    }

    /// Settings > Notifications > Remove: unregister Islo and restart without
    /// the identity (the row shows Set up again).
    fn remove_notifications(&mut self) {
        if package::remove_and_relaunch() {
            quit();
            return;
        }
        self.notify_setup_error = "Couldn't start PowerShell to remove Islo's registration.".into();
        self.push_settings();
    }

    fn flash_saved(&mut self, key: &str) {
        let Some(s) = &self.settings else { return };
        s.set_saved_key(key.into());
        self.saved_timer.start(TimerMode::SingleShot, Duration::from_millis(1600), || {
            with_app(|a| {
                if let Some(s) = &a.settings {
                    s.set_saved_key("".into());
                }
            });
        });
    }

    // ---------------- updates ----------------

    /// With "Check automatically" on: check shortly after start (so startup stays
    /// quick), then once a day. One timer, no polling in between.
    fn schedule_update_checks(&mut self) {
        if !self.cfg.update_check {
            self.update_timer.stop();
            return;
        }
        Timer::single_shot(Duration::from_secs(20), || {
            with_app(|a| a.check_update(false));
        });
        self.update_timer.start(TimerMode::Repeated, Duration::from_secs(24 * 60 * 60), || {
            with_app(|a| a.check_update(false));
        });
    }

    /// `manual`: the user pressed "Check now", so report every outcome.
    /// Automatic checks stay silent unless there's something new.
    fn check_update(&mut self, manual: bool) {
        if self.update_state == U_CHECKING || self.update_state == U_INSTALLING {
            return;
        }
        if !manual && !self.cfg.update_check {
            return;
        }
        self.update_state = U_CHECKING;
        self.update_desc = "Asking GitHub for the newest version…".into();
        self.push_update_ui();
        update::check(move |result| post(move |a| a.on_update_checked(result, manual)));
    }

    fn on_update_checked(&mut self, result: Result<Option<update::Release>, String>, manual: bool) {
        match result {
            Ok(Some(release)) => {
                self.update_state = U_AVAILABLE;
                self.update_desc = if release.notes.is_empty() {
                    "Islo restarts in a second and keeps all your settings.".into()
                } else {
                    release.notes.clone()
                };
                if !manual && self.update_told != release.version {
                    self.update_told = release.version.clone();
                    system::tray_balloon(
                        platform::host_hwnd(),
                        &format!("Islo {} is available", release.version),
                        "Click to see what's new and install it.",
                    );
                }
                self.update = Some(release);
            }
            Ok(None) => {
                self.update = None;
                self.update_state = U_CURRENT;
                self.update_desc = format!("You have the newest version ({}).", env!("CARGO_PKG_VERSION"));
            }
            Err(msg) if manual => {
                self.update_state = U_ERROR;
                self.update_desc = msg;
            }
            Err(_) => {
                // an automatic check failed (offline, GitHub busy): try again tomorrow, quietly
                self.update_state = if self.update.is_some() { U_AVAILABLE } else { U_IDLE };
                self.update_desc = self.update.as_ref().map(|r| r.notes.clone()).unwrap_or_default();
            }
        }
        if self.update_desc.is_empty() {
            self.update_desc = "Islo looks for a new version once a day.".into();
        }
        self.push_update_ui();
    }

    fn install_update(&mut self) {
        if self.update_state == U_INSTALLING {
            return;
        }
        let Some(release) = self.update.clone() else { return };
        self.update_state = U_INSTALLING;
        self.update_desc = "Downloading. Islo restarts by itself when it's ready.".into();
        self.push_update_ui();
        update::install(release, |result| post(move |a| a.on_update_installed(result)));
    }

    fn on_update_installed(&mut self, result: Result<std::path::PathBuf, String>) {
        match result {
            Ok(exe) => {
                if update::relaunch(&exe) {
                    quit();
                    return;
                }
                // The new exe is in place; it just didn't start.
                self.update = None;
                self.update_state = U_ERROR;
                self.update_desc = "The update is installed. Quit Islo and start it again to use it.".into();
            }
            Err(msg) => {
                self.update_state = U_ERROR;
                self.update_desc = msg;
            }
        }
        self.push_update_ui();
    }

    fn push_update_ui(&self) {
        let Some(s) = &self.settings else { return };
        s.set_update_state(self.update_state);
        s.set_update_version(self.update.as_ref().map(|r| r.version.clone()).unwrap_or_default().into());
        if !self.update_desc.is_empty() {
            s.set_update_desc(self.update_desc.clone().into());
        }
    }

    /// Settings, opened on the About page (where updates live).
    fn open_about(&mut self) {
        self.open_settings();
        if let Some(s) = &self.settings {
            s.set_page(4);
        }
    }

    /// Apply the whole config to the live islands.
    fn apply_config(&mut self) {
        self.sync_taskbar();
        shelf::set_accepting(self.cfg.shelf);
        self.update_notify();
        self.update_ble();
        self.beacon();
        self.update_pet_ui();
        self.cfg.hide_flyout = self.cfg.hud;
        if !self.cfg.hud && self.osd_hidden {
            system::set_native_osd_hidden(false);
            self.osd_hidden = false;
        }
        for i in 0..self.views.len() {
            let panels = self.panels(&self.views[i]);
            let v = &mut self.views[i];
            if !panels.contains(&v.panel) {
                v.panel = panels[0];
            }
        }
        self.sync_views();
        self.update_pet_ui();
        self.update_timer_ui();
        self.on_power(true);
        self.check_fullscreen();
        self.retarget();
    }

    // ---------------- host window messages ----------------

    fn host_message(&mut self, msg: u32, wp: WPARAM, _lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_POWERBROADCAST => {
                if wp.0 as u32 == PBT_POWERSETTINGCHANGE || wp.0 as u32 == PBT_APMPOWERSTATUSCHANGE {
                    self.on_power(false);
                }
                Some(LRESULT(1))
            }
            system::WM_APP_APPBAR => {
                if wp.0 as u32 == ABN_FULLSCREENAPP {
                    self.check_fullscreen();
                }
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                match wp.0 as i32 {
                    system::HOTKEY_TOGGLE => self.toggle_from_keyboard(),
                    system::HOTKEY_ESC => {
                        self.unpin();
                        for i in 0..self.views.len() {
                            self.collapse(i);
                        }
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            system::WM_APP_FOREGROUND => {
                self.check_fullscreen();
                if self.cfg.taskbar {
                    // the taskbar comes forward when it's clicked; stay on top of it.
                    // Show desktop (Win + D) raises it once more a moment later.
                    self.raise_islands();
                    for ms in [150, 600] {
                        slint::Timer::single_shot(Duration::from_millis(ms), || {
                            with_app(|a| a.raise_islands());
                        });
                    }
                }
                if self.cfg.show_on == 2 {
                    // follow the display you're working on
                    let m = system::monitor_at_cursor();
                    if self.views.first().is_some_and(|v| v.monitor != m) {
                        self.sync_views();
                    }
                }
                Some(LRESULT(0))
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                self.sync_views();
                None
            }
            WM_TIMECHANGE => {
                self.refresh_date();
                None
            }
            WM_SETTINGCHANGE => {
                // the taskbar may have moved to another edge
                let (edge, th) = (self.cfg.edge, self.taskbar_h);
                self.sync_taskbar();
                if self.cfg.edge != edge || self.taskbar_h != th {
                    self.push_settings();
                    self.retarget();
                }
                self.place_all();
                system::tray_refresh(platform::host_hwnd());
                self.refresh_date(); // 12/24-hour or region format may have changed
                let anim = system::system_animations_enabled();
                if anim != self.sys_anim {
                    self.sys_anim = anim;
                    self.retarget();
                }
                if self.settings.is_some() {
                    let dark = self.settings_dark();
                    if let Some(s) = &self.settings {
                        s.set_dark(dark);
                        s.set_system_reduce_motion(!self.sys_anim);
                        if let Some(w) = platform::native(s.window()) {
                            system::set_dark_title_bar(w.hwnd(), dark);
                        }
                    }
                }
                self.place_all();
                None
            }
            _ => None,
        }
    }
}

fn subtitle(info: &media::Info) -> String {
    match (info.artist.is_empty(), info.app.is_empty()) {
        (false, false) => format!("{} · {}", info.artist, info.app),
        (false, true) => info.artist.clone(),
        (true, false) => info.app.clone(),
        _ => String::new(),
    }
}

fn update_memory_text(s: &SettingsWindow) {
    let (ws, private) = system::memory_mb();
    s.set_memory_text(format!("{private:.1} MB in use (working set {ws:.1} MB)").into());
}

fn quit() {
    system::set_native_osd_hidden(false);
    // taskbar mode: give the middle of the taskbar back while Islo isn't running
    // (it moves the apps left again on the next start)
    let c = Config::load();
    if c.taskbar && c.taskbar_al >= 0 {
        system::set_taskbar_alignment(c.taskbar_al as u32);
    }
    let host = platform::host_hwnd();
    system::tray_remove(host);
    system::unregister_fullscreen_watch(host);
    let _ = slint::quit_event_loop();
}

/// The clock text for `room` px of the open header: the longest form of the
/// chosen setting that fits ("Wed 28 Oct" loses its weekday first).
fn fit_clock(clock: i32, time: &str, date: &str, room: f32) -> String {
    let width = |t: &str| t.chars().count() as f32 * 6.2 + 4.0;
    let short_date = date.split_once(' ').map_or(date, |(_, rest)| rest);
    let tries = match clock {
        0 => vec![time.to_string()],
        2 => vec![date.to_string(), short_date.to_string()],
        _ => vec![format!("{time} · {date}"), format!("{time} · {short_date}"), time.to_string()],
    };
    tries.into_iter().find(|t| width(t) <= room).unwrap_or_default()
}

/// Every setting into the settings window's controls.
fn fill_settings(s: &SettingsWindow, c: &Config) {
    s.set_width_px(c.width);
    s.set_edge(c.edge.index());
    s.set_taskbar(c.taskbar);
    s.set_side_style(c.side_style);
    s.set_hover_expand(c.hover_expand);
    s.set_hover_delay(c.hover_delay as f32);
    s.set_theme(c.theme);
    s.set_reduce_motion(c.reduce_motion);
    s.set_show_on(c.show_on);
    s.set_pill_glance(c.pill_glance);
    s.set_header_date(c.header_date);
    s.set_featured(c.featured);
    s.set_clock(c.clock);
    s.set_keep_open(c.keep_open);
    s.set_soft_shadow(c.soft_shadow);
    s.set_media_rest(c.media_rest);
    s.set_lyric_align(c.lyric_align);
    s.set_pet_touch(c.pet_touch);
    s.set_games(c.games);
    s.set_games_online(c.games_online);
    s.set_mod_media(c.media);
    s.set_art_tint(c.art_tint);
    s.set_mod_lyrics(c.lyrics);
    s.set_lyric_lines(c.lyric_lines as f32);
    s.set_mod_hud(c.hud);
    s.set_mod_battery(c.battery);
    s.set_mod_shelf(c.shelf);
    s.set_mod_notifications(c.notifications);
    s.set_mod_timer(c.timer);
    s.set_mod_privacy(c.privacy);
    s.set_mod_focus(c.focus);
    s.set_mod_pet(c.pet);
    s.set_pet_nearby(c.pet_nearby);
    s.set_pet_share_name(c.pet_share_name);
    s.set_pet_peek(c.pet_peek);
    s.set_pet_track(c.pet_track);
    s.set_pet_name(c.pet_name.clone().into());
    s.set_pet_skin(c.pet_skin);
    push_pet_colors(s, c.pet_body, c.pet_eyes);
    s.set_pet_size(c.pet_size);
    s.set_pet_moves(c.pet_moves);
    s.set_pet_often(c.pet_often);
    s.set_hide_fullscreen(c.hide_fullscreen);
    s.set_saver_pause(c.saver_pause);
    s.set_autostart(system::autostart_enabled());
    s.set_update_check(c.update_check);
}

/// Mochi's body and eye colors into the settings window, with the hints that go with them.
fn push_pet_colors(s: &SettingsWindow, body: u32, eyes: u32) {
    const BODIES: [u32; 8] = [0x000000, 0x3a3a3a, 0xf5f5f5, 0xfc90bc, 0xf99f55, 0xd3b63b, 0x6fd087, 0x71bfff];
    const EYES: [u32; 7] = [0xd3b63b, 0x000000, 0xffffff, 0xfc90bc, 0xf99f55, 0x6fd087, 0x71bfff];
    s.set_pet_body(rgb(body));
    s.set_pet_eyes(rgb(eyes));
    s.set_pet_body_custom(!BODIES.contains(&body));
    s.set_pet_eyes_custom(!EYES.contains(&eyes));
    let eyes_desc = match pet::colors(body, eyes).3 {
        None => "Also its mouth while eating.".to_string(),
        Some(ring) => {
            let other = if ring == 0 { "black" } else { "white" };
            format!("Close to the body color, so the eyes get a thin {other} outline. Your color stays.")
        }
    };
    s.set_pet_eyes_desc(eyes_desc.into());
}

/// Everything the settings window shows, into a config (Save).
fn read_settings(s: &SettingsWindow, c: &mut Config) {
    c.width = s.get_width_px().clamp(160.0, 300.0);
    let new_edge = Edge::from_index(s.get_edge());
    let edge_moved = new_edge != c.edge;
    if edge_moved {
        c.edge = new_edge;
        c.edge_pos = 0.5;
    }
    // taskbar mode docks to the taskbar's edge; picking another edge leaves it
    let tb = system::taskbar_edge();
    if s.get_taskbar() && !c.taskbar {
        if let Some(e) = tb {
            c.edge = e;
        }
        c.edge_pos = 0.5;
        c.taskbar = true;
    } else if !s.get_taskbar() || (edge_moved && tb != Some(c.edge)) {
        c.taskbar = false;
    }
    c.side_style = s.get_side_style().clamp(0, 2);
    c.hover_expand = s.get_hover_expand();
    c.hover_delay = s.get_hover_delay().clamp(100.0, 1000.0) as u32;
    c.theme = s.get_theme().clamp(0, 2);
    c.reduce_motion = s.get_reduce_motion();
    c.show_on = s.get_show_on().clamp(0, 2);
    c.pill_glance = s.get_pill_glance();
    c.header_date = s.get_header_date();
    c.featured = s.get_featured().clamp(0, 4);
    c.clock = s.get_clock().clamp(0, 2);
    c.keep_open = s.get_keep_open();
    c.soft_shadow = s.get_soft_shadow();
    c.media_rest = s.get_media_rest();
    c.lyric_align = s.get_lyric_align().clamp(0, 2);
    c.pet_touch = s.get_pet_touch();
    c.games = s.get_games();
    c.games_online = s.get_games_online();
    c.media = s.get_mod_media();
    c.art_tint = s.get_art_tint();
    c.lyrics = s.get_mod_lyrics();
    c.lyric_lines = (s.get_lyric_lines().round() as i32).clamp(1, 10);
    c.hud = s.get_mod_hud();
    c.battery = s.get_mod_battery();
    c.shelf = s.get_mod_shelf();
    c.notifications = s.get_mod_notifications();
    c.timer = s.get_mod_timer();
    c.privacy = s.get_mod_privacy();
    c.focus = s.get_mod_focus();
    c.pet = s.get_mod_pet();
    c.pet_nearby = s.get_pet_nearby();
    c.pet_share_name = s.get_pet_share_name();
    c.pet_peek = s.get_pet_peek();
    c.pet_track = s.get_pet_track();
    c.pet_skin = s.get_pet_skin().clamp(-1, pet::SKINS.len() as i32 - 1);
    c.pet_body = rgb_u32(s.get_pet_body());
    c.pet_eyes = rgb_u32(s.get_pet_eyes());
    c.pet_size = s.get_pet_size().clamp(0, 2);
    c.pet_moves = s.get_pet_moves().clamp(0, 2);
    c.pet_often = s.get_pet_often().clamp(0, 2);
    let name: String = s.get_pet_name().trim().chars().filter(|c| !c.is_control()).take(24).collect();
    if !name.is_empty() {
        c.pet_name = name;
    }
    c.hide_fullscreen = s.get_hide_fullscreen();
    c.saver_pause = s.get_saver_pause();
    c.update_check = s.get_update_check();
}

fn rgb(c: u32) -> slint::Color {
    slint::Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn rgb_u32(c: slint::Color) -> u32 {
    ((c.red() as u32) << 16) | ((c.green() as u32) << 8) | c.blue() as u32
}

/// Mochi's colors (any body and eye color; see pet::colors) and size.
fn apply_skin(isl: &Island, cfg: &Config) {
    let (head, eyes, island_head, ring) = pet::colors(cfg.pet_body, cfg.pet_eyes);
    let g = isl.global::<PetSkin>();
    g.set_head(rgb(head));
    g.set_eyes(rgb(eyes));
    g.set_island_head(rgb(island_head));
    g.set_eye_ring(ring.map(rgb).unwrap_or(slint::Color::from_argb_u8(0, 0, 0, 0)));
    g.set_size(pet::SIZES[cfg.pet_size.clamp(0, 2) as usize]);
}

/// Connect one island's UI callbacks to the app (by view index).
fn wire_island(island: &Island, i: usize) {
    island.on_press_changed(move |p| {
        with_app(|a| a.on_press(i, p));
    });
    island.on_clicked(move || {
        with_app(|a| a.on_click(i));
    });
    island.on_right_clicked(|| {
        with_app(|a| a.open_settings());
    });
    island.on_open_settings(|| {
        with_app(|a| a.open_settings());
    });
    island.on_peek_poked(|| {
        with_app(|a| a.poke_peek());
    });
    island.on_scrolled(move |dy| {
        with_app(|a| a.on_scroll(i, dy));
    });
    island.on_scrolled_x(move |dx| {
        with_app(|a| a.on_scroll_x(i, dx));
    });
    island.on_tab_step(move |dir| {
        with_app(|a| a.tab_step(i, dir));
    });
    island.on_swipe(move |dx, done| {
        with_app(|a| a.on_swipe(i, dx, done));
    });
    island.on_toggle_pin(move || {
        with_app(|a| a.toggle_pin(i));
    });
    island.on_shelf_add(|| {
        let host = platform::host_hwnd();
        shelf::pick_files(host, |files| {
            post(move |a| {
                a.add_to_shelf(files);
            })
        });
    });
    island.on_shelf_paste(|| {
        with_app(|a| {
            let files = shelf::clipboard_files();
            if a.add_to_shelf(files) == 0 {
                a.each(|isl| {
                    isl.set_error_title("Nothing to paste".into());
                    isl.set_error_text("Copy files first".into());
                });
                a.show_transient(C_ERROR, 2200);
            }
        });
    });
    island.on_pet_stroke(|| {
        with_app(|a| a.pet_stroke());
    });
    island.on_pet_carry(move |x, y| {
        with_app(|a| a.pet_carry(i, x, y));
    });
    island.on_pet_drop(move |x, y| {
        with_app(|a| a.pet_drop(i, x, y));
    });
    island.on_game_play(|k| {
        with_app(|a| a.game_play(k.max(0) as usize));
    });
    island.on_game_new(|| {
        with_app(|a| a.game_new());
    });
    island.on_game_level_set(|k| {
        with_app(|a| {
            a.cfg.game_level = k.clamp(0, 2);
            a.cfg.save();
            a.update_game_ui();
        });
    });
    island.on_game_mode_set(|k| {
        with_app(|a| a.game_mode_set(k));
    });
    island.on_game_host(|| {
        with_app(|a| a.game_host());
    });
    island.on_game_join(|| {
        with_app(|a| a.game_join());
    });
    island.on_game_copy(|| {
        with_app(|a| platform::clipboard_set(&a.game_code));
    });
    island.on_game_leave(|| {
        with_app(|a| a.game_leave());
    });
    island.on_play_pause(|| {
        with_app(|a| a.media_cmd(media::Cmd::PlayPause));
    });
    island.on_next_track(|| {
        with_app(|a| a.media_cmd(media::Cmd::Next));
    });
    island.on_prev_track(|| {
        with_app(|a| a.media_cmd(media::Cmd::Prev));
    });
    island.on_select_panel(move |p| {
        with_app(|a| a.open_panel(i, p));
    });
    island.on_note_open(|k| {
        with_app(|a| a.note_open(k as usize));
    });
    island.on_notes_clear(|| {
        with_app(|a| a.notes_clear());
    });
    island.on_pet_feed(|| {
        with_app(|a| a.pet_feed());
    });
    island.on_pet_toggle_list(|| {
        with_app(|a| {
            a.pet_list_open = !a.pet_list_open;
            a.update_pet_ui();
            a.retarget();
        });
    });
    island.on_pet_send(|k| {
        with_app(|a| a.pet_send(k as usize));
    });
    island.on_pet_call_home(|| {
        with_app(|a| a.pet_call_home());
    });
    island.on_pet_send_home(|| {
        with_app(|a| a.pet_send_home());
    });
    island.on_timer_start(|m| {
        with_app(|a| a.timer_start(m));
    });
    island.on_timer_pause(|| {
        with_app(|a| a.timer_pause());
    });
    island.on_timer_cancel(|| {
        with_app(|a| a.timer_cancel());
    });
    island.on_shelf_open(|k| {
        with_app(|a| {
            if let Some(e) = a.shelf.get(k as usize) {
                system::open_path(&e.path);
            }
        });
    });
    island.on_shelf_remove(|k| {
        with_app(|a| {
            if (k as usize) < a.shelf.len() {
                a.shelf.remove(k as usize);
                a.rebuild_shelf();
                a.apply_panels_after_shelf_change();
            }
        });
    });
    island.on_shelf_clear(|| {
        with_app(|a| {
            a.shelf.clear();
            a.rebuild_shelf();
            a.apply_panels_after_shelf_change();
        });
    });
    island.on_shelf_drag(|k| {
        with_app(|a| a.pending_drag = Some(k as usize));
        unsafe {
            let _ = PostMessageW(Some(platform::host_hwnd()), system::WM_APP_START_DRAG, WPARAM(0), LPARAM(0));
        }
    });
}

impl App {
    fn apply_panels_after_shelf_change(&mut self) {
        if self.shelf.is_empty() && self.cfg.media {
            for v in &mut self.views {
                if v.panel == P_SHELF {
                    v.panel = P_MEDIA;
                }
            }
        }
        self.retarget();
    }
}

/// Messages that open modal loops (menus, drag & drop) must run *outside*
/// `with_app`, because those loops dispatch more messages back into the app.
fn host_handler(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, taskbar_created: u32, quit_msg: u32) -> Option<LRESULT> {
    match msg {
        system::WM_APP_TRAY => {
            match (lp.0 & 0xffff) as u32 {
                WM_LBUTTONUP => {
                    with_app(|a| a.open_settings());
                }
                system::NIN_BALLOON_CLICK => {
                    with_app(|a| a.open_about());
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let (hidden, new_version) =
                        with_app(|a| (a.hidden_user, a.update.as_ref().map(|r| r.version.clone()))).unwrap_or((false, None));
                    match system::tray_menu(hwnd, hidden, new_version.as_deref()) {
                        system::TrayChoice::Settings => {
                            with_app(|a| a.open_settings());
                        }
                        system::TrayChoice::Update => {
                            with_app(|a| a.open_about());
                        }
                        system::TrayChoice::ToggleHidden => {
                            with_app(|a| {
                                a.hidden_user = !a.hidden_user;
                                a.apply_visibility();
                            });
                        }
                        system::TrayChoice::Quit => quit(),
                        system::TrayChoice::None => {}
                    }
                }
                _ => {}
            }
            Some(LRESULT(0))
        }
        system::WM_APP_START_DRAG => {
            let path = with_app(|a| {
                let k = a.pending_drag.take()?;
                // Tell Slint the button is "released" so the tile doesn't stay pressed.
                for v in &a.views {
                    v.island.window().dispatch_event(slint::platform::WindowEvent::PointerReleased {
                        position: slint::LogicalPosition::new(0.0, 0.0),
                        button: slint::platform::PointerEventButton::Left,
                    });
                }
                a.shelf.get(k).map(|e| e.path.clone())
            })
            .flatten();
            if let Some(path) = path {
                unsafe {
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                }
                let moved = shelf::drag_out(&path);
                with_app(|a| {
                    if moved {
                        a.shelf.retain(|e| e.path != path);
                        a.rebuild_shelf();
                    }
                    a.retarget();
                });
            }
            Some(LRESULT(0))
        }
        m if m == taskbar_created && m != 0 => {
            system::tray_add(hwnd);
            Some(LRESULT(0))
        }
        // asked to quit (`islo.exe --quit`, a dev build taking over, WM_CLOSE)
        m if m == WM_CLOSE || (m == quit_msg && m != 0) => {
            quit();
            Some(LRESULT(0))
        }
        _ => with_app(|a| a.host_message(msg, wp, lp)).flatten(),
    }
}

pub fn run() -> Result<(), slint::PlatformError> {
    let cfg = Config::load();
    cfg.save(); // rewrites an older islo.ini in the current format
    let app = App {
        views: Vec::new(),
        settings: None,
        cfg,
        hud_active: false,
        transient: None,
        flash_wide: false,
        hidden_user: false,
        sys_anim: system::system_animations_enabled(),
        taskbar_h: 0.0,
        media: None,
        art_id: 0,
        art: slint::Image::default(),
        accent: slint::Color::from_rgb_u8(0xfc, 0x90, 0xbc),
        media_tx: None,
        audio_tx: None,
        battery: system::battery(),
        hud: (0, 0.0, false),
        shelf: Vec::new(),
        shelf_model: Rc::new(VecModel::<ShelfItem>::default()),
        pending_drag: None,
        hud_timer: Timer::default(),
        transient_timer: Timer::default(),
        flash_timer: Timer::default(),
        progress_timer: Timer::default(),
        wave_timer: Timer::default(),
        wave_start: Instant::now(),
        mem_timer: Timer::default(),
        saved_timer: Timer::default(),
        click_timer: Timer::default(),
        timer: None,
        timer_tick: Timer::default(),
        privacy: live::Privacy::default(),
        focus_on: false,
        esc_armed: false,
        osd_hidden: false,
        lyrics: Vec::new(),
        lyrics_synced: false,
        lyric_state: L_HIDDEN,
        clock: (0.0, None),
        lyrics_key: String::new(),
        lyric_idx: usize::MAX,
        lyrics_model: Rc::new(VecModel::default()),
        lyrics_tx: None,
        lyric_timer: Timer::default(),
        owner: pet::owner_name(),
        beacon_id: pet::session_id(),
        ble_tx: None,
        ble_ok: true,
        peers: Vec::new(),
        greeted: Vec::new(),
        pet_list_open: false,
        away: None,
        visitor: None,
        sending_home: None,
        pet_timer: Timer::default(),
        date_text: String::new(),
        date_timer: Timer::default(),
        time_text: String::new(),
        media_resting: false,
        rest_timer: Timer::default(),
        page_timer: Timer::default(),
        xacc: 0.0,
        xacc_at: Instant::now(),
        xgo_at: Instant::now(),
        strokes: 0,
        stroke_at: Instant::now(),
        squish_timer: Timer::default(),
        hearts_timer: Timer::default(),
        carrying: false,
        carry_watch: Timer::default(),
        game_board: [0; 9],
        game_over: None,
        game_thinking: false,
        game_timer: Timer::default(),
        game_score: (0, 0, 0),
        game_mode: 0,
        game_net: 0,
        game_code: String::new(),
        game_net_text: String::new(),
        game_room: None,
        game_tx: None,
        pointer_on: false,
        pointer_at: (0, 0),
        pointer_awake: false,
        nap_reset_at: Instant::now(),
        eye_timer: Timer::default(),
        nap_timer: Timer::default(),
        blink_timer: Timer::default(),
        peek: None,
        peek_timer: Timer::default(),
        peek_step: Timer::default(),
        rng: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1,
        pet_bob: false,
        mood: pet::face::NEUTRAL,
        mood_lively: false,
        mood_timer: Timer::default(),
        lively_timer: Timer::default(),
        act: -1,
        act_lively: false,
        act_timer: Timer::default(),
        eating: false,
        feeds: Vec::new(),
        pokes: Vec::new(),
        excited_at: None,
        visit_timer: Timer::default(),
        home_timer: Timer::default(),
        notify_tx: None,
        notify_state: 0,
        notify_ask: std::env::args().any(|a| a == package::READY_ARG),
        notify_setting_up: false,
        notify_setup_error: if std::env::args().any(|a| a == package::REMOVE_FAILED_ARG) {
            "Windows didn't remove Islo's registration. Try Remove again.".into()
        } else {
            String::new()
        },
        notes: Vec::new(),
        known_notes: Vec::new(),
        dismissed: Vec::new(),
        unread: 0,
        call: None,
        call_timer: Timer::default(),
        update: None,
        update_state: U_IDLE,
        update_desc: String::new(),
        update_timer: Timer::default(),
        update_told: String::new(),
    };
    APP.with(|a| *a.borrow_mut() = Some(app));

    // ---- platform hooks ----
    platform::set_frame_hook(|| with_app(|a| a.frame()).unwrap_or(false));
    platform::set_pointer_hook(|hwnd, p| {
        with_app(|a| {
            if let Some(i) = a.views.iter().position(|v| v.win.hwnd() == hwnd) {
                a.on_pointer(i, p);
            }
        });
    });
    let taskbar_created = system::taskbar_created_msg();
    let quit_msg = system::quit_msg();
    platform::set_host_handler(move |hwnd, msg, wp, lp| host_handler(hwnd, msg, wp, lp, taskbar_created, quit_msg));

    // ---- system services (each sleeps until Windows has news) ----
    let host = platform::host_hwnd();
    system::register_power_notifications(host);
    system::register_fullscreen_watch(host);
    system::tray_add(host);
    with_app(|a| {
        a.refresh_date();
        a.update_game_ui();
        // pinned last time: open again
        if a.cfg.keep_open && !a.views.is_empty() {
            a.expand(0);
        }
    });

    let media_tx = media::start(|info| post(move |a| a.on_media(info)));
    let audio_tx = audio::start(|v, muted, initial| {
        if !initial {
            post(move |a| {
                if a.cfg.hud {
                    a.show_hud(0, v, muted)
                }
            });
        }
    });
    brightness::start(|b, initial| {
        if !initial {
            post(move |a| {
                if a.cfg.hud {
                    a.show_hud(1, b as f32 / 100.0, false)
                }
            });
        }
    });

    let lyrics_tx = lyrics::start(|key, found| post(move |a| a.on_lyrics(key, found)));
    live::start_privacy(|p| post(move |a| a.on_privacy(p)));
    live::start_focus(|on| post(move |a| a.on_focus(on)));
    system::register_toggle_hotkey(host);

    with_app(|a| {
        a.media_tx = Some(media_tx);
        a.lyrics_tx = Some(lyrics_tx);
        a.audio_tx = Some(audio_tx);
        a.rebuild_shelf();
        a.apply_config(); // creates the island(s), places and shows them
        a.schedule_update_checks();
    });

    slint::run_event_loop_until_quit()?;
    APP.with(|a| a.borrow_mut().take());
    Ok(())
}

/// Now, in the notification clock (100 ns since 1601, like FILETIME).
fn notify_now() -> i64 {
    let ft = unsafe { windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

/// Like `f32::clamp`, but never panics: if the range is empty (lo > hi) or a
/// value is NaN it returns `lo` instead. (`clamp` aborts the app on that, which
/// is what crashed Islo when the tall lyrics panel opened.)
fn clamp_safe(v: f32, lo: f32, hi: f32) -> f32 {
    if !(lo <= hi) || v.is_nan() {
        return lo;
    }
    v.clamp(lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};

    thread_local! {
        static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    }

    struct TestPlatform;
    impl Platform for TestPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            let w = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            LAST.with(|l| *l.borrow_mut() = Some(w.clone()));
            Ok(w)
        }
    }

    /// Lay out and paint once, which builds the controls of the visible page.
    fn paint(w: &MinimalSoftwareWindow) {
        use slint::platform::software_renderer::PremultipliedRgbaColor;
        let (pw, ph) = (1000u32, 720u32);
        w.set_size(slint::PhysicalSize::new(pw, ph));
        slint::platform::update_timers_and_animations();
        w.request_redraw();
        let mut buf = vec![PremultipliedRgbaColor::default(); (pw * ph) as usize];
        w.draw_if_needed(|r| {
            r.render(&mut buf, pw as usize);
        });
    }

    /// Every clock choice shows something in the header, shortening as tabs take the room.
    #[test]
    fn header_clock_shortens_instead_of_vanishing() {
        let (t, d) = ("23:13", "Wed 28 Oct");
        assert_eq!(fit_clock(1, t, d, 200.0), "23:13 · Wed 28 Oct");
        assert_eq!(fit_clock(1, t, d, 100.0), "23:13 · 28 Oct");
        assert_eq!(fit_clock(1, t, d, 50.0), "23:13");
        assert_eq!(fit_clock(2, t, d, 100.0), "Wed 28 Oct");
        assert_eq!(fit_clock(2, t, d, 62.0), "28 Oct"); // 7 tabs: the full date doesn't fit
        assert_eq!(fit_clock(0, t, d, 62.0), "23:13");
        assert_eq!(fit_clock(0, t, d, 10.0), "");
    }

    /// Settings: what goes into the window comes back out on Save, also after
    /// switching pages (each page's controls are rebuilt when it's shown).
    #[test]
    fn settings_window_round_trip() {
        let _ = slint::platform::set_platform(Box::new(TestPlatform));
        let s = SettingsWindow::new().expect("settings window");
        let mut c = Config {
            width: 270.0,
            edge: Edge::Right,
            side_style: 1,
            hover_expand: false,
            hover_delay: 750,
            theme: 1,
            show_on: 1,
            featured: 2,
            clock: 0,
            keep_open: true,
            soft_shadow: false,
            media_rest: false,
            lyric_align: 1,
            lyrics: false,
            lyric_lines: 6,
            pet_name: "Bean".into(),
            pet_skin: -1,
            pet_body: 0x123456,
            pet_eyes: 0xabcdef,
            pet_size: 2,
            pet_moves: 1,
            pet_often: 0,
            games_online: false,
            update_check: false,
            ..Config::default()
        };
        c.hide_flyout = c.hud;
        let w = LAST.with(|l| l.borrow().clone()).expect("window adapter");
        paint(&w);
        fill_settings(&s, &c);
        for page in [0, 1, 2, 3, 4, 1, 2, 0] {
            s.set_page(page);
            paint(&w);
        }
        let mut back = Config::default();
        back.edge_pos = c.edge_pos;
        back.monitor = c.monitor;
        back.pet_fed = c.pet_fed;
        read_settings(&s, &mut back);
        back.edge_pos = c.edge_pos; // a new edge recenters; that's expected
        assert_eq!(back, c);
    }
}
