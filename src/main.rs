//! Memrium Term: a terminal emulator with a Jelly Glass look.
//!
//! A Chebrium program (`chebrium::app`) around alacritty_terminal. The window draws its own chrome: traffic lights,
//! tabs and the "+" button live in the same translucent toolbar as the content, so there is no separate system
//! titlebar. The terminal paints its window itself and hands it to the runtime as translucent pictures, one band for
//! the toolbar and one per text row: a row that did not change keeps its picture, so only the rows that moved are damaged.

mod fonts;
mod keys;
mod tab;
mod theme;

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb as VteRgb};
use chebrium::canvas::{Canvas, Rgb};
use chebrium::lucide;
use fonts::{Fonts, Style};
use chebrium::appmenu::{Item, Menu};
use chebrium::app::{self, Config, Decorations, Host, Inbox, KeyEvent, Keysym, Modifiers, Outbox, PointerEvt, PointerKind, Waker};
use chebrium::picture::Pic;
use chebrium::spring::Spring;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tab::{AppEvent, Tab};

const TOOLBAR: f32 = 34.0;
/// the cursor's spring: stiff enough to keep up with typing, loose enough to overshoot a little
const CURSOR_K: f32 = 650.0;
const CURSOR_ZETA: f32 = 0.7;
const PAD_X: f32 = 12.0;
const PAD_TOP: f32 = 6.0;
const PAD_BOTTOM: f32 = 10.0;
const FONT_PX: f32 = 13.0;
const UI_PX: f32 = 12.5;
const TITLE_PX: f32 = 14.0;
const RADIUS: f32 = 18.0;
/// default window size in cells (like Terminal.app)
const DEFAULT_COLS: f32 = 80.0;
const DEFAULT_ROWS: f32 = 25.0;
/// default body opacity (override with MEMRIUM_TERM_OPACITY, 0.3..1.0): lower lets the blurred desktop through
const DEFAULT_ALPHA: f32 = 0.94;
const SEL_BG: Rgb = [0x2c, 0x62, 0xb8];
const BTN_LEFT: u32 = 0x110;
const BTN_MIDDLE: u32 = 0x112;

// ------------------------------------------------------------------ layout (logical px)

fn plus_rect(w: f32) -> (f32, f32, f32, f32) {
    (w - 12.0 - 26.0, (TOOLBAR - 26.0) / 2.0, 26.0, 26.0)
}

fn tab_rects(w: f32, n: usize) -> Vec<(f32, f32, f32, f32)> {
    if n < 2 {
        return Vec::new();
    }
    let (x0, gap) = (92.0, 6.0);
    let avail = w - 52.0 - x0;
    let tw = ((avail - gap * (n as f32 - 1.0)) / n as f32).clamp(70.0, 210.0);
    (0..n).map(|i| (x0 + i as f32 * (tw + gap), (TOOLBAR - 24.0) / 2.0, tw, 24.0)).collect()
}

fn light_center(i: usize) -> (f32, f32) {
    (16.0 + i as f32 * 23.0, TOOLBAR / 2.0)
}

