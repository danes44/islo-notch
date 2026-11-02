//! A tiny Win32 backend for Slint.
//!
//! Why not Slint's default (winit + OpenGL)? Loading a GPU driver into the process
//! costs tens of MB on its own. Islo instead renders with Slint's CPU renderer
//! straight into a GDI bitmap and hands it to Windows:
//!
//! * the island is a *layered* window (`UpdateLayeredWindow` with per-pixel alpha),
//!   so the rounded pill is anti-aliased, everything outside it is see-through, and
//!   fully transparent pixels let clicks fall through to whatever is underneath;
//! * the settings window is an ordinary window that blits the same kind of bitmap.
//!
//! The event loop sleeps in `MsgWaitForMultipleObjectsEx` when nothing moves
//! (0 % CPU), and while something animates it paces frames with `DwmFlush`, which
//! returns once per compositor frame, i.e. in sync with the display refresh rate.
//!
//! Rust notes for a TS developer:
//! * `Rc<T>` is a reference-counted pointer (single thread), `Cell`/`RefCell` give
//!   "interior mutability": changing data behind a shared `&` reference. `RefCell`
//!   checks borrows at runtime and panics on overlap, so we keep borrows short.
//! * `unsafe { }` marks calls the compiler can't verify (raw Win32 FFI).

use slint::platform::software_renderer::{PremultipliedRgbaColor, RepaintBufferType, SoftwareRenderer, TargetPixel};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, LogicalSize, PhysicalSize, PlatformError, Window, WindowSize};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::INFINITE;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PCWSTR, w};

pub const WM_APP_WAKE: u32 = WM_APP + 1;
pub const WM_APP_QUIT: u32 = WM_APP + 2;

/// What kind of native window the next Slint component should get.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Click-through, never-focused, always-on-top, per-pixel-alpha (the island).
    Island,
    /// A normal decorated window (settings).
    Normal,
}

type Job = Box<dyn FnOnce() + Send>;
type HostHandler = Box<dyn FnMut(HWND, u32, WPARAM, LPARAM) -> Option<LRESULT>>;

thread_local! {
    static NEXT_KIND: Cell<Kind> = const { Cell::new(Kind::Normal) };
    static WINDOWS: RefCell<Vec<Rc<WinWindow>>> = const { RefCell::new(Vec::new()) };
    static FRAME_HOOK: RefCell<Option<Box<dyn FnMut() -> bool>>> = const { RefCell::new(None) };
    static HOST_HANDLER: RefCell<Option<HostHandler>> = const { RefCell::new(None) };
    static HOST: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static POINTER_HOOK: RefCell<Option<Rc<dyn Fn(HWND, Option<LogicalPosition>)>>> = const { RefCell::new(None) };
}

/// Choose the window kind for the next component created with `Component::new()`.
pub fn set_next_kind(kind: Kind) {
    NEXT_KIND.with(|k| k.set(kind));
}

