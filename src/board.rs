//! Доска: сессии, подготовленные к показу, и действия человека над ними.
//!
//! Этим пользуются и команды (`disp ls`, `disp ok`), и живая панель (`disp watch`),
//! поэтому правила отбора, сортировки и приёмки записаны один раз.

use std::cmp::Ordering;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::config::{self, Paths, Settings};
use crate::detect;
use crate::error::Result;
use crate::model::{Session, Status, View};
use crate::panes;
use crate::procs::{self, Activity};
use crate::store::{self, JournalEvent, Store};
use crate::text::{age, dur, home_relative, local_time, unmarked};

/// Сколько секунд простаивающая сессия остаётся в таблице.
const IDLE_VISIBLE_SEC: i64 = 43_200;

/// Уточняет молчащие строки по процессам агентов. `/proc` смотрится, только если
/// такие строки есть.
fn settle_quiet(rows: &mut [Row], now: i64) {
    let quiet: Vec<usize> = (0..rows.len())
        .filter(|index| rows[*index].view == View::Quiet)
        .collect();
    if quiet.is_empty() {
        return;
    }
    let sessions: Vec<&Session> = quiet.iter().map(|index| &rows[*index].session).collect();
    let activities = procs::activities(&sessions, now);
    for (index, activity) in quiet.into_iter().zip(activities) {
        rows[index].settle(&activity, now);
    }
}

/// Сессия вместе с тем, что вычисляется для показа.
#[derive(Clone, Debug)]
pub struct Row {
    /// Файл сессии.
    pub path: PathBuf,
    /// Запись о сессии.
    pub session: Session,
    /// Короткий идентификатор.
    pub sid: String,
    /// Вид на доске.
    pub view: View,
    /// Что показала проверка процессов молчащей сессии: идущая команда или закрытый агент.
    pub note: Option<String>,
}

impl Row {
    /// Готовит сессию к показу в момент `now`.
    pub fn new(path: PathBuf, session: Session, now: i64, stale_min: i64) -> Self {
        let sid = session.short_id();
        let view = session.view(now, stale_min);
        Row {
            path,
            session,
            sid,
            view,
            note: None,
        }
    }

    /// Уточняет вид молчащей сессии по её процессам (`procs::Activity`): идёт команда —
    /// сессия работает, агента нет — прервана, агент жив без команд — завис.
    pub fn settle(&mut self, activity: &Activity, now: i64) {
        if self.view != View::Quiet {
            return;
        }
        match activity {
            Activity::Command { text, since } => {
                self.view = View::Work;
                self.note = Some(format!("команда {}: {text}", age(now - since)));
            }
            Activity::Gone => {
                self.view = View::Lost;
                self.note = Some("агент закрыт".to_string());
            }
            Activity::Stuck => self.view = View::Stuck,
            Activity::Unknown => {}
        }
    }

    /// Ключ строки, не меняющийся при пересортировке: агент и полный id сессии.
    pub fn key(&self) -> String {
        format!("{}-{}", self.session.agent, self.session.session_id)
    }

    /// Колонка «ГДЕ»: место сессии, а для закрытой — пометка об этом.
    pub fn place(&self) -> &str {
        if self.session.ended {
            "закрыта"
        } else {
            or_question(&self.session.place)
        }
    }

