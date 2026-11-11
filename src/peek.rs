//! Picture paths in the terminal's text: hover one and a small preview floats next to it, Ctrl+click opens it in the
//! picture viewer. A path is found around the pointer (also when the shell wrapped a long one over two rows), made
//! absolute against the shell's current folder, and counts when it names an existing picture file.

use super::tab::AppEvent;
use chebrium::app::Outbox;
use chebrium::picture::Pic;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const EXTS: [&str; 10] = ["png", "jpg", "jpeg", "jfif", "webp", "gif", "bmp", "tif", "tiff", "ico"];
/// pictures bigger than this are not read just to be peeked at
const MAX_BYTES: u64 = 96 << 20;
/// the longest side of the decoded preview (px)
const MAX_SIDE: u32 = 720;

/// A path under the pointer: the file, and where its characters are (screen line, first and one past last column).
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub spans: Vec<(i32, usize, usize)>,
}

pub struct Preview {
    pub pic: Arc<Pic>,
    /// the picture's own size
    pub size: (u32, u32),
    pub bytes: u64,
}

enum Entry {
    Loading,
    Ready(Arc<Preview>),
    Failed,
}

#[derive(Default)]
pub struct Peek {
    entries: HashMap<PathBuf, Entry>,
}

fn is_path_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '*' | '?' | '\0' | '│' | '└' | '├' | '·')
}

/// The text of a path out of the characters that surround it: the ends trimmed of punctuation and a `:line:col` tail.
/// Returns the start and end (exclusive) within `chars`, and the text.
pub fn path_in(chars: &[char], at: usize) -> Option<(usize, usize, String)> {
    if at >= chars.len() || !is_path_char(chars[at]) {
        return None;
    }
    let (mut a, mut b) = (at, at + 1);
    while a > 0 && is_path_char(chars[a - 1]) {
        a -= 1;
    }
    while b < chars.len() && is_path_char(chars[b]) {
        b += 1;
    }
    // a ( [ { at the start and closing marks or sentence punctuation at the end are not part of the path
    while a < b && matches!(chars[a], '(' | '[' | '{' | '=' | ',' | ';') {
        a += 1;
    }
    while b > a && matches!(chars[b - 1], ')' | ']' | '}' | ',' | ';' | '.' | ':' | '!') {
        b -= 1;
    }
    // `file.png:12:3` -> `file.png`, `file://x` -> `x`
    let whole: String = chars[a..b].iter().collect();
    let mut rest = whole.as_str();
    for _ in 0..2 {
        if let Some((head, tail)) = rest.rsplit_once(':') {
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                rest = head;
            }
        }
    }
    let (skip, text) = match rest.strip_prefix("file://") {
        Some(t) => (7, t),
        None => (0, rest),
    };
    let ext = Path::new(text).extension()?.to_str()?.to_ascii_lowercase();
    if !EXTS.contains(&ext.as_str()) || text.len() < 3 {
        return None;
    }
    Some((a + skip, a + skip + text.chars().count(), text.to_string()))
}

/// `~/x`, `/x` or `x` made into a path against `cwd`.
pub fn resolve(text: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    let p = if let Some(rest) = text.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME")?).join(rest)
    } else if text.starts_with('/') {
        PathBuf::from(text)
    } else {
        cwd?.join(text.strip_prefix("./").unwrap_or(text))
    };
    p.is_file().then_some(p)
}

fn size_text(bytes: u64) -> String {
    match bytes {
        b if b < 1 << 10 => format!("{b} B"),
        b if b < 1 << 20 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / 1048576.0),
    }
}

impl Preview {
    /// "1920 x 1080 · 240 KB"
    pub fn caption(&self) -> String {
        format!("{} \u{d7} {}  \u{b7}  {}", self.size.0, self.size.1, size_text(self.bytes))
    }
}

