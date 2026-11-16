//! Что показывает значок: цвет, число и пункты меню. Считается из строк доски и не знает
//! ничего о трее, поэтому проверяется обычными тестами.

use std::path::PathBuf;

use disp::board::{Counts, Row, or_question};
use disp::model::{Status, View};
use disp::panes;
use disp::text::{clip, oneline};

/// Сколько сессий одной группы показывается в меню; остальные — строкой «ещё N».
const ENTRIES_MAX: usize = 10;
/// Сколько символов названия сессии помещается в пункт меню.
const TITLE_MAX: usize = 48;

/// Насколько срочно: определяет вид значка.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// Кто-то ждёт человека: ответа, разрешения или разбора сбоя.
    Attention,
    /// Сессии работают, человек не нужен.
    Busy,
    /// Не работает ничего, остались законченные сессии, результат которых ещё не убран.
    Done,
    /// Ни работающих, ни законченных: открытых сессий нет или они простаивают.
    #[default]
    Calm,
}

/// Что делает пункт меню.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Ответить ждущему агенту «да»: Enter в его панели.
    Approve(String),
    /// Перевести фокус zellij в панель сессии.
    Focus(String),
    /// Убрать сессию из списка: результат посмотрен.
    Remove(String),
    /// Открыть графическую оболочку.
    Open,
    /// Убрать значок.
    Quit,
}

impl Action {
    /// Идентификатор пункта меню: вид действия и ключ сессии.
    pub fn id(&self) -> String {
        match self {
            Action::Approve(key) => format!("approve:{key}"),
            Action::Focus(key) => format!("focus:{key}"),
            Action::Remove(key) => format!("remove:{key}"),
            Action::Open => "open".to_string(),
            Action::Quit => "quit".to_string(),
        }
    }

    /// Действие по идентификатору пункта меню; `None` для чужих идентификаторов.
    pub fn parse(id: &str) -> Option<Action> {
        match id.split_once(':') {
            Some(("approve", key)) => Some(Action::Approve(key.to_string())),
            Some(("focus", key)) => Some(Action::Focus(key.to_string())),
            Some(("remove", key)) => Some(Action::Remove(key.to_string())),
            None if id == "open" => Some(Action::Open),
            None if id == "quit" => Some(Action::Quit),
            _ => None,
        }
    }

    /// Ключ сессии, над которой выполняется действие.
    pub fn key(&self) -> Option<&str> {
        match self {
            Action::Approve(key) | Action::Focus(key) | Action::Remove(key) => Some(key),
            Action::Open | Action::Quit => None,
        }
    }
}

/// Сессия в меню: подменю со сведениями и кнопками.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Ключ сессии (`Row::key`).
    pub key: String,
    /// Файл сессии: действие перечитывает его перед тем, как что-то делать.
    pub path: PathBuf,
    /// Заголовок подменю: отметка статуса, задача, агент и название.
    pub text: String,
    /// Неактивные строки со сведениями: статус с уточнением, место.
    pub info: Vec<String>,
    /// Кнопки: подпись и действие.
    pub actions: Vec<(String, Action)>,
}

/// Группа сессий в меню.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Заголовок группы с числом сессий в ней.
    pub title: String,
    /// Первые сессии группы.
    pub entries: Vec<Entry>,
    /// Сколько сессий группы не поместилось.
    pub more: usize,
}

/// Всё, что показывает значок.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tray {
    /// Цвет значка.
    pub level: Level,
    /// Число на значке: сколько сессий в группе, которая задала цвет, — ждут, работают,
    /// готовы или простаивают; 0 — открытых сессий нет, значок без числа.
    pub count: usize,
    /// Сводка: первая строка меню и подсказка значка.
    pub summary: String,
    /// Группы сессий.
    pub sections: Vec<Section>,
}

