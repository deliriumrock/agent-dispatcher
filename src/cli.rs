//! Команды для человека: таблица, подробности, приёмка, журнал, уборка, проверка окружения.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde_json::Value;

use crate::VERSION;
use crate::board::{self, Accepted, Board, Counts, Row, or_question, status_name};
use crate::config::{Paths, env_nonempty};
use crate::detect::{self, which};
use crate::model::Status;
use crate::observe;
use crate::panes;
use crate::store::{self, Store};
use crate::text::{age, clip, emit, local_time, pad, warn};
use crate::{t, tr};

/// Суммарная ширина всех колонок таблицы, кроме последней («ЧТО»).
const FIXED_COLUMNS: usize = 81;
/// До скольких строк `disp gc` укорачивает журнал.
const JOURNAL_KEEP: usize = 5000;

/// `disp ls [-a] [--json]`: таблица незакрытых сессий.
pub fn ls(args: &[String]) -> u8 {
    // Сначала — что изменилось без хуков: агенты в контейнерах, сессии, ушедшие в фон.
    observe::sync(&Store::new(Paths::from_env()));
    let board = Board::load();
    let mut all = false;
    for arg in args {
        match arg.as_str() {
            "-a" | "--all" => all = true,
            "--json" => {
                emit(&format!("{}\n", json_rows(&board)));
                return 0;
            }
            _ => {}
        }
    }
    emit(&format!(
        "{}\n",
        table(&board, term_cols(), all, color_wanted())
    ));
    0
}

