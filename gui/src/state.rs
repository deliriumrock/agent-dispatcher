//! Состояние оболочки: строки, выбор, сортировка — и действия над сессиями.
//!
//! Окна здесь нет: состояние превращается в данные для окна функцией [`State::view`],
//! а клавиши и щелчки приходят как [`Pressed`] и имена команд.

use std::time::SystemTime;

use chrono::Local;
use disp::board::{self, Accepted, Board, Counts, Row, Sort, Tone, short_place};
use disp::config::{self, Paths};
use disp::observe;
use disp::model::View;
use disp::panes;
use disp::store::{self, Signature, Store};
use disp::text::{age, latin_key};
use disp::timeline::{History, Window, span_label};
use disp::{t, tr};
use slint::{ModelRc, SharedString, VecModel};

use crate::{DetailField, Hint, Pressed, Segment, SessionRow, Tick};

/// Какой длины бывает окно ленты времени, в секундах: от получаса до суток.
const SPANS: [i64; 6] = [1_800, 3_600, 10_800, 21_600, 43_200, 86_400];
/// Окно ленты при запуске: три часа.
const SPAN_DEFAULT: i64 = 10_800;
/// Сколько секунд сообщение о результате действия остаётся в нижней полосе.
const NOTICE_SEC: i64 = 5;
/// Сколько последних записей журнала идёт в ленты времени.
const HISTORY_LINES: usize = 20_000;
/// На сколько делений разбито окно ленты: раз в деление она сдвигается.
const LANE_SLOTS: usize = 600;
/// На сколько строк сдвигают выбор клавиши PageUp и PageDown.
const PAGE: usize = 8;
/// Ширина окна, с которой видны ленты времени.
const LANE_FROM_WIDTH: f32 = 620.0;

/// Что нужно сделать с самим окном после действия.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Ничего, только показать новое состояние.
    None,
    /// Прокрутить список к выбранной строке.
    Reveal,
    /// Передать ввод строке названия.
    StartRename,
    /// Закрыть окно.
    Quit,
}

/// Сообщение в нижней полосе.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: String,
    error: bool,
    until: i64,
}

/// Данные для окна.
pub struct ViewData {
    /// Ширина ленты времени в логических пикселях; ноль — лент нет.
    pub lane_width: f32,
    /// Строки списка.
    pub rows: ModelRc<SessionRow>,
    /// Подписи шкалы над лентами.
    pub ticks: ModelRc<Tick>,
    /// Подробности выбранной сессии; пусто, если они закрыты.
    pub details: ModelRc<DetailField>,
    /// Подсказки по клавишам.
    pub hints: ModelRc<Hint>,
    /// Счётчики: ждут человека, готово, работают, молчат.
    pub counts: [i32; 4],
    /// Время.
    pub clock: SharedString,
    /// Сортировка словами.
    pub sort: SharedString,
    /// Длина окна ленты словами.
    pub span: SharedString,
    /// Показаны ли принятые сессии.
    pub show_all: bool,
    /// Открыты ли подробности.
    pub details_open: bool,
    /// Номер выбранной строки.
    pub selected: i32,
    /// Вид выбранной строки: по нему окно берёт цвет.
    pub selected_kind: i32,
    /// Заголовок подробностей.
    pub selected_title: SharedString,
    /// Сообщение о последнем действии.
    pub notice: SharedString,
    /// Сообщение об ошибке или отказе.
    pub notice_error: bool,
    /// Вопрос о приёмке работающей сессии; пусто, если вопроса нет.
    pub question: SharedString,
    /// Идёт ли ввод названия.
    pub renaming: bool,
}

/// Состояние оболочки.
pub struct State {
    paths: Paths,
    rows: Vec<Row>,
    now: i64,
    selected: usize,
    selected_key: Option<String>,
    sort: Sort,
    show_all: bool,
    details: bool,
    span: i64,
    history: History,
    seen: Option<(Signature, i64)>,
    history_seen: Option<Option<(SystemTime, u64)>>,
    notice: Option<Notice>,
    confirm: bool,
    renaming: bool,
}

