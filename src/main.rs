use std::{env, process::ExitCode};

const HELP: &str = "herdr-p4 - compact Perforce review pane for Herdr\n\n\
Usage:\n  herdr-p4 --version\n  herdr-p4 --help\n\n\
The interactive Herdr pane entrypoint is not available in this foundation build.";

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some(arg), None) if arg == "--version" || arg == "-V" => {
            println!("herdr-p4 {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        (None, None) | (Some(_), None)
            if env::args_os()
                .nth(1)
                .is_none_or(|arg| arg == "--help" || arg == "-h") =>
        {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("unsupported arguments; run `herdr-p4 --help`");
            ExitCode::from(64)
        }
    }
}