/// Все сессии в виде JSON-массива: поля файла плюс вычисленные `sid` и `view`.
fn json_rows(board: &Board) -> String {
    let rows: Vec<Value> = board
        .rows()
        .into_iter()
        .filter_map(|row| {
            let mut value = serde_json::to_value(&row.session).ok()?;
            let object = value.as_object_mut()?;
            object.insert("sid".to_string(), Value::from(row.sid));
            object.insert("view".to_string(), Value::from(row.view.key()));
            Some(value)
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string())
}

/// Таблица сессий: сверху то, что требует человека, внутри группы — самое давнее.
fn table(board: &Board, cols: usize, all: bool, color: bool) -> String {
    let rows = board.visible(all);
    if rows.is_empty() {
        return tr!(
            "No open sessions.  (disp ls -a shows removed ones too)",
            "Незакрытых сессий нет.  (disp ls -a покажет и убранные)",
            "Незакритих сесій немає.  (disp ls -a покаже й прибрані)"
        );
    }

    let mut lines = vec![Counts::of(&rows).line()];
    let header = format!(
        "{} {} {} {} {} {} {} {}",
        pad("ID", 6),
        pad(t!("STATUS", "СТАТУС", "СТАТУС"), 10),
        pad(t!("AGENT", "АГЕНТ", "АГЕНТ"), 6),
        pad(t!("TASK", "ЗАДАЧА", "ЗАВДАННЯ"), 22),
        pad(t!("SUB", "ПОДЗ", "ПІДЗ"), 7),
        pad(t!("WHERE", "ГДЕ", "ДЕ"), 18),
        pad(t!("AGE", "ВОЗР", "ВІК"), 4),
        t!("WHAT", "ЧТО", "ЩО"),
    );
    lines.push(paint(&header, "1", color));

    let what_width = cols.saturating_sub(FIXED_COLUMNS).max(12);
    for row in &rows {
        let s = &row.session;
        let line = format!(
            "{} {} {} {} {} {} {} {}{}",
            pad(&row.sid, 6),
            pad(row.view.label(), 10),
            pad(or_question(&s.agent), 6),
            pad(&clip(&s.label(), 22), 22),
            pad(&clip(&row.subtasks(), 7), 7),
            pad(&clip(row.place(), 18), 18),
            pad(&age(board.now - s.updated), 4),
            clip(&row.what(), what_width),
            if s.accepted {
                t!("  [removed]", "  [убрано]", "  [прибрано]")
            } else {
                ""
            },
        );
        lines.push(paint(&line, row.view.color(), color));
    }
    lines.join("\n")
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// Нужен ли цвет: да в терминале, а `DISP_COLOR=1` и `DISP_COLOR=0` решают явно.
fn color_wanted() -> bool {
    match env_nonempty("DISP_COLOR").as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => io::stdout().is_terminal(),
    }
}

/// Ширина терминала: `COLUMNS`, иначе размер окна, иначе 120.
fn term_cols() -> usize {
    env_nonempty("COLUMNS")
        .and_then(|value| value.parse().ok())
        .or_else(|| terminal_size::terminal_size().map(|(width, _)| usize::from(width.0)))
        .unwrap_or(120)
}

/// Живая таблица без управления: перерисовывается раз в `pause`. Так `disp watch`
/// работает там, где нет полноценного терминала.
pub fn watch_plain(pause: Duration) -> u8 {
    // Перед выходом по Ctrl-C возвращаем курсор.
    let _ = ctrlc::set_handler(|| {
        let _ = io::stdout().write_all(b"\x1b[?25h");
        std::process::exit(0);
    });
    emit("\x1b[?25l");
    observe::watch(Store::new(Paths::from_env()));
    loop {
        let board = Board::load();
        let table = table(&board, term_cols(), false, true);
        let frame = format!(
            "\x1b[H\x1b[2J{}  disp {VERSION}\n\n{table}\n",
            Local::now().format("%H:%M:%S")
        );
        let mut out = io::stdout().lock();
        if out
            .write_all(frame.as_bytes())
            .and_then(|()| out.flush())
            .is_err()
        {
            // Вывод закрыт: терминал пропал или читатель трубы вышел. Показывать некому.
            return 0;
        }
        drop(out);
        thread::sleep(pause);
    }
}

/// `disp show <id|задача>`: подробности сессии.
pub fn show(args: &[String]) -> u8 {
    let Some(query) = args.first() else {
        warn(t!(
            "usage: disp show <id|task>",
            "использование: disp show <id|задача>",
            "використання: disp show <id|завдання>"
        ));
        return 2;
    };
    let board = Board::load();
    let found: Vec<Row> = board
        .rows()
        .into_iter()
        .filter(|row| {
            row.sid == *query
                || row.sid.starts_with(query.as_str())
                || row.session.task == *query
                || row.session.session_id == *query
        })
        .collect();
    if found.is_empty() {
        emit(&not_found(query));
        emit("\n");
        return 0;
    }
    let mut out = String::new();
    for row in &found {
        out.push_str(&details(row));
    }
    emit(&out);
    0
}

/// Текст подробностей одной сессии.
fn details(row: &Row) -> String {
    let s = &row.session;
    let mut lines = vec![format!("{}  {}  [{}]", row.sid, s.label(), s.agent)];
    // Поле подробностей: название и значение, значения выровнены.
    let field = |key: &str, value: &str| format!("  {:<10} {value}", format!("{key}:"));
    let closed = if s.ended {
        let reason = s
            .end_reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
            .map(|reason| format!(": {reason}"))
            .unwrap_or_default();
        tr!(
            " (session closed{reason})",
            " (сессия закрыта{reason})",
            " (сесію закрито{reason})"
        )
    } else {
        String::new()
    };
    let removed = if s.accepted {
        t!(" [removed]", " [убрано]", " [прибрано]")
    } else {
        ""
    };
    let status = format!("{}{closed}{removed}", s.status.as_str());
    lines.push(field(t!("status", "статус", "статус"), &status));
    if !s.detail.is_empty() {
        lines.push(field(t!("detail", "деталь", "деталь"), &s.detail));
    }
    lines.push(field(t!("what", "что", "що"), &s.title));
    if let Some(prompt) = s
        .last_prompt
        .as_deref()
        .filter(|text| !text.is_empty() && *text != s.title)
    {
        lines.push(field(t!("prompt", "промпт", "промпт"), prompt));
    }
    if let Some(reply) = s.last_reply.as_deref().filter(|text| !text.is_empty()) {
        lines.push(field(t!("reply", "ответ", "відповідь"), reply));
    }
    lines.push(field(t!("where", "где", "де"), or_question(&s.place)));
    lines.push(field(t!("directory", "каталог", "каталог"), &s.cwd));
    lines.push(field(t!("session", "сессия", "сесія"), &s.session_id));
    lines.push(field(t!("started", "начата", "почато"), &utc(s.started)));
    lines.push(field(t!("updated", "обновлена", "оновлено"), &utc(s.updated)));
    if let Some(done_at) = s.done_at {
        lines.push(field(t!("turn ended", "ход окончен", "хід завершено"), &utc(done_at)));
    }
    if s.subagents > 0 {
        let count = s.subagents;
        lines.push(tr!(
            "  subagents running: {count}",
            "  субагентов в работе: {count}",
            "  субагентів у роботі: {count}"
        ));
    }
    if s.bg > 0 {
        let count = s.bg;
        lines.push(tr!(
            "  background tasks at stop: {count}",
            "  фоновых задач на момент остановки: {count}",
            "  фонових задач на момент зупинки: {count}"
        ));
    }
    if !s.subtasks.is_empty() {
        lines.push(tr!("  subtasks:", "  подзадачи:", "  підзадачі:"));
        for task in s.subtasks.values() {
            lines.push(format!(
                "    [{}] {}",
                if task.d { "x" } else { " " },
                task.s
            ));
        }
    }
    lines.push(String::new());
    lines.join("\n") + "\n"
}

fn utc(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| "—".to_string())
}

/// Сессии, подходящие под селектор команд `ok` и `name`: короткий id (или его начало
/// от трёх символов), имя задачи, полный id, а также `--ended` и `--done`.
fn matching(board: &Board, query: &str) -> Vec<Row> {
    board
        .rows()
        .into_iter()
        .filter(|row| {
            let s = &row.session;
            match query {
                "--ended" => s.ended && !s.accepted,
                "--done" => s.status == Status::Done && !s.accepted,
                _ => {
                    row.sid == query
                        || (query.chars().count() >= 3 && row.sid.starts_with(query))
                        || s.task == query
                        || s.session_id == query
                }
            }
        })
        .collect()
}

/// «Не найдено: …» на языке интерфейса.
fn not_found(query: &str) -> String {
    tr!("Not found: {query}", "Не найдено: {query}", "Не знайдено: {query}")
}

