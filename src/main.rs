//! Программа `disp`: разбирает команду и передаёт её ядру диспетчера.

use std::process::ExitCode;

use disp::{VERSION, cli, hook, install, text, tui};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => ("ls", &[][..]),
    };
    let arg = |index: usize| rest.get(index).map(String::as_str).unwrap_or_default();
    let code = match command {
        "hook" => {
            hook::run(arg(0), arg(1));
            0
        }
        "ls" | "list" => cli::ls(rest),
        "watch" => tui::watch(rest),
        "show" => cli::show(rest),
        "ok" | "accept" => cli::ok(rest),
        "name" => cli::name(rest),
        "log" => cli::log(rest),
        "gc" => cli::gc(rest),
        "label" => cli::label(),
        "unlabel" => cli::unlabel(),
        "repair" => cli::repair(),
        "gui" => cli::gui(rest),
        "config" => install::config(rest),
        "install" => install::install(rest),
        "doctor" => cli::doctor(),
        "-h" | "--help" | "help" => {
            text::emit(&cli::usage());
            0
        }
        "-V" | "--version" | "version" => {
            text::emit(&format!("disp {VERSION}\n"));
            0
        }
        _ => {
            text::warn(cli::usage().trim_end());
            2
        }
    };
    ExitCode::from(code)
}