    /// Колонка «ЧТО»: уточнение к статусу и название.
    pub fn what(&self) -> String {
        let s = &self.session;
        let detail = [self.note.as_deref().unwrap_or_default(), s.detail.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        if detail.is_empty() {
            s.title.clone()
        } else {
            format!("{detail} · {}", s.title)
        }
    }

    /// Колонка «ПОДЗ»: `1/3` — подзадачи, `а2` — активные субагенты, `ф1` — фоновые задачи.
    pub fn subtasks(&self) -> String {
        let s = &self.session;
        let mut out = if s.subtasks.is_empty() {
            "—".to_string()
        } else {
            let done = s.subtasks.values().filter(|task| task.d).count();
            format!("{done}/{}", s.subtasks.len())
        };
        if s.subagents > 0 {
            out.push_str(&format!(" а{}", s.subagents));
        }
        if s.bg > 0 {
            out.push_str(&format!(" ф{}", s.bg));
        }
        out
    }

    /// Колонка «ЧТО» для панели, с пометкой о приёмке. У сессии без названия показывается
    /// последний промпт, а если и его нет — начало последнего ответа.
    pub fn headline(&self) -> String {
        let s = &self.session;
        let filled = |text: &Option<String>| text.clone().filter(|text| !text.is_empty());
        let mut what = self.what();
        if s.title.is_empty() {
            let fallback = filled(&s.last_prompt).or_else(|| {
                filled(&s.last_reply).map(|reply| format!("ответ: {}", unmarked(&reply)))
            });
            what.push_str(&fallback.unwrap_or_default());
        }
        if s.accepted {
            format!("[убрано] {what}")
        } else {
            what
        }
    }

    /// Строка для сообщений о приёмке: задача, агент и название.
    pub fn summary(&self) -> String {
        let s = &self.session;
        format!(
            "{} [{}] {}",
            or_question(&s.task),
            or_question(&s.agent),
            s.title
        )
    }

    /// Идёт ли в сессии работа: такую нельзя убрать из списка без явного подтверждения.
    /// Сессия, агента которой больше нет (проверка процессов), не работает.
    pub fn is_active(&self) -> bool {
        let s = &self.session;
        matches!(s.status, Status::Working | Status::Waiting) && !s.ended && self.view != View::Lost
    }
}

/// Место без слова `zellij:`: в узкой колонке важны сессия и номер панели.
pub fn short_place(place: &str) -> &str {
    place.strip_prefix("zellij:").unwrap_or(place)
}

/// Чем выделить поле подробностей сессии.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Обычный текст.
    Plain,
    /// Цветом статуса сессии.
    Status,
    /// Приглушённо: служебное или уже сделанное.
    Muted,
}

/// Поле подробностей сессии: название и значение.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    /// Название поля; пусто у продолжения предыдущего (строки подзадач).
    pub key: &'static str,
    /// Значение.
    pub value: String,
    /// Как выделить значение.
    pub tone: Tone,
}

/// Подробности сессии для показа: статус, название, промпт, ответ, каталоги, времена,
/// подзадачи. Пустые поля пропущены.
pub fn details(row: &Row, now: i64) -> Vec<Detail> {
    let s = &row.session;
    let mut out = Vec::new();
    let mut field = |key: &'static str, value: String, tone: Tone| {
        if !value.is_empty() {
            out.push(Detail { key, value, tone });
        }
    };

    let mut status = row.view.label().to_string();
    if !s.detail.is_empty() {
        status.push_str(&format!(" · {}", s.detail));
    }
    if s.ended {
        status.push_str(" · сессия закрыта");
        if let Some(reason) = s.end_reason.as_deref().filter(|reason| !reason.is_empty()) {
            status.push_str(&format!(" ({reason})"));
        }
    }
    if s.accepted {
        status.push_str(" · убрано из списка");
    }
    field("статус", status, Tone::Status);
    field("что", s.title.clone(), Tone::Plain);
    if let Some(prompt) = s.last_prompt.as_deref().filter(|text| *text != s.title) {
        field("промпт", prompt.to_string(), Tone::Plain);
    }
    if let Some(reply) = s.last_reply.as_deref() {
        field("ответ", reply.to_string(), Tone::Plain);
    }
    field("где", or_question(&s.place).to_string(), Tone::Plain);
    // «Каталог» — тот, по которому названа задача: проект, над которым агент работает,
    // а если отдельного проекта нет — каталог запуска. Остальное показываем, когда отличается.
    let root = s.root.as_deref().unwrap_or(&s.cwd);
    let home = s.project.as_deref().unwrap_or(root);
    field("каталог", home_relative(home), Tone::Plain);
    if home != root {
        field("запущен в", home_relative(root), Tone::Plain);
    }
    if s.cwd != home && s.cwd != root {
        field("сейчас в", home_relative(&s.cwd), Tone::Plain);
    }

    let mut times = vec![
        format!("начата {}", local_time(s.started, "%d.%m %H:%M")),
        format!("событие {} назад", age(now - s.updated)),
    ];
    match (s.turn_started, s.done_at) {
        (Some(from), _) if s.status == Status::Working => {
            times.push(format!("ход идёт {}", dur(now - from)));
        }
        (Some(from), Some(to)) if to >= from => times.push(format!("ход занял {}", dur(to - from))),
        _ => {}
    }
    field("время", times.join(" · "), Tone::Plain);

    let mut extra = Vec::new();
    if s.subagents > 0 {
        extra.push(format!("субагентов в работе: {}", s.subagents));
    }
    if s.bg > 0 {
        extra.push(format!("фоновых задач: {}", s.bg));
    }
    field("ещё", extra.join(" · "), Tone::Plain);
    field("сессия", s.session_id.clone(), Tone::Muted);
    if !s.subtasks.is_empty() {
        field("подзадачи", row.subtasks(), Tone::Plain);
        for task in s.subtasks.values() {
            let (mark, tone) = if task.d {
                ("[x]", Tone::Muted)
            } else {
                ("[ ]", Tone::Plain)
            };
            field("", format!("{mark} {}", task.s), tone);
        }
    }
    out
}