/// `disp ok [-f] <id|задача>... | --ended | --done`: принять результат.
pub fn ok(args: &[String]) -> u8 {
    if args.is_empty() {
        warn(t!(
            "usage: disp ok [-f] <id|task>... | --ended | --done",
            "использование: disp ok [-f] <id|задача>... | --ended | --done",
            "використання: disp ok [-f] <id|завдання>... | --ended | --done"
        ));
        return 2;
    }
    let is_force = |arg: &String| arg == "-f" || arg == "--force";
    let force = args.iter().any(is_force);
    let board = Board::load();
    let _lock = board.store.lock();
    let (mut accepted, mut skipped) = (0, 0);
    for query in args.iter().filter(|arg| !is_force(arg)) {
        for mut row in matching(&board, query) {
            let summary = row.summary();
            match board::accept(&mut row, board.now, force) {
                Ok(Accepted::Yes) => {
                    accepted += 1;
                    emit(&tr!(
                        "removed: {summary}\n",
                        "убрано: {summary}\n",
                        "прибрано: {summary}\n"
                    ));
                }
                Ok(Accepted::StillActive) => {
                    skipped += 1;
                    let state = if row.session.status == Status::Waiting {
                        t!("waiting for you", "ждёт тебя", "чекає на тебе")
                    } else {
                        t!("working", "работает", "працює")
                    };
                    warn(&tr!(
                        "skipped (still {state}): {summary}",
                        "пропущено (ещё {state}): {summary}",
                        "пропущено (ще {state}): {summary}"
                    ));
                }
                Err(_) => {}
            }
        }
    }
    if skipped > 0 {
        warn(t!(
            "Active sessions were not removed. If a session is dead: disp ok -f <id>",
            "Активные сессии не убраны. Если сессия мертва: disp ok -f <id>",
            "Активні сесії не прибрано. Якщо сесія мертва: disp ok -f <id>"
        ));
    }
    if accepted == 0 {
        if skipped == 0 {
            let query = args.join(" ");
            warn(&tr!(
                "Nothing found for: {query}",
                "Ничего не найдено под: {query}",
                "Нічого не знайдено за: {query}"
            ));
        }
        return 1;
    }
    0
}

