//! Font discovery (via fontconfig's `fc-match`) and a glyph cache on top of Chebrium's glyph engine.

use chebrium::glyphs::Font;
use std::collections::HashMap;
use std::process::Command;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Style {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

pub use chebrium::canvas::Glyph;

/// One face plus its rasterised glyphs for a given pixel size.
pub struct Face {
    pub font: Font,
    cache: HashMap<(char, u32), Rc<Glyph>>,
}

impl Face {
    fn new(font: Font) -> Self {
        Self { font, cache: HashMap::new() }
    }

    pub fn has(&self, c: char) -> bool {
        self.font.lookup_glyph_index(c) != 0
    }

    pub fn glyph(&mut self, c: char, px: f32) -> Rc<Glyph> {
        let key = (c, px.to_bits());
        if let Some(g) = self.cache.get(&key) {
            return g.clone();
        }
        let (m, bitmap) = self.font.rasterize(c, px);
        let g = Rc::new(Glyph { xmin: m.xmin, ymin: m.ymin, width: m.width, height: m.height, bitmap });
        self.cache.insert(key, g.clone());
        g
    }
}

fn fc_match(pattern: &str) -> Option<String> {
    let out = Command::new("fc-match").args(["-f", "%{file}", pattern]).output().ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn load(path: &str) -> Option<Font> {
    let bytes = std::fs::read(path).ok()?;
    Font::from_vec(bytes)
}

pub struct Fonts {
    /// the monospace faces, loaded when first needed (a Nerd Font parses to tens of megabytes: a program that never shows
    /// a terminal grid, or never a bold or italic one, should not pay for them)
    mono: HashMap<Style, Face>,
    mono_family: String,
    /// UI (proportional) faces: regular, bold
    pub ui: Face,
    pub ui_bold: Face,
    /// fallback faces, found on demand by character
    fallbacks: Vec<Face>,
    /// characters we already failed to find a fallback for
    missing: std::collections::HashSet<char>,
    tried_paths: std::collections::HashSet<String>,
}

impl Fonts {
    pub fn load() -> Fonts {
        let family = std::env::var("MEMRIUM_TERM_FONT").unwrap_or_else(|_| "JetBrainsMono Nerd Font Mono".to_string());
        let bundled = |b: &'static [u8]| Font::from_static(b);
        let (ui_regular, ui_bold) = match (std::env::var("MEMRIUM_UI_FONT"), bundled(chebrium::text::INTER_REGULAR), bundled(chebrium::text::INTER_SEMIBOLD)) {
            (Err(_), Some(r), Some(b)) => (r, b),
            (name, ..) => {
                let ui_name = name.unwrap_or_else(|_| "Inter".to_string());
                let r = fc_match(&format!("{ui_name}:regular")).and_then(|p| load(&p)).or_else(|| fc_match("sans").and_then(|p| load(&p))).expect("no UI font");
                let b = fc_match(&format!("{ui_name}:bold")).and_then(|p| load(&p)).unwrap_or_else(|| r.clone());
                (r, b)
            }
        };
        Fonts {
            mono: HashMap::new(),
            mono_family: family,
            ui: Face::new(ui_regular),
            ui_bold: Face::new(ui_bold),
            fallbacks: Vec::new(),
            missing: Default::default(),
            tried_paths: Default::default(),
        }
    }

    /// The monospace face for `style`, loaded now if it has not been. A style the family does not have shares the regular
    /// face (the same parsed font, not a copy).
    fn mono_face(&mut self, style: Style) -> &mut Face {
        if !self.mono.contains_key(&style) {
            let regular_font = match self.mono.get(&Style::Regular) {
                Some(f) => f.font.clone(),
                None => {
                    let family = self.mono_family.clone();
                    let f = fc_match(&format!("{family}:regular")).and_then(|p| load(&p)).or_else(|| fc_match("monospace").and_then(|p| load(&p))).expect("no monospace font found (set MEMRIUM_TERM_FONT)");
                    self.mono.insert(Style::Regular, Face::new(f.clone()));
                    f
                }
            };
            if style != Style::Regular && !self.mono.contains_key(&style) {
                let name = match style {
                    Style::Bold => "bold",
                    Style::Italic => "italic",
                    _ => "bold:italic",
                };
                let family = self.mono_family.clone();
                let font = fc_match(&format!("{family}:{name}")).and_then(|p| load(&p)).unwrap_or(regular_font);
                self.mono.insert(style, Face::new(font));
            }
        }
        self.mono.get_mut(&style).unwrap()
    }

    /// Cell metrics (cell width, cell height, baseline offset from the top) for a physical pixel size.
    pub fn cell_metrics(&mut self, px: f32) -> (f32, f32, f32) {
        let f = self.mono_face(Style::Regular).font.clone();
        let adv = f.metrics('M', px).advance_width.round().max(1.0);
        let (ascent, descent, gap) = f.horizontal_line_metrics(px).map(|l| (l.ascent, l.descent, l.line_gap)).unwrap_or((px * 0.9, -px * 0.25, 0.0));
        let natural = ascent - descent + gap;
        let height = (natural * 1.12).round();
        let baseline = ((height - natural) / 2.0 + ascent).round();
        (adv, height, baseline)
    }

    /// Glyph for `c` in `style`, with fontconfig-based fallback for characters the main font lacks.
    pub fn mono_glyph(&mut self, c: char, style: Style, px: f32) -> Option<Rc<Glyph>> {
        if self.mono_face(style).has(c) {
            return Some(self.mono_face(style).glyph(c, px));
        }
        if self.mono_face(Style::Regular).has(c) {
            return Some(self.mono_face(Style::Regular).glyph(c, px));
        }
        for f in &mut self.fallbacks {
            if f.has(c) {
                return Some(f.glyph(c, px));
            }
        }
        if self.missing.contains(&c) {
            return None;
        }
        // ask fontconfig for a font that has this character
        if let Some(path) = fc_match(&format!(":charset={:x}", c as u32)) {
            if self.tried_paths.insert(path.clone()) {
                if let Some(font) = load(&path) {
                    self.fallbacks.push(Face::new(font));
                }
            }
            for f in &mut self.fallbacks {
                if f.has(c) {
                    return Some(f.glyph(c, px));
                }
            }
        }
        self.missing.insert(c);
        None
    }

    /// Advance width of `text` in the UI font.
    pub fn ui_width(&mut self, text: &str, px: f32, bold: bool) -> f32 {
        let face = if bold { &mut self.ui_bold } else { &mut self.ui };
        text.chars().map(|c| face.font.advance(c, px)).sum()
    }

    pub fn ui_face(&mut self, bold: bool) -> &mut Face {
        if bold {
            &mut self.ui_bold
        } else {
            &mut self.ui
        }
    }
}