/// Текст или знак вопроса, если текста нет.
pub fn or_question(text: &str) -> &str {
    if text.is_empty() { "?" } else { text }
}

/// Поле, по которому отсортирован список сессий.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    /// По статусу: требующие человека, работающие, простаивающие, готовые.
    #[default]
    Status,
    /// По имени агента.
    Agent,
    /// По имени задачи.
    Task,
    /// По месту: сессия zellij и номер панели.
    Place,
    /// По возрасту: сначала те, у кого событие было недавно.
    Age,
}

impl SortKey {
    /// Следующее поле по кругу, в порядке колонок таблицы.
    pub fn next(self) -> SortKey {
        match self {
            SortKey::Status => SortKey::Agent,
            SortKey::Agent => SortKey::Task,
            SortKey::Task => SortKey::Place,
            SortKey::Place => SortKey::Age,
            SortKey::Age => SortKey::Status,
        }
    }

    /// Название поля для сообщений.
    pub fn title(self) -> &'static str {
        match self {
            SortKey::Status => "статус",
            SortKey::Agent => "агент",
            SortKey::Task => "задача",
            SortKey::Place => "где",
            SortKey::Age => "возраст",
        }
    }
}

/// Порядок строк списка: поле и направление.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sort {
    /// Поле сортировки.
    pub key: SortKey,
    /// Обратный порядок.
    pub reversed: bool,
}

impl Sort {
    /// Поле и направление словами: `задача ▲`, `возраст ▼`.
    pub fn label(self) -> String {
        format!("{} {}", self.key.title(), self.arrow())
    }

    /// Стрелка направления: `▲` — прямой порядок, `▼` — обратный.
    pub fn arrow(self) -> char {
        if self.reversed { '▼' } else { '▲' }
    }
}

/// Сортирует строки. При равенстве по выбранному полю строки идут по статусу,
/// а внутри статуса — самое давнее выше.
pub fn sort_rows(rows: &mut [Row], sort: Sort) {
    rows.sort_by(|a, b| {
        let primary = match sort.key {
            SortKey::Status => Ordering::Equal,
            SortKey::Agent => lowercase(&a.session.agent).cmp(&lowercase(&b.session.agent)),
            SortKey::Task => lowercase(&a.session.label()).cmp(&lowercase(&b.session.label())),
            SortKey::Place => place_key(a).cmp(&place_key(b)),
            SortKey::Age => b.session.updated.cmp(&a.session.updated),
        };
        primary.then_with(|| (a.view, a.session.updated).cmp(&(b.view, b.session.updated)))
    });
    if sort.reversed {
        rows.reverse();
    }
}

fn lowercase(text: &str) -> String {
    text.to_lowercase()
}

/// Ключ сортировки по месту: панели zellij по сессии и номеру (номер — как число,
/// чтобы `#10` шла после `#9`), затем прочие места по алфавиту, в конце закрытые сессии.
fn place_key(row: &Row) -> (u8, String, u64, String) {
    if row.session.ended {
        return (2, String::new(), 0, String::new());
    }
    match panes::target(&row.session.place) {
        Some((zellij, pane)) => (
            0,
            lowercase(zellij),
            pane.parse().unwrap_or(u64::MAX),
            String::new(),
        ),
        None => (1, String::new(), 0, lowercase(&row.session.place)),
    }
}

