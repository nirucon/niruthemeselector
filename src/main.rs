//! NIRU Theme Selector — native graphical theme browser and manager.

mod backend;
mod text;
mod theme;
mod thumbs;
mod window;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("open") => run_window(),
        Some("status") => print_status(),
        Some("list") => print_list(),
        Some("--version") => {
            println!("NIRUTHEMESELECTOR {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help") => {
            usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("niruthemeselector: unknown command: {other}");
            usage();
            ExitCode::from(2)
        }
    }
}

fn usage() {
    println!("NIRUTHEMESELECTOR {}", env!("CARGO_PKG_VERSION"));
    println!("Usage: niruthemeselector [open]");
    println!("       niruthemeselector status  print the active theme and catalog");
    println!("       niruthemeselector list    print available themes");
    println!("       niruthemeselector --version | --help");
    println!("       Open the window to preview, apply, import, remove or hide themes.");
}

fn print_status() -> ExitCode {
    match backend::status_text() {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("niruthemeselector: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_list() -> ExitCode {
    match backend::catalog() {
        Ok(catalog) => {
            for entry in catalog.themes {
                let origin = if entry.overridden {
                    "built-in override"
                } else if entry.builtin {
                    "built-in"
                } else {
                    "user"
                };
                println!(
                    "{}\t{}\t{origin}{}{}",
                    entry.id,
                    entry.name,
                    if entry.hidden { "\thidden" } else { "" },
                    if entry.valid { "" } else { "\tinvalid" }
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("niruthemeselector: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_window() -> ExitCode {
    let state = match window::State::load() {
        Ok(state) => state,
        Err(error) => {
            eprintln!("niruthemeselector: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut app = match window::App::open() {
        Ok(app) => app,
        Err(error) => {
            eprintln!("niruthemeselector: cannot open a window: {error}");
            return ExitCode::FAILURE;
        }
    };
    match window::run(&mut app, state) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("niruthemeselector: {error}");
            ExitCode::FAILURE
        }
    }
}
