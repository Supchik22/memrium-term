//! One terminal tab: an alacritty_terminal `Term` driven by a PTY thread.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Shell};
use chebrium::app::Outbox;
use std::sync::Arc;

pub type TabId = u64;

/// Messages from background threads to the program (each one wakes it).
pub enum AppEvent {
    Term(TabId, Event),
}

#[derive(Clone)]
pub struct Listener {
    id: TabId,
    tx: Outbox<AppEvent>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let _ = self.tx.send(AppEvent::Term(self.id, event));
    }
}

pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct Tab {
    pub id: TabId,
    pub term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    pub title: String,
    /// the shell's process, to ask where it is
    pid: u32,
}

impl Tab {
    /// `cwd`: the folder the shell starts in (the home folder when none).
    pub fn spawn(id: TabId, cols: usize, rows: usize, cell: (u16, u16), tx: Outbox<AppEvent>, cwd: Option<std::path::PathBuf>) -> std::io::Result<Tab> {
        let listener = Listener { id, tx };
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize { cols, rows }, listener.clone())));
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: cell.0, cell_height: cell.1 };
        // the user's login shell from the account, not $SHELL: a session started from a dev shell inherits that one's bash
        let options = Options { shell: login_shell().map(|p| Shell::new(p, Vec::new())), working_directory: cwd.filter(|d| d.is_dir()), ..Options::default() };
        let pty = tty::new(&options, size, id)?;
        let pid = pty.child().id();
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Tab { id, term, sender, title: String::from("Terminal"), pid })
    }

    /// The folder the shell is in now (its current directory, read from the system).
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/{}/cwd", self.pid)).ok()
    }

    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.sender.send(Msg::Input(std::borrow::Cow::Owned(bytes.into())));
    }

    pub fn resize(&self, cols: usize, rows: usize, cell: (u16, u16)) {
        self.term.lock().resize(TermSize { cols, rows });
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: cell.0, cell_height: cell.1 };
        let _ = self.sender.send(Msg::Resize(size));
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

/// The shell the account is set up with (`/etc/passwd`), if it can be read and exists.
fn login_shell() -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let mut pw = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: the buffers outlive the call and `pw_shell` is read only when getpwuid_r reports an entry
    let shell = unsafe {
        libc::getpwuid_r(libc::getuid(), pw.as_mut_ptr(), buf.as_mut_ptr().cast(), buf.len(), &mut out);
        if out.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*out).pw_shell).to_str().ok()?.to_string()
    };
    std::path::Path::new(&shell).is_file().then_some(shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tab_starts_in_the_folder_it_is_given_and_reports_where_it_is() {
        let (tx, _rx) = chebrium::app::channel();
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let first = Tab::spawn(1, 80, 24, (8, 16), tx.clone(), Some(dir.clone())).expect("a shell");
        // the shell needs a moment to start
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(first.cwd(), Some(dir.clone()));
        // a second tab opened from the first one's place is in the same folder
        let second = Tab::spawn(2, 80, 24, (8, 16), tx, first.cwd()).expect("a shell");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(second.cwd(), Some(dir));
    }
}