impl Tray {
    /// Значок по строкам доски, уже отсортированным (`Board::visible`).
    ///
    /// Старшинство то же, что в таблице и панели: сначала те, кто ждёт человека, затем
    /// работающие, затем простаивающие, в конце готовые. По нему же выбирается вид
    /// значка: готовые красят его, только когда больше ничего не идёт. Число на значке —
    /// сколько сессий в группе, которая его покрасила; молчащие считаются с работающими,
    /// как и в меню.
    pub fn from_rows(rows: &[Row]) -> Tray {
        let counts = Counts::of(rows);
        let idle = rows.iter().filter(|row| row.view == View::Idle).count();
        let working = counts.working + counts.quiet;
        let (level, count) = if counts.attention > 0 {
            (Level::Attention, counts.attention)
        } else if working > 0 {
            (Level::Busy, working)
        } else if counts.done > 0 {
            (Level::Done, counts.done)
        } else {
            (Level::Calm, idle)
        };
        let summary = if rows.is_empty() {
            "Незакрытых сессий нет".to_string()
        } else {
            counts.line_with_idle(idle)
        };
        let groups: [(&str, &[View]); 4] = [
            ("Ждут тебя", &[View::Wait, View::Err, View::Lost, View::Stuck]),
            ("Работают", &[View::Work, View::Quiet]),
            ("Простой", &[View::Idle]),
            ("Готово", &[View::Done]),
        ];
        let sections = groups
            .into_iter()
            .filter_map(|(title, views)| section(title, rows, views))
            .collect();
        Tray {
            level,
            count,
            summary,
            sections,
        }
    }

    /// Сессия меню по ключу.
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.sections
            .iter()
            .flat_map(|section| &section.entries)
            .find(|entry| entry.key == key)
    }
}

/// Группа из строк с видами `views`; `None`, если таких строк нет.
fn section(title: &str, rows: &[Row], views: &[View]) -> Option<Section> {
    let members: Vec<&Row> = rows.iter().filter(|row| views.contains(&row.view)).collect();
    if members.is_empty() {
        return None;
    }
    Some(Section {
        title: format!("{title}: {}", members.len()),
        entries: members.iter().take(ENTRIES_MAX).map(|row| entry(row)).collect(),
        more: members.len().saturating_sub(ENTRIES_MAX),
    })
}