impl State {
    /// Состояние по файлам диспетчера. Заодно исправляет записи прежних версий.
    pub fn new() -> Self {
        board::repair_roots(&Board::load());
        let paths = Paths::from_env();
        // Агенты в контейнерах: фоновый поток записывает перемены у них в файлы сессий,
        // а окно видит их, как события хуков, не дожидаясь `docker exec`.
        observe::watch(Store::new(paths.clone()));
        State::empty(paths)
    }

    fn empty(paths: Paths) -> Self {
        State {
            paths,
            rows: Vec::new(),
            now: 0,
            selected: 0,
            selected_key: None,
            sort: Sort::default(),
            show_all: false,
            details: false,
            span: SPAN_DEFAULT,
            history: History::default(),
            seen: None,
            history_seen: None,
            notice: None,
            confirm: false,
            renaming: false,
        }
    }

    /// Перечитывает сессии и журнал.
    pub fn reload(&mut self) {
        let board = Board::load();
        let rows = board.visible(self.show_all);
        self.set_rows(rows, board.now);
        // Журнал для лент перечитывается, только когда он изменился.
        let journal = store::file_stamp(&self.paths.events());
        if self.history_seen != Some(journal) {
            self.history = History::from_events(&board::journal(&self.paths, HISTORY_LINES));
            self.history_seen = Some(journal);
        }
        self.seen = Some(self.stamp(config::now()));
    }

    /// Перечитывает сессии, если изменился их каталог, истекло сообщение или пришло время
    /// обновить возрасты. Возвращает, было ли что перечитывать.
    pub fn refresh(&mut self) -> bool {
        let now = config::now();
        let expired = self
            .notice
            .as_ref()
            .is_some_and(|notice| now >= notice.until);
        if self.seen.as_ref() == Some(&self.stamp(now)) && !expired {
            return false;
        }
        self.reload();
        true
    }

    /// Отпечаток того, что видно в окне: файлы сессий и время с точностью, с какой
    /// оно показано. В панели терминала каждая перерисовка — целый кадр, поэтому
    /// без подробностей окно обновляется раз в минуту, а с ними — раз в секунду:
    /// там видно, сколько секунд назад было событие.
    fn stamp(&self, now: i64) -> (Signature, i64) {
        let step = if self.details { 1 } else { 60 };
        (
            store::dir_signature(&self.paths.sessions),
            now.div_euclid(step),
        )
    }