fn inside(p: (f32, f32), r: (f32, f32, f32, f32)) -> bool {
    p.0 >= r.0 && p.0 <= r.0 + r.2 && p.1 >= r.1 && p.1 <= r.1 + r.3
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hover {
    None,
    Lights,
    Tab(usize),
    TabClose(usize),
    Plus,
}

// ------------------------------------------------------------------ app

struct Term {
    /// what the terminal's threads send (output, title changes, exit), and the way to send it
    inbox: Inbox<AppEvent>,
    tx: Outbox<AppEvent>,

    width: u32,
    height: u32,
    scale: f32,
    configured: bool,
    activated: bool,
    exit: bool,
    dirty: bool,

    mods: Modifiers,
    last_key_serial: u32,

    fonts: Fonts,
    icons: Icons,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,

    mouse: (f32, f32),
    hover: Hover,
    selecting: bool,
    buttons: u8,
    last_click: Option<(Instant, Point, u8)>,
    scroll_acc: f64,

    alpha: f32,
    /// requests for the runtime, handed over after each event: a new title, the window to be dragged, the clipboard
    want_title: Option<String>,
    want_drag: bool,
    /// where a press on the empty toolbar began: dragging from there moves the window
    press_at: Option<(f32, f32)>,
    want_copy: Option<String>,
    want_paste: bool,
    /// the window's pixels (physical, premultiplied) and the bands they are cut into
    buf: Vec<u8>,
    bands: Vec<Band>,
    /// what the bands were laid out for: size in pixels, grid, scale, opacity
    layout: Option<(usize, usize, usize, usize, u32, u32)>,
    /// the text cursor floats over the rows on springs (logical px of its top left corner), so it glides between cells
    cur_x: Spring,
    cur_y: Spring,
    /// the cursor was drawn last frame (it snaps to its place when it appears) and in which tab and layout
    cur_seen: Option<(usize, Option<(usize, usize, usize, usize, u32, u32)>)>,
    cur_pic: Option<(CursorKey, Arc<Pic>)>,
}

/// What a cursor picture was drawn from: shape, focus, the character under a block, wide cell, cell size.
type CursorKey = (u8, bool, char, bool, u32, u32);

/// The text cursor of the active tab: its cell (physical px), shape and the character under it.
struct Cur {
    x: f32,
    y: f32,
    w: f32,
    shape: CursorShape,
    c: char,
    wide: bool,
}

/// A horizontal strip of the window (the toolbar, a text row, the padding below) with the picture it was last painted as.
struct Band {
    y0: i32,
    y1: i32,
    /// what the strip was painted from; `u64::MAX` when it must be painted again
    hash: u64,
    pic: Option<Arc<Pic>>,
}

impl Term {
    // ---- geometry

    fn cell(&mut self) -> (f32, f32, f32) {
        self.fonts.cell_metrics(FONT_PX * self.scale)
    }

    fn grid_dims(&mut self) -> (usize, usize) {
        let s = self.scale;
        let (cw, ch, _) = self.cell();
        let usable_w = self.width as f32 * s - 2.0 * PAD_X * s;
        let usable_h = self.height as f32 * s - (TOOLBAR + PAD_TOP + PAD_BOTTOM) * s;
        (((usable_w / cw) as usize).max(2), ((usable_h / ch) as usize).max(1))
    }

    fn cell_px(&mut self) -> (u16, u16) {
        let (cw, ch, _) = self.cell();
        (cw as u16, ch as u16)
    }

    fn relayout(&mut self) {
        let (cols, rows) = self.grid_dims();
        let cell = self.cell_px();
        for t in &self.tabs {
            t.resize(cols, rows, cell);
        }
        self.schedule();
    }

    // ---- tabs

    fn new_tab(&mut self) {
        let (cols, rows) = self.grid_dims();
        let cell = self.cell_px();
        self.next_id += 1;
        // a new tab opens where the one in front is
        let cwd = self.tabs.get(self.active).and_then(|t| t.cwd());
        match Tab::spawn(self.next_id, cols, rows, cell, self.tx.clone(), cwd) {
            Ok(t) => {
                self.tabs.push(t);
                self.active = self.tabs.len() - 1;
                self.update_title();
                self.schedule();
            }
            Err(e) => {
                eprintln!("could not start a shell: {e}");
                if self.tabs.is_empty() {
                    self.exit = true;
                }
            }
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.exit = true;
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.update_title();
        self.schedule();
    }

    fn update_title(&mut self) {
        if let Some(t) = self.tabs.get(self.active) {
            self.want_title = Some(t.title.clone());
        }
    }

    // ---- events from PTY threads

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(id, ev) => {
                let Some(idx) = self.tabs.iter().position(|t| t.id == id) else { return };
                match ev {
                    TermEvent::Title(t) => {
                        self.tabs[idx].title = if t.is_empty() { "Terminal".into() } else { t };
                        if idx == self.active {
                            self.update_title();
                        }
                    }
                    TermEvent::ResetTitle => self.tabs[idx].title = "Terminal".into(),
                    TermEvent::PtyWrite(s) => self.tabs[idx].write(s.into_bytes()),
                    TermEvent::ClipboardStore(_, text) => self.set_clipboard(text, self.last_key_serial),
                    TermEvent::ColorRequest(i, f) => {
                        let c = if i == 256 {
                            theme::FG
                        } else if i == 257 {
                            theme::BG
                        } else {
                            theme::resolve(Color::Indexed(i as u8), &Default::default())
                        };
                        self.tabs[idx].write(f(VteRgb { r: c[0], g: c[1], b: c[2] }).into_bytes());
                    }
                    TermEvent::TextAreaSizeRequest(f) => {
                        let (cols, rows) = self.grid_dims();
                        let (cw, ch) = self.cell_px();
                        let size = alacritty_terminal::event::WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: cw, cell_height: ch };
                        self.tabs[idx].write(f(size).into_bytes());
                    }
                    TermEvent::Exit | TermEvent::ChildExit(_) => {
                        self.close_tab(idx);
                        return;
                    }
                    _ => {}
                }
                self.schedule();
            }
        }
    }

    // ---- clipboard

    fn set_clipboard(&mut self, text: String, _serial: u32) {
        self.want_copy = Some(text);
    }

    fn copy_selection(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let text = tab.term.lock().selection_to_string();
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.want_copy = Some(text);
        }
    }

    /// Ask for the clipboard; the text arrives in `clipboard` below.
    fn paste(&mut self) {
        self.want_paste = true;
    }

    fn write_paste(&mut self, text: &str) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let bracketed = tab.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            let filtered: String = text.chars().filter(|c| *c != '\x1b').collect();
            tab.write(format!("\x1b[200~{filtered}\x1b[201~").into_bytes());
        } else {
            tab.write(text.replace("\r\n", "\r").replace('\n', "\r").into_bytes());
        }
        tab.term.lock().scroll_display(Scroll::Bottom);
        self.schedule();
    }

    // ---- keyboard

    fn on_key(&mut self, event: KeyEvent) {
        let m = self.mods;
        let cmd = m.logo || (m.ctrl && m.shift);
        if cmd {
            match event.keysym {
                Keysym::C | Keysym::c => return self.copy_selection(),
                Keysym::V | Keysym::v => return self.paste(),
                Keysym::T | Keysym::t => return self.new_tab(),
                Keysym::W | Keysym::w => {
                    let i = self.active;
                    return self.close_tab(i);
                }
                _ => {}
            }
        }
        if m.ctrl && !m.alt {
            if event.keysym == Keysym::Tab && !m.shift && self.tabs.len() > 1 {
                self.active = (self.active + 1) % self.tabs.len();
                self.update_title();
                return self.schedule();
            }
            if event.keysym == Keysym::ISO_Left_Tab && self.tabs.len() > 1 {
                self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
                self.update_title();
                return self.schedule();
            }
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        if m.shift && (event.keysym == Keysym::Page_Up || event.keysym == Keysym::Page_Down) {
            let s = if event.keysym == Keysym::Page_Up { Scroll::PageUp } else { Scroll::PageDown };
            tab.term.lock().scroll_display(s);
            return self.schedule();
        }
        let mode = *tab.term.lock().mode();
        if let Some(bytes) = keys::encode(&event, &m, mode) {
            tab.write(bytes);
            tab.term.lock().scroll_display(Scroll::Bottom);
            self.schedule();
        }
    }

    // ---- pointer helpers

    fn hover_at(&self, p: (f32, f32)) -> Hover {
        let w = self.width as f32;
        if p.1 > TOOLBAR {
            return Hover::None;
        }
        if (0..3).any(|i| {
            let c = light_center(i);
            (p.0 - c.0).hypot(p.1 - c.1) <= 9.0
        }) {
            return Hover::Lights;
        }
        if inside(p, plus_rect(w)) {
            return Hover::Plus;
        }
        for (i, r) in tab_rects(w, self.tabs.len()).into_iter().enumerate() {
            if inside(p, r) {
                return if p.0 < r.0 + 26.0 { Hover::TabClose(i) } else { Hover::Tab(i) };
            }
        }
        Hover::None
    }

    /// Terminal cell under a logical position (physical-pixel based), with the side of the cell.
    fn cell_at(&mut self, p: (f32, f32)) -> Option<(Point, Side, usize, usize)> {
        let s = self.scale;
        let (cw, ch, _) = self.cell();
        let (cols, rows) = self.grid_dims();
        let x = p.0 * s - PAD_X * s;
        let y = p.1 * s - (TOOLBAR + PAD_TOP) * s;
        if y < 0.0 {
            return None;
        }
        let col = ((x / cw).floor().max(0.0) as usize).min(cols - 1);
        let row = ((y / ch).floor().max(0.0) as usize).min(rows - 1);
        let side = if (x / cw).fract() < 0.5 { Side::Left } else { Side::Right };
        let off = self.tabs.get(self.active).map(|t| t.term.lock().grid().display_offset()).unwrap_or(0) as i32;
        Some((Point::new(Line(row as i32 - off), Column(col)), side, col, row))
    }

    fn report_mouse(&mut self, code: u8, col: usize, row: usize, press: bool) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let sgr = tab.term.lock().mode().contains(TermMode::SGR_MOUSE);
        let (c, r) = (col + 1, row + 1);
        if sgr {
            tab.write(format!("\x1b[<{code};{c};{r}{}", if press { 'M' } else { 'm' }).into_bytes());
        } else {
            let b = if press { code } else { 3 };
            let mut v = b"\x1b[M".to_vec();
            v.extend([32 + b, (32 + c.min(222)) as u8, (32 + r.min(222)) as u8]);
            tab.write(v);
        }
    }

    fn mouse_mode(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|t| t.term.lock().mode().intersects(TermMode::MOUSE_MODE)) && !self.mods.shift
    }

    fn press(&mut self, button: u32, serial: u32) {
        let p = self.mouse;
        if p.1 <= TOOLBAR {
            if button != BTN_LEFT {
                return;
            }
            match self.hover_at(p) {
                Hover::Lights => {
                    // only the red light is wired up: it closes the window
                    if (p.0 - light_center(0).0).abs() <= 9.0 {
                        self.exit = true;
                    }
                }
                Hover::Plus => self.new_tab(),
                Hover::Tab(i) => {
                    self.active = i;
                    self.update_title();
                    self.schedule();
                }
                Hover::TabClose(i) => self.close_tab(i),
                Hover::None => {
                    // the window moves once the pointer really travels, so a click never shifts it
                    self.press_at = Some(p);
                    let _ = serial;
                }
            }
            return;
        }
        let Some((point, side, col, row)) = self.cell_at(p) else { return };
        if self.mouse_mode() {
            let code = match button {
                BTN_LEFT => 0,
                BTN_MIDDLE => 1,
                _ => 2,
            };
            self.report_mouse(code, col, row, true);
            return;
        }
        match button {
            BTN_LEFT => {
                let now = Instant::now();
                let count = match self.last_click {
                    Some((t, pt, n)) if now.duration_since(t) < Duration::from_millis(400) && pt == point => (n % 3) + 1,
                    _ => 1,
                };
                self.last_click = Some((now, point, count));
                let ty = match count {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                if let Some(tab) = self.tabs.get(self.active) {
                    tab.term.lock().selection = Some(Selection::new(ty, point, side));
                }
                self.selecting = true;
                self.schedule();
            }
            BTN_MIDDLE => self.paste(),
            _ => {}
        }
    }

    fn release(&mut self, button: u32) {
        self.press_at = None;
        let p = self.mouse;
        if self.mouse_mode() {
            if let Some((_, _, col, row)) = self.cell_at(p) {
                let code = match button {
                    BTN_LEFT => 0,
                    BTN_MIDDLE => 1,
                    _ => 2,
                };
                self.report_mouse(code, col, row, false);
            }
            return;
        }
        if button == BTN_LEFT && self.selecting {
            self.selecting = false;
            if let Some(tab) = self.tabs.get(self.active) {
                let mut term = tab.term.lock();
                // a plain click without drag selects nothing
                if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                    term.selection = None;
                }
            }
            self.schedule();
        }
    }

    fn motion(&mut self, p: (f32, f32)) {
        self.mouse = p;
        if let Some(a) = self.press_at {
            if self.buttons == 0 {
                self.press_at = None;
            } else if (p.0 - a.0).hypot(p.1 - a.1) > 4.0 {
                self.press_at = None;
                self.want_drag = true;
            }
        }
        let h = self.hover_at(p);
        if h != self.hover {
            self.hover = h;
            self.schedule();
        }
        if p.1 <= TOOLBAR {
            return;
        }
        let Some((point, side, col, row)) = self.cell_at(p) else { return };
        if self.mouse_mode() {
            let mode = self.tabs.get(self.active).map(|t| *t.term.lock().mode()).unwrap_or(TermMode::NONE);
            let drag = mode.contains(TermMode::MOUSE_DRAG) && self.buttons != 0;
            if drag || mode.contains(TermMode::MOUSE_MOTION) {
                self.report_mouse(32 + if self.buttons != 0 { 0 } else { 3 }, col, row, true);
            }
            return;
        }
        if self.selecting {
            if let Some(tab) = self.tabs.get(self.active) {
                if let Some(sel) = tab.term.lock().selection.as_mut() {
                    sel.update(point, side);
                }
            }
            self.schedule();
        }
    }

    fn scroll(&mut self, absolute: f64, discrete: i32) {
        let lines = if discrete != 0 { discrete as f64 } else { absolute / (FONT_PX as f64 * 1.3) };
        self.scroll_acc += lines;
        let n = self.scroll_acc.trunc() as i32;
        if n == 0 {
            return;
        }
        self.scroll_acc -= n as f64;
        let Some(tab) = self.tabs.get(self.active) else { return };
        let mode = *tab.term.lock().mode();
        if self.mouse_mode() {
            if let Some((_, _, col, row)) = self.cell_at(self.mouse) {
                let code = if n < 0 { 64 } else { 65 };
                for _ in 0..n.abs() {
                    self.report_mouse(code, col, row, true);
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key: &[u8] = if n < 0 { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..n.abs() {
                tab.write(key.to_vec());
            }
        } else {
            // wayland: positive = down; scroll_display: positive = towards history
            tab.term.lock().scroll_display(Scroll::Delta(-n));
        }
        self.schedule();
    }

    // ---- drawing

    fn schedule(&mut self) {
        self.dirty = true;
    }

    /// Bring the bands up to date: a band is painted again only when what it shows has changed.
    fn update_bands(&mut self) {
        self.dirty = false;
        let s = self.scale;
        let (cols, rows) = self.grid_dims();
        let (pw, ph) = ((self.width as f32 * s).round() as usize, (self.height as f32 * s).round() as usize);
        let (_, ch, _) = self.fonts.cell_metrics(FONT_PX * s);
        let layout = (pw, ph, cols, rows, s.to_bits(), self.alpha.to_bits());
        if self.layout != Some(layout) || self.buf.len() != pw * ph * 4 {
            self.layout = Some(layout);
            self.buf = vec![0; pw * ph * 4];
            let oy = (TOOLBAR + PAD_TOP) * s;
            let mut edges = vec![0i32];
            edges.push(oy.round() as i32);
            for r in 1..=rows {
                edges.push((oy + r as f32 * ch).round() as i32);
            }
            edges.push(ph as i32);
            for i in 1..edges.len() {
                edges[i] = edges[i].clamp(edges[i - 1], ph as i32);
            }
            self.bands = edges.windows(2).map(|e| Band { y0: e[0], y1: e[1], hash: u64::MAX, pic: None }).collect();
        }
        let view = View { w: self.width as f32, s, activated: self.activated, hover: self.hover, alpha: self.alpha, cols, rows };
        let mut chrome = Fnv::new();
        chrome.write(format!("{:?}|{}|{}|{}|{}|{}", self.hover, self.activated, self.active, cols, rows, self.tabs.len()).as_bytes());
        for t in &self.tabs {
            chrome.write(t.title.as_bytes());
        }
        let row_hashes = match self.tabs.get(self.active) {
            Some(tab) => grid_hashes(tab, rows),
            None => vec![0; rows],
        };
        for i in 0..self.bands.len() {
            let hash = if i == 0 {
                chrome.finish()
            } else if i <= rows {
                row_hashes[i - 1]
            } else {
                0
            };
            let (y0, y1) = (self.bands[i].y0, self.bands[i].y1);
            if self.bands[i].hash == hash || y1 <= y0 {
                continue;
            }
            {
                let mut cv = Canvas::new(&mut self.buf, pw, ph);
                cv.set_clip([0, y0, pw as i32, y1]);
                cv.clear_rect([0, y0, pw as i32, y1], theme::BG, self.alpha);
                if i == 0 {
                    paint_chrome(&mut cv, &mut self.fonts, &mut self.icons, &self.tabs, self.active, &view);
                } else if i <= rows {
                    if let Some(tab) = self.tabs.get(self.active) {
                        paint_grid(&mut cv, &mut self.fonts, tab, &view, Some(i as i32 - 1));
                    }
                }
                window_edge(&mut cv, y0, y1, s);
            }
            let data = self.buf[y0 as usize * pw * 4..y1 as usize * pw * 4].to_vec();
            self.bands[i].pic = Some(Arc::new(Pic::from_bgra_window(pw as u32, (y1 - y0) as u32, data)));
            self.bands[i].hash = hash;
        }
    }

    /// The text cursor floats over the rows: it springs to its cell, so it glides when it moves.
    fn draw_cursor(&mut self, ui: &mut chebrium::ui::Ui<'_>, dt: f32, host: &mut Host) {
        let s = self.scale;
        let (cols, rows) = self.grid_dims();
        let view = View { w: self.width as f32, s, activated: self.activated, hover: self.hover, alpha: self.alpha, cols, rows };
        let cur = match self.tabs.get(self.active) {
            Some(tab) => cursor_cell(tab, &view, &mut self.fonts),
            None => None,
        };
        let Some(cur) = cur else {
            self.cur_seen = None;
            return;
        };
        let (tx, ty) = (cur.x / s, cur.y / s);
        // a new tab, a new layout or a cursor that was hidden: no gliding from where it used to be
        let seen = Some((self.active, self.layout));
        if self.cur_seen != seen {
            self.cur_seen = seen;
            for (sp, t) in [(&mut self.cur_x, tx), (&mut self.cur_y, ty)] {
                sp.x = t;
                sp.v = 0.0;
                sp.target = t;
            }
        }
        self.cur_x.target = tx;
        self.cur_y.target = ty;
        let dt = dt.clamp(0.0, 0.05);
        self.cur_x.update(dt);
        self.cur_y.update(dt);
        let key: CursorKey = (cur.shape as u8, self.activated, cur.c, cur.wide, cur.w.to_bits(), s.to_bits());
        if self.cur_pic.as_ref().map(|(k, _)| *k) != Some(key) {
            let pic = Arc::new(paint_cursor(&mut self.fonts, &cur, &view));
            self.cur_pic = Some((key, pic));
        }
        if let Some((_, pic)) = &self.cur_pic {
            let m = (2.0 * s).ceil() / s;
            // on whole device pixels, so the edges stay sharp while it glides
            let snap = |v: f32| (v * s).round() / s;
            ui.picture([snap(self.cur_x.x - m), snap(self.cur_y.x - m), pic.w as f32 / s, pic.h as f32 / s], pic, [0.0, 0.0, pic.w as f32, pic.h as f32], 1.0);
        }
        if !self.cur_x.at_rest() || !self.cur_y.at_rest() {
            host.redraw = true;
        }
    }

    /// Hand the runtime what the last event asked of it.
    fn flush(&mut self, host: &mut Host) {
        if self.exit {
            host.quit = true;
        }
        if std::mem::take(&mut self.want_drag) {
            host.drag = true;
        }
        if let Some(t) = self.want_title.take() {
            host.title = Some(t);
        }
        if let Some(c) = self.want_copy.take() {
            host.copy = Some(c);
        }
        if std::mem::take(&mut self.want_paste) {
            host.paste = true;
        }
    }
}

impl app::App for Term {
    fn menus(&self) -> Vec<Menu> {
        vec![
            Menu::new("File", vec![Item::new(1, "New Tab").key("Ctrl+Shift+T"), Item::new(2, "Close Tab").key("Ctrl+Shift+W").enabled(!self.tabs.is_empty())]),
            Menu::new("Edit", vec![Item::new(10, "Copy").key("Ctrl+Shift+C"), Item::new(11, "Paste").key("Ctrl+Shift+V")]),
        ]
    }

    fn menu_action(&mut self, id: u32, host: &mut Host) {
        match id {
            1 => self.new_tab(),
            2 => self.close_tab(self.active),
            10 => self.copy_selection(),
            11 => self.paste(),
            _ => return,
        }
        host.redraw = true;
    }

    fn start(&mut self, waker: Waker) {
        self.inbox.connect(waker);
    }

    // the picture carries its own translucency: the shell frosts what is behind it
    fn background(&self, _look: &chebrium::theme::Appearance) -> ([f32; 3], f32) {
        ([0.0, 0.0, 0.0], 0.0)
    }

    fn tick(&mut self, host: &mut Host) {
        for ev in self.inbox.drain() {
            self.handle_event(ev);
        }
        self.flush(host);
    }

    fn raw_key(&mut self, event: &KeyEvent, mods: &Modifiers, serial: u32, host: &mut Host) -> bool {
        self.mods = *mods;
        self.last_key_serial = serial;
        self.on_key(event.clone());
        self.flush(host);
        true
    }

    fn raw_pointer(&mut self, event: &PointerEvt, mods: &Modifiers, host: &mut Host) -> bool {
        self.mods = *mods;
        let p = (event.pos[0], event.pos[1]);
        match event.kind {
            PointerKind::Enter | PointerKind::Motion => self.motion(p),
            PointerKind::Leave => {
                self.hover = Hover::None;
                self.schedule();
            }
            PointerKind::Press { button, serial } => {
                self.mouse = p;
                self.buttons |= 1;
                self.press(button, serial);
            }
            PointerKind::Release { button } => {
                self.mouse = p;
                self.buttons = 0;
                self.release(button);
            }
            PointerKind::Axis { dy, discrete_y, .. } => self.scroll(dy as f64, discrete_y),
        }
        self.flush(host);
        true
    }

    fn clipboard(&mut self, text: String, host: &mut Host) {
        self.write_paste(&text);
        self.flush(host);
    }

    fn dropped(&mut self, files: Vec<std::path::PathBuf>, text: String, host: &mut Host) {
        // files go to the command line quoted, like in other terminals; text is pasted
        let line = if files.is_empty() { text } else { files.iter().map(|p| shell_quote(&p.to_string_lossy())).collect::<Vec<_>>().join(" ") + " " };
        self.write_paste(&line);
        self.flush(host);
    }

    fn draw(&mut self, ui: &mut chebrium::ui::Ui<'_>, size: [f32; 2], host: &mut Host) {
        let (w, h, s) = (size[0] as u32, size[1] as u32, host.scale.max(1.0));
        if !self.configured || (w, h, s) != (self.width, self.height, self.scale) {
            self.configured = true;
            self.width = w;
            self.height = h;
            self.scale = s;
            self.activated = host.activated;
            self.relayout();
        }
        if host.activated != self.activated {
            self.activated = host.activated;
            self.dirty = true;
        }
        if self.dirty || self.bands.is_empty() {
            self.update_bands();
        }
        for b in &self.bands {
            if let Some(pic) = &b.pic {
                ui.picture([0.0, b.y0 as f32 / self.scale, pic.w as f32 / self.scale, (b.y1 - b.y0) as f32 / self.scale], pic, [0.0, 0.0, pic.w as f32, pic.h as f32], 1.0);
            }
        }
        self.draw_cursor(ui, ui.dt(), host);
        self.flush(host);
    }
}

// ------------------------------------------------------------------ painting

struct View {
    w: f32,
    s: f32,
    activated: bool,
    hover: Hover,
    alpha: f32,
    cols: usize,
    rows: usize,
}

/// A path for the shell: plain when it is safe, else in single quotes.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%=~".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn draw_text(cv: &mut Canvas, fonts: &mut Fonts, text: &str, x: f32, baseline: f32, px: f32, color: Rgb, alpha: f32, bold: bool) -> f32 {
    let face = fonts.ui_face(bold);
    let mut pen = x;
    for c in text.chars() {
        let adv = face.font.advance(c, px);
        let g = face.glyph(c, px);
        cv.glyph(&g, pen.round() as i32, baseline.round() as i32, color, alpha);
        pen += adv;
    }
    pen - x
}

fn fit_text(fonts: &mut Fonts, text: &str, px: f32, max_w: f32) -> String {
    if fonts.ui_width(text, px, false) <= max_w {
        return text.to_string();
    }
    let mut out: String = text.to_string();
    while !out.is_empty() && fonts.ui_width(&format!("{out}…"), px, false) > max_w {
        out.pop();
    }
    format!("{out}…")
}

fn lerp3(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [(a[0] as f32 + (b[0] as f32 - a[0] as f32) * t) as u8, (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t) as u8, (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t) as u8]
}

/// Rasterised Lucide icons, cached per (icon, size, colour).
#[derive(Default)]
struct Icons {
    cache: std::collections::HashMap<(u32, u32, Rgb), Vec<u8>>,
}

impl Icons {
    /// Draw `name` centred on (cx, cy), `size` physical pixels square.
    fn draw(&mut self, cv: &mut Canvas, name: &str, cx: f32, cy: f32, size: f32, color: Rgb, alpha: f32) {
        let px = size.round().max(4.0) as u32;
        let bitmap = self.cache.entry((lucide::id(name), px, color)).or_insert_with(|| lucide::rasterize(name, px, color));
        cv.blit_premul((cx - px as f32 / 2.0).round() as i32, (cy - px as f32 / 2.0).round() as i32, px as usize, px as usize, bitmap, alpha);
    }
}

/// The toolbar band: the strip, the traffic lights, the tabs and the "+" button.
fn paint_chrome(cv: &mut Canvas, fonts: &mut Fonts, icons: &mut Icons, tabs: &[Tab], active: usize, v: &View) {
    let s = v.s;
    let pw = cv.w as i32;

    // titlebar: a slightly lighter strip, like Terminal.app
    let tb = (TOOLBAR * s) as i32;
    let bar_alpha = (v.alpha + 0.04).min(1.0);
    for y in 0..tb {
        let t = y as f32 / tb as f32;
        cv.fill_rect(0, y, pw, y + 1, lerp3([0x3c, 0x3c, 0x3e], [0x2b, 0x2b, 0x2d], t), bar_alpha);
    }
    cv.fill_rect(0, tb, pw, tb + (s as i32).max(1), [0, 0, 0], 0.6);

    // ---- chrome
    let ui_px = UI_PX * s;
    let base = (TOOLBAR / 2.0 + 4.5) * s;
    // traffic lights (grey when the window is not active, like macOS)
    let colors: [Rgb; 3] = if v.activated { [[0xff, 0x5f, 0x57], [0xfe, 0xbc, 0x2e], [0x28, 0xc8, 0x40]] } else { [[0x5c, 0x5c, 0x60]; 3] };
    for i in 0..3 {
        let (cx, cy) = light_center(i);
        cv.fill_circle(cx * s, cy * s, 7.0 * s, colors[i], 1.0);
        cv.stroke_round_rect((cx - 7.0) * s, (cy - 7.0) * s, 14.0 * s, 14.0 * s, 7.0 * s, s.max(1.0), [0, 0, 0], 0.22);
        if v.hover == Hover::Lights && v.activated {
            let g = [0x4a, 0x10, 0x10];
            let (x, y, k) = (cx * s, cy * s, 2.7 * s);
            match i {
                0 => {
                    cv.line(x - k, y - k, x + k, y + k, 1.3 * s, g, 0.75);
                    cv.line(x - k, y + k, x + k, y - k, 1.3 * s, g, 0.75);
                }
                1 => cv.line(x - 3.0 * s, y, x + 3.0 * s, y, 1.3 * s, g, 0.75),
                _ => {
                    cv.line(x - k, y + k, x + k, y - k, 1.3 * s, g, 0.75);
                    cv.fill_rect((x - k) as i32, (y - k) as i32, (x - k + 3.0 * s) as i32, (y - k + 1.2 * s) as i32, g, 0.75);
                }
            }
        }
    }

    let rects = tab_rects(v.w, tabs.len());
    if tabs.len() < 2 {
        // single tab: folder icon + "dir — process — cols×rows" after the lights
        let raw = tabs.first().map(|t| t.title.as_str()).unwrap_or("Terminal");
        let title = format!("{raw} — {}×{}", v.cols, v.rows);
        let x0 = 84.0;
        icons.draw(cv, "folder", (x0 + 9.0) * s, (TOOLBAR / 2.0) * s, 18.0 * s, [0xb4, 0x92, 0xf2], 1.0);
        let tpx = TITLE_PX * s;
        let title = fit_text(fonts, &title, tpx, (v.w - x0 - 28.0 - 14.0) * s);
        draw_text(cv, fonts, &title, (x0 + 25.0) * s, base + 0.5 * s, tpx, [0xc9, 0xc9, 0xcc], if v.activated { 1.0 } else { 0.55 }, true);
    }
    for (i, r) in rects.iter().enumerate() {
        let is_active = i == active;
        let hot = matches!(v.hover, Hover::Tab(j) | Hover::TabClose(j) if j == i);
        let (x, y, w, h) = (r.0 * s, r.1 * s, r.2 * s, r.3 * s);
        cv.fill_round_rect(x, y, w, h, h / 2.0, [255, 255, 255], if is_active { 0.14 } else if hot { 0.08 } else { 0.035 });
        if is_active {
            cv.stroke_round_rect(x, y, w, h, h / 2.0, s.max(1.0), [255, 255, 255], 0.16);
        }
        if is_active || hot {
            let (cx, cy) = (x + 13.0 * s, y + h / 2.0);
            if v.hover == Hover::TabClose(i) {
                cv.fill_circle(cx, cy, 8.0 * s, [255, 255, 255], 0.16);
            }
            icons.draw(cv, "x", cx, cy, 12.0 * s, theme::FG, 0.75);
        }
        let title = fit_text(fonts, &tabs[i].title, ui_px, w - 52.0 * s);
        let tw = fonts.ui_width(&title, ui_px, false);
        let tx = x + 26.0 * s + (w - 26.0 * s - 12.0 * s - tw).max(0.0) / 2.0;
        draw_text(cv, fonts, &title, tx, y + h / 2.0 + 4.5 * s, ui_px, theme::FG, if is_active { 0.95 } else { 0.6 }, false);
    }

    // "+" button: a new tab (Ctrl+Shift+T does the same)
    {
        let pr = plus_rect(v.w);
        let hot = v.hover == Hover::Plus;
        cv.fill_circle((pr.0 + pr.2 / 2.0) * s, (pr.1 + pr.3 / 2.0) * s, pr.2 / 2.0 * s, [255, 255, 255], if hot { 0.16 } else { 0.07 });
        icons.draw(cv, "plus", (pr.0 + pr.2 / 2.0) * s, (pr.1 + pr.3 / 2.0) * s, 16.0 * s, theme::FG, 0.85);
    }
}

/// The window's thin light border and rounded corners on the rows `y0..y1` (what `stroke_round_rect` and
/// `mask_round_corners` do for the whole window, limited to a band: only the pixels near an edge are visited).
fn window_edge(cv: &mut Canvas, y0: i32, y1: i32, s: f32) {
    let (w, h) = (cv.w as i32, cv.h as i32);
    let (r, t) = (RADIUS * s, s.max(1.0));
    let ri = r.ceil() as i32;
    let k = t.ceil() as i32 + 2;
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let sd = |px: f32, py: f32| {
        let qx = (px - cx).abs() - cx + r;
        let qy = (py - cy).abs() - cy + r;
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
    };
    for y in y0..y1 {
        let corner_row = y < ri || y >= h - ri;
        let full = y < k || y >= h - k;
        let reach = if corner_row { ri.max(k) } else { k };
        let spans = if full { [(0, w), (0, 0)] } else { [(0, reach.min(w)), ((w - reach).max(0), w)] };
        for (a, b) in spans {
            for x in a..b {
                let d = sd(x as f32 + 0.5, y as f32 + 0.5);
                let cov = (0.5 - (d + t / 2.0).abs() + t / 2.0).clamp(0.0, 1.0);
                if cov > 0.0 {
                    cv.blend(x, y, [255, 255, 255], 0.2 * cov);
                }
                if corner_row && (x < ri || x >= w - ri) {
                    let m = (0.5 - d).clamp(0.0, 1.0);
                    if m < 1.0 {
                        let i = (y as usize * cv.w + x as usize) * 4;
                        for c in 0..4 {
                            cv.data[i + c] = (cv.data[i + c] as f32 * m + 0.5) as u8;
                        }
                    }
                }
            }
        }
    }
}

/// A small, quick hash for telling whether a row of the grid changed.
struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf29ce484222325)
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x100000001b3);
        }
    }
    fn word(&mut self, v: u64) {
        self.0 = (self.0 ^ v).wrapping_mul(0x100000001b3);
        self.0 ^= self.0 >> 29;
    }
    fn finish(&self) -> u64 {
        self.0 & !(1 << 63)
    }
}

