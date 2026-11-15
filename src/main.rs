//! disp — диспетчер сессий AI-агентов (Claude Code, Codex CLI, Gemini CLI).
//!
//! Хуки агентов вызывают `disp hook <агент>`; программа ведёт по одному JSON-файлу
//! на сессию и показывает общую таблицу: что работает, что ждёт человека, что закончено
//! и ещё не принято. Формат файлов и команды те же, что у прежней bash-версии.
//!
//! Состояние: `${XDG_STATE_HOME:-~/.local/state}/disp/`.
//! Настройки: `${XDG_CONFIG_HOME:-~/.config}/disp/env` (строки `КЛЮЧ=значение`).

mod board;
mod cli;
mod config;
mod detect;
mod error;
mod hook;
mod install;
mod model;
mod notify;
mod panes;
mod store;
mod text;
mod timeline;
mod tui;

use std::process::ExitCode;

/// Версия программы в виде `0.8.001`: третья цифра — номер сборки, всегда тремя
/// знаками. Собирается в `build.rs` из версии в `Cargo.toml`.
pub const VERSION: &str = env!("DISP_VERSION");

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
        "yes" => cli::yes(rest),
        "name" => cli::name(rest),
        "log" => cli::log(rest),
        "gc" => cli::gc(rest),
        "label" => cli::label(),
        "unlabel" => cli::unlabel(),
        "repair" => cli::repair(),
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