    /// Заменяет строки свежими. Выбор остаётся на той же сессии; если её больше нет
    /// в списке, он остаётся на том же месте.
    fn set_rows(&mut self, mut rows: Vec<Row>, now: i64) {
        board::sort_rows(&mut rows, self.sort);
        self.rows = rows;
        self.now = now;
        let kept = self
            .selected_key
            .as_ref()
            .and_then(|key| self.rows.iter().position(|row| row.key() == *key));
        self.select(kept.unwrap_or(self.selected));
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| now >= notice.until)
        {
            self.notice = None;
        }
    }

    /// Ставит выбор на строку `index` (или на последнюю, если строк меньше).
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        self.selected_key = self.selected_row().map(Row::key);
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Показывает сообщение в нижней полосе. Срок отсчитывается от текущего времени:
    /// строки в простое перечитываются раз в минуту, и их время может отставать.
    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
            until: config::now() + NOTICE_SEC,
        });
    }

    /// Обрабатывает клавишу.
    pub fn key(&mut self, key: Pressed) -> Outcome {
        if self.confirm {
            self.confirm = false;
            if matches!(key, Pressed::Char('y' | 'Y' | 'н' | 'Н' | 'д' | 'Д')) {
                self.accept(true);
            } else {
                self.notify(t!("not removed", "не убрано", "не прибрано"), false);
            }
            return Outcome::None;
        }
        self.notice = None;
        let key = match key {
            Pressed::Char(ch) => Pressed::Char(latin_key(ch)),
            other => other,
        };
        match key {
            Pressed::Char('q') => return Outcome::Quit,
            Pressed::Up | Pressed::Char('k') => self.select(self.selected.saturating_sub(1)),
            Pressed::Down | Pressed::Char('j') => self.select(self.selected + 1),
            Pressed::PageUp => self.select(self.selected.saturating_sub(PAGE)),
            Pressed::PageDown => self.select(self.selected + PAGE),
            Pressed::Home => self.select(0),
            Pressed::End => self.select(usize::MAX),
            Pressed::Enter | Pressed::Char('d') => {
                self.details = !self.details;
            }
            Pressed::Char('a') => return self.command("approve"),
            Pressed::Char('x') => return self.command("remove"),
            Pressed::Char('u') => {
                if self.selected_row().is_some_and(|row| row.session.accepted) {
                    self.unaccept();
                }
            }
            Pressed::Char('g') => return self.command("focus"),
            Pressed::Char('n') => return self.command("rename"),
            Pressed::Char('s') => {
                self.sort.key = self.sort.key.next();
                self.sort.reversed = false;
                self.resort();
            }
            Pressed::Char('S') => {
                self.sort.reversed = !self.sort.reversed;
                self.resort();
            }
            Pressed::Tab | Pressed::Char('A') => {
                self.show_all = !self.show_all;
                self.reload();
            }
            Pressed::Char('+' | '=') => self.zoom(-1),
            Pressed::Char('-' | '_') => self.zoom(1),
            Pressed::Char('r') => self.reload(),
            _ => return Outcome::None,
        }
        Outcome::Reveal
    }

    /// Выполняет команду над выбранной строкой: `approve` (принять — ответить агенту «да»),
    /// `remove` (убрать из списка), `focus` (перейти) или `rename` (назвать).
    /// Так же называются кнопки в подробностях.
    pub fn command(&mut self, name: &str) -> Outcome {
        let Some(row) = self.selected_row() else {
            return Outcome::None;
        };
        match name {
            // Enter уходит в панель агента: решаем по только что перечитанной записи.
            "approve" if row.view != View::Wait => {
                self.notify(
                    t!(
                        "the agent is not asking anything now",
                        "агент сейчас ничего не спрашивает",
                        "агент зараз нічого не питає"
                    ),
                    true,
                );
            }
            "approve" => self.approve(),
            // Убранную строку клавиша возвращает, про работающую сначала спрашивает.
            "remove" if row.session.accepted => self.unaccept(),
            "remove" if row.is_active() => self.confirm = true,
            "remove" => self.accept(false),
            "focus" => match panes::focus(&row.session) {
                Ok(text) => self.notify(text, false),
                Err(text) => self.notify(text, true),
            },
            "rename" => {
                self.renaming = true;
                return Outcome::StartRename;
            }
            _ => {}
        }
        Outcome::None
    }

    /// Заканчивает ввод названия: непустое записывается выбранной строке.
    pub fn rename(&mut self, title: &str) {
        self.renaming = false;
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let Some((board, lock, mut row)) = self.fresh() else {
            return;
        };
        match board::rename(&mut row, title) {
            Ok(()) => {
                if panes::enabled(&board.settings) && !row.session.ended {
                    panes::apply(&row.session, true);
                }
                self.notify(tr!("named: {title}", "названо: {title}", "названо: {title}"), false);
            }
            Err(err) => self.notify(tr!("not saved: {err}", "не записано: {err}", "не записано: {err}"), true),
        }
        drop(lock);
        self.reload();
    }

    fn accept(&mut self, force: bool) {
        let Some((board, lock, mut row)) = self.fresh() else {
            return;
        };
        match board::accept(&mut row, board.now, force) {
            Ok(Accepted::Yes) => {
                let summary = row.summary();
                self.notify(tr!("removed: {summary}", "убрано: {summary}", "прибрано: {summary}"), false);
            }
            // Пока мы смотрели на экран, сессия снова заработала: спрашиваем.
            Ok(Accepted::StillActive) => self.confirm = true,
            Err(err) => self.notify(tr!("not saved: {err}", "не записано: {err}", "не записано: {err}"), true),
        }
        drop(lock);
        self.reload();
    }

    /// Отвечает агенту выбранной сессии вариантом по умолчанию.
    fn approve(&mut self) {
        let Some((_board, lock, row)) = self.fresh() else {
            return;
        };
        match panes::approve(&row.session) {
            Ok(text) => self.notify(text, false),
            Err(text) => self.notify(text, true),
        }
        drop(lock);
        self.reload();
    }

    fn unaccept(&mut self) {
        let Some((_board, lock, mut row)) = self.fresh() else {
            return;
        };
        match board::unaccept(&mut row) {
            Ok(()) => {
                let summary = row.summary();
                self.notify(
                    tr!(
                        "back in the list: {summary}",
                        "возвращено в список: {summary}",
                        "повернуто до списку: {summary}"
                    ),
                    false,
                );
            }
            Err(err) => self.notify(tr!("not saved: {err}", "не записано: {err}", "не записано: {err}"), true),
        }
        drop(lock);
        self.reload();
    }

    /// Выбранная строка, заново прочитанная с диска под блокировкой хранилища.
    fn fresh(&mut self) -> Option<(Board, Option<std::fs::File>, Row)> {
        let path = self.selected_row()?.path.clone();
        let board = Board::load();
        match board.fresh(&path) {
            Some((lock, row)) => Some((board, lock, row)),
            None => {
                self.notify(
                    t!(
                        "this session is no longer in the list",
                        "этой сессии уже нет в списке",
                        "цієї сесії вже немає в списку"
                    ),
                    true,
                );
                None
            }
        }
    }

    /// Пересортировывает строки по текущему полю. Выбор остаётся на той же сессии.
    fn resort(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        self.set_rows(rows, self.now);
        let sort = self.sort.label();
        self.notify(tr!("sort: {sort}", "сортировка: {sort}", "сортування: {sort}"), false);
    }

    /// Меняет длину окна ленты на `steps` ступеней: меньше нуля — ближе, больше — дальше.
    fn zoom(&mut self, steps: isize) {
        let at = SPANS
            .iter()
            .position(|span| *span == self.span)
            .unwrap_or(2);
        self.span = SPANS[at.saturating_add_signed(steps).min(SPANS.len() - 1)];
    }

    /// Данные для окна шириной `width` логических пикселей.
    pub fn view(&self, width: f32) -> ViewData {
        let window = Window::ending_at(self.now, self.span, LANE_SLOTS);
        let rows: Vec<SessionRow> = self
            .rows
            .iter()
            .map(|row| self.session_row(row, window))
            .collect();
        let counts = Counts::of(self.rows.iter().filter(|row| !row.session.accepted));
        let selected = self.selected_row();
        let details: Vec<DetailField> = match selected {
            Some(row) if self.details => board::details(row, self.now)
                .into_iter()
                .map(|detail| DetailField {
                    key: detail.key.into(),
                    value: detail.value.into(),
                    tone: match detail.tone {
                        Tone::Plain => 0,
                        Tone::Status => 1,
                        Tone::Muted => 2,
                    },
                })
                .collect(),
            _ => Vec::new(),
        };
        let question = match selected {
            Some(row) if self.confirm => {
                let sid = &row.sid;
                let state = if row.view == View::Wait {
                    t!("waiting for you", "ждёт тебя", "чекає на тебе")
                } else {
                    t!("working", "работает", "працює")
                };
                tr!(
                    "{sid} is still {state}. Remove it from the list anyway?  y — yes, any other key — no",
                    "{sid} ещё {state}. Убрать из списка всё равно?  y — да, любая другая клавиша — нет",
                    "{sid} ще {state}. Прибрати зі списку все одно?  y — так, будь-яка інша клавіша — ні"
                )
            }
            _ => String::new(),
        };
        let count = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
        ViewData {
            lane_width: lane_width(width),
            rows: model(rows),
            ticks: model(ticks(window, width)),
            details: model(details),
            hints: model(hints(width)),
            counts: [
                count(counts.attention),
                count(counts.done),
                count(counts.working),
                count(counts.quiet),
            ],
            clock: Local::now().format("%H:%M").to_string().into(),
            sort: self.sort.label().into(),
            span: span_label(self.span).into(),
            show_all: self.show_all,
            details_open: self.details,
            selected: count(self.selected),
            selected_kind: selected.map_or(kind(View::Idle), |row| kind(row.view)),
            selected_title: selected
                .map(|row| {
                    format!(
                        "{} · {} · {}",
                        row.sid,
                        row.session.label(),
                        board::or_question(&row.session.agent)
                    )
                })
                .unwrap_or_default()
                .into(),
            notice: self
                .notice
                .as_ref()
                .map(|notice| notice.text.clone())
                .unwrap_or_default()
                .into(),
            notice_error: self.notice.as_ref().is_some_and(|notice| notice.error),
            question: question.into(),
            renaming: self.renaming,
        }
    }

    /// Строка списка для окна.
    fn session_row(&self, row: &Row, window: Window) -> SessionRow {
        let s = &row.session;
        let subtasks = row.subtasks();
        // Важное рисуется последним и перекрывает остальное.
        let mut lane = self.history.lane(row, self.now);
        lane.sort_by_key(|segment| segment.phase);
        let span = window.span as f32;
        let segments: Vec<Segment> = lane
            .iter()
            .filter(|segment| segment.to > window.start && segment.from < window.end())
            .map(|segment| {
                let start = ((segment.from - window.start) as f32 / span).clamp(0.0, 1.0);
                let end = ((segment.to - window.start) as f32 / span).clamp(0.0, 1.0);
                Segment {
                    start,
                    width: end - start,
                    phase: segment.phase as i32,
                }
            })
            .collect();
        SessionRow {
            sid: row.sid.as_str().into(),
            kind: kind(row.view),
            status: row.view.label().into(),
            agent: board::or_question(&s.agent).into(),
            task: s.label().into(),
            place: short_place(row.place()).into(),
            age: coarse_age(self.now - s.updated).into(),
            headline: row.headline().into(),
            subtasks: if subtasks == "—" {
                SharedString::new()
            } else {
                subtasks.into()
            },
            accepted: s.accepted,
            segments: model(segments),
        }
    }
}