/// What each visible row looks like, as a hash: its characters, colours, attributes, selection and cursor.
fn grid_hashes(tab: &Tab, rows: usize) -> Vec<u64> {
    let mut hs: Vec<Fnv> = (0..rows).map(|_| Fnv::new()).collect();
    let term = tab.term.lock();
    let content = term.renderable_content();
    let off = content.display_offset as i32;
    let colors = content.colors;
    let selection = content.selection;
    for ind in content.display_iter {
        let row = ind.point.line.0 + off;
        if row < 0 || row as usize >= hs.len() {
            continue;
        }
        let cell = ind.cell;
        let fg = theme::resolve_fg(cell.fg, colors, cell.flags.contains(Flags::BOLD));
        let bg = theme::resolve(cell.bg, colors);
        let h = &mut hs[row as usize];
        h.word(cell.c as u64 | (cell.flags.bits() as u64) << 32);
        h.word(u32::from_le_bytes([fg[0], fg[1], fg[2], 0]) as u64 | (u32::from_le_bytes([bg[0], bg[1], bg[2], matches!(cell.bg, Color::Named(NamedColor::Background)) as u8]) as u64) << 32);
        if selection.is_some_and(|r| r.contains(ind.point)) {
            h.word(0x5e1ec7 ^ ind.point.column.0 as u64);
        }
    }
    hs.iter().map(|h| h.finish()).collect()
}

