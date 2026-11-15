//! Команды для человека: таблица, подробности, приёмка, журнал, уборка, проверка окружения.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Local};
use serde_json::Value;

use crate::VERSION;
use crate::board::{self, Accepted, Board, Counts, Row, or_question, status_name};
use crate::config::env_nonempty;
use crate::detect::{self, which};
use crate::model::Status;
use crate::panes;
use crate::store;
use crate::text::{age, clip, emit, pad, warn};

/// Суммарная ширина всех колонок таблицы, кроме последней («ЧТО»).
const FIXED_COLUMNS: usize = 81;
/// До скольких строк `disp gc` укорачивает журнал.
const JOURNAL_KEEP: usize = 5000;

/// `disp ls [-a] [--json]`: таблица незакрытых сессий.
pub fn ls(args: &[String]) -> u8 {
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
        return "Незакрытых сессий нет.  (disp ls -a покажет и убранные)".to_string();
    }

    let mut lines = vec![Counts::of(&rows).line()];
    let header = format!(
        "{} {} {} {} {} {} {} ЧТО",
        pad("ID", 6),
        pad("СТАТУС", 10),
        pad("АГЕНТ", 6),
        pad("ЗАДАЧА", 22),
        pad("ПОДЗ", 7),
        pad("ГДЕ", 18),
        pad("ВОЗР", 4),
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
            if s.accepted { "  [убрано]" } else { "" },
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
    loop {
        let board = Board::load();
        let table = table(&board, term_cols(), false, true);
        emit(&format!(
            "\x1b[H\x1b[2J{}  disp {VERSION}\n\n{table}\n",
            Local::now().format("%H:%M:%S")
        ));
        thread::sleep(pause);
    }
}

/// `disp show <id|задача>`: подробности сессии.
pub fn show(args: &[String]) -> u8 {
    let Some(query) = args.first() else {
        warn("использование: disp show <id|задача>");
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
        emit(&format!("Не найдено: {query}\n"));
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
    let closed = if s.ended {
        let reason = s.end_reason.as_deref().filter(|reason| !reason.is_empty());
        format!(
            " (сессия закрыта{})",
            reason
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        )
    } else {
        String::new()
    };
    lines.push(format!(
        "  статус:    {}{closed}{}",
        s.status.as_str(),
        if s.accepted { " [убрано]" } else { "" }
    ));
    if !s.detail.is_empty() {
        lines.push(format!("  деталь:    {}", s.detail));
    }
    lines.push(format!("  что:       {}", s.title));
    if let Some(prompt) = s
        .last_prompt
        .as_deref()
        .filter(|text| !text.is_empty() && *text != s.title)
    {
        lines.push(format!("  промпт:    {prompt}"));
    }
    if let Some(reply) = s.last_reply.as_deref().filter(|text| !text.is_empty()) {
        lines.push(format!("  ответ:     {reply}"));
    }
    lines.push(format!("  где:       {}", or_question(&s.place)));
    lines.push(format!("  каталог:   {}", s.cwd));
    lines.push(format!("  сессия:    {}", s.session_id));
    lines.push(format!("  начата:    {}", utc(s.started)));
    lines.push(format!("  обновлена: {}", utc(s.updated)));
    if let Some(done_at) = s.done_at {
        lines.push(format!("  ход окончен: {}", utc(done_at)));
    }
    if s.subagents > 0 {
        lines.push(format!("  субагентов в работе: {}", s.subagents));
    }
    if s.bg > 0 {
        lines.push(format!("  фоновых задач на момент остановки: {}", s.bg));
    }
    if !s.subtasks.is_empty() {
        lines.push("  подзадачи:".to_string());
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

/// `disp ok [-f] <id|задача>... | --ended | --done`: принять результат.
pub fn ok(args: &[String]) -> u8 {
    if args.is_empty() {
        warn("использование: disp ok [-f] <id|задача>... | --ended | --done");
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
                    emit(&format!("убрано: {summary}\n"));
                }
                Ok(Accepted::StillActive) => {
                    skipped += 1;
                    let state = if row.session.status == Status::Waiting {
                        "ждёт тебя"
                    } else {
                        "работает"
                    };
                    warn(&format!("пропущено (ещё {state}): {summary}"));
                }
                Err(_) => {}
            }
        }
    }
    if skipped > 0 {
        warn("Активные сессии не убраны. Если сессия мертва: disp ok -f <id>");
    }
    if accepted == 0 {
        if skipped == 0 {
            warn(&format!("Ничего не найдено под: {}", args.join(" ")));
        }
        return 1;
    }
    0
}

