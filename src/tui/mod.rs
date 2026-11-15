//! Живая панель `disp watch`: список сессий с выбором строки и клавишами.
//!
//! Панель только читает файлы сессий и пишет в них то же, что команды `disp ok`
//! и `disp name`. Сессии перечитываются, когда меняется каталог с их файлами,
//! и раз в секунду — чтобы шли возрасты.

mod app;
mod gfx;
mod view;

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use chrono::Local;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use self::app::{App, Command, Mode};
use self::gfx::{CellSize, Gfx};
use crate::board::{self, Accepted, Board, Row};
use crate::cli;
use crate::config::{self, Paths, Settings};
use crate::error::Result;
use crate::panes;
use crate::store::{self, Signature};
use crate::text::warn;
use crate::timeline::History;

/// Как часто панель проверяет клавиши и файлы сессий.
const POLL: Duration = Duration::from_millis(200);
/// Сколько ждать, пока панель сама выйдет по сигналу или после пропажи терминала,
/// прежде чем завершить процесс.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// Как часто проверять, что терминал ещё есть.
const TERMINAL_CHECK: Duration = Duration::from_secs(2);
/// Сколько последних записей журнала показывает панель.
const JOURNAL_LINES: usize = 500;
/// Сколько последних записей журнала идёт в ленты времени.
const HISTORY_LINES: usize = 20_000;

/// `disp watch [--plain] [сек]`: живая панель. Без терминала или с `--plain` —
/// простая таблица, которая перерисовывается раз в `сек` секунд.
pub fn watch(args: &[String]) -> u8 {
    let plain = args.iter().any(|arg| arg == "--plain");
    if plain || !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        let seconds = args
            .iter()
            .find_map(|arg| arg.parse::<f64>().ok())
            .filter(|value| *value > 0.0);
        return cli::watch_plain(Duration::from_secs_f64(seconds.unwrap_or(2.0)));
    }
    match run() {
        Ok(()) => 0,
        Err(err) => {
            warn(&format!("disp watch: {err}"));
            1
        }
    }
}

/// Занимает терминал, крутит цикл панели и возвращает терминал в прежний вид.
fn run() -> Result<()> {
    // SIGTERM и SIGHUP: выходим через общий путь, чтобы терминал был восстановлен.
    // Но если терминал пропал, главный поток может застрять в чтении клавиш: crossterm
    // на конце потока и на EIO читает по кругу и к проверке флага не возвращается,
    // занимая ядро целиком. Поэтому через секунду процесс завершается сам.
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || stop_then_exit(&flag));
    watch_terminal(Arc::clone(&stop));

    // О поддержке картинок терминал нужно спросить до того, как начнётся чтение клавиш.
    enable_raw_mode()?;
    let mut gfx = graphics(&Settings::load(&Paths::from_env().conf_file));
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = disable_raw_mode();
            return Err(err.into());
        }
    };
    let outcome = event_loop(&mut terminal, &stop, &mut gfx);
    if let Some(gfx) = gfx.as_mut() {
        let _ = send(&gfx.clear());
    }
    ratatui::restore();
    outcome
}

/// Просит цикл панели остановиться, а если он не успел за `EXIT_GRACE`, завершает процесс.
/// Выход из `run` завершает процесс раньше, и до `process::exit` дело не доходит.
fn stop_then_exit(stop: &AtomicBool) {
    stop.store(true, Ordering::Relaxed);
    thread::sleep(EXIT_GRACE);
    process::exit(1);
}

/// Сторож терминала: когда терминал закрыли, а сигнал до панели не дошёл (она не в
/// переднем плане своего терминала), панель всё равно завершается. У пропавшего
/// терминала `isatty` ложно: ядро отвечает на запросы к нему ошибкой EIO.
fn watch_terminal(stop: Arc<AtomicBool>) {
    let spawned = thread::Builder::new()
        .name("disp-tty".to_string())
        .spawn(move || {
            loop {
                thread::sleep(TERMINAL_CHECK);
                if !io::stdin().is_terminal() {
                    stop_then_exit(&stop);
                }
            }
        });
    if let Err(err) = spawned {
        warn(&format!("disp watch: сторож терминала не запущен: {err}"));
    }
}

/// Решает, рисовать ли ленты картинками. `DISP_GFX=0` выключает картинки, `DISP_GFX=1`
/// включает без вопроса к терминалу; иначе терминал спрашивают, знает ли он протокол.
/// Без размера знакоместа в пикселях картинки невозможны в любом случае.
fn graphics(settings: &Settings) -> Option<Gfx> {
    let wanted = match settings.get("DISP_GFX") {
        Some("0") => false,
        Some("1") => true,
        _ => gfx::probe(),
    };
    if !wanted {
        return None;
    }
    CellSize::detect().map(Gfx::new)
}

/// Пишет команды прямо в терминал, мимо отрисовки панели.
fn send(commands: &str) -> io::Result<()> {
    if commands.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(commands.as_bytes())?;
    out.flush()
}