/// `disp yes <id|задача>`: ответить агенту вариантом по умолчанию — нажать Enter в его
/// панели zellij. Только для одной сессии, которая сейчас ждёт человека.
pub fn yes(args: &[String]) -> u8 {
    let usage = || {
        warn(t!(
            "usage: disp yes <id|task>",
            "использование: disp yes <id|задача>",
            "використання: disp yes <id|завдання>"
        ));
        2
    };
    let [query] = args else {
        return usage();
    };
    if query.starts_with('-') {
        return usage();
    }
    let board = Board::load();
    let found = matching(&board, query);
    let row = match found.as_slice() {
        [] => {
            warn(&not_found(query));
            return 1;
        }
        [row] => row,
        _ => {
            let ids = found
                .iter()
                .map(|row| row.sid.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            warn(&tr!(
                "Several sessions match «{query}» ({ids}); give the id",
                "Под «{query}» несколько сессий ({ids}); укажи id",
                "Під «{query}» кілька сесій ({ids}); вкажи id"
            ));
            return 1;
        }
    };
    // Enter уходит в панель агента: решаем по только что перечитанной записи.
    let _lock = board.store.lock();
    let Some(session) = store::read_session(&row.path) else {
        warn(&not_found(query));
        return 1;
    };
    match panes::approve(&session) {
        Ok(text) => {
            emit(&format!("{text}\n"));
            0
        }
        Err(text) => {
            warn(&text);
            1
        }
    }
}

/// `disp name <id> <текст>`: дать строке понятное название.
pub fn name(args: &[String]) -> u8 {
    let usage = || {
        warn(t!(
            "usage: disp name <id> <text>",
            "использование: disp name <id> <текст>",
            "використання: disp name <id> <текст>"
        ));
        2
    };
    let [query, words @ ..] = args else {
        return usage();
    };
    if words.is_empty() {
        return usage();
    }
    let title = words.join(" ");
    let board = Board::load();
    let _lock = board.store.lock();
    let mut renamed = 0;
    for mut row in matching(&board, query) {
        if board::rename(&mut row, &title).is_ok() {
            renamed += 1;
            if panes::enabled(&board.settings) && !row.session.ended {
                panes::apply(&row.session, true);
            }
        }
    }
    if renamed == 0 {
        warn(&not_found(query));
        return 1;
    }
    0
}

/// `disp label`: поставить подписи `#номер название` на панели zellij всех открытых
/// сессий из списка сразу, не дожидаясь их следующего события.
///
/// У сессии, названия которой диспетчер ещё не знает, оно берётся из заголовка панели:
/// иначе подпись заменила бы понятный заголовок программы именем задачи.
pub fn label() -> u8 {
    let board = Board::load();
    // Убранные строки не подписываются: их запись могла устареть, а панель с тем же
    // номером уже занята другой сессией.
    let open = |row: &Row| {
        !row.session.ended && !row.session.accepted && panes::target(&row.session.place).is_some()
    };
    let rows: Vec<Row> = board.rows().into_iter().filter(open).collect();

    // Прежняя подпись скрывает заголовок программы: сначала снимаем её.
    let untitled: Vec<&Row> = rows
        .iter()
        .filter(|row| row.session.title.is_empty())
        .collect();
    for row in &untitled {
        panes::clear(&row.session.place, true);
    }
    let mut listings: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut found: Vec<(&Row, String)> = Vec::new();
    for row in untitled {
        let Some((zellij, pane)) = panes::target(&row.session.place) else {
            continue;
        };
        let listing = listings
            .entry(zellij.to_string())
            .or_insert_with(|| panes::titles(zellij));
        let dir = detect::base_name(&row.session.cwd);
        if let Some(title) = listing
            .get(pane)
            .and_then(|raw| detect::clean_pane_title(raw, &row.session.agent, &dir))
        {
            found.push((row, title));
        }
    }
    // Найденные названия записываем под блокировкой и в свежую копию: пока шёл опрос
    // zellij, сессию мог изменить хук.
    if !found.is_empty() {
        let _lock = board.store.lock();
        for (row, title) in found {
            if let Some(mut session) = store::read_session(&row.path)
                && session.title.is_empty()
            {
                session.title = title;
                session.tsrc = Some("agent".to_string());
                let _ = store::write_session(&row.path, &session);
            }
        }
    }

    let mut labelled = 0;
    for row in &rows {
        if let Some(session) = store::read_session(&row.path).filter(|session| !session.ended) {
            panes::apply(&session, true);
            labelled += 1;
        }
    }
    emit(&tr!(
        "panes labelled: {labelled}\n",
        "подписано панелей: {labelled}\n",
        "підписано панелей: {labelled}\n"
    ));
    if !panes::enabled(&board.settings) {
        emit(t!(
            "Labels will not update by themselves: add DISP_PANE_LABEL=1 to the settings.\n",
            "Подписи не будут обновляться сами: добавь DISP_PANE_LABEL=1 в настройки.\n",
            "Підписи не оновлюватимуться самі: додай DISP_PANE_LABEL=1 до налаштувань.\n"
        ));
    }
    0
}

/// `disp repair`: дополнить записи прежних версий каталогом запуска агента
/// и исправить по нему задачу. Панель делает это сама при запуске.
pub fn repair() -> u8 {
    let repaired = board::repair_roots(&Board::load());
    emit(&tr!(
        "records repaired: {repaired}\n",
        "исправлено записей: {repaired}\n",
        "виправлено записів: {repaired}\n"
    ));
    0
}

/// `disp gui [--window]`: графическая оболочка.
///
/// В панели терминала, который сообщает свой размер в пикселях, оболочка показывается
/// прямо в панели через term-everything; иначе и с ключом `--window` — обычным окном
/// на рабочем столе. Команда заменяет собой процесс `disp`.
pub fn gui(args: &[String]) -> u8 {
    let board = Board::load();
    let Some(program) = gui_program(&board) else {
        warn(t!(
            "disp gui: disp-gui not found (build and install: make install-gui)",
            "disp gui: не найдена программа disp-gui (собрать и поставить: make install-gui)",
            "disp gui: не знайдено програму disp-gui (зібрати й поставити: make install-gui)"
        ));
        return 1;
    };
    tray_alongside(&board);
    let window = args.iter().any(|arg| arg == "--window");
    let size = if window { None } else { pane_pixels() };
    let mut command = match (size, term_everything(&board)) {
        (Some((width, height)), Some(compositor)) => {
            let size = format!("{width}x{height}");
            let mut command = Command::new(compositor);
            // term-everything на Wayland считает кадр RGBA, хотя программы отдают BGRA:
            // без подсказки красный и синий меняются местами.
            if env::var_os("TERM_EVERYTHING_PIXEL_TYPE").is_none() {
                command.env("TERM_EVERYTHING_PIXEL_TYPE", "BGRA8");
            }
            command
                .args(["--virtual-monitor-size", &size, "--"])
                .arg(&program)
                .args(["--size", &size]);
            command
        }
        (Some(_), None) => {
            warn(t!(
                "disp gui: term-everything not found, the window opens on the desktop \
                 (its path: DISP_TERM_EVERYTHING in the settings)",
                "disp gui: term-everything не найден, окно откроется на рабочем столе \
                 (путь к нему: DISP_TERM_EVERYTHING в настройках)",
                "disp gui: term-everything не знайдено, вікно відкриється на робочому столі \
                 (шлях до нього: DISP_TERM_EVERYTHING у налаштуваннях)"
            ));
            Command::new(&program)
        }
        (None, _) => Command::new(&program),
    };
    // При успехе управление сюда не возвращается.
    let err = command.exec();
    warn(&tr!(
        "disp gui: could not start: {err}",
        "disp gui: не удалось запустить: {err}",
        "disp gui: не вдалося запустити: {err}"
    ));
    1
}

/// Программа графической оболочки: `DISP_GUI_BIN`, рядом с самой `disp`, в `PATH`.
pub fn gui_program(board: &Board) -> Option<PathBuf> {
    if let Some(path) = board.settings.get("DISP_GUI_BIN") {
        return Some(PathBuf::from(path));
    }
    companion("disp-gui")
}

/// Программа из той же поставки, что `disp`: рядом с ней, а если там нет — в `PATH`.
pub fn companion(name: &str) -> Option<PathBuf> {
    let beside = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.is_file());
    beside.or_else(|| which(name))
}

