//! `disp-tray`: значок диспетчера сессий AI-агентов в трее — на Linux (StatusNotifierItem
//! по D-Bus) и на macOS (строка меню).
//!
//! Цвет значка говорит, нужен ли ты агентам, число — сколько их; меню — сессии с кнопками
//! «ответить да», «перейти к панели», «убрать». Данные — те же файлы, что у `disp watch`,
//! и та же библиотека `disp`.

mod actions;
mod autostart;
mod icon;
mod menu;
mod model;
mod run;

use std::process::ExitCode;

use disp::VERSION;
use disp::board::Board;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => start(),
        Some("--autostart") => autostart::enable(),
        Some("--no-autostart") => autostart::disable(),
        Some("-h" | "--help") => {
            print!("{}", usage());
            0
        }
        Some("-V" | "--version") => {
            println!("disp-tray {VERSION}");
            0
        }
        Some(_) => {
            eprint!("{}", usage());
            2
        }
    };
    ExitCode::from(code)
}

/// Показывает значок, если его ещё нет.
fn start() -> u8 {
    let store = Board::load().store;
    // Замок держится до выхода программы: второй значок не появится.
    let _lock = match store.tray_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("disp-tray: значок уже в трее");
            return 0;
        }
        Err(err) => {
            eprintln!(
                "disp-tray: не удалось взять замок {}: {err}",
                store.paths.tray_lock().display()
            );
            return 1;
        }
    };
    run::run(run::App::new(store))
}

/// Справка.
fn usage() -> String {
    format!(
        "disp-tray {VERSION} — значок диспетчера сессий AI-агентов в трее

  disp-tray                  показать значок (обычно через `disp tray`)
  disp-tray --autostart      показывать значок при входе в систему
  disp-tray --no-autostart   больше не показывать при входе

Цвет значка: жёлтый — кто-то ждёт тебя (число — сколько), зелёный — есть готовые,
голубое кольцо — сессии работают, серое — открытых сессий нет.
На Linux значок показывает оболочка с поддержкой StatusNotifierItem; в GNOME для этого
нужно расширение AppIndicator.
"
    )
}
