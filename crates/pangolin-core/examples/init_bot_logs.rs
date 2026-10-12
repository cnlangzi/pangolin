//! Write the dev bot-log fixture used by `make init-data`.
//!
//! Overwrites the last [`pangolin_core::bot_log::DEV_FIXTURE_DAYS`] UTC
//! files under `--dir` (default `./logs/bots`). Older files stay.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut dir = PathBuf::from("./logs/bots");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--dir" {
            let Some(value) = args.next() else {
                eprintln!("init-data: --dir needs a path");
                return ExitCode::from(2);
            };
            dir = PathBuf::from(value);
        } else {
            eprintln!("usage: init_bot_logs [--dir PATH]");
            return ExitCode::from(2);
        }
    }

    let today = chrono::Utc::now().date_naive();
    match pangolin_core::bot_log::write_dev_fixture(&dir, today) {
        Ok(n) => {
            println!(
                "init-data: wrote {n} bot log files ending {today} under {}",
                dir.display()
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("init-data: {err}");
            ExitCode::from(1)
        }
    }
}