/// Light/heavy arms for box-drawing characters: (up, down, left, right), 0 none / 1 light / 2 heavy.
fn box_arms(c: char) -> Option<(u8, u8, u8, u8)> {
    Some(match c {
        '─' => (0, 0, 1, 1),
        '━' => (0, 0, 2, 2),
        '│' => (1, 1, 0, 0),
        '┃' => (2, 2, 0, 0),
        '┌' | '╭' => (0, 1, 0, 1),
        '┏' => (0, 2, 0, 2),
        '┐' | '╮' => (0, 1, 1, 0),
        '┓' => (0, 2, 2, 0),
        '└' | '╰' => (1, 0, 0, 1),
        '┗' => (2, 0, 0, 2),
        '┘' | '╯' => (1, 0, 1, 0),
        '┛' => (2, 0, 2, 0),
        '├' => (1, 1, 0, 1),
        '┣' => (2, 2, 0, 2),
        '┤' => (1, 1, 1, 0),
        '┫' => (2, 2, 2, 0),
        '┬' => (0, 1, 1, 1),
        '┳' => (0, 2, 2, 2),
        '┴' => (1, 0, 1, 1),
        '┻' => (2, 0, 2, 2),
        '┼' => (1, 1, 1, 1),
        '╋' => (2, 2, 2, 2),
        '═' => (0, 0, 2, 2),
        '║' => (2, 2, 0, 0),
        _ => return None,
    })
}