/// Программа значка в трее: `DISP_TRAY_BIN`, рядом с самой `disp`, в `PATH`.
fn tray_program(board: &Board) -> Option<PathBuf> {
    match board.settings.get("DISP_TRAY_BIN") {
        Some(path) => Some(PathBuf::from(path)),
        None => companion("disp-tray"),
    }
}

/// Чем кончилась попытка показать значок в трее.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayStart {
    /// Значок запущен.
    Started,
    /// Значок уже был в трее.
    Running,
    /// Программы `disp-tray` нет.
    Missing,
    /// Запустить не удалось; текст для человека.
    Failed(String),
}

/// Код выхода `disp-tray --detach`, когда значок уже в трее.
pub const TRAY_RUNNING_EXIT: i32 = 3;

/// Показывает значок в трее, если его ещё нет.
///
/// Запускает `disp-tray --detach` и ждёт его: тот запускает сам значок и сразу выходит.
/// Так значок не становится дочерним процессом `disp` и не остаётся зомби, когда его
/// убирают, а `disp` тем временем ещё работает. При этом результат честный: ошибку
/// запуска самой программы (нет файла, нет права на исполнение) возвращает `spawn`,
/// а ошибку запуска значка — `disp-tray --detach` своим кодом выхода и текстом.
/// Своя группа процессов — закрытие терминала и Ctrl+C до значка не доходят.
/// Переменные zellij убираются: значок должен вести себя одинаково, откуда бы его ни
/// запустили, в том числе при входе в систему.
pub fn start_tray(board: &Board) -> TrayStart {
    let Some(program) = tray_program(board) else {
        return TrayStart::Missing;
    };
    match board.store.tray_lock() {
        Ok(Some(_free)) => {}
        Ok(None) => return TrayStart::Running,
        Err(err) => {
            return TrayStart::Failed(tr!(
                "cannot check whether the icon is running: {err}",
                "не проверить, запущен ли значок: {err}",
                "не перевірити, чи запущено значок: {err}"
            ));
        }
    }
    let output = Command::new(&program)
        .arg("--detach")
        .env_remove("ZELLIJ")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("ZELLIJ_PANE_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .output();
    match output {
        Ok(output) if output.status.success() => TrayStart::Started,
        Ok(output) if output.status.code() == Some(TRAY_RUNNING_EXIT) => TrayStart::Running,
        Ok(output) => {
            let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
            TrayStart::Failed(if said.is_empty() {
                let (program, status) = (program.display(), output.status);
                tr!(
                    "{program} exited with {status}",
                    "{program} вышла с {status}",
                    "{program} завершилася з {status}"
                )
            } else {
                said
            })
        }
        Err(err) => TrayStart::Failed(format!("{}: {err}", program.display())),
    }
}

/// Значок вместе с панелью: `disp watch` и `disp gui` показывают его сами. Не показывают
/// при `DISP_TRAY=0`, в сеансе SSH (рабочий стол, где появился бы значок, не перед
/// глазами) и без рабочего стола. Молча: панель сейчас займёт экран, а значок — не то,
/// ради чего её открывали.
pub fn tray_alongside(board: &Board) {
    let desktop = cfg!(target_os = "macos")
        || env_nonempty("WAYLAND_DISPLAY").is_some()
        || env_nonempty("DISPLAY").is_some();
    if board.settings.get("DISP_TRAY") == Some("0")
        || env_nonempty("SSH_CONNECTION").is_some()
        || !desktop
    {
        return;
    }
    let _ = start_tray(board);
}

/// `disp tray [ключи]`: значок в трее.
///
/// Без ключей запускает `disp-tray` в фоне, отдельно от терминала, и сразу возвращается;
/// если значок уже в трее, второй не запускается. С ключами (`--autostart` и другие)
/// передаёт их `disp-tray` и заменяется им.
pub fn tray(args: &[String]) -> u8 {
    let board = Board::load();
    if !args.is_empty() {
        let Some(program) = tray_program(&board) else {
            warn(tray_missing());
            return 1;
        };
        let err = Command::new(&program).args(args).exec();
        warn(&tray_failed(&err.to_string()));
        return 1;
    }
    match start_tray(&board) {
        TrayStart::Started => {
            emit(t!(
                "disp tray: the tray icon is running\n",
                "disp tray: значок в трее запущен\n",
                "disp tray: значок у треї запущено\n"
            ));
            0
        }
        TrayStart::Running => {
            emit(t!(
                "disp tray: the icon is already in the tray\n",
                "disp tray: значок уже в трее\n",
                "disp tray: значок уже в треї\n"
            ));
            0
        }
        TrayStart::Missing => {
            warn(tray_missing());
            1
        }
        TrayStart::Failed(err) => {
            warn(&tray_failed(&err));
            1
        }
    }
}

/// Сообщение о том, что программы значка нет.
fn tray_missing() -> &'static str {
    t!(
        "disp tray: disp-tray not found (build and install: make install-tray)",
        "disp tray: не найдена программа disp-tray (собрать и поставить: make install-tray)",
        "disp tray: не знайдено програму disp-tray (зібрати й поставити: make install-tray)"
    )
}