/// `disp yes <id|задача>`: ответить агенту вариантом по умолчанию — нажать Enter в его
/// панели zellij. Только для одной сессии, которая сейчас ждёт человека.
pub fn yes(args: &[String]) -> u8 {
    let [query] = args else {
        warn("использование: disp yes <id|задача>");
        return 2;
    };
    if query.starts_with('-') {
        warn("использование: disp yes <id|задача>");
        return 2;
    }
    let board = Board::load();
    let found = matching(&board, query);
    let row = match found.as_slice() {
        [] => {
            warn(&format!("Не найдено: {query}"));
            return 1;
        }
        [row] => row,
        _ => {
            warn(&format!(
                "Под «{query}» несколько сессий ({}); укажи id",
                found
                    .iter()
                    .map(|row| row.sid.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return 1;
        }
    };
    // Enter уходит в панель агента: решаем по только что перечитанной записи.
    let _lock = board.store.lock();
    let Some(session) = store::read_session(&row.path) else {
        warn(&format!("Не найдено: {query}"));
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
    let [query, words @ ..] = args else {
        warn("использование: disp name <id> <текст>");
        return 2;
    };
    if words.is_empty() {
        warn("использование: disp name <id> <текст>");
        return 2;
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
        warn(&format!("Не найдено: {query}"));
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
    emit(&format!("подписано панелей: {labelled}\n"));
    if !panes::enabled(&board.settings) {
        emit("Подписи не будут обновляться сами: добавь DISP_PANE_LABEL=1 в настройки.\n");
    }
    0
}

/// `disp repair`: дополнить записи прежних версий каталогом запуска агента
/// и исправить по нему задачу. Панель делает это сама при запуске.
pub fn repair() -> u8 {
    let repaired = board::repair_roots(&Board::load());
    emit(&format!("исправлено записей: {repaired}\n"));
    0
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
    emit(&format!("снято подписей: {cleared}\n"));
    if panes::enabled(&board.settings) {
        emit("Подписи появятся снова: убери DISP_PANE_LABEL=1 из настроек.\n");
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
        emit("Журнал пуст.\n");
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

/// Местное время в заданном формате; пусто, если метка времени некорректна.
pub fn local_time(ts: i64, format: &str) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|time| time.with_timezone(&Local).format(format).to_string())
        .unwrap_or_default()
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
    emit(&format!(
        "удалено записей: {removed} (убранные и пустые старше {days} дн.)\n"
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
    out.push_str(&format!("программа: {}\n", exe.display()));
    out.push_str(&format!("состояние: {}\n", paths.state_dir.display()));
    let conf_note = if paths.conf_file.is_file() {
        ""
    } else {
        " (нет файла)"
    };
    out.push_str(&format!(
        "настройки: {}{conf_note}\n",
        paths.conf_file.display()
    ));
    for tool in ["notify-send", "curl", "zellij"] {
        let mark = if which(tool).is_some() {
            "ok "
        } else {
            "нет"
        };
        out.push_str(&format!("  {mark}  {tool}\n"));
    }
    let telegram = board.settings.get("DISP_TG_TOKEN").is_some()
        && board.settings.get("DISP_TG_CHAT").is_some();
    out.push_str(&format!(
        "подписи панелей: {}\n",
        if panes::enabled(&board.settings) {
            "включены"
        } else {
            "выключены (DISP_PANE_LABEL=1 включает)"
        }
    ));
    out.push_str(&format!(
        "Telegram:  {}\n",
        if telegram {
            "настроен"
        } else {
            "не настроен"
        }
    ));

    let rows = board.rows();
    for agent in ["claude", "codex", "gemini"] {
        let last = rows
            .iter()
            .filter(|row| row.session.agent == agent)
            .map(|row| row.session.updated)
            .max();
        let seen = match last {
            Some(ts) => format!(
                "последнее событие {} мин назад",
                (board.now - ts).max(0) / 60
            ),
            None => "событий не было".to_string(),
        };
        out.push_str(&format!("  {agent:<7} {seen}\n"));
    }
    if let Ok(log) = fs::read(paths.hook_log())
        && !log.is_empty()
    {
        out.push_str("последние строки hook.log:\n");
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
    format!(
        "disp {VERSION} — диспетчер сессий AI-агентов

  disp               таблица незакрытых сессий (то же, что disp ls)
  disp ls [-a]       -a: показать и убранные; --json: сырые данные
  disp watch         живая панель для отдельной панели zellij: стрелки — выбор,
                     a — ответить агенту «да», x — убрать, g — перейти к агенту,
                     t — лента времени,
                     ? — все клавиши
  disp watch --plain [сек]   таблица без управления, обновляется раз в N секунд
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
"
    )
}
