//! Settings, stored as a tiny `islo.ini` next to Islo.exe (portable: nothing is
//! written to C: unless the folder isn't writable, then %APPDATA%\Islo is used).

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

impl Edge {
    pub fn index(self) -> i32 {
        match self {
            Edge::Top => 0,
            Edge::Bottom => 1,
            Edge::Left => 2,
            Edge::Right => 3,
        }
    }
    pub fn from_index(i: i32) -> Edge {
        match i {
            1 => Edge::Bottom,
            2 => Edge::Left,
            3 => Edge::Right,
            _ => Edge::Top,
        }
    }
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    // appearance
    pub width: f32, // compact width at rest, 160..300 logical px
    pub edge: Edge,
    pub edge_pos: f32,   // 0..1 along the edge (0.5 = centered)
    pub side_style: i32, // 0 icon tab, 1 thin sliver, 2 full vertical pill
    pub hover_expand: bool,
    pub hover_delay: u32, // ms, 100..1000
    pub theme: i32,       // settings window: 0 follow Windows, 1 light, 2 dark
    pub reduce_motion: bool,
    pub show_on: i32, // 0 main display, 1 all displays, 2 display with the pointer
    pub monitor: i32, // 0 = primary, n = n-th display (used with show_on = 0)
    // at-a-glance info
    pub pill_glance: bool, // idle pill: featured feature left, date + battery right
    pub header_date: bool, // expanded header: date beside the battery
    pub featured: i32,     // left of the idle pill: 0 none, 1 media, 2 timer, 3 pet, 4 focus
    // modules
    pub media: bool,
    pub lyrics: bool,
    pub lyric_lines: i32, // 1..10
    pub hud: bool,        // volume + brightness
    pub hide_flyout: bool,
    pub battery: bool,
    pub shelf: bool,
    pub notifications: bool,
    pub timer: bool,
    pub privacy: bool, // mic / camera / screen recording indicators
    pub focus: bool,
    pub pet: bool,
    pub pet_name: String,
    pub pet_nearby: bool,
    pub pet_peek: bool,  // peek around the island while it's quiet
    pub pet_track: bool, // eyes follow the pointer
    pub pet_skin: i32,   // preset look in pet::SKINS (0 = Classic), -1 = custom colors
    pub pet_body: u32,   // 0xRRGGBB, any color
    pub pet_eyes: u32,   // 0xRRGGBB, any color
    pub pet_size: i32,   // peeking Mochi: 0 small, 1 medium, 2 large
    pub pet_moves: i32,  // 0 peek, 1 walk around the island, 2 both
    pub pet_often: i32,  // 0 rarely, 1 sometimes, 2 often
    pub pet_share_name: bool, // put the user's first name in the Bluetooth beacon
    pub pet_fed: u64,         // unix seconds, 0 = never
    // full screen & power
    pub hide_fullscreen: bool,
    pub saver_pause: bool,
    // updates
    pub update_check: bool, // look for a new version on GitHub once a day
    // 0.3.12
    pub clock: i32,         // right side of the quiet pill and the open header: 0 clock, 1 clock + date, 2 date
    pub keep_open: bool,    // pinned: the island stays expanded until unpinned
    pub soft_shadow: bool,  // a soft shadow under the island
    pub media_rest: bool,   // back to the quiet pill 5 s after the music pauses
    pub lyric_align: i32,   // 0 left, 1 center, 2 right
    pub pet_touch: bool,    // Mochi can be petted and carried
    pub games: bool,        // the tic-tac-toe tab
    pub games_online: bool, // play friends through the relay with a room code
    pub game_level: i32,    // 0 easy, 1 medium, 2 hard
    // taskbar mode
    pub taskbar: bool,    // bottom edge: sit inside the taskbar instead of above it
    pub taskbar_al: i32,  // taskbar alignment to restore when leaving taskbar mode, -1 = untouched
    pub art_tint: bool,   // tint the waveform and controls with the album art's color (off: white)
}

