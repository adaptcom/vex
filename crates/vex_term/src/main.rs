use std::{
    env,
    ffi::OsString,
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
};
use vex_term::{app::App, terminal};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vex: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let mut path = None;
    let mut literal = false;
    for argument in env::args_os().skip(1) {
        if !literal && (argument == "--help" || argument == "-h") {
            println!(
                "Vex — a terminal text editor\n\nUsage: vex [--] [FILE]\n\nOpen a UTF-8 file, create a new file, or start a scratch buffer.\n\ni/a  insert/append    Esc  normal mode    v  select mode\nhjkl / arrows  move  w/b/e  word motions  u/U  undo/redo\n:w [PATH]  save       :q  quit            :q!  discard and quit\n:wq  save and quit    :help [COMMAND]     Ctrl-s/Ctrl-q  save/quit"
            );
            return Ok(());
        }
        if !literal && argument == "--version" {
            println!("vex {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        if !literal && argument == "--" {
            literal = true;
            continue;
        }
        if !literal && argument.to_string_lossy().starts_with('-') {
            return Err(invalid_argument(argument));
        }
        if path.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected at most one file; use --help",
            ));
        }
        path = Some(PathBuf::from(argument));
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive input and output terminals are required",
        ));
    }
    let mut app = App::open(path.as_deref(), crossterm::terminal::size()?)?;
    terminal::install_panic_cleanup();
    terminal::run(&mut app)
}

fn invalid_argument(argument: OsString) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown option {:?}; use --help", argument),
    )
}
