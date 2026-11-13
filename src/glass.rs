//! Liquid Glass: the island bends, frosts and tints the screen behind it.
//!
//! Slint's software renderer can't read what's behind a window, so we do it
//! here, in two steps:
//!
//! 1. `capture`: copy the screen under the island's window (GDI BitBlt). The copy
//!    needs CAPTUREBLT, or Windows leaves out every layered and DirectComposition
//!    window (Chrome, Electron and Store apps: you'd see the wallpaper). To not
//!    see the island itself, it's excluded from capture for just that moment
//!    (one compositor frame), so Islo still shows up in your own screenshots.
//!    The copy is frosted (blurred) once, right away.
//! 2. `Backdrop::render`: draw the pill: every pixel samples the frosted copy
//!    through a lens (pulled toward the center = magnified, and at the rim
//!    pulled in from outside = the edge bends), then gets brighter and more
//!    colorful, takes the tint, and an inner glow lights the rim.
//!
//! Cost: a capture happens only on events (the island shows, a window moves,
//! the foreground app changes), never on a timer. Rendering runs only while
//! the pill changes size, and is a few hundred thousand cheap pixel steps.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};

/// The screen behind one island window, frosted. RGB, row by row.
pub struct Backdrop {
    w: usize,
    h: usize,
    rgb: Vec<[u8; 3]>,
}

/// Frost (blur) at 100% in Settings, in logical px. The default 40% is 2.6 px
/// (the design's 3.1 / 1.7, between).
const FROST_MAX: f32 = 6.5;
/// Brightest the glass gets under white ink, as light (relative luminance):
/// white text on it is at least (1 + 0.05) / (0.16 + 0.05) = 5:1, and just
/// over 4.5:1 under the brightest pages.
const WHITE_INK_MAX: f32 = 0.16;

/// Copy the screen rectangle (physical px) without the `ours` windows, and
/// frost it by `frost` (0..1 of FROST_MAX). `scale` = DPI scale.
pub fn capture(x: i32, y: i32, w: i32, h: i32, scale: f32, frost: f32, ours: &[HWND]) -> Option<Backdrop> {
    if w <= 0 || h <= 0 {
        return None;
    }
    // Leave our windows out of the copy, wait for the compositor to show a frame
    // without them in captures (they stay on screen), copy, and put them back.
    unsafe {
        for &h in ours {
            let _ = SetWindowDisplayAffinity(h, WDA_EXCLUDEFROMCAPTURE);
        }
        let _ = DwmFlush();
    }
    let out = copy_screen(x, y, w, h);
    unsafe {
        for &h in ours {
            let _ = SetWindowDisplayAffinity(h, WDA_NONE);
        }
    }
    let mut rgb = out?;
    // three box blurs in a row look like a Gaussian blur
    let r = (FROST_MAX * frost.clamp(0.0, 1.0) * scale * 0.85).round() as usize;
    if r > 0 {
        for _ in 0..3 {
            box_blur(&mut rgb, w as usize, h as usize, r);
        }
    }
    Some(Backdrop { w: w as usize, h: h as usize, rgb })
}

/// The screen rectangle as RGB rows (GDI, layered windows included).
fn copy_screen(x: i32, y: i32, w: i32, h: i32) -> Option<Vec<[u8; 3]>> {
    let (wu, hu) = (w as usize, h as usize);
    let mut rgb = vec![[0u8; 3]; wu * hu];
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // top-down rows
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;
        let mut bits = std::ptr::null_mut();
        let ok = match CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bmp) => {
                let old = SelectObject(mem, bmp.into());
                let ok = BitBlt(mem, 0, 0, w, h, Some(screen), x, y, SRCCOPY | CAPTUREBLT).is_ok();
                if ok {
                    // SAFETY: the DIB section holds exactly w*h 32-bit BGRA pixels.
                    let src = std::slice::from_raw_parts(bits as *const [u8; 4], wu * hu);
                    for (d, s) in rgb.iter_mut().zip(src) {
                        *d = [s[2], s[1], s[0]];
                    }
                }
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                ok
            }
            Err(_) => false,
        };
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok {
            return None;
        }
    }
    Some(rgb)
}

/// One horizontal and one vertical pass of a box blur of radius `r` (running sums).
fn box_blur(px: &mut [[u8; 3]], w: usize, h: usize, r: usize) {
    let mut line = vec![[0u8; 3]; w.max(h)];
    let mut pass = |len: usize, get: &dyn Fn(usize) -> usize, px: &mut [[u8; 3]]| {
        let n = (2 * r + 1) as u32;
        let mut sum = [0u32; 3];
        let at = |i: isize| get(i.clamp(0, len as isize - 1) as usize);
        for i in -(r as isize)..=(r as isize) {
            let p = px[at(i)];
            for c in 0..3 {
                sum[c] += p[c] as u32;
            }
        }
        for i in 0..len {
            line[i] = [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8];
            let (add, sub) = (px[at(i as isize + r as isize + 1)], px[at(i as isize - r as isize)]);
            for c in 0..3 {
                sum[c] = sum[c] + add[c] as u32 - sub[c] as u32;
            }
        }
        for i in 0..len {
            px[get(i)] = line[i];
        }
    };
    for y in 0..h {
        pass(w, &|x| y * w + x, px);
    }
    for x in 0..w {
        pass(h, &|y| y * w + x, px);
    }
}

