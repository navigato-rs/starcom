#![cfg_attr(
    all(not(debug_assertions), feature = "gui"),
    windows_subsystem = "windows"
)]

use std::{env, fs, io, path, process};

use anyhow::Context;
use starcom::{core, replay};

const HELP: &str = "Starcom: Session Terminal And Remote COMmander\n\
\n\
Tabbed desktop client, with headless replay and read-only SSH inspection tools.\n\
\n\
Usage: starcom [--demo] [--snapshot PNG]\n\
       starcom --replay FILE [--size COLSxROWS]\n\
       starcom --help\n\
\n\
Use - as FILE to read a synthetic control-mode transcript from stdin.\n\
The default pane size is 80x24. All replay panes use the same fixed size.\n\
Output rows are quoted/escaped so remote text cannot control this terminal.\n";

fn main() -> process::ExitCode {
    match run() {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("starcom: {}", format!("{error:#}").escape_debug());
            process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut arguments = env::args_os().skip(1);
    let mut input = None;
    let mut demo = false;
    let mut snapshot = None;
    let mut size = core::Size::default();
    let mut explicit_size = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--help" | "-h") => {
                print!("{HELP}");
                return Ok(());
            }
            Some("--version") => {
                println!("starcom {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--demo") => {
                anyhow::ensure!(!demo, "--demo supplied twice");
                demo = true;
            }
            Some("--snapshot") => {
                anyhow::ensure!(snapshot.is_none(), "--snapshot supplied twice");
                snapshot = Some(path::PathBuf::from(
                    arguments.next().context("--snapshot needs a PNG path")?,
                ));
            }
            Some("--replay") => {
                anyhow::ensure!(input.is_none(), "--replay was supplied more than once");
                input = Some(path::PathBuf::from(
                    arguments.next().context("--replay needs a file")?,
                ));
            }
            Some("--size") => {
                anyhow::ensure!(!explicit_size, "--size supplied twice");
                explicit_size = true;
                let argument = arguments.next().context("--size needs COLSxROWS")?;
                let text = argument.to_str().context("size must be UTF-8")?;
                let (columns, rows) = text.split_once('x').context("size must be COLSxROWS")?;
                size = core::Size::new(columns.parse()?, rows.parse()?)?;
            }
            _ => anyhow::bail!("unrecognized argument {argument:?}; use --help"),
        }
    }
    anyhow::ensure!(
        input.is_none() || (!demo && snapshot.is_none()),
        "--replay cannot be combined with desktop options"
    );
    anyhow::ensure!(
        !explicit_size || input.is_some(),
        "--size requires --replay"
    );
    let Some(input) = input else {
        #[cfg(feature = "gui")]
        {
            let _ = env_logger::try_init();
            if let Some(path) = snapshot {
                anyhow::ensure!(
                    demo,
                    "--snapshot requires --demo; real sessions are never captured implicitly"
                );
                return starcom::desktop::save_demo(&path);
            }
            let _support = (!demo).then(|| {
                navigato_support::init(
                    starcom::SUPPORT,
                    (!cfg!(debug_assertions))
                        .then(|| navigato_support::state_directory(navigato_support::App::Starcom))
                        .flatten(),
                )
            });
            let result = starcom::desktop::run(if demo {
                starcom::desktop::Startup::Demo
            } else {
                starcom::desktop::Startup::ConnectionForm
            });
            if result.is_err() {
                navigato_support::failure(navigato_support::Failure::DesktopExit);
            }
            return result;
        }
        #[cfg(not(feature = "gui"))]
        {
            anyhow::ensure!(
                !demo && snapshot.is_none(),
                "desktop support requires the gui feature"
            );
            print!("{HELP}");
            return Ok(());
        }
    };
    let mut reader: Box<dyn io::Read> = if input == path::Path::new("-") {
        Box::new(io::stdin())
    } else {
        Box::new(fs::File::open(&input).with_context(|| format!("open {input:?}"))?)
    };
    let mut replay = replay::Replay::new(size);
    let mut buffer = [0; 4096];
    let mut total = 0usize;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count;
        anyhow::ensure!(
            total <= 16 * 1024 * 1024,
            "replay exceeds the 16 MiB input budget"
        );
        replay.feed(&buffer[..count])?;
    }
    replay.finish()?;
    println!(
        "Starcom replay: {} panes, {}x{} cells (no network connection)",
        replay.panes().len(),
        size.columns(),
        size.rows()
    );
    for (pane, terminal) in replay.panes() {
        println!("Pane {pane}:");
        for (row, text) in terminal.screen_lines().iter().enumerate() {
            if !text.is_empty() {
                println!("  {row:>3}: {text:?}");
            }
        }
    }
    Ok(())
}