/// Сводка по строкам доски для верхней строки.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Требуют человека: ждут, сбой, прерваны, зависли.
    pub attention: usize,
    /// Закончены, но ещё не убраны из списка.
    pub done: usize,
    /// Работают.
    pub working: usize,
    /// Числятся работающими, давно молчат, и проверить, что с ними, нельзя.
    pub quiet: usize,
}

impl Counts {
    /// Считает сводку по строкам.
    pub fn of<'a>(rows: impl IntoIterator<Item = &'a Row>) -> Self {
        let views: Vec<View> = rows.into_iter().map(|row| row.view).collect();
        let count = |wanted: &[View]| views.iter().filter(|view| wanted.contains(view)).count();
        Counts {
            attention: count(&[View::Wait, View::Err, View::Lost, View::Stuck]),
            done: count(&[View::Done]),
            working: count(&[View::Work]),
            quiet: count(&[View::Quiet]),
        }
    }

    /// Сводка одной строкой, в порядке таблицы: ждут тебя, работают, молчат, готово.
    pub fn line(&self) -> String {
        self.line_with_idle(0)
    }

    /// Сводка одной строкой, и с простаивающими, если они есть: между молчащими
    /// и готовыми, где они стоят и в таблице.
    pub fn line_with_idle(&self, idle: usize) -> String {
        let idle = if idle > 0 {
            format!(" · простой: {idle}")
        } else {
            String::new()
        };
        format!(
            "ждут тебя: {} · работают: {} · молчат: {}{idle} · готово: {}",
            self.attention, self.working, self.quiet, self.done
        )
    }
}

/// Открытое хранилище, настройки и текущее время: то, с чего начинает каждая команда.
#[derive(Clone, Debug)]
pub struct Board {
    /// Файлы состояния.
    pub store: Store,
    /// Настройки.
    pub settings: Settings,
    /// Текущее время в unix-секундах.
    pub now: i64,
}

impl Board {
    /// Открывает хранилище по переменным окружения и читает настройки.
    pub fn load() -> Self {
        let store = Store::new(Paths::from_env());
        let settings = Settings::load(&store.paths.conf_file);
        Board {
            store,
            settings,
            now: config::now(),
        }
    }

    /// Через сколько минут молчания работающая сессия считается молчащей.
    pub fn stale_min(&self) -> i64 {
        self.settings.int("DISP_STALE_MIN", 30)
    }

    /// Готовит одну сессию к показу.
    pub fn row(&self, path: PathBuf, session: Session) -> Row {
        Row::new(path, session, self.now, self.stale_min())
    }

    /// Сессия из файла `path`, заново прочитанная под блокировкой хранилища: между показом
    /// и действием человека файл мог изменить хук. Блокировка держится, пока жив первый
    /// элемент результата. `None`, если файла уже нет.
    pub fn fresh(&self, path: &Path) -> Option<(Option<File>, Row)> {
        let lock = self.store.lock();
        let session = store::read_session(path)?;
        let mut row = self.row(path.to_path_buf(), session);
        settle_quiet(std::slice::from_mut(&mut row), self.now);
        Some((lock, row))
    }

    /// Все сессии в порядке имён файлов; молчащие уточнены по процессам.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .store
            .all()
            .into_iter()
            .map(|(path, session)| self.row(path, session))
            .collect();
        settle_quiet(&mut rows, self.now);
        rows
    }

    /// Строки доски, отсортированные по статусу: сверху то, что требует человека,
    /// затем работающие, в конце готовые; внутри группы — самое давнее.
    /// Без `all` скрыты убранные сессии и давно простаивающие.
    pub fn visible(&self, all: bool) -> Vec<Row> {
        let mut rows = self.rows();
        if !all {
            rows.retain(|row| {
                let s = &row.session;
                let stale_idle =
                    row.view == View::Idle && (s.ended || self.now - s.updated > IDLE_VISIBLE_SEC);
                !s.accepted && !stale_idle
            });
        }
        sort_rows(&mut rows, Sort::default());
        rows
    }
}

