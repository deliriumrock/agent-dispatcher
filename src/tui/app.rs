//! Состояние панели и разбор клавиш. Здесь нет ни терминала, ни файлов:
//! клавиша превращается в [`Command`], а выполняет её цикл панели.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::board::{self, Counts, Row, Sort};
use crate::model::View;
use crate::store::JournalEvent;
use crate::timeline::{History, Segment, Window};

/// Сколько секунд сообщение о результате действия остаётся в нижней строке.
const NOTICE_SEC: i64 = 5;
/// Наибольшая длина названия, которое можно ввести в панели.
const TITLE_MAX: usize = 200;
/// Какой длины бывает окно ленты времени, в секундах: от получаса до суток.
const SPANS: [i64; 6] = [1_800, 3_600, 10_800, 21_600, 43_200, 86_400];
/// Окно ленты при запуске: три часа.
const SPAN_DEFAULT: i64 = 10_800;

/// Лента времени одной строки: где она на экране и что на ней.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneBox {
    /// Колонка левого края ленты.
    pub x: u16,
    /// Строка экрана.
    pub y: u16,
    /// Ширина ленты в знакоместах.
    pub cells: u16,
    /// Отрезки состояний.
    pub segments: Vec<Segment>,
    /// Окно времени, которое показывает лента.
    pub window: Window,
    /// Моменты делений шкалы.
    pub ticks: Vec<i64>,
}

/// Что сейчас на экране и куда идут клавиши.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Таблица сессий.
    Board,
    /// Ввод названия для выбранной строки.
    Rename {
        /// Набранный текст.
        input: String,
    },
    /// Вопрос, убрать ли из списка сессию, которая ещё работает или ждёт.
    Confirm,
    /// Журнал смен статуса.
    Journal {
        /// Сколько записей прокручено от самой новой.
        scroll: usize,
    },
    /// Справка по клавишам.
    Help,
}

/// Действие, которое панель должна выполнить по клавише.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Выйти.
    Quit,
    /// Ответить агенту выбранной сессии вариантом по умолчанию: нажать Enter в его панели.
    Approve,
    /// Убрать выбранную сессию из списка: результат посмотрен.
    Accept {
        /// Убрать, даже если сессия ещё работает.
        force: bool,
    },
    /// Вернуть убранную сессию в список.
    Unaccept,
    /// Дать выбранной строке название.
    Rename(String),
    /// Перейти в панель, где идёт выбранная сессия.
    Focus,
    /// Открыть журнал.
    Journal,
    /// Перечитать сессии.
    Reload,
}

/// Сообщение в нижней строке.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Текст сообщения.
    pub text: String,
    /// Сообщение об ошибке или отказе.
    pub error: bool,
    /// До какого времени показывать.
    until: i64,
}

/// Состояние панели.
#[derive(Debug)]
pub struct App {
    /// Строки таблицы в порядке показа.
    pub rows: Vec<Row>,
    /// Сводка для верхней строки.
    pub counts: Counts,
    /// Время, на которое посчитаны строки.
    pub now: i64,
    /// Номер выбранной строки.
    pub selected: usize,
    /// Первая строка, попадающая на экран; за ней следит отрисовка.
    pub offset: usize,
    /// Сколько строк помещается на экране; обновляется при отрисовке.
    pub page: usize,
    /// Показывать ли принятые и давно простаивающие сессии.
    pub show_all: bool,
    /// По какому полю и в каком направлении отсортирован список.
    pub sort: Sort,
    /// Открыта ли панель подробностей.
    pub details: bool,
    /// Что сейчас на экране.
    pub mode: Mode,
    /// Сообщение о последнем действии.
    pub notice: Option<Notice>,
    /// Записи журнала, от старых к новым; читаются при открытии журнала.
    pub journal: Vec<JournalEvent>,
    /// Показывать ли вместо колонки «ЧТО» ленту времени.
    pub timeline: bool,
    /// Длина окна ленты в секундах.
    pub span: i64,
    /// Смены статуса из журнала, по которым строятся ленты.
    pub history: History,
    /// Ширина знакоместа в пикселях, если ленты рисуются картинками; иначе символами.
    pub cell_px: Option<u16>,
    /// Смещение местного времени от UTC в секундах: для подписей шкалы.
    pub utc_offset: i64,
    /// Ленты, для которых при последней отрисовке оставлено место под картинки.
    pub lanes: Vec<LaneBox>,
    /// Ключ выбранной строки: выбор следует за сессией, а не за номером строки.
    selected_key: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            rows: Vec::new(),
            counts: Counts::default(),
            now: 0,
            selected: 0,
            offset: 0,
            page: 1,
            show_all: false,
            sort: Sort::default(),
            details: false,
            mode: Mode::Board,
            notice: None,
            journal: Vec::new(),
            timeline: false,
            span: SPAN_DEFAULT,
            history: History::default(),
            cell_px: None,
            utc_offset: 0,
            lanes: Vec::new(),
            selected_key: None,
        }
    }
}