/// Сообщение о том, что значок не запустился.
fn tray_failed(err: &str) -> String {
    tr!(
        "disp tray: could not start: {err}",
        "disp tray: не удалось запустить: {err}",
        "disp tray: не вдалося запустити: {err}"
    )
}

/// Программа term-everything: `DISP_TERM_EVERYTHING` или `term-everything` в `PATH`.
fn term_everything(board: &Board) -> Option<PathBuf> {
    match board.settings.get("DISP_TERM_EVERYTHING") {
        Some(path) => Some(PathBuf::from(path)).filter(|path| path.is_file()),
        None => which("term-everything"),
    }
}

/// Размер места под окно в панели терминала, в пикселях: панель без одной строки,
/// которую term-everything занимает своей строкой состояния. `None`, если вывод идёт
/// не в терминал или терминал не сообщает размер в пикселях. `DISP_GUI_SIZE=ШxВ`
/// задаёт размер явно.
fn pane_pixels() -> Option<(u32, u32)> {
    if let Some(given) = env_nonempty("DISP_GUI_SIZE") {
        let (width, height) = given.split_once('x')?;
        return Some((width.parse().ok()?, height.parse().ok()?));
    }
    if !io::stdout().is_terminal() {
        return None;
    }
    let size = ratatui::crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.rows < 2 {
        return None;
    }
    let row = size.height / size.rows;
    Some((u32::from(size.width), u32::from(size.height - row)))
}

/// `disp unlabel`: снять подписи диспетчера со всех панелей zellij.
pub fn unlabel() -> u8 {
    let board = Board::load();
    // Сессии zellij, где могли остаться подписи: текущая и те, где шли сессии агентов.
    let mut sessions: Vec<String> = env_nonempty("ZELLIJ_SESSION_NAME").into_iter().collect();
    for row in board.rows() {
        if let Some((zellij, _)) = panes::target(&row.session.place)
            && !sessions.iter().any(|known| known == zellij)
        {
            sessions.push(zellij.to_string());
        }
    }
    let cleared: usize = sessions.iter().map(|zellij| panes::clear_all(zellij)).sum();
    emit(&tr!(
        "labels removed: {cleared}\n",
        "снято подписей: {cleared}\n",
        "знято підписів: {cleared}\n"
    ));
    if panes::enabled(&board.settings) {
        emit(t!(
            "Labels will come back: remove DISP_PANE_LABEL=1 from the settings.\n",
            "Подписи появятся снова: убери DISP_PANE_LABEL=1 из настроек.\n",
            "Підписи з'являться знову: прибери DISP_PANE_LABEL=1 з налаштувань.\n"
        ));
    }
    0
}

/// `disp log [N]`: последние смены статуса.
pub fn log(args: &[String]) -> u8 {
    let limit = args
        .first()
        .filter(|arg| !arg.is_empty() && arg.chars().all(|ch| ch.is_ascii_digit()))
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(30);
    let board = Board::load();
    let paths = &board.store.paths;
    if fs::metadata(paths.events()).map_or(0, |meta| meta.len()) == 0 {
        emit(t!("The journal is empty.\n", "Журнал пуст.\n", "Журнал порожній.\n"));
        return 0;
    }
    let mut out = String::new();
    for event in board::journal(paths, limit) {
        out.push_str(&format!(
            "{}  {}  {}  {} → {}  ({})\n",
            local_time(event.ts, "%m-%d %H:%M"),
            event.agent,
            event.task,
            status_name(&event.from),
            status_name(&event.to),
            event.place
        ));
    }
    emit(&out);
    0
}

/// `disp gc [дней]`: удалить принятые и пустые записи старше N дней, укоротить журнал.
pub fn gc(args: &[String]) -> u8 {
    let days = args
        .first()
        .filter(|arg| !arg.is_empty() && arg.chars().all(|ch| ch.is_ascii_digit()))
        .and_then(|arg| arg.parse::<i64>().ok())
        .unwrap_or(7);
    let board = Board::load();
    let _lock = board.store.lock();
    let mut removed = 0;
    for row in board.rows() {
        let s = &row.session;
        let disposable = s.accepted || s.status == Status::Idle;
        if disposable && board.now - s.updated > days * 86_400 && fs::remove_file(&row.path).is_ok()
        {
            removed += 1;
        }
    }
    let events = board.store.paths.events();
    if let Ok(journal) = fs::read(&events) {
        let text = String::from_utf8_lossy(&journal);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > JOURNAL_KEEP {
            let kept = lines[lines.len() - JOURNAL_KEEP..].join("\n") + "\n";
            let _ = store::write_atomic(&events, kept.as_bytes(), 0o600);
        }
    }
    emit(&tr!(
        "records deleted: {removed} (removed and empty ones older than {days} days)\n",
        "удалено записей: {removed} (убранные и пустые старше {days} дн.)\n",
        "видалено записів: {removed} (прибрані й порожні, старші за {days} дн.)\n"
    ));
    0
}

