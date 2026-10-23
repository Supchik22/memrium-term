//! One terminal tab: an alacritty_terminal `Term` driven by a PTY thread.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options};
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
}

impl Tab {
    pub fn spawn(id: TabId, cols: usize, rows: usize, cell: (u16, u16), tx: Outbox<AppEvent>) -> std::io::Result<Tab> {
        let listener = Listener { id, tx };
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize { cols, rows }, listener.clone())));
        let size = WindowSize { num_lines: rows as u16, num_cols: cols as u16, cell_width: cell.0, cell_height: cell.1 };
        let pty = tty::new(&Options::default(), size, id)?;
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let sender = event_loop.channel();
        event_loop.spawn();
        Ok(Tab { id, term, sender, title: String::from("Terminal") })
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