/// Decode a picture for the preview: scaled to at most `MAX_SIDE`, with rounded corners baked in.
pub fn load(path: &Path) -> Option<Preview> {
    let bytes = std::fs::metadata(path).ok()?.len();
    if bytes > MAX_BYTES {
        return None;
    }
    let img = image::open(path).ok()?;
    let size = (img.width(), img.height());
    let img = if size.0.max(size.1) > MAX_SIDE { img.thumbnail(MAX_SIDE, MAX_SIDE) } else { img };
    let (w, h) = (img.width(), img.height());
    let mut px = img.to_rgba8().into_raw();
    let r = (w.min(h) as f32 * 0.05).clamp(4.0, 28.0);
    let (a, b) = (w as f32 / 2.0, h as f32 / 2.0);
    for y in 0..h {
        for x in 0..w {
            let (qx, qy) = ((x as f32 + 0.5 - a).abs() - (a - r), (y as f32 + 0.5 - b).abs() - (b - r));
            let d = if qx > 0.0 && qy > 0.0 { ((qx / r).powf(3.4) + (qy / r).powf(3.4)).powf(1.0 / 3.4) * r - r } else { qx.max(qy) - r };
            let cover = (0.5 - d).clamp(0.0, 1.0);
            let i = ((y * w + x) * 4 + 3) as usize;
            px[i] = (px[i] as f32 * cover) as u8;
        }
    }
    Some(Preview { pic: Arc::new(Pic::from_rgba(w, h, &px)), size, bytes })
}

impl Peek {
    /// The preview of `path` when it is ready; the first call starts decoding it on a thread (which wakes the window).
    pub fn get(&mut self, path: &Path, tx: &Outbox<AppEvent>) -> Option<Arc<Preview>> {
        match self.entries.get(path) {
            Some(Entry::Ready(p)) => return Some(p.clone()),
            Some(_) => return None,
            None => {}
        }
        if self.entries.len() > 24 {
            self.entries.retain(|_, e| matches!(e, Entry::Loading));
        }
        self.entries.insert(path.to_path_buf(), Entry::Loading);
        let (path, tx) = (path.to_path_buf(), tx.clone());
        std::thread::spawn(move || {
            let p = load(&path);
            let _ = tx.send(AppEvent::Peek(path, p.map(Arc::new)));
        });
        None
    }

    pub fn loaded(&mut self, path: PathBuf, p: Option<Arc<Preview>>) {
        self.entries.insert(path, p.map_or(Entry::Failed, Entry::Ready));
    }
}

/// Open a picture in the viewer (the one next to this program, else whatever the desktop uses).
pub fn open_viewer(path: &Path) {
    let viewer = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("memrium_view"))).filter(|v| v.is_file());
    let mut cmd = match viewer {
        Some(v) => std::process::Command::new(v),
        None => std::process::Command::new("xdg-open"),
    };
    let _ = cmd.arg(path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn a_path_is_found_around_the_pointer() {
        let c = chars("Reading /tmp/shots/a.png (11s)");
        let (a, b, t) = path_in(&c, 14).unwrap();
        assert_eq!(t, "/tmp/shots/a.png");
        assert_eq!(c[a..b].iter().collect::<String>(), "/tmp/shots/a.png");
        // between words: none; a word that is no picture: none
        assert!(path_in(&c, 7).is_none());
        assert!(path_in(&c, 2).is_none());
    }

    #[test]
    fn punctuation_and_line_numbers_are_not_part_of_it() {
        assert_eq!(path_in(&chars("see (docs/a.jpg)."), 8).unwrap().2, "docs/a.jpg");
        assert_eq!(path_in(&chars("out.webp:12:3"), 2).unwrap().2, "out.webp");
        assert_eq!(path_in(&chars("file:///tmp/x.PNG"), 10).unwrap().2, "/tmp/x.PNG");
        assert_eq!(path_in(&chars("\"~/Pictures/a b.png\""), 3).map(|x| x.2), None);
    }

    #[test]
    fn the_preview_is_rounded_at_the_corners_only() {
        let dir = std::env::temp_dir().join("peek-test-round");
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("r.png");
        image::RgbaImage::from_pixel(400, 250, image::Rgba([200, 60, 60, 255])).save(&f).unwrap();
        let p = load(&f).unwrap();
        let (px, w, h) = p.pic.pixels();
        let a = |x: usize, y: usize| px[(y * w + x) * 4 + 3];
        assert_eq!(a(0, 0), 0, "the corner is cut");
        for (x, y) in [(w / 2, 0), (w / 2, h - 1), (0, h / 2), (w - 1, h / 2), (w / 2, h / 2)] {
            assert_eq!(a(x, y), 255, "pixel {x},{y} must be solid");
        }
    }

    #[test]
    fn resolving_needs_the_file_to_exist() {
        let dir = std::env::temp_dir().join("peek-test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("p.png"), b"x").unwrap();
        assert_eq!(resolve("p.png", Some(&dir)), Some(dir.join("p.png")));
        assert_eq!(resolve("./p.png", Some(&dir)), Some(dir.join("p.png")));
        assert!(resolve("missing.png", Some(&dir)).is_none());
        assert!(resolve("p.png", None).is_none());
    }
}