/// Draw box-drawing / block characters ourselves so they join seamlessly across cells.
fn draw_box(cv: &mut Canvas, c: char, x: f32, y: f32, cw: f32, ch: f32, s: f32, color: Rgb) -> bool {
    let (x0, y0, x1, y1) = (x.round() as i32, y.round() as i32, (x + cw).round() as i32, (y + ch).round() as i32);
    let (mx, my) = ((x0 + x1) / 2, (y0 + y1) / 2);
    if let Some((u, d, l, r)) = box_arms(c) {
        let t = |w: u8| ((w as f32) * s).round().max(1.0) as i32;
        let (tv, th) = (t(u.max(d).max(1)), t(l.max(r).max(1)));
        let (hx, hy) = (tv / 2, th / 2);
        if u > 0 {
            cv.fill_rect(mx - hx, y0, mx - hx + t(u), my + hy + 1, color, 1.0);
        }
        if d > 0 {
            cv.fill_rect(mx - hx, my - hy, mx - hx + t(d), y1, color, 1.0);
        }
        if l > 0 {
            cv.fill_rect(x0, my - hy, mx + hx + 1, my - hy + t(l), color, 1.0);
        }
        if r > 0 {
            cv.fill_rect(mx - hx, my - hy, x1, my - hy + t(r), color, 1.0);
        }
        return true;
    }
    match c {
        '▀' => cv.fill_rect(x0, y0, x1, my, color, 1.0),
        '▄' => cv.fill_rect(x0, my, x1, y1, color, 1.0),
        '█' => cv.fill_rect(x0, y0, x1, y1, color, 1.0),
        '▌' => cv.fill_rect(x0, y0, mx, y1, color, 1.0),
        '▐' => cv.fill_rect(mx, y0, x1, y1, color, 1.0),
        '░' => cv.fill_rect(x0, y0, x1, y1, color, 0.25),
        '▒' => cv.fill_rect(x0, y0, x1, y1, color, 0.5),
        '▓' => cv.fill_rect(x0, y0, x1, y1, color, 0.75),
        '▁'..='▇' => {
            let n = (c as u32 - '▁' as u32 + 1) as f32 / 8.0;
            cv.fill_rect(x0, (y1 as f32 - ch * n).round() as i32, x1, y1, color, 1.0);
        }
        '▉'..='▏' => {
            let n = (8 - (c as u32 - '▉' as u32 + 1)) as f32 / 8.0;
            cv.fill_rect(x0, y0, (x as f32 + cw * n).round() as i32, y1, color, 1.0);
        }
        _ => return false,
    }
    true
}

