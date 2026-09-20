//! Terminal lifetime, panic cleanup, signal notification, and the event loop.

use crate::{
    app::App,
    events::{AppEvent, BackgroundEvent, Runtime},
    screen::Renderer,
};
use crossterm::{
    cursor::{Hide, SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, Event,
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
        DisableMouseCapture,
        EnableLineWrap,
        Show,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

fn set_mouse_capture(output: &mut impl Write, enabled: bool) -> io::Result<()> {
    if enabled {
        crossterm::queue!(output, EnableMouseCapture)?;
        // Crossterm also requests every pointer movement. Button-event tracking
        // is sufficient here: report dragging and wheels, without idle motion.
        #[cfg(unix)]
        output.write_all(b"\x1b[?1003l\x1b[?1002h")?;
    } else {
        crossterm::queue!(output, DisableMouseCapture)?;
    }
    output.flush()
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
    let runtime = Runtime::start()?;
    app.editor.set_background_search(true);
    app.enable_jump_navigation();
    app.editor.set_background_syntax(true);
    app.editor.set_deferred_repeat(true);
    app.enable_lsp();
    app.enable_git();
    app.enable_file_polling(Instant::now());
    if let Some(update) = app.take_lsp_update() {
        runtime.update_lsp(update);
    }
    if let Some(update) = app.take_lsp_workspace_update() {
        runtime.update_lsp_workspace(update);
    }
    let mut renderer = Renderer::default();
    let mut output = io::stdout();
    let mut redraw = true;
    let mut mouse_enabled = false;
    while !app.should_quit() {
        if app.mouse_enabled() != mouse_enabled {
            set_mouse_capture(&mut output, app.mouse_enabled())?;
            mouse_enabled = app.mouse_enabled();
        }
        if signals.interrupted.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminated by signal",
            ));
        }
        // Deadline processing also runs after an idle wait, so automatic
        // completion never depends on another terminal event arriving.
        redraw |= app.advance_repeat();
        if let Some(job) = app.editor.take_search_job() {
            runtime.submit(job);
        }
        if let Some(update) = app.take_lsp_update() {
            runtime.update_lsp(update);
            redraw = true;
        }
        if let Some(update) = app.take_lsp_workspace_update() {
            runtime.update_lsp_workspace(update);
        }
        if let Some(batch) = app.take_git_batch(Instant::now()) {
            runtime.submit_git(batch);
        }
        if let Some(batch) = app.take_status_batch(Instant::now()) {
            runtime.submit_status(batch);
        }
        if let Some(batch) = app.take_file_poll(Instant::now()) {
            runtime.submit_file_poll(batch);
        }
        if let Some(job) = app.take_clipboard_job() {
            runtime.submit_clipboard(job);
        }
        if let Some(job) = app.take_prompt_completion_job() {
            runtime.submit_prompt(job);
        }
        while let Some(job) = app.take_git_write() {
            runtime.submit_git_write(job);
        }
        if redraw {
            let (width, height) = app.size();
            app.paint(renderer.frame(width, height)?)?;
            renderer.present(&mut output)?;
            if let Some(job) = app.take_syntax_batch() {
                runtime.submit_syntax(job);
            }
            if let Some(job) = app.take_picker_job() {
                runtime.submit_picker(job);
            }
            if let Some(job) = app.take_diagnostic_job() {
                runtime.submit_diagnostics(job);
            }
            if let Some(job) = app.take_location_job() {
                runtime.submit_locations(job);
            }
            if let Some(job) = app.take_workspace_edit() {
                runtime.submit_workspace_edit(job);
            }
            if let Some(job) = app.take_location_navigation() {
                runtime.submit_location_navigation(job);
            }
            if let Some(job) = app.take_symbol_job() {
                runtime.submit_symbols(job);
            }
            if let Some(job) = app.take_buffer_job() {
                runtime.submit_buffers(job);
            }
            if let Some(job) = app.take_jump_job() {
                runtime.submit_jumps(job);
            }
            if let Some(job) = app.take_jump_navigation() {
                runtime.submit_jump_navigation(job);
            }
            if let Some(job) = app.take_preview_job() {
                runtime.submit_preview(job);
            }
            redraw = false;
        }
        // Search debounce must expire even without an input event or repaint.
        // File-picker cleanup is submitted first, so it cannot replace this job.
        if let Some(job) = app.take_workspace_search_job(Instant::now()) {
            runtime.submit_workspace_search(job);
        }
        let timeout = if app.editor.repeat_ready() {
            Duration::ZERO
        } else {
            app.completion_deadline()
                .into_iter()
                .chain(app.signature_deadline())
                .chain(app.symbol_deadline())
                .chain(app.workspace_search_deadline())
                .chain(app.git_deadline())
                .chain(app.status_deadline())
                .chain(app.file_poll_deadline())
                .min()
                .map_or(Duration::from_millis(100), |deadline| {
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(100))
                })
        };
        let Some(mut event) = runtime.events.next(timeout, app.input_waiting()) else {
            continue;
        };
        let started = Instant::now();
        // Coalesce bursts, but put a time and count bound on work before drawing.
        for index in 0..128 {
            match event {
                AppEvent::MouseScroll(event, count) => redraw |= app.handle_mouse(event, count),
                AppEvent::Terminal(event) => {
                    if matches!(event, Event::FocusGained) {
                        renderer.invalidate();
                    }
                    redraw |= app.handle(event);
                }
                AppEvent::Background(BackgroundEvent::Search(result)) => {
                    redraw |= app.handle_search_result(result);
                }
                AppEvent::Background(BackgroundEvent::Syntax(result)) => {
                    redraw |= app.handle_syntax_results(result);
                }
                AppEvent::Background(BackgroundEvent::Files(result)) => {
                    redraw |= app.handle_picker_result(result)
                }
                AppEvent::Background(BackgroundEvent::Diagnostics(result)) => {
                    redraw |= app.handle_diagnostic_result(result);
                }
                AppEvent::Background(BackgroundEvent::Locations(result)) => {
                    redraw |= app.handle_location_result(result);
                }
                AppEvent::Background(BackgroundEvent::WorkspaceEdit(result)) => {
                    redraw |= app.handle_workspace_edit(result);
                }
                AppEvent::Background(BackgroundEvent::LocationNavigation(result)) => {
                    redraw |= app.handle_location_navigation(result);
                }
                AppEvent::Background(BackgroundEvent::Symbols(result)) => {
                    redraw |= app.handle_symbol_result(result)
                }
                AppEvent::Background(BackgroundEvent::Buffers(result)) => {
                    redraw |= app.handle_buffer_result(result)
                }
                AppEvent::Background(BackgroundEvent::Jumps(result)) => {
                    redraw |= app.handle_jump_result(result)
                }
                AppEvent::Background(BackgroundEvent::JumpNavigation(result)) => {
                    redraw |= app.handle_jump_navigation(result)
                }
                AppEvent::Background(BackgroundEvent::Prompt(result)) => {
                    redraw |= app.handle_prompt_completion(result);
                }
                AppEvent::Background(BackgroundEvent::WorkspaceSearch(result)) => {
                    redraw |= app.handle_workspace_search_result(result);
                }
                AppEvent::Background(BackgroundEvent::Preview(result)) => {
                    redraw |= app.handle_preview_result(result)
                }
                AppEvent::Background(BackgroundEvent::Git(result)) => {
                    redraw |= app.handle_git_result(result);
                }
                AppEvent::Background(BackgroundEvent::GitStatus(result)) => {
                    redraw |= app.handle_status_result(result);
                }
                AppEvent::Background(BackgroundEvent::FilePoll(result)) => {
                    redraw |= app.handle_file_poll(result, Instant::now());
                }
                AppEvent::Background(BackgroundEvent::Clipboard(result)) => {
                    redraw |= app.handle_clipboard_result(result);
                }
                AppEvent::Lsp(event) => redraw |= app.handle_lsp_event(event),
                AppEvent::GitWrite(result) => redraw |= app.handle_git_write(result),
                AppEvent::Failed(error) => return Err(error),
            }
            if let Some(job) = app.editor.take_search_job() {
                runtime.submit(job);
            }
            if let Some(job) = app.take_clipboard_job() {
                runtime.submit_clipboard(job);
            }
            if let Some(job) = app.take_prompt_completion_job() {
                runtime.submit_prompt(job);
            }
            if let Some(update) = app.take_lsp_update() {
                runtime.update_lsp(update);
            }
            if let Some(update) = app.take_lsp_workspace_update() {
                runtime.update_lsp_workspace(update);
            }
            if let Some(batch) = app.take_git_batch(Instant::now()) {
                runtime.submit_git(batch);
            }
            if let Some(batch) = app.take_status_batch(Instant::now()) {
                runtime.submit_status(batch);
            }
            while let Some(job) = app.take_git_write() {
                runtime.submit_git_write(job);
            }
            if let Some(job) = app.take_picker_job() {
                runtime.submit_picker(job);
            }
            if let Some(job) = app.take_diagnostic_job() {
                runtime.submit_diagnostics(job);
            }
            if let Some(job) = app.take_location_job() {
                runtime.submit_locations(job);
            }
            if let Some(job) = app.take_workspace_edit() {
                runtime.submit_workspace_edit(job);
            }
            if let Some(job) = app.take_location_navigation() {
                runtime.submit_location_navigation(job);
            }
            if let Some(job) = app.take_symbol_job() {
                runtime.submit_symbols(job);
            }
            if let Some(job) = app.take_buffer_job() {
                runtime.submit_buffers(job);
            }
            if let Some(job) = app.take_jump_job() {
                runtime.submit_jumps(job);
            }
            if let Some(job) = app.take_jump_navigation() {
                runtime.submit_jump_navigation(job);
            }
            if let Some(job) = app.take_workspace_search_job(Instant::now()) {
                runtime.submit_workspace_search(job);
            }
            if let Some(job) = app.take_preview_job() {
                runtime.submit_preview(job);
            }
            if app.should_quit() || index == 127 || started.elapsed() >= Duration::from_millis(4) {
                break;
            }
            let Some(next) = runtime.events.next(Duration::ZERO, app.input_waiting()) else {
                break;
            };
            event = next;
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
            set_mouse_capture(&mut io::stdout(), true).unwrap();
            assert!(terminal::is_raw_mode_enabled().unwrap());
            panic!("intentional terminal-cleanup test");
        });
        assert!(result.is_err());
        assert!(!terminal::is_raw_mode_enabled().unwrap());
    }
}