/// Пункт меню одной сессии.
fn entry(row: &Row) -> Entry {
    let s = &row.session;
    let mark = row.view.label().chars().next().unwrap_or(' ');
    let mut text = format!("{mark} {} · {}", or_question(&s.label()), or_question(&s.agent));
    let name = [Some(&s.title), s.last_prompt.as_ref()]
        .into_iter()
        .flatten()
        .map(|text| oneline(text))
        .find(|text| !text.is_empty());
    if let Some(name) = name {
        text.push_str(&format!(" — {}", clip(&name, TITLE_MAX)));
    }

    let status = [row.view.label(), row.note.as_deref().unwrap_or_default(), &s.detail]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let info = vec![status, row.place().to_string()];

    let key = row.key();
    let mut actions = Vec::new();
    if s.status == Status::Waiting && !s.ended {
        actions.push(("Ответить «да»".to_string(), Action::Approve(key.clone())));
    }
    if !s.ended && panes::target(&s.place).is_some() {
        actions.push(("Перейти к панели".to_string(), Action::Focus(key.clone())));
    }
    if !row.is_active() && !s.accepted {
        actions.push(("Убрать из списка".to_string(), Action::Remove(key.clone())));
    }
    Entry {
        key,
        path: row.path.clone(),
        text,
        info,
        actions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disp::model::Session;
    use std::path::Path;

    const NOW: i64 = 1_800_000_000;

    fn row(sid: &str, status: Status, place: &str, title: &str) -> Row {
        let session = Session {
            agent: "claude".to_string(),
            session_id: sid.to_string(),
            task: "site-31".to_string(),
            place: place.to_string(),
            status,
            title: title.to_string(),
            updated: NOW - 60,
            ..Session::default()
        };
        Row::new(PathBuf::from(format!("/s/claude-{sid}.json")), session, NOW, 30)
    }

    fn labels(entry: &Entry) -> Vec<&str> {
        entry.actions.iter().map(|(label, _)| label.as_str()).collect()
    }

    #[test]
    fn empty_board_is_calm() {
        let tray = Tray::from_rows(&[]);
        assert_eq!(tray.level, Level::Calm);
        assert_eq!(tray.count, 0);
        assert_eq!(tray.summary, "Незакрытых сессий нет");
        assert!(tray.sections.is_empty());
    }

    #[test]
    fn waiting_beats_done_and_is_counted() {
        let mut lost = row("c3", Status::Working, "zellij:dev#5", "Сборка");
        lost.session.ended = true;
        lost.view = View::Lost;
        let rows = [
            row("a1", Status::Waiting, "zellij:dev#4", "Починить импорт"),
            lost,
            row("b2", Status::Done, "zellij:dev#3", "Отчёт"),
            row("d4", Status::Working, "vscode", "Тесты"),
        ];
        let tray = Tray::from_rows(&rows);
        assert_eq!(tray.level, Level::Attention);
        assert_eq!(tray.count, 2);
        let titles: Vec<&str> = tray.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Ждут тебя: 2", "Работают: 1", "Готово: 1"]);

        let waiting = &tray.sections[0].entries[0];
        assert_eq!(waiting.text, "! site-31 · claude — Починить импорт");
        assert_eq!(waiting.info, ["! ЖДЁТ", "zellij:dev#4"]);
        // Ждущую сессию не убрать: она ещё идёт.
        assert_eq!(labels(waiting), ["Ответить «да»", "Перейти к панели"]);
        // Закрытую — некуда переходить, но можно убрать.
        let lost = &tray.sections[0].entries[1];
        assert_eq!(lost.info, ["x ПРЕРВАНА", "закрыта"]);
        assert_eq!(labels(lost), ["Убрать из списка"]);
        // Работающая не в zellij: кнопок нет, только сведения.
        assert!(tray.sections[1].entries[0].actions.is_empty());
        assert_eq!(labels(&tray.sections[2].entries[0]), ["Перейти к панели", "Убрать из списка"]);
    }

    #[test]
    fn working_outranks_idle_and_done() {
        let rows = [
            row("a1", Status::Done, "zellij:dev#4", ""),
            row("b2", Status::Done, "zellij:dev#3", ""),
            row("c3", Status::Idle, "zellij:dev#5", "Черновик"),
            row("d4", Status::Working, "zellij:dev#2", ""),
        ];
        let tray = Tray::from_rows(&rows);
        // Пока что-то работает, на значке — сколько работает, а не сколько готово.
        assert_eq!((tray.level, tray.count), (Level::Busy, 1));
        let titles: Vec<&str> = tray.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Работают: 1", "Простой: 1", "Готово: 2"]);
        // Сводка в порядке меню и таблицы; простой — перед готовыми.
        assert_eq!(
            tray.summary,
            "ждут тебя: 0 · работают: 1 · молчат: 0 · простой: 1 · готово: 2"
        );
        // Простаивающую можно убрать: она не работает.
        assert_eq!(labels(&tray.sections[1].entries[0]), ["Перейти к панели", "Убрать из списка"]);
        // Без названия — только задача и агент.
        assert_eq!(tray.sections[2].entries[0].text, "✓ site-31 · claude");

        let done = Tray::from_rows(&rows[..3]);
        assert_eq!((done.level, done.count), (Level::Done, 2));
        let idle = Tray::from_rows(&rows[2..3]);
        assert_eq!((idle.level, idle.count), (Level::Calm, 1));
        assert_ne!(idle.summary, "Незакрытых сессий нет");
    }

    #[test]
    fn quiet_sessions_count_as_working() {
        let mut quiet = row("q1", Status::Working, "zellij:dev#6", "");
        quiet.session.updated = NOW - 3600;
        quiet.view = View::Quiet;
        let rows = [row("w1", Status::Working, "zellij:dev#2", ""), quiet];
        let tray = Tray::from_rows(&rows);
        assert_eq!((tray.level, tray.count), (Level::Busy, 2));
        assert_eq!(tray.sections[0].title, "Работают: 2");
    }

    #[test]
    fn long_groups_are_cut() {
        let rows: Vec<Row> = (0..13)
            .map(|n| row(&format!("s{n:02}"), Status::Done, "zellij:dev#1", "x"))
            .collect();
        let tray = Tray::from_rows(&rows);
        assert_eq!(tray.sections[0].entries.len(), ENTRIES_MAX);
        assert_eq!(tray.sections[0].more, 3);
    }

    #[test]
    fn action_ids_round_trip() {
        let actions = [
            Action::Approve("claude-0193-ab".to_string()),
            Action::Focus("codex-x:y".to_string()),
            Action::Remove("gemini-1".to_string()),
            Action::Open,
            Action::Quit,
        ];
        for action in actions {
            assert_eq!(Action::parse(&action.id()), Some(action.clone()));
        }
        assert_eq!(Action::parse("1000"), None);
        assert_eq!(Action::parse("delete:claude-1"), None);
    }

    #[test]
    fn entries_are_found_by_key() {
        let rows = [row("a1", Status::Waiting, "zellij:dev#4", "x")];
        let tray = Tray::from_rows(&rows);
        let key = rows[0].key();
        let path = tray.entry(&key).map(|entry| entry.path.as_path());
        assert_eq!(path, Some(Path::new("/s/claude-a1.json")));
        assert!(tray.entry("claude-zz").is_none());
    }
}