/// `disp doctor`: проверка окружения и того, какие агенты шлют события.
pub fn doctor() -> u8 {
    let board = Board::load();
    let paths = &board.store.paths;
    let exe = env::current_exe()
        .and_then(fs::canonicalize)
        .unwrap_or_default();
    let mut out = format!("disp {VERSION}\n");
    let (program, state, conf) = (
        exe.display(),
        paths.state_dir.display(),
        paths.conf_file.display(),
    );
    out.push_str(&tr!(
        "program: {program}\n",
        "программа: {program}\n",
        "програма: {program}\n"
    ));
    out.push_str(&tr!(
        "state: {state}\n",
        "состояние: {state}\n",
        "стан: {state}\n"
    ));
    let conf_note = if paths.conf_file.is_file() {
        ""
    } else {
        t!(" (no file)", " (нет файла)", " (немає файлу)")
    };
    out.push_str(&tr!(
        "settings: {conf}{conf_note}\n",
        "настройки: {conf}{conf_note}\n",
        "налаштування: {conf}{conf_note}\n"
    ));
    let lang = crate::i18n::lang().code();
    out.push_str(&tr!(
        "language: {lang} (DISP_LANG=en|ru|uk)\n",
        "язык: {lang} (DISP_LANG=en|ru|uk)\n",
        "мова: {lang} (DISP_LANG=en|ru|uk)\n"
    ));
    for tool in [crate::notify::DESKTOP_TOOL, "curl", "zellij"] {
        let mark = if which(tool).is_some() {
            "ok "
        } else {
            t!("no ", "нет", "нема")
        };
        out.push_str(&format!("  {mark}  {tool}\n"));
    }
    let telegram = board.settings.get("DISP_TG_TOKEN").is_some()
        && board.settings.get("DISP_TG_CHAT").is_some();
    let labels = if panes::enabled(&board.settings) {
        t!("on", "включены", "увімкнено")
    } else {
        t!(
            "off (DISP_PANE_LABEL=1 turns them on)",
            "выключены (DISP_PANE_LABEL=1 включает)",
            "вимкнено (DISP_PANE_LABEL=1 вмикає)"
        )
    };
    out.push_str(&tr!(
        "pane labels: {labels}\n",
        "подписи панелей: {labels}\n",
        "підписи панелей: {labels}\n"
    ));
    let telegram = if telegram {
        t!("configured", "настроен", "налаштовано")
    } else {
        t!("not configured", "не настроен", "не налаштовано")
    };
    out.push_str(&format!("Telegram:  {telegram}\n"));

    let rows = board.rows();
    for agent in ["claude", "codex", "gemini"] {
        let last = rows
            .iter()
            .filter(|row| row.session.agent == agent)
            .map(|row| row.session.updated)
            .max();
        let seen = match last {
            Some(ts) => {
                let minutes = (board.now - ts).max(0) / 60;
                tr!(
                    "last event {minutes} min ago",
                    "последнее событие {minutes} мин назад",
                    "остання подія {minutes} хв тому"
                )
            }
            None => tr!("no events yet", "событий не было", "подій не було"),
        };
        out.push_str(&format!("  {agent:<7} {seen}\n"));
    }
    if let Ok(log) = fs::read(paths.hook_log())
        && !log.is_empty()
    {
        out.push_str(t!(
            "last lines of hook.log:\n",
            "последние строки hook.log:\n",
            "останні рядки hook.log:\n"
        ));
        let text = String::from_utf8_lossy(&log);
        let lines: Vec<&str> = text.lines().collect();
        for line in &lines[lines.len().saturating_sub(5)..] {
            out.push_str(&format!("  {line}\n"));
        }
    }
    emit(&out);
    0
}