/// Чем закончилась попытка принять результат.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepted {
    /// Сессия убрана из списка, запись сохранена.
    Yes,
    /// Сессия ещё работает или ждёт человека; без `force` она не принимается.
    StillActive,
}

/// Принимает результат сессии. Активную сессию принять нельзя: она исчезла бы из списка,
/// пока ещё работает или ждёт человека; `force` снимает это ограничение.
///
/// Вызывающий держит блокировку хранилища и передаёт только что прочитанную строку.
pub fn accept(row: &mut Row, now: i64, force: bool) -> Result<Accepted> {
    if !force && row.is_active() {
        return Ok(Accepted::StillActive);
    }
    row.session.accepted = true;
    row.session.accepted_at = Some(now);
    store::write_session(&row.path, &row.session)?;
    Ok(Accepted::Yes)
}

/// Возвращает принятую сессию в список.
///
/// Вызывающий держит блокировку хранилища и передаёт только что прочитанную строку.
pub fn unaccept(row: &mut Row) -> Result<()> {
    row.session.accepted = false;
    row.session.accepted_at = None;
    store::write_session(&row.path, &row.session)
}

/// Даёт строке название, которое события агента уже не перезапишут.
///
/// Вызывающий держит блокировку хранилища и передаёт только что прочитанную строку.
pub fn rename(row: &mut Row, title: &str) -> Result<()> {
    row.session.title = title.to_string();
    row.session.tsrc = Some("name".to_string());
    store::write_session(&row.path, &row.session)
}

/// Что нужно дописать в запись сессии, сделанную прежней версией.
struct Repair {
    /// Файл сессии.
    path: PathBuf,
    /// Каталог запуска, если в записи его нет.
    root: Option<String>,
    /// Проект, над которым работает агент, если он не определён.
    project: Option<String>,
    /// Каталог, по которому нужно заново назвать задачу.
    rename_by: Option<String>,
}

/// Дополняет записи прежних версий каталогом запуска агента и проектом, над которым
/// он работает, и исправляет по ним задачу. Возвращает, сколько записей изменено.
///
/// Хук делает то же самое на событиях сессии, но сессия, в которой ничего не происходит,
/// так и показывала бы задачу по временному каталогу.
pub fn repair_roots(board: &Board) -> usize {
    // Каталоги ищем без блокировки: для Claude Code это чтение его транскрипта.
    let repairs: Vec<Repair> = board
        .rows()
        .into_iter()
        .filter_map(|row| {
            let s = &row.session;
            let launched = (s.root.is_none() && s.agent == "claude")
                .then(|| detect::claude_launch_dir(&s.session_id))
                .flatten();
            // Задачу меняем, только когда каталог запуска известен точно: у остальных
            // сессий запоминаем текущий каталог, а имя, возможно заданное вручную, не трогаем.
            let exact = launched.is_some();
            let new_root = match &s.root {
                Some(_) => None,
                None => launched.or_else(|| (!s.cwd.is_empty()).then(|| s.cwd.clone())),
            };
            let root = s.root.clone().or_else(|| new_root.clone())?;
            // Проект ещё не определён, а последний известный каталог агента — глубже
            // каталога запуска: вероятно, там он и работает.
            let project = (s.project.is_none() && s.cwd != root)
                .then(|| detect::project_dir(&root, Path::new(&s.cwd), true, None))
                .flatten();
            let rename_by = project.clone().or_else(|| exact.then(|| root.clone()));
            (new_root.is_some() || project.is_some()).then_some(Repair {
                path: row.path,
                root: new_root,
                project,
                rename_by,
            })
        })
        .collect();
    if repairs.is_empty() {
        return 0;
    }
    let _lock = board.store.lock();
    let mut repaired = 0;
    for repair in repairs {
        let Some(mut session) = store::read_session(&repair.path) else {
            continue;
        };
        // Пока искали каталоги, запись мог дополнить хук: его данные точнее.
        let mut changed = false;
        if session.root.is_none() && repair.root.is_some() {
            session.root = repair.root;
            changed = true;
        }
        if session.project.is_none() && repair.project.is_some() {
            session.project = repair.project;
            changed = true;
        }
        if !changed {
            continue;
        }
        if let Some(dir) = &repair.rename_by {
            let (task, branch) = detect::project(dir);
            session.task = task;
            session.branch = branch;
        }
        if store::write_session(&repair.path, &session).is_ok() {
            repaired += 1;
        }
    }
    repaired
}