/// The cells of the grid, or of one row of it (`only`).
fn paint_grid(cv: &mut Canvas, fonts: &mut Fonts, tab: &Tab, v: &View, only: Option<i32>) {
    let s = v.s;
    let (cw, ch, baseline) = fonts.cell_metrics(FONT_PX * s);
    let px = FONT_PX * s;
    let (ox, oy) = (PAD_X * s, (TOOLBAR + PAD_TOP) * s);

    let term = tab.term.lock();
    let content = term.renderable_content();
    let off = content.display_offset as i32;
    let colors = content.colors;
    let selection = content.selection;
    for ind in content.display_iter {
        let cell = ind.cell;
        let row = ind.point.line.0 + off;
        if row < 0 || only.is_some_and(|o| o != row) || cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let col = ind.point.column.0 as f32;
        let wide = cell.flags.contains(Flags::WIDE_CHAR);
        let (x, y) = (ox + col * cw, oy + row as f32 * ch);
        let w = if wide { 2.0 * cw } else { cw };

        let mut fg = theme::resolve_fg(cell.fg, colors, cell.flags.contains(Flags::BOLD));
        let mut bg = theme::resolve(cell.bg, colors);
        let inverse = cell.flags.contains(Flags::INVERSE);
        if inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut draw_bg = inverse || !matches!(cell.bg, Color::Named(NamedColor::Background));
        if selection.is_some_and(|r| r.contains(ind.point)) {
            bg = SEL_BG;
            fg = [255, 255, 255];
            draw_bg = true;
        }
        if cell.flags.contains(Flags::DIM) {
            fg = theme::dimmed(fg);
        }
        if draw_bg {
            cv.fill_rect(x.round() as i32, y.round() as i32, (x + w).round() as i32, (y + ch).round() as i32, bg, 0.94);
        }
        if cell.c != ' ' && !cell.flags.contains(Flags::HIDDEN) && !draw_box(cv, cell.c, x, y, w, ch, s, fg) {
            let style = match (cell.flags.contains(Flags::BOLD), cell.flags.contains(Flags::ITALIC)) {
                (true, true) => Style::BoldItalic,
                (true, false) => Style::Bold,
                (false, true) => Style::Italic,
                _ => Style::Regular,
            };
            if let Some(g) = fonts.mono_glyph(cell.c, style, px) {
                cv.glyph(&g, x.round() as i32, (y + baseline).round() as i32, fg, 1.0);
            }
        }
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            let uy = (y + baseline + 2.0 * s).round() as i32;
            cv.fill_rect(x.round() as i32, uy, (x + w).round() as i32, uy + (s as i32).max(1), fg, 1.0);
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            let sy = (y + ch * 0.5).round() as i32;
            cv.fill_rect(x.round() as i32, sy, (x + w).round() as i32, sy + (s as i32).max(1), fg, 1.0);
        }
    }

}