/// Справка по командам.
pub fn usage() -> String {
    tr!(
        "disp {VERSION} — dispatcher for AI agent sessions

  disp               table of open sessions (same as disp ls)
  disp ls [-a]       -a: show removed ones too; --json: raw data
  disp watch         live panel for its own zellij pane: arrows — select,
                     a — answer the agent “yes”, x — remove, g — go to the agent,
                     t — timeline,
                     ? — all keys; also shows the tray icon
                     (DISP_TRAY=0 in the settings — do not show it)
  disp watch --plain [sec]   table without controls, refreshed every N seconds
  disp gui           graphical shell: inside the terminal pane via term-everything,
                     with --window — as a normal desktop window
  disp tray          only the tray icon (Linux, macOS): who is waiting, “yes”,
                     “go to”, “remove” buttons; --autostart — show at login,
                     --no-autostart — stop
  disp show <id>     details: prompt, last reply, subtasks
  disp ok <id|task>...     remove from the list: result reviewed
  disp ok --ended    remove all closed sessions; --done: all finished ones
  disp ok -f <id>    remove an active session (if it is actually dead)
  disp yes <id>      answer a waiting agent “yes”: Enter in its zellij pane
  disp name <id> <text>    give the row a clear name
  disp log [N]       status journal: what happened while you were away
  disp gc [days]     delete removed and empty records older than N days (7)
  disp label         label the zellij panes of open sessions: “#number name”
                     (they update by themselves with DISP_PANE_LABEL=1 in the settings)
  disp unlabel       remove these labels from all panes
  disp repair        fix the task of records from older versions (the panel does it itself)
  disp install <claude|codex|gemini>  connect the hooks to the agent settings
                     (adds only what is missing, keeps a .bak-disp copy)
  disp config <claude|codex|gemini>   print the hook block for the agent
  disp doctor        check the environment and which agents send events
  disp hook <agent>  called by the agent hooks, not needed by hand

Agents in containers are seen without hooks (DISP_CONTAINERS=0 turns it off).
Language: DISP_LANG=en|ru|uk, otherwise the system language.
",
        "disp {VERSION} — диспетчер сессий AI-агентов

  disp               таблица незакрытых сессий (то же, что disp ls)
  disp ls [-a]       -a: показать и убранные; --json: сырые данные
  disp watch         живая панель для отдельной панели zellij: стрелки — выбор,
                     a — ответить агенту «да», x — убрать, g — перейти к агенту,
                     t — лента времени,
                     ? — все клавиши; заодно показывает значок в трее
                     (DISP_TRAY=0 в настройках — не показывать)
  disp watch --plain [сек]   таблица без управления, обновляется раз в N секунд
  disp gui           графическая оболочка: в панели терминала через term-everything,
                     с ключом --window — обычным окном на рабочем столе
  disp tray          только значок в трее (Linux, macOS): кто ждёт, кнопки «да»,
                     «перейти», «убрать»; --autostart — показывать при входе в систему,
                     --no-autostart — перестать
  disp show <id>     подробности: промпт, последний ответ, подзадачи
  disp ok <id|задача>...   убрать из списка: результат посмотрел
  disp ok --ended    убрать все закрытые сессии; --done: все законченные
  disp ok -f <id>    убрать активную сессию (если она на самом деле мертва)
  disp yes <id>      ответить ждущему агенту «да»: Enter в его панели zellij
  disp name <id> <текст>   дать строке понятное название
  disp log [N]       журнал смен статуса: что происходило, пока тебя не было
  disp gc [дней]     удалить убранные и пустые записи старше N дней (7)
  disp label         подписать панели zellij открытых сессий: «#номер название»
                     (сами обновляются при DISP_PANE_LABEL=1 в настройках)
  disp unlabel       снять эти подписи со всех панелей
  disp repair        исправить задачу у записей прежних версий (панель делает это сама)
  disp install <claude|codex|gemini>  подключить хуки к настройкам агента
                     (добавляет только недостающее, делает копию .bak-disp)
  disp config <claude|codex|gemini>   напечатать блок хуков для агента
  disp doctor        проверка окружения и того, какие агенты шлют события
  disp hook <agent>  вызывается хуками агента, вручную не нужен

Агенты в контейнерах видны без хуков (DISP_CONTAINERS=0 — выключить).
Язык: DISP_LANG=en|ru|uk, иначе — язык системы.
",
        "disp {VERSION} — диспетчер сесій AI-агентів

  disp               таблиця незакритих сесій (те саме, що disp ls)
  disp ls [-a]       -a: показати й прибрані; --json: сирі дані
  disp watch         жива панель для окремої панелі zellij: стрілки — вибір,
                     a — відповісти агентові «так», x — прибрати, g — перейти до агента,
                     t — стрічка часу,
                     ? — усі клавіші; заодно показує значок у треї
                     (DISP_TRAY=0 у налаштуваннях — не показувати)
  disp watch --plain [сек]   таблиця без керування, оновлюється раз на N секунд
  disp gui           графічна оболонка: у панелі термінала через term-everything,
                     з ключем --window — звичайним вікном на робочому столі
  disp tray          лише значок у треї (Linux, macOS): хто чекає, кнопки «так»,
                     «перейти», «прибрати»; --autostart — показувати під час входу в систему,
                     --no-autostart — припинити
  disp show <id>     подробиці: промпт, остання відповідь, підзадачі
  disp ok <id|завдання>... прибрати зі списку: результат переглянуто
  disp ok --ended    прибрати всі закриті сесії; --done: усі завершені
  disp ok -f <id>    прибрати активну сесію (якщо вона насправді мертва)
  disp yes <id>      відповісти агентові, що чекає, «так»: Enter у його панелі zellij
  disp name <id> <текст>   дати рядку зрозумілу назву
  disp log [N]       журнал змін статусу: що відбувалося, поки тебе не було
  disp gc [днів]     видалити прибрані й порожні записи, старші за N днів (7)
  disp label         підписати панелі zellij відкритих сесій: «#номер назва»
                     (самі оновлюються з DISP_PANE_LABEL=1 у налаштуваннях)
  disp unlabel       зняти ці підписи з усіх панелей
  disp repair        виправити завдання в записах попередніх версій (панель робить це сама)
  disp install <claude|codex|gemini>  підключити хуки до налаштувань агента
                     (додає лише відсутнє, робить копію .bak-disp)
  disp config <claude|codex|gemini>   надрукувати блок хуків для агента
  disp doctor        перевірка оточення й того, які агенти надсилають події
  disp hook <agent>  викликається хуками агента, вручну не потрібен

Агентів у контейнерах видно без хуків (DISP_CONTAINERS=0 — вимкнути).
Мова: DISP_LANG=en|ru|uk, інакше — мова системи.
"
    )
}