/// Called once per loop iteration, after timers/animations ran. Return `true`
/// while you are animating something yourself (e.g. springs) to keep frames coming.
pub fn set_frame_hook(f: impl FnMut() -> bool + 'static) {
    FRAME_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

/// Messages for the hidden host window (tray, power, display changes, ...).
pub fn set_host_handler(f: impl FnMut(HWND, u32, WPARAM, LPARAM) -> Option<LRESULT> + 'static) {
    HOST_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

/// Raw pointer position over the island (logical px), `None` when it leaves.
/// The app uses this for hover so it doesn't depend on which Slint element is on top.
pub fn set_pointer_hook(f: impl Fn(HWND, Option<LogicalPosition>) + 'static) {
    POINTER_HOOK.with(|h| *h.borrow_mut() = Some(Rc::new(f)));
}

fn pointer_hook(hwnd: HWND, p: Option<LogicalPosition>) {
    // Clone the Rc out first so the hook may itself touch POINTER_HOOK safely.
    if let Some(f) = POINTER_HOOK.with(|h| h.borrow().clone()) {
        f(hwnd, p);
    }
}

pub fn host_hwnd() -> HWND {
    HOST.with(|h| h.get())
}

/// Find our native window behind a Slint `Window`.
pub fn native(window: &Window) -> Option<Rc<WinWindow>> {
    WINDOWS.with(|ws| ws.borrow().iter().find(|w| std::ptr::eq(&w.window, window)).cloned())
}

/// Drop our reference to a window (the HWND is destroyed once Slint lets go too).
pub fn forget(window: &Window) {
    WINDOWS.with(|ws| ws.borrow_mut().retain(|w| !std::ptr::eq(&w.window, window)));
}

fn find(hwnd: HWND) -> Option<Rc<WinWindow>> {
    WINDOWS.with(|ws| ws.borrow().iter().find(|w| w.hwnd.get() == hwnd).cloned())
}

// ---------------------------------------------------------------------------
// Pixel format: GDI wants premultiplied BGRA, so we let Slint render straight
// into that layout. No conversion pass, no second buffer.
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Bgra {
    b: u8,
    g: u8,
    r: u8,
    a: u8,
}

impl TargetPixel for Bgra {
    fn blend(&mut self, c: PremultipliedRgbaColor) {
        let ia = (255 - c.alpha) as u16;
        self.r = (self.r as u16 * ia / 255) as u8 + c.red;
        self.g = (self.g as u16 * ia / 255) as u8 + c.green;
        self.b = (self.b as u16 * ia / 255) as u8 + c.blue;
        self.a = (self.a as u16 + c.alpha as u16 - (self.a as u16 * c.alpha as u16) / 255) as u8;
    }
    fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Bgra { b, g, r, a: 255 }
    }
    fn background() -> Self {
        Bgra::default() // fully transparent
    }
}

/// A 32-bit top-down DIB selected into a memory DC.
struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut Bgra,
    w: u32,
    h: u32,
}