/// Where the cursor is, when it is to be seen.
fn cursor_cell(tab: &Tab, v: &View, fonts: &mut Fonts) -> Option<Cur> {
    let (cw, ch, _) = fonts.cell_metrics(FONT_PX * v.s);
    let term = tab.term.lock();
    let content = term.renderable_content();
    let cursor = content.cursor;
    let row = cursor.point.line.0 + content.display_offset as i32;
    if !content.mode.contains(TermMode::SHOW_CURSOR) || cursor.shape == CursorShape::Hidden || row < 0 || row as usize >= v.rows {
        return None;
    }
    let cell = &term.grid()[cursor.point];
    let wide = cell.flags.contains(Flags::WIDE_CHAR);
    Some(Cur { x: PAD_X * v.s + cursor.point.column.0 as f32 * cw, y: (TOOLBAR + PAD_TOP) * v.s + row as f32 * ch, w: if wide { 2.0 * cw } else { cw }, shape: cursor.shape, c: cell.c, wide })
}

/// The cursor as a small picture (with a margin for the outline), cell sized, ready to be moved around.
fn paint_cursor(fonts: &mut Fonts, cur: &Cur, v: &View) -> Pic {
    let s = v.s;
    let (_, ch, baseline) = fonts.cell_metrics(FONT_PX * s);
    let m = (2.0 * s).ceil();
    let (pw, ph) = ((cur.w + 2.0 * m).ceil() as usize, (ch + 2.0 * m).ceil() as usize);
    let mut buf = vec![0u8; pw * ph * 4];
    {
        let mut cv = Canvas::new(&mut buf, pw, ph);
        let (x, y) = (m, m);
        let (xi, yi, xe, ye) = (x.round() as i32, y.round() as i32, (x + cur.w).round() as i32, (y + ch).round() as i32);
        match (cur.shape, v.activated) {
            (CursorShape::Block, true) => {
                cv.fill_rect(xi, yi, xe, ye, theme::CURSOR, 1.0);
                if cur.c != ' ' {
                    if let Some(g) = fonts.mono_glyph(cur.c, Style::Regular, FONT_PX * s) {
                        cv.glyph(&g, xi, (y + baseline).round() as i32, theme::BG, 1.0);
                    }
                }
            }
            (CursorShape::Underline, _) => cv.fill_rect(xi, ye - (2.0 * s) as i32, xe, ye, theme::CURSOR, 1.0),
            (CursorShape::Beam, _) => cv.fill_rect(xi, yi, xi + (2.0 * s) as i32, ye, theme::CURSOR, 1.0),
            _ => cv.stroke_round_rect(x, y, cur.w, ch, 2.0 * s, s.max(1.0), theme::CURSOR, 1.0),
        }
    }
    // blended over the rows; a "window" picture would replace them (and punch the transparent margin through)
    Pic::from_bgra(pw as u32, ph as u32, buf, false, false)
}

