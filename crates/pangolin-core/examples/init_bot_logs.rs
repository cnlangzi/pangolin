//! Write the dev bot-log fixture used by `make init-data`.
//!
//! Overwrites the last [`pangolin_core::bot_log::DEV_FIXTURE_DAYS`] UTC
//! files under `--dir` (default `./logs/bots`). Older files stay.
//!
//! `--force` replaces the safety check in
//! [`pangolin_core::bot_log::write_dev_fixture`]: without it, an
//! existing file that does not look like the dev fixture is left
//! alone and the command exits non-zero. The Makefile recipe
//! passes `--force` because `init-data` is the dev convenience
//! for replacing the previous run's data; ad-hoc uses (e.g.
//! from the CLI) get the safety check for free.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut dir = PathBuf::from("./logs/bots");
    let mut force = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--dir" {
            let Some(value) = args.next() else {
                eprintln!("init-data: --dir needs a path");
                return ExitCode::from(2);
            };
            dir = PathBuf::from(value);
        } else if arg == "--force" {
            force = true;
        } else {
            eprintln!("usage: init_bot_logs [--dir PATH] [--force]");
            return ExitCode::from(2);
        }
    }

    let today = chrono::Utc::now().date_naive();
    match pangolin_core::bot_log::write_dev_fixture(&dir, today, force) {
        Ok(n) => {
            println!(
                "init-data: wrote {n} bot log files ending {today} under {}",
                dir.display()
            );
            ExitCode::SUCCESS
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            eprintln!("init-data: {err}");
            eprintln!("hint: pass --force to overwrite the existing file");
            ExitCode::from(3)
        }
        Err(err) => {
            eprintln!("init-data: {err}");
            ExitCode::from(1)
        }
    }
}