/// Цикл панели: перечитать сессии, если они изменились, нарисовать, обработать клавишу.
fn event_loop(
    terminal: &mut DefaultTerminal,
    stop: &AtomicBool,
    gfx: &mut Option<Gfx>,
) -> Result<()> {
    let paths = Paths::from_env();
    // Записи прежних версий: задача могла быть названа по временному каталогу агента.
    board::repair_roots(&Board::load());
    let mut app = App::default();
    let mut seen: Option<(Signature, i64)> = None;
    let mut history_seen: Option<Option<(SystemTime, u64)>> = None;
    let mut reload = true;
    let mut redraw = true;
    while !stop.load(Ordering::Relaxed) {
        let stamp = (store::dir_signature(&paths.sessions), config::now());
        if reload || seen.as_ref() != Some(&stamp) {
            let board = Board::load();
            app.set_rows(board.visible(app.show_all), board.now);
            seen = Some(stamp);
            reload = false;
            redraw = true;
            // Журнал для лент перечитывается, только когда он изменился.
            let journal = store::file_stamp(&paths.events());
            if app.timeline && history_seen != Some(journal) {
                app.history = History::from_events(&board::journal(&paths, HISTORY_LINES));
                history_seen = Some(journal);
            }
        }
        if redraw {
            let local = Local::now();
            app.utc_offset = i64::from(local.offset().local_minus_utc());
            app.cell_px = gfx.as_ref().map(|gfx| gfx.cell.width);
            let clock = local.format("%H:%M:%S").to_string();
            terminal.draw(|frame| view::draw(frame, &mut app, &clock))?;
            if let Some(gfx) = gfx.as_mut() {
                send(&gfx.sync(&app.lanes))?;
            }
            redraw = false;
        }
        if !event::poll(POLL)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                redraw = true;
                match app.on_key(key) {
                    Some(Command::Quit) => break,
                    Some(command) => {
                        execute(command, &mut app, &paths);
                        reload = true;
                    }
                    None => {}
                }
            }
            Event::Resize(..) => {
                // При смене размера панель очищает экран, а с ним пропадают и картинки.
                if let Some(gfx) = gfx.as_mut() {
                    send(&gfx.reset())?;
                }
                redraw = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Выполняет действие над выбранной строкой и сообщает результат в нижней строке.
fn execute(command: Command, app: &mut App, paths: &Paths) {
    match command {
        Command::Quit | Command::Reload => {}
        Command::Journal => {
            app.journal = board::journal(paths, JOURNAL_LINES);
            app.mode = Mode::Journal { scroll: 0 };
        }
        Command::Focus => {
            let outcome = app.selected_row().map(|row| panes::focus(&row.session));
            match outcome {
                Some(Ok(text)) => app.notify(text, false),
                Some(Err(text)) => app.notify(text, true),
                None => {}
            }
        }
        Command::Approve => {
            // Нажатие уходит в панель агента: решаем по только что перечитанной записи.
            let Some((_board, _lock, row)) = fresh(app) else {
                return;
            };
            match panes::approve(&row.session) {
                Ok(text) => app.notify(text, false),
                Err(text) => app.notify(text, true),
            }
        }
        Command::Accept { force } => {
            let Some((board, _lock, mut row)) = fresh(app) else {
                return;
            };
            match board::accept(&mut row, board.now, force) {
                Ok(Accepted::Yes) => app.notify(format!("убрано: {}", row.summary()), false),
                // Пока мы смотрели на экран, сессия снова заработала: спрашиваем.
                Ok(Accepted::StillActive) => app.mode = Mode::Confirm,
                Err(err) => app.notify(format!("не записано: {err}"), true),
            }
        }
        Command::Unaccept => {
            let Some((_board, _lock, mut row)) = fresh(app) else {
                return;
            };
            match board::unaccept(&mut row) {
                Ok(()) => app.notify(format!("возвращено в список: {}", row.summary()), false),
                Err(err) => app.notify(format!("не записано: {err}"), true),
            }
        }
        Command::Rename(title) => {
            let Some((board, _lock, mut row)) = fresh(app) else {
                return;
            };
            match board::rename(&mut row, &title) {
                Ok(()) => {
                    if panes::enabled(&board.settings) && !row.session.ended {
                        panes::apply(&row.session, true);
                    }
                    app.notify(format!("названо: {title}"), false);
                }
                Err(err) => app.notify(format!("не записано: {err}"), true),
            }
        }
    }
}

/// Выбранная строка, заново прочитанная с диска под блокировкой хранилища. Блокировка
/// держится, пока жив второй элемент результата.
fn fresh(app: &mut App) -> Option<(Board, Option<File>, Row)> {
    let path = app.selected_row()?.path.clone();
    let board = Board::load();
    match board.fresh(&path) {
        Some((lock, row)) => Some((board, lock, row)),
        None => {
            app.notify("этой сессии уже нет в списке", true);
            None
        }
    }
}