/// Возраст строки с точностью до минуты: окно в простое обновляется раз в минуту.
fn coarse_age(seconds: i64) -> String {
    if seconds < 60 {
        tr!("<1m", "<1м", "<1хв")
    } else {
        age(seconds)
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// Вид строки числом, как его понимает описание окна: по нему берётся цвет.
fn kind(view: View) -> i32 {
    match view {
        View::Wait => 0,
        View::Err => 1,
        View::Lost => 2,
        View::Stuck => 3,
        View::Work => 4,
        View::Quiet => 5,
        View::Idle => 6,
        View::Done => 7,
    }
}

/// Ширина ленты времени: треть окна, а в узком окне лент нет.
fn lane_width(width: f32) -> f32 {
    if width < LANE_FROM_WIDTH {
        0.0
    } else {
        (width * 0.34).max(180.0)
    }
}

/// Подписи шкалы над лентами: круглое местное время, без наложений друг на друга
/// и на подпись длины окна у левого края.
fn ticks(window: Window, width: f32) -> Vec<Tick> {
    // Подпись вида `17:00` с промежутком занимает около 56 пикселей.
    let labels = (lane_width(width) / 56.0).floor().max(1.0) as usize;
    let offset = i64::from(Local::now().offset().local_minus_utc());
    window
        .marks(window.label_step(labels * 7), offset)
        .into_iter()
        .filter_map(|mark| {
            let position = (mark - window.start) as f32 / window.span as f32;
            let local = (mark + offset).rem_euclid(86_400);
            (0.1..0.9).contains(&position).then(|| Tick {
                position,
                label: format!("{:02}:{:02}", local / 3_600, local % 3_600 / 60).into(),
            })
        })
        .collect()
}

/// Подсказки по клавишам: сколько помещается в окно шириной `width`.
fn hints(width: f32) -> Vec<Hint> {
    let hint = |key: &str, text: &str| Hint {
        key: key.into(),
        text: text.into(),
    };
    let lanes = width >= LANE_FROM_WIDTH;
    let mut out = vec![
        hint("↑↓", t!("select", "выбор", "вибір")),
        hint("Enter", t!("details", "подробности", "подробиці")),
    ];
    out.extend([
        hint("a", t!("approve", "принять", "прийняти")),
        hint("x", t!("remove", "убрать", "прибрати")),
        hint("g", t!("go to", "перейти", "перейти")),
    ]);
    if width >= 760.0 {
        out.extend([
            hint("n", t!("name", "назвать", "назвати")),
            hint("s", t!("sort", "сортировка", "сортування")),
        ]);
    }
    if lanes && width >= 900.0 {
        out.push(hint("+/-", t!("timeline zoom", "масштаб ленты", "масштаб стрічки")));
    }
    if width >= 1040.0 {
        out.push(hint("Tab", t!("all", "все", "усі")));
    }
    out.push(hint("q", t!("quit", "выход", "вихід")));
    if width < 520.0 {
        out.drain(..2);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use disp::model::{Session, Status};
    use slint::Model;

    use super::*;

    fn row(agent: &str, task: &str, status: Status, updated: i64) -> Row {
        // Ожидания тестов написаны по-русски.
        disp::i18n::set(disp::i18n::Lang::Ru);
        let session = Session {
            agent: agent.to_string(),
            session_id: format!("0000-{task}"),
            task: task.to_string(),
            title: format!("Задача {task}"),
            place: "zellij:dev#3".to_string(),
            status,
            updated,
            ..Session::default()
        };
        Row::new(
            PathBuf::from(format!("/nonexistent/{task}.json")),
            session,
            1000,
            30,
        )
    }

    fn state() -> State {
        disp::i18n::set(disp::i18n::Lang::Ru);
        let paths = Paths {
            state_dir: PathBuf::from("/nonexistent/state"),
            sessions: PathBuf::from("/nonexistent/state/sessions"),
            conf_file: PathBuf::from("/nonexistent/env"),
        };
        let mut state = State::empty(paths);
        state.set_rows(
            vec![
                row("codex", "done", Status::Done, 800),
                row("claude", "wait", Status::Waiting, 900),
                row("claude", "work", Status::Working, 990),
            ],
            1000,
        );
        state
    }

    fn tasks(state: &State) -> Vec<&str> {
        state
            .rows
            .iter()
            .map(|row| row.session.task.as_str())
            .collect()
    }

    #[test]
    fn rows_are_sorted_and_selection_moves() {
        let mut state = state();
        assert_eq!(tasks(&state), ["wait", "work", "done"]);
        assert_eq!(state.key(Pressed::Down), Outcome::Reveal);
        assert_eq!(state.key(Pressed::Char('о')), Outcome::Reveal);
        assert_eq!(state.selected, 2);
        state.key(Pressed::Home);
        assert_eq!(state.selected, 0);
        // Сортировка по задаче; выбор остаётся на той же сессии.
        state.key(Pressed::Char('s'));
        state.key(Pressed::Char('s'));
        assert_eq!(tasks(&state), ["done", "wait", "work"]);
        assert_eq!(state.selected, 1);
        assert_eq!(state.key(Pressed::Char('q')), Outcome::Quit);
    }

    #[test]
    fn active_session_is_asked_about_before_removing() {
        let mut state = state();
        assert_eq!(state.key(Pressed::Char('x')), Outcome::None);
        assert!(state.confirm);
        let view = state.view(960.0);
        assert_eq!(
            view.question.as_str(),
            "0000wait ещё ждёт тебя. Убрать из списка всё равно?  y — да, любая другая клавиша — нет"
                .replace("0000wait", &state.rows[0].sid)
        );
        // Любая другая клавиша — отказ.
        state.key(Pressed::Char('n'));
        assert!(!state.confirm);
        assert_eq!(
            state.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("не убрано")
        );
        assert_eq!(state.key(Pressed::Char('n')), Outcome::StartRename);
        assert!(state.renaming);
        // Пустое название — отмена ввода, файлы не трогаются.
        state.rename("   ");
        assert!(!state.renaming);
    }

    #[test]
    fn answer_is_only_for_a_waiting_agent() {
        let mut state = state();
        // Вторая строка работает: отвечать нечего, нажатие не уходит.
        state.key(Pressed::Down);
        assert_eq!(state.key(Pressed::Char('a')), Outcome::None);
        assert_eq!(
            state.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("агент сейчас ничего не спрашивает")
        );
        // Пробел больше ничего не делает.
        assert_eq!(state.key(Pressed::Char(' ')), Outcome::None);
        assert!(!state.confirm);
    }

    #[test]
    fn view_describes_rows_for_the_window() {
        let mut state = state();
        state.rows[2].session.accepted = true;
        state.details = true;
        let view = state.view(960.0);
        let rows: Vec<SessionRow> = view.rows.iter().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].kind, rows[1].kind, rows[2].kind), (0, 4, 7));
        assert_eq!(rows[0].status.as_str(), "! ЖДЁТ");
        assert_eq!(rows[0].place.as_str(), "dev#3");
        assert_eq!(rows[0].age.as_str(), "1м");
        assert_eq!(rows[1].age.as_str(), "<1м");
        assert_eq!(rows[2].headline.as_str(), "[убрано] Задача done");
        assert!(rows[2].accepted);
        // Принятая строка в счётчики не входит.
        assert_eq!(view.counts, [1, 0, 1, 0]);
        assert_eq!(view.selected_kind, 0);
        let details: Vec<DetailField> = view.details.iter().collect();
        assert_eq!(details[0].key.as_str(), "статус");
        assert_eq!(details[0].tone, 1);
        assert_eq!(view.sort.as_str(), "статус ▲");
        assert_eq!(view.span.as_str(), "3ч");
    }

    #[test]
    fn lane_segments_are_fractions_of_the_window() {
        let mut state = state();
        state.history = History::from_events(&[disp::store::JournalEvent {
            ts: 100,
            agent: "claude".to_string(),
            sid: "0000-work".to_string(),
            to: "working".to_string(),
            ..disp::store::JournalEvent::default()
        }]);
        state.span = 1_800;
        let window = Window {
            start: -800,
            span: 1_800,
        };
        let work = state
            .rows
            .iter()
            .find(|row| row.session.task == "work")
            .map(|row| state.session_row(row, window));
        let segments: Vec<Segment> = work
            .map(|row| row.segments.iter().collect())
            .unwrap_or_default();
        assert_eq!(segments.len(), 1);
        // Отрезок с 100 по 1000 в окне от −800 длиной 1800: от половины до правого края.
        assert!((segments[0].start - 0.5).abs() < 1e-6);
        assert!((segments[0].width - 0.5).abs() < 1e-6);
        assert_eq!(segments[0].phase, 3);
    }

    #[test]
    fn idle_window_is_refreshed_once_a_minute() {
        let mut state = state();
        // Без подробностей время в отпечатке — минуты, с подробностями — секунды.
        assert_eq!(state.stamp(119).1, 1);
        assert_eq!(state.stamp(120).1, 2);
        state.details = true;
        assert_eq!(state.stamp(119).1, 119);
        assert_eq!(coarse_age(59), "<1м");
        assert_eq!(coarse_age(61), "1м");
    }

    #[test]
    fn notice_lives_from_now_even_when_rows_are_old() {
        let mut state = state();
        // Строки перечитаны давно: их время на минуту позади.
        state.now = config::now() - 60;
        state.notify("эта сессия не в zellij: vscode", true);
        let until = state.notice.as_ref().map_or(0, |notice| notice.until);
        assert!(until >= config::now() + NOTICE_SEC - 1);
        // Перечитанные строки с тем же отставанием сообщение не стирают.
        let rows = std::mem::take(&mut state.rows);
        state.set_rows(rows, config::now() - 60);
        assert!(state.notice.is_some());
    }

    #[test]
    fn narrow_window_gets_fewer_hints() {
        let keys = |width: f32| -> Vec<String> {
            hints(width)
                .iter()
                .map(|hint| hint.key.to_string())
                .collect()
        };
        assert_eq!(keys(480.0), ["a", "x", "g", "q"]);
        assert_eq!(keys(640.0), ["↑↓", "Enter", "a", "x", "g", "q"]);
        assert!(keys(1100.0).contains(&"Tab".to_string()));
    }
}