impl Default for Config {
    fn default() -> Self {
        Config {
            width: 200.0,
            edge: Edge::Top,
            edge_pos: 0.5,
            side_style: 0,
            hover_expand: true,
            hover_delay: 300,
            theme: 0,
            reduce_motion: false,
            show_on: 0,
            monitor: 0,
            pill_glance: true,
            header_date: true,
            featured: 3,
            media: true,
            lyrics: true,
            lyric_lines: 3,
            hud: true,
            hide_flyout: true,
            battery: true,
            shelf: true,
            notifications: true,
            timer: true,
            privacy: true,
            focus: true,
            pet: true,
            pet_name: "Mochi".into(),
            pet_nearby: true,
            pet_peek: true,
            pet_track: true,
            pet_skin: 0,
            pet_body: 0x000000,
            pet_eyes: 0xd3b63b,
            pet_size: 1,
            pet_moves: 2,
            pet_often: 1,
            pet_share_name: false,
            pet_fed: 0,
            hide_fullscreen: true,
            saver_pause: true,
            update_check: true,
            clock: 1,
            keep_open: false,
            soft_shadow: true,
            media_rest: true,
            lyric_align: 0,
            pet_touch: true,
            games: true,
            games_online: true,
            game_level: 1,
            taskbar: false,
            taskbar_al: -1,
            art_tint: true,
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let probe = dir.join(".islo-write-test");
                if std::fs::write(&probe, b"").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return dir.join("islo.ini");
                }
            }
        }
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let dir = base.join("Islo");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("islo.ini")
    }

    pub fn load() -> Config {
        let mut c = Config::default();
        let Ok(text) = std::fs::read_to_string(Self::path()) else { return c };
        let mut has_colors = false;
        let hex = |v: &str, d: u32| u32::from_str_radix(v.trim_start_matches('#'), 16).map_or(d, |c| c & 0xff_ffff);
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let b = v == "1" || v.eq_ignore_ascii_case("true");
            let f = |d: f32| v.parse::<f32>().unwrap_or(d);
            let i = |d: i32| v.parse::<i32>().unwrap_or(d);
            match k {
                "width" => c.width = f(200.0).clamp(160.0, 300.0),
                "edge" => c.edge = Edge::from_index(i(0)),
                "edge_pos" => c.edge_pos = f(0.5).clamp(0.0, 1.0),
                "side_style" => c.side_style = i(0).clamp(0, 2),
                "hover_expand" => c.hover_expand = b,
                "hover_delay" => c.hover_delay = i(300).clamp(100, 1000) as u32,
                "theme" => c.theme = i(0).clamp(0, 2),
                "reduce_motion" => c.reduce_motion = b,
                "show_on" => c.show_on = i(0).clamp(0, 2),
                "monitor" => c.monitor = i(0).max(0),
                "pill_glance" => c.pill_glance = b,
                "header_date" => c.header_date = b,
                "featured" => c.featured = i(3).clamp(0, 4),
                "media" => c.media = b,
                "lyrics" => c.lyrics = b,
                "lyric_lines" => c.lyric_lines = i(3).clamp(1, 10),
                "hud" => c.hud = b,
                "hide_flyout" => c.hide_flyout = b,
                "battery" => c.battery = b,
                "shelf" => c.shelf = b,
                "notifications" => c.notifications = b,
                "timer" => c.timer = b,
                "privacy" => c.privacy = b,
                "focus" => c.focus = b,
                "pet" => c.pet = b,
                "pet_name" => c.pet_name = v.chars().filter(|c| !c.is_control()).take(24).collect(),
                "pet_nearby" => c.pet_nearby = b,
                "pet_peek" => c.pet_peek = b,
                "pet_track" => c.pet_track = b,
                "pet_skin" => c.pet_skin = i(0).clamp(-1, crate::pet::SKINS.len() as i32 - 1),
                "pet_body" => {
                    c.pet_body = hex(v, 0x000000);
                    has_colors = true;
                }
                "pet_eyes" => c.pet_eyes = hex(v, 0xd3b63b),
                "pet_size" => c.pet_size = i(1).clamp(0, 2),
                "pet_moves" => c.pet_moves = i(2).clamp(0, 2),
                "pet_often" => c.pet_often = i(1).clamp(0, 2),
                "pet_share_name" => c.pet_share_name = b,
                "pet_fed" => c.pet_fed = v.parse().unwrap_or(0),
                "hide_fullscreen" => c.hide_fullscreen = b,
                "saver_pause" => c.saver_pause = b,
                "update_check" => c.update_check = b,
                "clock" => c.clock = i(1).clamp(0, 2),
                "keep_open" => c.keep_open = b,
                "soft_shadow" => c.soft_shadow = b,
                "media_rest" => c.media_rest = b,
                "lyric_align" => c.lyric_align = i(0).clamp(0, 2),
                "pet_touch" => c.pet_touch = b,
                "games" => c.games = b,
                "games_online" => c.games_online = b,
                "game_level" => c.game_level = i(1).clamp(0, 2),
                "taskbar" => c.taskbar = b,
                "taskbar_al" => c.taskbar_al = i(-1).clamp(-1, 1),
                "art_tint" => c.art_tint = b,
                _ => {}
            }
        }
        if c.pet_name.trim().is_empty() {
            c.pet_name = "Mochi".into();
        }
        // an islo.ini from before custom colors: take the preset's
        if !has_colors {
            let (_, body, eyes) = crate::pet::SKINS[c.pet_skin.max(0) as usize];
            (c.pet_body, c.pet_eyes) = (body, eyes);
        }
        c
    }

    /// Write islo.ini, ignoring errors (for the island's own small changes).
    pub fn save(&self) {
        let _ = self.try_save();
    }

    /// Write islo.ini and read it back, so Settings can tell whether it really
    /// stuck. Goes through islo.ini.tmp and a rename, so a crash or a full disk
    /// mid-write never leaves half a file.
    pub fn try_save(&self) -> Result<PathBuf, String> {
        let b = |v: bool| if v { 1 } else { 0 };
        let text = format!(
            "# Islo settings\n\
             width={}\nedge={}\nedge_pos={:.3}\nside_style={}\nhover_expand={}\nhover_delay={}\ntheme={}\n\
             reduce_motion={}\nshow_on={}\nmonitor={}\npill_glance={}\nheader_date={}\nfeatured={}\n\
             media={}\nlyrics={}\nlyric_lines={}\nhud={}\nhide_flyout={}\nbattery={}\nshelf={}\nnotifications={}\n\
             timer={}\nprivacy={}\nfocus={}\npet={}\npet_name={}\npet_nearby={}\npet_peek={}\npet_track={}\npet_skin={}\npet_body=#{:06x}\npet_eyes=#{:06x}\npet_size={}\npet_moves={}\npet_often={}\npet_share_name={}\npet_fed={}\nhide_fullscreen={}\nsaver_pause={}\nupdate_check={}\n",
            self.width.round(),
            self.edge.index(),
            self.edge_pos,
            self.side_style,
            b(self.hover_expand),
            self.hover_delay,
            self.theme,
            b(self.reduce_motion),
            self.show_on,
            self.monitor,
            b(self.pill_glance),
            b(self.header_date),
            self.featured,
            b(self.media),
            b(self.lyrics),
            self.lyric_lines,
            b(self.hud),
            b(self.hide_flyout),
            b(self.battery),
            b(self.shelf),
            b(self.notifications),
            b(self.timer),
            b(self.privacy),
            b(self.focus),
            b(self.pet),
            self.pet_name,
            b(self.pet_nearby),
            b(self.pet_peek),
            b(self.pet_track),
            self.pet_skin,
            self.pet_body,
            self.pet_eyes,
            self.pet_size,
            self.pet_moves,
            self.pet_often,
            b(self.pet_share_name),
            self.pet_fed,
            b(self.hide_fullscreen),
            b(self.saver_pause),
            b(self.update_check),
        );
        let text = text
            + &format!(
                "clock={}\nkeep_open={}\nsoft_shadow={}\nmedia_rest={}\nlyric_align={}\npet_touch={}\ngames={}\ngames_online={}\ngame_level={}\ntaskbar={}\ntaskbar_al={}\nart_tint={}\n",
                self.clock,
                b(self.keep_open),
                b(self.soft_shadow),
                b(self.media_rest),
                self.lyric_align,
                b(self.pet_touch),
                b(self.games),
                b(self.games_online),
                self.game_level,
                b(self.taskbar),
                self.taskbar_al,
                b(self.art_tint),
            );
        let path = Self::path();
        let tmp = path.with_extension("ini.tmp");
        let fail = |e: std::io::Error| format!("Couldn't save to {}: {e}", path.display());
        std::fs::write(&tmp, &text).map_err(fail)?;
        if std::fs::rename(&tmp, &path).is_err() {
            // rename can fail while another program holds islo.ini open: write in place
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(&path, &text).map_err(fail)?;
        }
        match std::fs::read_to_string(&path) {
            Ok(back) if back == text => Ok(path),
            Ok(_) => Err(format!("{} changed while saving. Try again.", path.display())),
            Err(e) => Err(fail(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every setting differs from the default, so a field that save() or load()
    /// forgets shows up as a mismatch.
    fn all_changed() -> Config {
        Config {
            width: 250.0,
            edge: Edge::Bottom,
            edge_pos: 0.25,
            side_style: 2,
            hover_expand: false,
            hover_delay: 650,
            theme: 2,
            reduce_motion: true,
            show_on: 2,
            monitor: 1,
            pill_glance: false,
            header_date: false,
            featured: 4,
            media: false,
            lyrics: false,
            lyric_lines: 7,
            hud: false,
            hide_flyout: false,
            battery: false,
            shelf: false,
            notifications: false,
            timer: false,
            privacy: false,
            focus: false,
            pet: false,
            pet_name: "Bean".into(),
            pet_nearby: false,
            pet_peek: false,
            pet_track: false,
            pet_skin: -1,
            pet_body: 0x123456,
            pet_eyes: 0xabcdef,
            pet_size: 2,
            pet_moves: 0,
            pet_often: 2,
            pet_share_name: true,
            pet_fed: 1_700_000_000,
            hide_fullscreen: false,
            saver_pause: false,
            update_check: false,
            clock: 2,
            keep_open: true,
            soft_shadow: false,
            media_rest: false,
            lyric_align: 2,
            pet_touch: false,
            games: false,
            games_online: false,
            game_level: 0,
        }
    }

    #[test]
    fn every_setting_survives_save_and_load() {
        let c = all_changed();
        assert_ne!(c, Config::default());
        let path = c.try_save().expect("save");
        assert!(!path.with_extension("ini.tmp").exists(), "temp file left behind");
        assert_eq!(Config::load(), c);
        // and back to the defaults (every bool flips the other way)
        let d = Config::default();
        d.try_save().expect("save defaults");
        let back = Config::load();
        assert_eq!(back, d);
        let _ = std::fs::remove_file(path);
    }
}
