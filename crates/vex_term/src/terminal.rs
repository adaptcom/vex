//! Terminal lifetime, panic cleanup, signal notification, and the event loop.

use crate::{app::App, screen::Renderer};
use crossterm::{
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
        Event,
    },
    execute,
    style::ResetColor,
    terminal::{
        self, DisableLineWrap, EnableLineWrap, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use std::{
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct Session;

impl Session {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let session = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            DisableLineWrap,
            EnableBracketedPaste,
            EnableFocusChange,
            Hide
        )?;
        Ok(session)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    // Attempt both output restoration and raw-mode restoration even if one fails.
    let _ = execute!(
        io::stdout(),
        EndSynchronizedUpdate,
        ResetColor,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste,
        DisableFocusChange,
        EnableLineWrap,
        Show,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

/// Restore the user's terminal before Rust prints a panic diagnostic.
pub fn install_panic_cleanup() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        restore();
        previous(information);
    }));
}

struct Signals {
    interrupted: Arc<AtomicBool>,
    #[cfg(unix)]
    ids: Vec<signal_hook::SigId>,
}

impl Signals {
    fn new() -> io::Result<Self> {
        let mut signals = Self {
            interrupted: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            ids: Vec::new(),
        };
        #[cfg(unix)]
        for signal in [
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGINT,
        ] {
            signals.ids.push(signal_hook::flag::register(
                signal,
                Arc::clone(&signals.interrupted),
            )?);
        }
        #[cfg(not(unix))]
        let _ = &mut signals;
        Ok(signals)
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

pub fn run(app: &mut App) -> io::Result<()> {
    let signals = Signals::new()?;
    let _session = Session::enter()?;
    let mut renderer = Renderer::default();
    let mut output = io::stdout();
    let mut redraw = true;
    while !app.should_quit() {
        if signals.interrupted.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminated by signal",
            ));
        }
        if redraw {
            let (width, height) = app.size();
            app.paint(renderer.frame(width, height)?)?;
            renderer.present(&mut output)?;
            redraw = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let started = Instant::now();
        // Coalesce bursts, but put a time and count bound on work before drawing.
        for _ in 0..128 {
            let event = event::read()?;
            if matches!(event, Event::FocusGained) {
                renderer.invalidate();
            }
            redraw |= app.handle(event);
            if app.should_quit()
                || started.elapsed() >= Duration::from_millis(4)
                || !event::poll(Duration::ZERO)?
            {
                break;
            }
        }
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IsTerminal;

    // Also run under a PTY by tools/terminal_smoke.py to exercise actual termios.
    #[test]
    fn panic_restores_terminal() {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return;
        }
        install_panic_cleanup();
        let result = std::panic::catch_unwind(|| {
            let _session = Session::enter().unwrap();
            assert!(terminal::is_raw_mode_enabled().unwrap());
            panic!("intentional terminal-cleanup test");
        });
        assert!(result.is_err());
        assert!(!terminal::is_raw_mode_enabled().unwrap());
    }
}
