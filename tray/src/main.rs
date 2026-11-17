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
use disp::tr;

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
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: the icon is already in the tray",
                    "disp-tray: значок уже в трее",
                    "disp-tray: значок уже в треї"
                )
            );
            told(&Outcome::Running);
            return 0;
        }
        Err(err) => {
            let lock = store.paths.tray_lock();
            let lock = lock.display();
            let text = tr!(
                "could not take the lock {lock}: {err}",
                "не удалось взять замок {lock}: {err}",
                "не вдалося взяти замок {lock}: {err}"
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
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: cannot check whether the icon is running: {err}",
                    "disp-tray: не проверить, запущен ли значок: {err}",
                    "disp-tray: не перевірити, чи запущено значок: {err}"
                )
            );
            return 1;
        }
    }
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: cannot find its own program: {err}",
                    "disp-tray: не найти свою программу: {err}",
                    "disp-tray: не знайти власну програму: {err}"
                )
            );
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
            let exe = exe.display();
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: could not start {exe}: {err}",
                    "disp-tray: не удалось запустить {exe}: {err}",
                    "disp-tray: не вдалося запустити {exe}: {err}"
                )
            );
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
    tr!(
        "disp-tray {VERSION} — tray icon of the AI agent session dispatcher

  disp-tray                  show the icon (usually via `disp tray`)
  disp-tray --autostart      show the icon at login
  disp-tray --no-autostart   stop showing it at login
  disp-tray --detach         start the icon in the background and exit once it is shown
                             (this is how disp starts it); errors of the running icon go
                             to tray.log in the state directory

The icon is a tile with the number of sessions, coloured by precedence: yellow — waiting
for you; blue — working; green — nothing is working, finished ones are left; grey —
idle. An empty grey frame — no open sessions.
The menu follows the table order: waiting for you, working, idle, done.
On Linux the icon is shown by a shell that supports StatusNotifierItem; GNOME needs
the AppIndicator extension for it.
Language: DISP_LANG=en|ru|uk, otherwise the system language.
",
        "disp-tray {VERSION} — значок диспетчера сессий AI-агентов в трее

  disp-tray                  показать значок (обычно через `disp tray`)
  disp-tray --autostart      показывать значок при входе в систему
  disp-tray --no-autostart   больше не показывать при входе
  disp-tray --detach         запустить значок в фоне и выйти, когда он показался (так
                             его запускает disp); ошибки работающего значка — в
                             tray.log в каталоге состояния

Значок — плашка с числом сессий, цвет по старшинству: жёлтый — ждут тебя;
синий — работают; зелёный — не работает ничего, остались готовые; серый —
простаивают. Пустая серая рамка — открытых сессий нет.
Меню в том же порядке, что таблица: ждут тебя, работают, простой, готово.
На Linux значок показывает оболочка с поддержкой StatusNotifierItem; в GNOME для этого
нужно расширение AppIndicator.
Язык: DISP_LANG=en|ru|uk, иначе — язык системы.
",
        "disp-tray {VERSION} — значок диспетчера сесій AI-агентів у треї

  disp-tray                  показати значок (зазвичай через `disp tray`)
  disp-tray --autostart      показувати значок під час входу в систему
  disp-tray --no-autostart   більше не показувати під час входу
  disp-tray --detach         запустити значок у фоні й вийти, щойно він з'явився (так
                             його запускає disp); помилки робочого значка — у
                             tray.log у каталозі стану

Значок — плашка з кількістю сесій, колір за старшинством: жовтий — чекають на тебе;
синій — працюють; зелений — нічого не працює, лишилися готові; сірий —
простоюють. Порожня сіра рамка — відкритих сесій немає.
Меню в тому ж порядку, що й таблиця: чекають на тебе, працюють, простій, готово.
У Linux значок показує оболонка з підтримкою StatusNotifierItem; у GNOME для цього
потрібне розширення AppIndicator.
Мова: DISP_LANG=en|ru|uk, інакше — мова системи.
"
    )
}