impl App {
    /// Заменяет строки свежими. Выбор остаётся на той же сессии; если её больше нет
    /// в списке, он остаётся на том же месте.
    pub fn set_rows(&mut self, mut rows: Vec<Row>, now: i64) {
        board::sort_rows(&mut rows, self.sort);
        // Принятые строки видны только в режиме «все» и в сводку не входят.
        self.counts = Counts::of(rows.iter().filter(|row| !row.session.accepted));
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

    /// Выбранная строка.
    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Показывает сообщение в нижней строке.
    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some(Notice {
            text: text.into(),
            error,
            until: self.now + NOTICE_SEC,
        });
    }

    /// Обрабатывает клавишу: меняет состояние и возвращает действие, если оно нужно.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c' | 'с')) {
            return Some(Command::Quit);
        }
        match self.mode.clone() {
            Mode::Board => self.board_key(key.code),
            Mode::Rename { input } => self.rename_key(key.code, ctrl, input),
            Mode::Confirm => {
                self.mode = Mode::Board;
                if matches!(key.code, KeyCode::Char('y' | 'Y' | 'н' | 'Н' | 'д' | 'Д')) {
                    return Some(Command::Accept { force: true });
                }
                self.notify("не убрано", false);
                None
            }
            Mode::Journal { scroll } => {
                self.journal_key(key.code, scroll);
                None
            }
            Mode::Help => {
                self.mode = Mode::Board;
                None
            }
        }
    }

    fn board_key(&mut self, code: KeyCode) -> Option<Command> {
        self.notice = None;
        let code = match code {
            KeyCode::Char(ch) => KeyCode::Char(latin(ch)),
            other => other,
        };
        match code {
            KeyCode::Char('q') => return Some(Command::Quit),
            KeyCode::Esc => self.details = false,
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected + 1),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(self.page.max(1))),
            KeyCode::PageDown => self.select(self.selected + self.page.max(1)),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Enter | KeyCode::Char('d') => self.details = !self.details,
            KeyCode::Char('a') => return self.approve_key(),
            KeyCode::Char('x') => return self.accept_key(),
            KeyCode::Char('u') => {
                let accepted = self.selected_row().is_some_and(|row| row.session.accepted);
                return accepted.then_some(Command::Unaccept);
            }
            KeyCode::Char('g') => return self.selected_row().map(|_| Command::Focus),
            KeyCode::Char('n') => {
                if self.selected_row().is_some() {
                    self.mode = Mode::Rename {
                        input: String::new(),
                    };
                }
            }
            KeyCode::Char('l') => return Some(Command::Journal),
            KeyCode::Tab | KeyCode::Char('A') => {
                self.show_all = !self.show_all;
                return Some(Command::Reload);
            }
            KeyCode::Char('r') => return Some(Command::Reload),
            KeyCode::Char('s') => {
                self.sort.key = self.sort.key.next();
                self.sort.reversed = false;
                self.resort();
            }
            KeyCode::Char('S') => {
                self.sort.reversed = !self.sort.reversed;
                self.resort();
            }
            KeyCode::Char('t') => {
                self.timeline = !self.timeline;
                // Журнал для ленты читает цикл панели при перечитывании.
                return Some(Command::Reload);
            }
            KeyCode::Char('+' | '=') => self.zoom(-1),
            KeyCode::Char('-' | '_') => self.zoom(1),
            KeyCode::Char('?' | 'h') | KeyCode::F(1) => self.mode = Mode::Help,
            _ => {}
        }
        None
    }

    /// Клавиша ответа агенту: только для строки, где агент ждёт человека.
    fn approve_key(&mut self) -> Option<Command> {
        let row = self.selected_row()?;
        if row.view != View::Wait {
            self.notify("агент сейчас ничего не спрашивает", true);
            return None;
        }
        Some(Command::Approve)
    }

    /// Клавиша «убрать»: убранную строку возвращает, про работающую сначала спрашивает.
    fn accept_key(&mut self) -> Option<Command> {
        let row = self.selected_row()?;
        if row.session.accepted {
            return Some(Command::Unaccept);
        }
        if row.is_active() {
            self.mode = Mode::Confirm;
            return None;
        }
        Some(Command::Accept { force: false })
    }

    fn rename_key(&mut self, code: KeyCode, ctrl: bool, mut input: String) -> Option<Command> {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Board;
                return None;
            }
            KeyCode::Enter => {
                self.mode = Mode::Board;
                let title = input.trim();
                return (!title.is_empty()).then(|| Command::Rename(title.to_string()));
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char('u' | 'г') if ctrl => input.clear(),
            KeyCode::Char('w' | 'ц') if ctrl => {
                let kept = input.trim_end().rfind(' ').map_or(0, |at| at + 1);
                input.truncate(kept);
            }
            KeyCode::Char(ch) if !ctrl && input.chars().count() < TITLE_MAX => input.push(ch),
            _ => {}
        }
        self.mode = Mode::Rename { input };
        None
    }

    fn journal_key(&mut self, code: KeyCode, scroll: usize) {
        let code = match code {
            KeyCode::Char(ch) => KeyCode::Char(latin(ch)),
            other => other,
        };
        let last = self.journal.len().saturating_sub(1);
        let page = self.page.max(1);
        let scroll = match code {
            KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll + 1,
            KeyCode::PageUp => scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll + page,
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('l' | 'q') => {
                self.mode = Mode::Board;
                return;
            }
            _ => scroll,
        };
        self.mode = Mode::Journal {
            scroll: scroll.min(last),
        };
    }

    /// Пересортировывает строки по текущему полю. Выбор остаётся на той же сессии.
    fn resort(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        self.set_rows(rows, self.now);
        self.notify(format!("сортировка: {}", self.sort.label()), false);
    }

    /// Меняет длину окна ленты на `steps` ступеней: меньше нуля — ближе, больше — дальше.
    fn zoom(&mut self, steps: isize) {
        if !self.timeline {
            return;
        }
        let at = SPANS
            .iter()
            .position(|span| *span == self.span)
            .unwrap_or(2);
        let next = at.saturating_add_signed(steps).min(SPANS.len() - 1);
        self.span = SPANS[next];
    }

    /// Ставит выбор на строку `index` (или на последнюю, если строк меньше).
    fn select(&mut self, index: usize) {
        self.selected = index.min(self.rows.len().saturating_sub(1));
        self.selected_key = self.selected_row().map(Row::key);
    }
}