/// What a render gives back besides the image.
pub struct Rendered {
    pub image: Image,
    /// Average brightness behind the pill, 0..1 (to choose dark or white ink).
    pub luma: f32,
}

impl Backdrop {
    /// The pill at (x, y) with size w x h and corner radius `r`, all physical px
    /// inside the captured rectangle. `tint` is 0xRRGGBB (white = clear);
    /// `dark_ink`: the ink is currently dark (the glass then stays lighter).
    pub fn render(&self, x: f32, y: f32, w: f32, h: f32, r: f32, scale: f32, tint: u32, dark_ink: bool) -> Rendered {
        let (pw, ph) = (w.round().max(1.0) as usize, h.round().max(1.0) as usize);
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        // The lens grows with the pill: compact 36 px tall -> open (180 px).
        let open = ((h / scale - 36.0) / 144.0).clamp(0.0, 1.0);
        let mag = 0.10 + 0.02 * open; // pulled toward the center: magnified
        let bend = (11.0 + 9.0 * open) * scale; // how far the rim reaches outside
        let band = (14.0 + 12.0 * open) * scale; // how deep the rim bend goes
        let glow_w = (3.0 + 2.0 * open) * scale;
        let rim_w = 1.6 * scale;
        let clear = tint & 0xff_ffff == 0xff_ffff;
        let tint_rgb = [((tint >> 16) & 0xff) as f32 / 255.0, ((tint >> 8) & 0xff) as f32 / 255.0, (tint & 0xff) as f32 / 255.0];
        // Clear glass: a whisper of white, more over bright pages so dark ink reads.
        // A color: 38%, 62% over bright pages so white ink reads.
        let tint_a = match (clear, dark_ink) {
            (true, false) => 0.10,
            (true, true) => 0.32 + 0.13 * open,
            (false, _) => 0.38,
        };

        let (cx, cy) = (w / 2.0, h / 2.0);
        let (hx, hy) = (cx - r, cy - r);
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(pw as u32, ph as u32);
        let out = buf.make_mut_slice();
        let (mut luma_sum, mut lit) = (0f32, 0usize);
        for j in 0..ph {
            let ly = j as f32 + 0.5;
            let py = ly - cy;
            for i in 0..pw {
                let lx = i as f32 + 0.5;
                let px = lx - cx;
                // inside distance `d` and outward normal (nx, ny) of the rounded rectangle
                let (qx, qy) = (px.abs() - hx, py.abs() - hy);
                let (mut nx, mut ny, d) = if qx > 0.0 && qy > 0.0 {
                    let l = (qx * qx + qy * qy).sqrt().max(1e-4);
                    (qx / l, qy / l, r - l)
                } else if qx > qy {
                    (1.0, 0.0, r - qx)
                } else {
                    (0.0, 1.0, r - qy)
                };
                nx = nx.copysign(px);
                ny = ny.copysign(py);
                // Outside the rounded shape the image is see-through, with a one
                // pixel soft edge (Slint's software renderer clips children to the
                // pill's box, not to its rounded corners).
                let cover = (d + 0.5).clamp(0.0, 1.0);
                if cover <= 0.0 {
                    out[j * pw + i] = Rgba8Pixel { r: 0, g: 0, b: 0, a: 0 };
                    continue;
                }
                let t = (1.0 - d.max(0.0) / band).max(0.0);
                let e = bend * t * t * (3.0 - 2.0 * t); // smoothstep: strongest at the rim
                let sx = x + lx - px * mag + nx * e;
                let sy = y + ly - py * mag + ny * e;
                let [mut cr, mut cg, mut cb] = self.sample(sx, sy);
                let l = 0.2126 * cr + 0.7152 * cg + 0.0722 * cb;
                luma_sum += l;
                lit += 1;
                // the colors behind light the glass: brighter, more colorful
                cr = (l + (cr - l) * 1.8) * 1.15;
                cg = (l + (cg - l) * 1.8) * 1.15;
                cb = (l + (cb - l) * 1.8) * 1.15;
                // tint
                cr += (tint_rgb[0] - cr) * tint_a;
                cg += (tint_rgb[1] - cg) * tint_a;
                cb += (tint_rgb[2] - cb) * tint_a;
                // White ink: bright pages behind are toned down (a soft knee above
                // 45% brightness), so white text and controls stay readable over
                // a white page while dark areas keep their clear glass look.
                // The cap is on light (not on the gamma value), so a bright saturated
                // color such as yellow is held down too: white on it keeps about
                // 4.5:1. Squares stand in for the 2.2 gamma (cheap, close enough).
                if !dark_ink {
                    (cr, cg, cb) = (cr.clamp(0.0, 1.0), cg.clamp(0.0, 1.0), cb.clamp(0.0, 1.0));
                    let lin = 0.2126 * cr * cr + 0.7152 * cg * cg + 0.0722 * cb * cb;
                    if lin > WHITE_INK_MAX {
                        let k = ((WHITE_INK_MAX + (lin - WHITE_INK_MAX) * 0.02) / lin).sqrt();
                        cr *= k;
                        cg *= k;
                        cb *= k;
                    }
                }
                // inner glow, and a rim that's brightest top-left and bottom-right
                let mut glow = 0.35 * (-d.max(0.0) / glow_w).exp();
                if d < rim_w {
                    let tl = (-(nx + ny) * std::f32::consts::FRAC_1_SQRT_2).max(0.0);
                    let br = ((nx + ny) * std::f32::consts::FRAC_1_SQRT_2).max(0.0);
                    glow += (0.55 + 0.4 * tl + 0.1 * br) * (1.0 - d.max(0.0) / rim_w);
                }
                let glow = glow.min(1.0);
                let to8 = |c: f32| ((c + (1.0 - c) * glow).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[j * pw + i] = Rgba8Pixel { r: to8(cr), g: to8(cg), b: to8(cb), a: (cover * 255.0 + 0.5) as u8 };
            }
        }
        Rendered { image: Image::from_rgba8(buf), luma: luma_sum / lit.max(1) as f32 }
    }

    /// Bilinear sample at (x, y), clamped to the captured rectangle; 0..1 RGB.
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let x = (x - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (y - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let p = |x: usize, y: usize| self.rgb[y * self.w + x];
        let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
        let mut o = [0f32; 3];
        for k in 0..3 {
            let top = a[k] as f32 + (b[k] as f32 - a[k] as f32) * fx;
            let bot = c[k] as f32 + (d[k] as f32 - c[k] as f32) * fx;
            o[k] = (top + (bot - top) * fy) / 255.0;
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: usize, h: usize, c: [u8; 3]) -> Backdrop {
        Backdrop { w, h, rgb: vec![c; w * h] }
    }

    #[test]
    fn blur_keeps_a_flat_color() {
        let mut px = vec![[200u8, 100, 50]; 40 * 30];
        box_blur(&mut px, 40, 30, 4);
        assert!(px.iter().all(|p| *p == [200, 100, 50]));
    }

    #[test]
    fn render_is_the_pill_size_and_reads_brightness() {
        let white = flat(300, 120, [255, 255, 255]);
        let out = white.render(40.0, 20.0, 220.0, 36.0, 18.0, 1.0, 0xffffff, false);
        assert_eq!((out.image.size().width, out.image.size().height), (220, 36));
        assert!(out.luma > 0.99);
        let black = flat(300, 120, [0, 0, 0]);
        assert!(black.render(40.0, 20.0, 220.0, 36.0, 18.0, 1.0, 0xffffff, false).luma < 0.01);
    }

    #[test]
    fn rim_bends_in_from_outside() {
        // left half red, right half blue: near the pill's left rim the lens
        // reaches outside the pill, so a pill starting just right of the split
        // still shows red at its left edge. (A short pill: on a long one the
        // pull toward the center outweighs the rim at its far ends.)
        let mut b = flat(400, 100, [0, 0, 255]);
        for y in 0..100 {
            for x in 0..100 {
                b.rgb[y * 400 + x] = [255, 0, 0];
            }
        }
        let out = b.render(102.0, 30.0, 40.0, 36.0, 18.0, 1.0, 0xffffff, false);
        let buf = out.image.to_rgba8().expect("rgba");
        let mid = buf.as_slice()[18 * 40 + 3];
        assert!(mid.r > mid.b, "left rim should bend red in from outside: {mid:?}");
    }

    /// White ink stays readable over bright pages of any color, tinted glass too.
    #[test]
    fn white_ink_keeps_its_contrast() {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        for (bg, tint) in [([255, 255, 255], 0x4f7cff), ([255, 255, 0], 0xffffff), ([0, 255, 255], 0xffffff), ([255, 120, 200], 0xffffff), ([120, 255, 120], 0xfc90bc)] {
            let out = flat(300, 120, bg).render(40.0, 20.0, 220.0, 36.0, 18.0, 1.0, tint, false);
            let buf = out.image.to_rgba8().expect("rgba");
            let p = buf.as_slice()[18 * 220 + 110];
            let l = 0.2126 * lin(p.r) + 0.7152 * lin(p.g) + 0.0722 * lin(p.b);
            let contrast = 1.05 / (l + 0.05);
            assert!(contrast >= 4.5, "white on glass over {bg:?} (tint {tint:06x}): {contrast:.2}");
        }
    }

    #[test]
    fn corners_outside_the_pill_are_clear() {
        let out = flat(300, 120, [40, 40, 40]).render(40.0, 20.0, 220.0, 36.0, 18.0, 1.0, 0xffffff, false);
        let buf = out.image.to_rgba8().expect("rgba");
        let px = buf.as_slice();
        assert_eq!(px[0].a, 0, "top-left corner");
        assert_eq!(px[219].a, 0, "top-right corner");
        assert_eq!(px[35 * 220].a, 0, "bottom-left corner");
        assert_eq!(px[18 * 220 + 110].a, 255, "middle");
    }
}