/// Последние `limit` записей журнала смен статуса, от старых к новым.
pub fn journal(paths: &Paths, limit: usize) -> Vec<JournalEvent> {
    let bytes = fs::read(paths.events()).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(limit)..]
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Имя статуса в журнале, как его видит человек.
pub fn status_name(status: &str) -> &str {
    match status {
        "new" => "новая",
        "idle" => "простой",
        "working" => "работает",
        "waiting" => "ЖДЁТ",
        "done" => "ГОТОВО",
        "error" => "СБОЙ",
        "ended" => "закрыта",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Subtask;

    fn row(status: Status, ended: bool) -> Row {
        let session = Session {
            agent: "claude".to_string(),
            session_id: "aaaaaaaa-1111-4111-8111-00000000a1b2".to_string(),
            task: "site-31".to_string(),
            title: "Починить импорт цен".to_string(),
            status,
            ended,
            updated: 1000,
            ..Session::default()
        };
        Row::new(PathBuf::from("/nonexistent/x.json"), session, 1000, 30)
    }

    #[test]
    fn columns_describe_the_session() {
        let mut waiting = row(Status::Waiting, false);
        waiting.session.detail = "нужно разрешение: Bash".to_string();
        waiting.session.place = "zellij:dev#3".to_string();
        waiting.session.subtasks.insert(
            "1".to_string(),
            Subtask {
                s: "Шаг".to_string(),
                d: true,
            },
        );
        waiting
            .session
            .subtasks
            .insert("2".to_string(), Subtask::default());
        waiting.session.subagents = 2;
        assert_eq!(
            waiting.what(),
            "нужно разрешение: Bash · Починить импорт цен"
        );
        assert_eq!(waiting.place(), "zellij:dev#3");
        assert_eq!(waiting.subtasks(), "1/2 а2");
        assert_eq!(waiting.summary(), "site-31 [claude] Починить импорт цен");
        assert_eq!(waiting.key(), "claude-aaaaaaaa-1111-4111-8111-00000000a1b2");

        let closed = row(Status::Done, true);
        assert_eq!(closed.place(), "закрыта");
        assert_eq!(closed.subtasks(), "—");
    }

    #[test]
    fn active_session_is_not_accepted_without_force() {
        let mut working = row(Status::Working, false);
        assert_eq!(
            accept(&mut working, 1000, false).ok(),
            Some(Accepted::StillActive)
        );
        assert!(!working.session.accepted);
        // Закрытая посреди работы сессия уже не активна: её принимают без подтверждения.
        assert!(!row(Status::Working, true).is_active());
        assert!(!row(Status::Done, false).is_active());
    }

    #[test]
    fn default_order_is_attention_then_work_then_done() {
        let mut rows = vec![
            row(Status::Done, false),
            row(Status::Idle, false),
            row(Status::Working, false),
            row(Status::Error, false),
            row(Status::Waiting, false),
        ];
        sort_rows(&mut rows, Sort::default());
        let order: Vec<View> = rows.iter().map(|row| row.view).collect();
        assert_eq!(
            order,
            [View::Wait, View::Err, View::Work, View::Idle, View::Done]
        );
    }

    #[test]
    fn rows_sort_by_each_field() {
        let named = |agent: &str, task: &str, place: &str, updated: i64, status: Status| {
            let mut row = row(status, false);
            row.session.agent = agent.to_string();
            row.session.task = task.to_string();
            row.session.place = place.to_string();
            row.session.updated = updated;
            row
        };
        let rows = vec![
            named("codex", "trading", "zellij:dev#10", 900, Status::Working),
            named("claude", "site-31", "zellij:dev#9", 700, Status::Done),
            named("claude", "Nbx-142", "vscode", 950, Status::Waiting),
            named("gemini", "site-31", "zellij:dev#2", 800, Status::Working),
        ];
        let tasks = |key: SortKey, reversed: bool| -> Vec<String> {
            let mut sorted = rows.clone();
            sort_rows(&mut sorted, Sort { key, reversed });
            sorted
                .iter()
                .map(|row| format!("{}@{}", row.session.task, row.session.place))
                .collect()
        };
        assert_eq!(
            tasks(SortKey::Status, false),
            [
                "Nbx-142@vscode",
                "site-31@zellij:dev#2",
                "trading@zellij:dev#10",
                "site-31@zellij:dev#9"
            ]
        );
        // По агенту; у одного агента — по статусу.
        assert_eq!(
            tasks(SortKey::Agent, false),
            [
                "Nbx-142@vscode",
                "site-31@zellij:dev#9",
                "trading@zellij:dev#10",
                "site-31@zellij:dev#2"
            ]
        );
        // По задаче без учёта регистра.
        assert_eq!(
            tasks(SortKey::Task, false),
            [
                "Nbx-142@vscode",
                "site-31@zellij:dev#2",
                "site-31@zellij:dev#9",
                "trading@zellij:dev#10"
            ]
        );
        // По месту: номера панелей как числа, не в zellij — после.
        assert_eq!(
            tasks(SortKey::Place, false),
            [
                "site-31@zellij:dev#2",
                "site-31@zellij:dev#9",
                "trading@zellij:dev#10",
                "Nbx-142@vscode"
            ]
        );
        // По возрасту: сначала самое свежее событие; в обратном порядке — самое давнее.
        assert_eq!(tasks(SortKey::Age, false)[0], "Nbx-142@vscode");
        assert_eq!(tasks(SortKey::Age, true)[0], "site-31@zellij:dev#9");
        assert_eq!(
            Sort {
                key: SortKey::Age,
                reversed: true
            }
            .label(),
            "возраст ▼"
        );
        assert_eq!(SortKey::Age.next(), SortKey::Status);
    }

    #[test]
    fn quiet_row_is_settled_by_its_processes() {
        // Работала и замолчала 40 минут назад.
        let quiet = || {
            let mut row = row(Status::Working, false);
            row.session.updated = 1000;
            Row::new(row.path, row.session, 1000 + 40 * 60, 30)
        };
        let now = 1000 + 40 * 60;
        assert_eq!(quiet().view, View::Quiet);

        let mut command = quiet();
        command.settle(
            &Activity::Command {
                text: "cargo test".to_string(),
                since: now - 42 * 60,
            },
            now,
        );
        assert_eq!(command.view, View::Work);
        assert_eq!(
            command.what(),
            "команда 42м: cargo test · Починить импорт цен"
        );
        assert!(command.is_active());

        let mut gone = quiet();
        gone.settle(&Activity::Gone, now);
        assert_eq!(gone.view, View::Lost);
        assert_eq!(gone.what(), "агент закрыт · Починить импорт цен");
        assert!(
            !gone.is_active(),
            "строку закрытого агента убирают без подтверждения"
        );

        let mut stuck = quiet();
        stuck.settle(&Activity::Stuck, now);
        assert_eq!(stuck.view, View::Stuck);
        assert_eq!(stuck.view.label(), "? завис");

        let mut unknown = quiet();
        unknown.settle(&Activity::Unknown, now);
        assert_eq!(unknown.view.label(), "? молчит");

        let counts = Counts::of([&command, &gone, &stuck, &unknown]);
        assert_eq!(
            counts.line(),
            "ждут тебя: 2 · работают: 1 · молчат: 1 · готово: 0"
        );

        // Проверка не трогает строки, которые не молчат.
        let mut working = row(Status::Working, false);
        working.settle(&Activity::Gone, now);
        assert_eq!(working.view, View::Work);
    }

    #[test]
    fn counts_group_views() {
        let rows = vec![
            row(Status::Waiting, false),
            row(Status::Error, false),
            row(Status::Working, true),
            row(Status::Done, false),
            row(Status::Working, false),
        ];
        let counts = Counts::of(&rows);
        assert_eq!(
            counts,
            Counts {
                attention: 3,
                done: 1,
                working: 1,
                quiet: 0
            }
        );
        assert_eq!(
            counts.line(),
            "ждут тебя: 3 · работают: 1 · молчат: 0 · готово: 1"
        );
    }
}