/// Латинская буква той же клавиши в русской раскладке: панелью можно управлять,
/// не переключая раскладку.
fn latin(ch: char) -> char {
    match ch {
        'й' | 'Й' => 'q',
        'ф' => 'a',
        'ч' | 'Ч' => 'x',
        'Ф' => 'A',
        'п' | 'П' => 'g',
        'т' | 'Т' => 'n',
        'д' | 'Д' => 'l',
        'о' | 'О' => 'j',
        'л' | 'Л' => 'k',
        'в' | 'В' => 'd',
        'к' | 'К' => 'r',
        'г' | 'Г' => 'u',
        'р' | 'Р' => 'h',
        'е' | 'Е' => 't',
        'ы' => 's',
        'Ы' => 'S',
        other => other,
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::board::SortKey;
    use crate::model::{Session, Status};

    /// Строка доски для тестов: сессия `agent` с задачей `task` в статусе `status`.
    pub fn row(agent: &str, task: &str, status: Status, updated: i64) -> Row {
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

    fn app() -> App {
        let mut app = App::default();
        app.set_rows(
            vec![
                row("claude", "wait", Status::Waiting, 900),
                row("codex", "done", Status::Done, 800),
                row("claude", "work", Status::Working, 990),
            ],
            1000,
        );
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Option<Command> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn selection_moves_and_stops_at_the_edges() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Up), None);
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('о'));
        assert_eq!(app.selected, 2);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.selected, 0);
        press(&mut app, KeyCode::End);
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn selection_follows_the_session_when_rows_reorder() {
        let mut app = app();
        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.selected_row().map(|row| row.session.task.as_str()),
            Some("work")
        );
        // Работавшая сессия закончила ход и ушла в конец списка: выбор ушёл за ней.
        app.set_rows(
            vec![
                row("claude", "work", Status::Done, 995),
                row("claude", "wait", Status::Waiting, 900),
                row("codex", "done", Status::Done, 800),
            ],
            1001,
        );
        assert_eq!(app.selected, 2);
        assert_eq!(
            app.selected_row().map(|row| row.session.task.as_str()),
            Some("work")
        );
        // Сессию приняли в другом месте: выбор остаётся на той же позиции.
        app.set_rows(vec![row("claude", "wait", Status::Waiting, 900)], 1002);
        assert_eq!(app.selected, 0);
        app.set_rows(Vec::new(), 1003);
        assert_eq!(app.selected_row().map(Row::key), None);
        assert_eq!(press(&mut app, KeyCode::Char('a')), None);
        assert_eq!(press(&mut app, KeyCode::Char('g')), None);
    }

    #[test]
    fn finished_session_is_removed_at_once() {
        let mut app = app();
        // Готовая сессия — последняя в списке.
        press(&mut app, KeyCode::End);
        assert_eq!(
            app.selected_row().map(|row| row.session.task.as_str()),
            Some("done")
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('x')),
            Some(Command::Accept { force: false })
        );
        assert_eq!(app.mode, Mode::Board);
        // Русская раскладка: та же клавиша.
        assert_eq!(
            press(&mut app, KeyCode::Char('ч')),
            Some(Command::Accept { force: false })
        );
        // Пробел больше ничего не делает: случайно не ответить и не убрать.
        assert_eq!(press(&mut app, KeyCode::Char(' ')), None);
    }

    #[test]
    fn waiting_agent_gets_the_default_answer() {
        let mut app = app();
        // Первая строка — ждущая: `a` отвечает агенту.
        assert_eq!(press(&mut app, KeyCode::Char('a')), Some(Command::Approve));
        assert_eq!(press(&mut app, KeyCode::Char('ф')), Some(Command::Approve));
        // У работающей и готовой отвечать нечего.
        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Char('a')), None);
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("агент сейчас ничего не спрашивает")
        );
        press(&mut app, KeyCode::End);
        assert_eq!(press(&mut app, KeyCode::Char('a')), None);
    }

    #[test]
    fn active_session_needs_confirmation() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Char('x')), None);
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(press(&mut app, KeyCode::Char('n')), None);
        assert_eq!(app.mode, Mode::Board);
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("не убрано")
        );

        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.mode, Mode::Confirm);
        assert_eq!(
            press(&mut app, KeyCode::Char('y')),
            Some(Command::Accept { force: true })
        );
        assert_eq!(app.mode, Mode::Board);
    }

    #[test]
    fn removed_row_goes_back_to_the_list() {
        let mut app = app();
        app.rows[1].session.accepted = true;
        press(&mut app, KeyCode::Down);
        assert_eq!(press(&mut app, KeyCode::Char('x')), Some(Command::Unaccept));
        assert_eq!(press(&mut app, KeyCode::Char('u')), Some(Command::Unaccept));
        press(&mut app, KeyCode::Up);
        assert_eq!(press(&mut app, KeyCode::Char('u')), None);
    }

    #[test]
    fn rename_collects_text_until_enter() {
        let mut app = app();
        press(&mut app, KeyCode::Char('n'));
        for ch in "Релиз  q".chars() {
            assert_eq!(press(&mut app, KeyCode::Char(ch)), None);
        }
        press(&mut app, KeyCode::Backspace);
        assert_eq!(
            app.mode,
            Mode::Rename {
                input: "Релиз  ".to_string()
            }
        );
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Some(Command::Rename("Релиз".to_string()))
        );
        assert_eq!(app.mode, Mode::Board);

        press(&mut app, KeyCode::Char('т'));
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(press(&mut app, KeyCode::Esc), None);
        assert_eq!(app.mode, Mode::Board);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(press(&mut app, KeyCode::Enter), None);
    }

    #[test]
    fn rename_edits_by_word_and_line() {
        let mut app = app();
        press(&mut app, KeyCode::Char('n'));
        for ch in "первое второе".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            app.mode,
            Mode::Rename {
                input: "первое ".to_string()
            }
        );
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            app.mode,
            Mode::Rename {
                input: String::new()
            }
        );
    }

    #[test]
    fn quit_and_views() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Char('q')), Some(Command::Quit));
        assert_eq!(press(&mut app, KeyCode::Char('й')), Some(Command::Quit));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Command::Quit)
        );
        assert_eq!(press(&mut app, KeyCode::Tab), Some(Command::Reload));
        assert!(app.show_all);
        press(&mut app, KeyCode::Enter);
        assert!(app.details);
        press(&mut app, KeyCode::Esc);
        assert!(!app.details);
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.mode, Mode::Help);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.mode, Mode::Board);
        assert_eq!(press(&mut app, KeyCode::Char('g')), Some(Command::Focus));
        assert_eq!(press(&mut app, KeyCode::Char('l')), Some(Command::Journal));
    }

    #[test]
    fn journal_scrolls_within_its_length() {
        let mut app = app();
        app.journal = vec![JournalEvent::default(); 5];
        app.mode = Mode::Journal { scroll: 0 };
        press(&mut app, KeyCode::Up);
        assert_eq!(app.mode, Mode::Journal { scroll: 0 });
        press(&mut app, KeyCode::End);
        assert_eq!(app.mode, Mode::Journal { scroll: 4 });
        press(&mut app, KeyCode::Down);
        assert_eq!(app.mode, Mode::Journal { scroll: 4 });
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.mode, Mode::Board);
    }

    #[test]
    fn sort_key_cycles_and_selection_follows_the_session() {
        let mut app = app();
        // По умолчанию по статусу: ждущая, работающая, готовая.
        let tasks = |app: &App| -> Vec<String> {
            app.rows
                .iter()
                .map(|row| row.session.task.clone())
                .collect()
        };
        assert_eq!(tasks(&app), ["wait", "work", "done"]);
        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.selected_row().map(|row| row.session.task.as_str()),
            Some("work")
        );

        // s: следующее поле — агент (claude, claude, codex).
        assert_eq!(press(&mut app, KeyCode::Char('s')), None);
        assert_eq!(app.sort.key, SortKey::Agent);
        assert_eq!(tasks(&app), ["wait", "work", "done"]);
        assert_eq!(
            app.notice.as_ref().map(|notice| notice.text.as_str()),
            Some("сортировка: агент ▲")
        );
        // S: обратный порядок; выбор остался на той же сессии.
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(tasks(&app), ["done", "work", "wait"]);
        assert_eq!(
            app.selected_row().map(|row| row.session.task.as_str()),
            Some("work")
        );
        // Следующее поле сбрасывает направление; русская раскладка работает так же.
        press(&mut app, KeyCode::Char('ы'));
        assert_eq!(
            app.sort,
            Sort {
                key: SortKey::Task,
                reversed: false
            }
        );
        assert_eq!(tasks(&app), ["done", "wait", "work"]);
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('s'));
        }
        assert_eq!(app.sort.key, SortKey::Status);
        // Новые данные приходят уже в выбранном порядке.
        app.sort.key = SortKey::Age;
        app.set_rows(
            vec![
                row("claude", "old", Status::Working, 100),
                row("claude", "new", Status::Done, 990),
            ],
            1000,
        );
        assert_eq!(tasks(&app), ["new", "old"]);
    }

    #[test]
    fn timeline_toggles_and_zooms() {
        let mut app = app();
        press(&mut app, KeyCode::Char('+'));
        assert_eq!(app.span, 10_800, "без ленты масштаб не меняется");
        assert_eq!(press(&mut app, KeyCode::Char('t')), Some(Command::Reload));
        assert!(app.timeline);
        press(&mut app, KeyCode::Char('+'));
        assert_eq!(app.span, 3_600);
        press(&mut app, KeyCode::Char('='));
        press(&mut app, KeyCode::Char('+'));
        assert_eq!(app.span, 1_800, "ближе получаса окна нет");
        for _ in 0..9 {
            press(&mut app, KeyCode::Char('-'));
        }
        assert_eq!(app.span, 86_400, "дальше суток окна нет");
        assert_eq!(press(&mut app, KeyCode::Char('е')), Some(Command::Reload));
        assert!(!app.timeline);
    }

    #[test]
    fn notice_expires() {
        let mut app = app();
        app.notify("принято: x", false);
        app.set_rows(Vec::new(), 1004);
        assert!(app.notice.is_some());
        app.set_rows(Vec::new(), 1005);
        assert!(app.notice.is_none());
    }
}