impl Surface {
    fn new(w: u32, h: u32) -> Option<Surface> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32); // negative = top-down rows
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let old = SelectObject(dc, bmp.into());
            Some(Surface { dc, bmp, old, bits: bits as *mut Bgra, w, h })
        }
    }
    fn pixels(&mut self) -> &mut [Bgra] {
        // SAFETY: CreateDIBSection allocated exactly w*h 32-bit pixels for us.
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

// ---------------------------------------------------------------------------
// The window adapter: one per Slint window.
// ---------------------------------------------------------------------------
pub struct WinWindow {
    window: Window,
    renderer: SoftwareRenderer,
    pub kind: Kind,
    hwnd: Cell<HWND>,
    size: Cell<PhysicalSize>,
    pos: Cell<(i32, i32)>,
    visible: Cell<bool>,
    needs_redraw: Cell<bool>,
    surface: RefCell<Option<Surface>>,
    /// Monitor DPI scale (1.0 = 96 dpi).
    dpi_scale: Cell<f32>,
    /// Extra user zoom (island size setting). Final Slint scale = dpi * user.
    user_scale: Cell<f32>,
    tracking_mouse: Cell<bool>,
    preferred: Cell<LogicalSize>,
    sized_once: Cell<bool>,
    /// Position and size of the last full upload, so later frames can send only the dirty rect.
    presented: Cell<Option<(i32, i32, u32, u32)>>,
    last_title: RefCell<String>,
}

impl WinWindow {
    fn new(kind: Kind) -> Result<Rc<Self>, PlatformError> {
        let ww = Rc::new_cyclic(|weak| WinWindow {
            window: Window::new(weak.clone() as std::rc::Weak<dyn WindowAdapter>),
            renderer: SoftwareRenderer::new_with_repaint_buffer_type(RepaintBufferType::ReusedBuffer),
            kind,
            hwnd: Cell::new(HWND::default()),
            size: Cell::new(PhysicalSize::new(0, 0)),
            pos: Cell::new((0, 0)),
            visible: Cell::new(false),
            needs_redraw: Cell::new(true),
            surface: RefCell::new(None),
            dpi_scale: Cell::new(1.0),
            user_scale: Cell::new(1.0),
            tracking_mouse: Cell::new(false),
            preferred: Cell::new(LogicalSize::new(480.0, 640.0)),
            sized_once: Cell::new(false),
            presented: Cell::new(None),
            last_title: RefCell::new(String::new()),
        });
        let hwnd = unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| PlatformError::Other(e.to_string()))?;
            let (ex, style, class) = match kind {
                Kind::Island => (
                    WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    WS_POPUP,
                    w!("IsloIsland"),
                ),
                Kind::Normal => (WINDOW_EX_STYLE(0), WS_OVERLAPPEDWINDOW, w!("IsloWindow")),
            };
            CreateWindowExW(ex, class, w!("Islo"), style, CW_USEDEFAULT, CW_USEDEFAULT, 100, 100, None, None, Some(hinst.into()), None)
                .map_err(|e| PlatformError::Other(e.to_string()))?
        };
        ww.hwnd.set(hwnd);
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        set_window_icons(hwnd, dpi);
        ww.dpi_scale.set(dpi as f32 / 96.0);
        ww.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: ww.scale() });
        if kind == Kind::Island {
            // Never animate the window itself (we animate the pixels) and never
            // let DWM add its own transition.
            let off = BOOL(1);
            unsafe {
                let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED, &off as *const _ as _, 4);
            }
        }
        Ok(ww)
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd.get()
    }

    /// Top-left of the window on screen (physical px).
    pub fn pos(&self) -> (i32, i32) {
        self.pos.get()
    }

    pub fn scale(&self) -> f32 {
        self.dpi_scale.get() * self.user_scale.get()
    }

    pub fn set_scales(&self, dpi_scale: f32, user_scale: f32) {
        let old = self.scale();
        self.dpi_scale.set(dpi_scale);
        self.user_scale.set(user_scale);
        if (old - self.scale()).abs() > f32::EPSILON {
            self.window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: self.scale() });
            self.needs_redraw.set(true);
        }
    }

    /// Island only: place the layered window at a physical position/size.
    pub fn place(&self, x: i32, y: i32, w: u32, h: u32) {
        self.pos.set((x, y));
        if self.size.get() != PhysicalSize::new(w, h) {
            self.resize_surface(w, h);
        }
        self.needs_redraw.set(true);
    }

    fn resize_surface(&self, w: u32, h: u32) {
        let w = w.max(1);
        let h = h.max(1);
        self.size.set(PhysicalSize::new(w, h));
        *self.surface.borrow_mut() = Surface::new(w, h);
        self.presented.set(None);
        // Fresh bitmap: make Slint repaint everything once.
        self.renderer.set_repaint_buffer_type(RepaintBufferType::ReusedBuffer);
        let s = self.scale();
        self.window.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(w as f32 / s, h as f32 / s) });
        self.needs_redraw.set(true);
    }

    fn logical(&self, lp: LPARAM) -> LogicalPosition {
        let x = (lp.0 & 0xffff) as i16 as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32;
        let s = self.scale();
        LogicalPosition::new(x / s, y / s)
    }

    /// Render if something changed, then hand the pixels to Windows.
    fn render_and_present(&self) {
        if !self.visible.get() || !self.needs_redraw.replace(false) {
            return;
        }
        let mut guard = self.surface.borrow_mut();
        let Some(surface) = guard.as_mut() else { return };
        let stride = surface.w as usize;
        let region = self.renderer.render(surface.pixels(), stride);
        unsafe {
            match self.kind {
                Kind::Island => {
                    let (x, y) = self.pos.get();
                    let now = (x, y, surface.w, surface.h);
                    let same_place = self.presented.get() == Some(now);
                    let o = region.bounding_box_origin();
                    let sz_dirty = region.bounding_box_size();
                    if same_place && (sz_dirty.width == 0 || sz_dirty.height == 0) {
                        return; // nothing visible changed
                    }
                    let pt = POINT { x, y };
                    let sz = SIZE { cx: surface.w as i32, cy: surface.h as i32 };
                    let src = POINT { x: 0, y: 0 };
                    let blend = BLENDFUNCTION {
                        BlendOp: AC_SRC_OVER as u8,
                        BlendFlags: 0,
                        SourceConstantAlpha: 255,
                        AlphaFormat: AC_SRC_ALPHA as u8,
                    };
                    // Only the changed rectangle is sent to the compositor when the
                    // window didn't move: small animations (equalizer bars) stay cheap.
                    let dirty = RECT {
                        left: o.x,
                        top: o.y,
                        right: o.x + sz_dirty.width as i32,
                        bottom: o.y + sz_dirty.height as i32,
                    };
                    let info = UPDATELAYEREDWINDOWINFO {
                        cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                        hdcDst: HDC::default(),
                        pptDst: &pt,
                        psize: &sz,
                        hdcSrc: surface.dc,
                        pptSrc: &src,
                        crKey: COLORREF(0),
                        pblend: &blend,
                        dwFlags: ULW_ALPHA,
                        prcDirty: if same_place { &dirty } else { std::ptr::null() },
                    };
                    if UpdateLayeredWindowIndirect(self.hwnd.get(), &info).as_bool() {
                        self.presented.set(Some(now));
                    } else {
                        self.presented.set(None);
                    }
                }
                Kind::Normal => {
                    let dc = GetDC(Some(self.hwnd.get()));
                    let _ = BitBlt(dc, 0, 0, surface.w as i32, surface.h as i32, Some(surface.dc), 0, 0, SRCCOPY);
                    ReleaseDC(Some(self.hwnd.get()), dc);
                }
            }
        }
    }

    fn handle(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let hwnd = self.hwnd.get();
        match msg {
            WM_MOUSEMOVE => {
                if !self.tracking_mouse.get() {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.tracking_mouse.set(true);
                }
                let position = self.logical(lp);
                self.window.dispatch_event(WindowEvent::PointerMoved { position });
                if self.kind == Kind::Island {
                    pointer_hook(hwnd, Some(position));
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_mouse.set(false);
                self.window.dispatch_event(WindowEvent::PointerExited);
                if self.kind == Kind::Island {
                    pointer_hook(hwnd, None);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                unsafe { SetCapture(hwnd) };
                let button = match msg {
                    WM_LBUTTONDOWN => PointerEventButton::Left,
                    WM_RBUTTONDOWN => PointerEventButton::Right,
                    _ => PointerEventButton::Middle,
                };
                self.window.dispatch_event(WindowEvent::PointerPressed { position: self.logical(lp), button });
                Some(LRESULT(0))
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
                unsafe {
                    let _ = ReleaseCapture();
                }
                let button = match msg {
                    WM_LBUTTONUP => PointerEventButton::Left,
                    WM_RBUTTONUP => PointerEventButton::Right,
                    _ => PointerEventButton::Middle,
                };
                self.window.dispatch_event(WindowEvent::PointerReleased { position: self.logical(lp), button });
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                // Wheel messages carry *screen* coordinates.
                let mut pt = POINT { x: (lp.0 & 0xffff) as i16 as i32, y: ((lp.0 >> 16) & 0xffff) as i16 as i32 };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                let s = self.scale();
                let delta = ((wp.0 >> 16) & 0xffff) as i16 as f32 / 120.0 * 60.0;
                let (dx, dy) = if msg == WM_MOUSEWHEEL { (0.0, delta) } else { (-delta, 0.0) };
                self.window.dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(pt.x as f32 / s, pt.y as f32 / s),
                    delta_x: dx,
                    delta_y: dy,
                });
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE if self.kind == Kind::Island => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_NCHITTEST if self.kind == Kind::Island => Some(LRESULT(HTCLIENT as isize)),
            // Windows minimizes the island when the taskbar is clicked; islands
            // never minimize, so put it straight back (and back on top).
            WM_SYSCOMMAND if self.kind == Kind::Island && (wp.0 & 0xfff0) as u32 == SC_MINIMIZE => Some(LRESULT(0)),
            WM_SIZE if self.kind == Kind::Island && wp.0 as u32 == SIZE_MINIMIZED => {
                unsafe {
                    let _ = ShowWindowAsync(hwnd, SW_SHOWNOACTIVATE);
                    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
                }
                self.presented.set(None); // present again at its real place
                self.needs_redraw.set(true);
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                if (lp.0 & 0xffff) as u32 == HTCLIENT {
                    unsafe {
                        if let Ok(c) = LoadCursorW(None, IDC_ARROW) {
                            SetCursor(Some(c));
                        }
                    }
                    Some(LRESULT(1))
                } else {
                    None
                }
            }
            WM_SIZE if self.kind == Kind::Normal => {
                let w = (lp.0 & 0xffff) as u32;
                let h = ((lp.0 >> 16) & 0xffff) as u32;
                if w > 0 && h > 0 && PhysicalSize::new(w, h) != self.size.get() {
                    self.resize_surface(w, h);
                    self.render_and_present();
                }
                Some(LRESULT(0))
            }
            WM_PAINT if self.kind == Kind::Normal => {
                unsafe {
                    let mut ps = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut ps);
                    if let Some(s) = self.surface.borrow().as_ref() {
                        let _ = BitBlt(dc, 0, 0, s.w as i32, s.h as i32, Some(s.dc), 0, 0, SRCCOPY);
                    }
                    let _ = EndPaint(hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_CLOSE => {
                self.window.dispatch_event(WindowEvent::CloseRequested);
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                set_window_icons(hwnd, (wp.0 & 0xffff) as u32);
                if self.kind == Kind::Normal {
                    let dpi = (wp.0 & 0xffff) as f32;
                    self.set_scales(dpi / 96.0, self.user_scale.get());
                    let r = unsafe { &*(lp.0 as *const RECT) };
                    unsafe {
                        let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
                // The island is positioned (and its DPI chosen) by the app itself.
                Some(LRESULT(0))
            }
            WM_SETFOCUS => {
                self.window.dispatch_event(WindowEvent::WindowActiveChanged(true));
                None
            }
            WM_KILLFOCUS => {
                self.window.dispatch_event(WindowEvent::WindowActiveChanged(false));
                None
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                use slint::platform::Key;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let vk = VIRTUAL_KEY(wp.0 as u16);
                let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
                // Keys without a character (text arrives separately as WM_CHAR).
                let key: Option<char> = match vk {
                    VK_TAB => Some(Key::Tab.into()),
                    VK_LEFT => Some(Key::LeftArrow.into()),
                    VK_RIGHT => Some(Key::RightArrow.into()),
                    VK_UP => Some(Key::UpArrow.into()),
                    VK_DOWN => Some(Key::DownArrow.into()),
                    VK_HOME => Some(Key::Home.into()),
                    VK_END => Some(Key::End.into()),
                    VK_RETURN => Some(Key::Return.into()),
                    VK_ESCAPE => Some(Key::Escape.into()),
                    VK_BACK => Some(Key::Backspace.into()),
                    VK_DELETE => Some(Key::Delete.into()),
                    VK_SHIFT | VK_LSHIFT | VK_RSHIFT => Some(Key::Shift.into()),
                    VK_CONTROL | VK_LCONTROL | VK_RCONTROL => Some(Key::Control.into()),
                    VK_MENU | VK_LMENU | VK_RMENU => Some(Key::Alt.into()),
                    // Ctrl+A/C/V/X/Z: WM_CHAR would deliver control codes, so send the letter here
                    v if ctrl && (0x41..=0x5A).contains(&v.0) => Some((v.0 as u8 + 32) as char),
                    _ => None,
                };
                if let Some(k) = key {
                    let text: slint::SharedString = k.into();
                    self.window.dispatch_event(if down { WindowEvent::KeyPressed { text } } else { WindowEvent::KeyReleased { text } });
                    return Some(LRESULT(0));
                }
                None
            }
            WM_CHAR => {
                // printable characters only; editing keys were handled on WM_KEYDOWN
                let c = wp.0 as u32;
                if c >= 0x20 && c != 0x7f {
                    if let Some(ch) = char::from_u32(c) {
                        let text: slint::SharedString = ch.into();
                        self.window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                        self.window.dispatch_event(WindowEvent::KeyReleased { text });
                    }
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

impl Drop for WinWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd.get());
        }
    }
}

impl WindowAdapter for WinWindow {
    fn window(&self) -> &Window {
        &self.window
    }

    fn renderer(&self) -> &dyn slint::platform::Renderer {
        &self.renderer
    }

    fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    fn set_size(&self, size: WindowSize) {
        if self.kind == Kind::Normal {
            let p = size.to_physical(self.scale());
            unsafe {
                let mut r = RECT { left: 0, top: 0, right: p.width as i32, bottom: p.height as i32 };
                let _ = AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0), GetDpiForWindow(self.hwnd.get()));
                let _ = SetWindowPos(self.hwnd.get(), None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
    }

    fn set_visible(&self, visible: bool) -> Result<(), PlatformError> {
        self.visible.set(visible);
        let hwnd = self.hwnd.get();
        unsafe {
            if !visible {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return Ok(());
            }
            match self.kind {
                Kind::Island => {
                    self.needs_redraw.set(true);
                    self.render_and_present();
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                }
                Kind::Normal => {
                    if !self.sized_once.replace(true) {
                        // First show: preferred size, centered on the work area.
                        let pref = self.preferred.get();
                        let dpi = GetDpiForWindow(hwnd).max(96);
                        let s = dpi as f32 / 96.0;
                        let mut r = RECT { left: 0, top: 0, right: (pref.width * s) as i32, bottom: (pref.height * s) as i32 };
                        let _ = AdjustWindowRectExForDpi(&mut r, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0), dpi);
                        let (w, h) = (r.right - r.left, r.bottom - r.top);
                        let mut work = RECT::default();
                        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
                        let x = work.left + ((work.right - work.left) - w) / 2;
                        let y = work.top + ((work.bottom - work.top) - h).max(0) / 2;
                        let _ = SetWindowPos(hwnd, None, x, y, w, h.min(work.bottom - work.top), SWP_NOZORDER);
                    }
                    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
                    let _ = SetForegroundWindow(hwnd);
                    self.needs_redraw.set(true);
                }
            }
        }
        Ok(())
    }

    fn request_redraw(&self) {
        self.needs_redraw.set(true);
    }

    fn update_window_properties(&self, props: slint::platform::WindowProperties<'_>) {
        if self.kind == Kind::Normal {
            let title = props.title().to_string();
            if *self.last_title.borrow() != title {
                let wide: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
                unsafe {
                    let _ = SetWindowTextW(self.hwnd.get(), PCWSTR(wide.as_ptr()));
                }
                *self.last_title.borrow_mut() = title;
            }
            let c = props.layout_constraints();
            self.preferred.set(c.preferred);
        }
    }
}

// ---------------------------------------------------------------------------
// Window procedures
// ---------------------------------------------------------------------------
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Clone the Rc and release the registry borrow before dispatching, because
    // Slint callbacks may create or show other windows (re-entrancy).
    if let Some(w) = find(hwnd) {
        if let Some(r) = w.handle(msg, wp, lp) {
            return r;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn host_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_WAKE => return LRESULT(0), // the loop drains the job queue right after
        WM_APP_QUIT => {
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        _ => {}
    }
    // Take the handler out while it runs so a nested message can't double-borrow it.
    let handler = HOST_HANDLER.with(|h| h.borrow_mut().take());
    if let Some(mut f) = handler {
        let r = f(hwnd, msg, wp, lp);
        HOST_HANDLER.with(|h| {
            let mut slot = h.borrow_mut();
            if slot.is_none() {
                *slot = Some(f);
            }
        });
        if let Some(r) = r {
            return r;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

// ---------------------------------------------------------------------------
// Platform + event loop
// ---------------------------------------------------------------------------
pub struct Win32Platform {
    jobs: Arc<Mutex<Vec<Job>>>,
    start: Instant,
}

struct Proxy {
    host: usize, // HWND as integer: raw pointers aren't `Send`, plain numbers are
    jobs: Arc<Mutex<Vec<Job>>>,
}

impl slint::platform::EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        unsafe {
            let _ = PostMessageW(Some(HWND(self.host as _)), WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Box<dyn FnOnce() + Send>) -> Result<(), slint::EventLoopError> {
        self.jobs.lock().unwrap().push(event);
        unsafe {
            let _ = PostMessageW(Some(HWND(self.host as _)), WM_APP_WAKE, WPARAM(0), LPARAM(0));
        }
        Ok(())
    }
}

/// Taskbar / Alt+Tab / title-bar icons at this window's DPI, so Windows picks
/// the right drawing from the .ico instead of scaling one size.
fn set_window_icons(hwnd: HWND, dpi: u32) {
    use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        for (which, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
            let cx = GetSystemMetricsForDpi(metric, dpi.max(96));
            if let Ok(h) = LoadImageW(Some(hinst.into()), PCWSTR(1 as _), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR | LR_SHARED) {
                SendMessageW(hwnd, WM_SETICON, Some(WPARAM(which as usize)), Some(LPARAM(h.0 as isize)));
            }
        }
    }
}

impl Win32Platform {
    pub fn new() -> Result<Self, PlatformError> {
        unsafe {
            let hinst = GetModuleHandleW(None).map_err(|e| PlatformError::Other(e.to_string()))?;
            let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
            for (name, proc_) in [
                (w!("IsloIsland"), wndproc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
                (w!("IsloWindow"), wndproc),
                (w!("IsloHost"), host_wndproc),
            ] {
                let load = |m| {
                    let cx = GetSystemMetrics(m);
                    LoadImageW(Some(hinst.into()), PCWSTR(1 as _), IMAGE_ICON, cx, cx, LR_DEFAULTCOLOR | LR_SHARED)
                        .map(|h| HICON(h.0))
                        .unwrap_or_default()
                };
                // the .ico has a separate small drawing for 16/24 px, so load each size itself
                let (icon, icon_sm) = (load(SM_CXICON), load(SM_CXSMICON));
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(proc_),
                    hInstance: hinst.into(),
                    hCursor: cursor,
                    hIcon: icon,
                    hIconSm: icon_sm,
                    lpszClassName: name,
                    ..Default::default()
                };
                RegisterClassExW(&wc);
            }
            // A hidden top-level window (not message-only: those miss broadcasts
            // like WM_DISPLAYCHANGE / WM_SETTINGCHANGE).
            let host = CreateWindowExW(WS_EX_TOOLWINDOW, w!("IsloHost"), w!("IsloHost"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst.into()), None)
                .map_err(|e| PlatformError::Other(e.to_string()))?;
            HOST.with(|h| h.set(host));
        }
        Ok(Win32Platform { jobs: Default::default(), start: Instant::now() })
    }

    fn drain_jobs(&self) {
        let jobs = std::mem::take(&mut *self.jobs.lock().unwrap());
        for job in jobs {
            job();
        }
    }
}

impl Platform for Win32Platform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let w = WinWindow::new(NEXT_KIND.with(|k| k.get()))?;
        WINDOWS.with(|ws| ws.borrow_mut().push(w.clone()));
        Ok(w)
    }

    fn duration_since_start(&self) -> core::time::Duration {
        self.start.elapsed()
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        Some(Box::new(Proxy { host: host_hwnd().0 as usize, jobs: self.jobs.clone() }))
    }

    fn click_interval(&self) -> core::time::Duration {
        core::time::Duration::from_millis(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() } as u64)
    }

    fn set_clipboard_text(&self, text: &str, clipboard: slint::platform::Clipboard) {
        if clipboard == slint::platform::Clipboard::DefaultClipboard {
            clipboard_set(text);
        }
    }

    fn clipboard_text(&self, clipboard: slint::platform::Clipboard) -> Option<String> {
        if clipboard == slint::platform::Clipboard::DefaultClipboard { clipboard_get() } else { None }
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let mut msg = MSG::default();
        loop {
            // 1. Handle all pending input/system messages.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            // 2. Work posted from other threads (media, audio, ...).
            self.drain_jobs();

            // 3. Advance Slint timers/animations, then our own springs.
            slint::platform::update_timers_and_animations();
            let springs = FRAME_HOOK.with(|h| h.borrow_mut().as_mut().map(|f| f()).unwrap_or(false));

            // 4. Paint whatever changed.
            let windows: Vec<Rc<WinWindow>> = WINDOWS.with(|ws| ws.borrow().clone());
            let mut animating = springs;
            for w in &windows {
                w.render_and_present();
                if w.visible.get() && w.window.has_active_animations() {
                    animating = true;
                }
            }

            if !self.jobs.lock().unwrap().is_empty() {
                continue;
            }

            // 5. Wait: one display frame while animating, otherwise until the next
            //    message or Slint timer. This is where idle CPU drops to zero.
            unsafe {
                if animating {
                    if DwmFlush().is_err() {
                        std::thread::sleep(std::time::Duration::from_millis(8));
                    }
                } else {
                    let timeout = slint::platform::duration_until_next_timer_update()
                        .map(|d| d.as_millis().min(u32::MAX as u128 - 1) as u32)
                        .unwrap_or(INFINITE);
                    MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Clipboard (plain Unicode text) for text fields
// ---------------------------------------------------------------------------
const CF_UNICODETEXT: u32 = 13;

pub fn clipboard_set(text: &str) {
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        if OpenClipboard(Some(host_hwnd())).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(h.0)));
            }
        }
        let _ = CloseClipboard();
    }
}

pub fn clipboard_get() -> Option<String> {
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    unsafe {
        OpenClipboard(Some(host_hwnd())).ok()?;
        let out = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let p = GlobalLock(HGLOBAL(h.0)) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}
