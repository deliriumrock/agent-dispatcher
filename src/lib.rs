//! disp — диспетчер сессий AI-агентов (Claude Code, Codex CLI, Gemini CLI).
//!
//! Хуки агентов вызывают `disp hook <агент>`; программа ведёт по одному JSON-файлу
//! на сессию и показывает общую таблицу: что работает, что ждёт человека, что закончено
//! и ещё не принято. Формат файлов и команды те же, что у прежней bash-версии.
//!
//! Состояние: `${XDG_STATE_HOME:-~/.local/state}/disp/`.
//! Настройки: `${XDG_CONFIG_HOME:-~/.config}/disp/env` (строки `КЛЮЧ=значение`).
//!
//! Этот крейт — ядро диспетчера: хук, хранилище, доска и текстовая панель. Им пользуются
//! программа `disp` и графическая оболочка `disp-gui`.

pub mod board;
pub mod cli;
pub mod config;
pub mod detect;
pub mod error;
#[cfg(test)]
mod fakeproc;
pub mod handover;
pub mod hook;
pub mod i18n;
pub mod install;
pub mod model;
pub mod notify;
pub mod observe;
pub mod panes;
pub mod procs;
pub mod store;
pub mod text;
pub mod timeline;
pub mod tui;

/// Версия программы в виде `0.8.001`: третья цифра — номер сборки, всегда тремя
/// знаками. Собирается в `build.rs` из версии в `Cargo.toml`; у `disp` и
/// `disp-gui` она общая.
pub const VERSION: &str = env!("DISP_VERSION");
