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
mod startup;

use std::env;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, ExitCode, Stdio};

use disp::VERSION;
use disp::board::Board;
use disp::cli::TRAY_RUNNING_EXIT;

use startup::Outcome;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => start(false),
        Some(startup::CHILD) => start(true),
        Some("--detach") => detach(),
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

/// Показывает значок, если его ещё нет. `report` — сообщить итог запуска в канал
/// `--detach` (см. `startup`); ошибки в любом случае пишутся в stderr.
fn start(report: bool) -> u8 {
    let told = move |outcome: &Outcome| {
        if report {
            startup::report(outcome);
        }
    };
    let store = Board::load().store;
    // Замок держится до выхода программы: второй значок не появится.
    let _lock = match store.tray_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("disp-tray: значок уже в трее");
            told(&Outcome::Running);
            return 0;
        }
        Err(err) => {
            let text = format!(
                "не удалось взять замок {}: {err}",
                store.paths.tray_lock().display()
            );
            eprintln!("disp-tray: {text}");
            told(&Outcome::Failed(text));
            return 1;
        }
    };
    run::run(run::App::new(store), told)
}

/// `disp-tray --detach`: запускает значок отдельным процессом и выходит, как только
/// значок показался.
///
/// Так значок запускает `disp`: он ждёт этот короткий процесс и узнаёт, удался ли
/// запуск, а сам значок остаётся без родителя и не становится ни дочерним процессом
/// панели, ни зомби. `--detach` ждёт, пока значок сообщит, показался ли он (не дольше
/// `startup::TIMEOUT`). Код выхода 0 — значок показан, `TRAY_RUNNING_EXIT` — уже в трее,
/// иначе — ошибка с текстом в stderr. Что значок пишет о своих ошибках потом,
/// попадает в `tray.log` в каталоге состояния.
fn detach() -> u8 {
    let store = Board::load().store;
    match store.tray_lock() {
        Ok(Some(_free)) => {}
        Ok(None) => return TRAY_RUNNING_EXIT as u8,
        Err(err) => {
            eprintln!("disp-tray: не проверить, запущен ли значок: {err}");
            return 1;
        }
    }
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("disp-tray: не найти свою программу: {err}");
            return 1;
        }
    };
    let log_path = store.paths.tray_log();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_or_else(|_| Stdio::null(), Stdio::from);
    let started = Command::new(&exe)
        .arg(startup::CHILD)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(err) => {
            eprintln!("disp-tray: не удалось запустить {}: {err}", exe.display());
            return 1;
        }
    };
    match startup::wait(&mut child, startup::TIMEOUT, &log_path) {
        Outcome::Shown => 0,
        Outcome::Running => TRAY_RUNNING_EXIT as u8,
        Outcome::Failed(text) => {
            eprintln!("disp-tray: {text}");
            1
        }
    }
}

/// Справка.
fn usage() -> String {
    format!(
        "disp-tray {VERSION} — значок диспетчера сессий AI-агентов в трее

  disp-tray                  показать значок (обычно через `disp tray`)
  disp-tray --autostart      показывать значок при входе в систему
  disp-tray --no-autostart   больше не показывать при входе
  disp-tray --detach         запустить значок в фоне и сразу выйти (так его запускает
                             disp); ошибки работающего значка — в tray.log в каталоге
                             состояния

Значок, по старшинству, с числом сессий: жёлтый — ждут тебя; синий — работают;
зелёный — не работает ничего, остались готовые; серый — простаивают.
Пустое серое кольцо — открытых сессий нет.
Меню в том же порядке, что таблица: ждут тебя, работают, простой, готово.
На Linux значок показывает оболочка с поддержкой StatusNotifierItem; в GNOME для этого
нужно расширение AppIndicator.
"
    )
}