// ------------------------------------------------------------------ wayland handlers

fn main() {
    alacritty_terminal::tty::setup_env();

    let (tx, inbox) = app::channel::<AppEvent>();
    let mut fonts = Fonts::load();
    let (cw, ch, _) = fonts.cell_metrics(FONT_PX);
    let default_w = (DEFAULT_COLS * cw + 2.0 * PAD_X).ceil() as u32;
    let default_h = (TOOLBAR + PAD_TOP + PAD_BOTTOM + DEFAULT_ROWS * ch).ceil() as u32;
    let mut term = Term {
        inbox,
        tx,
        width: default_w,
        height: default_h,
        scale: 1.0,
        configured: false,
        activated: true,
        exit: false,
        dirty: true,
        cur_x: Spring::new(0.0, CURSOR_K, CURSOR_ZETA),
        cur_y: Spring::new(0.0, CURSOR_K, CURSOR_ZETA),
        cur_seen: None,
        cur_pic: None,
        mods: Modifiers::default(),
        last_key_serial: 0,
        fonts,
        icons: Icons::default(),
        tabs: Vec::new(),
        active: 0,
        next_id: 0,
        mouse: (-1.0, -1.0),
        hover: Hover::None,
        selecting: false,
        buttons: 0,
        last_click: None,
        scroll_acc: 0.0,
        alpha: std::env::var("MEMRIUM_TERM_OPACITY").ok().and_then(|v| v.parse::<f32>().ok()).unwrap_or(DEFAULT_ALPHA).clamp(0.3, 1.0),
        want_title: None,
        want_drag: false,
        press_at: None,
        want_copy: None,
        want_paste: false,
        buf: Vec::new(),
        bands: Vec::new(),
        layout: None,
    };
    term.new_tab();

    // debugging aid: MEMRIUM_TERM_INIT / MEMRIUM_TERM_INIT2 are typed into tab 1 / 2 (\r and \n are expanded),
    // MEMRIUM_TERM_TABS=n opens n tabs in total
    let unescape = |v: String| v.replace("\\r", "\r").replace("\\n", "\n");
    if let Ok(init) = std::env::var("MEMRIUM_TERM_INIT") {
        term.tabs[0].write(unescape(init).into_bytes());
    }
    let n: usize = std::env::var("MEMRIUM_TERM_TABS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    for _ in 1..n {
        term.new_tab();
    }
    if let Ok(init) = std::env::var("MEMRIUM_TERM_INIT2") {
        if let Some(t) = term.tabs.get(1) {
            t.write(unescape(init).into_bytes());
        }
    }
    if let Some(a) = std::env::var("MEMRIUM_TERM_ACTIVE").ok().and_then(|v| v.parse::<usize>().ok()) {
        term.active = a.min(term.tabs.len() - 1);
    }

    // we draw our own titlebar: ask for client-side decorations
    app::run(Config::new("Memrium Terminal", "memrium-term").size(default_w, default_h).min_size(320, 200).decorations(Decorations::Client), term);
}
