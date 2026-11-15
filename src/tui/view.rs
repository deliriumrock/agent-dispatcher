//! Отрисовка панели: верхняя строка со сводкой, список сессий, подробности, нижняя строка.
//!
//! Список рисуется вручную, а не готовой таблицей: так известен прямоугольник каждой
//! строки, и в узкой панели строки можно показывать двухстрочными карточками.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::app::{App, LaneBox, Mode};
use crate::VERSION;
use crate::board::{Row, Sort, SortKey, or_question, status_name};
use crate::cli::local_time;
use crate::config::env_nonempty;
use crate::model::{Status, View};
use crate::text::{age, clip_cells, dur, fit, width, wrap};
use crate::timeline::{self, Phase, Segment, Window};

/// Меньше этого панель не рисуется.
const MIN_WIDTH: u16 = 24;
/// Меньше этого панель не рисуется.
const MIN_HEIGHT: u16 = 4;
/// Сколько строк экрана остаётся списку, когда открыты подробности.
const LIST_MIN_HEIGHT: u16 = 5;
/// Ширина колонки с названиями полей в подробностях.
const DETAIL_KEY: usize = 10;
/// Наименьшая ширина колонки «ЧТО», при которой таблица ещё читается.
const WHAT_MIN: usize = 20;
/// Уже этого лента времени не рисуется.
const LANE_MIN: usize = 6;
/// До этой ширины сжимается колонка «ЗАДАЧА», чтобы в таблице остались «ID» и «ГДЕ».
const TASK_MIN: usize = 12;
/// Ширина поля слева от списка: в нём стоит отметка выбранной строки.
const GUTTER: u16 = 2;
/// С какой ширины в журнале показываются агент и место.
const JOURNAL_WIDE: usize = 72;

/// Рисует панель целиком. `clock` — время для верхней строки.
pub fn draw(frame: &mut Frame, app: &mut App, clock: &str) {
    let area = frame.area();
    app.lanes.clear();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(Line::from("disp: мало места"), area);
        return;
    }
    let header = Rect { height: 1, ..area };
    let footer = Rect {
        y: area.bottom() - 1,
        height: 1,
        ..area
    };
    let body = Rect {
        y: area.y + 1,
        height: area.height - 2,
        ..area
    };
    draw_header(frame, header, app, clock);
    match app.mode {
        Mode::Journal { scroll } => draw_journal(frame, body, app, scroll),
        _ => draw_board(frame, body, app),
    }
    draw_footer(frame, footer, app);
    if app.mode == Mode::Help {
        draw_help(frame, area);
        // Картинки лент легли бы поверх справки.
        app.lanes.clear();
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

/// Фон полос сверху и снизу: тёмно-серый из 256-цветной палитры. Он не зависит от темы
/// терминала, поэтому полосы отличаются от списка и на прозрачном фоне.
const BAR_BG: Color = Color::Indexed(236);
/// Обычный текст на полосе.
const BAR_FG: Color = Color::Indexed(250);
/// Второстепенный текст на полосе.
const BAR_MUTED: Color = Color::Indexed(244);
/// Клавиши в подсказке.
const BAR_KEY: Color = Color::Indexed(231);

/// Обычный текст на полосе.
fn bar() -> Style {
    Style::new().fg(BAR_FG).bg(BAR_BG)
}

/// Второстепенный текст на полосе: разделители, нулевые счётчики, время.
fn bar_muted() -> Style {
    Style::new().fg(BAR_MUTED).bg(BAR_BG)
}

/// Клавиша в подсказке.
fn bar_key() -> Style {
    Style::new()
        .fg(BAR_KEY)
        .bg(BAR_BG)
        .add_modifier(Modifier::BOLD)
}

/// Полоса во всю ширину `area`; возвращает место под текст — с отступом в знакоместо по краям.
fn draw_bar(frame: &mut Frame, area: Rect, style: Style) -> Rect {
    frame.buffer_mut().set_style(area, style);
    Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    }
}

/// Цвет строки по её виду на доске: те же цвета, что у `disp ls`.
fn view_style(view: View) -> Style {
    match view {
        View::Wait => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        View::Err | View::Lost => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        View::Done => Style::new().fg(Color::Green),
        View::Quiet => Style::new().fg(Color::Magenta),
        View::Work => Style::new().fg(Color::Cyan),
        View::Idle => dim(),
    }
}

/// Заливка строки, которая требует человека: ждущая — жёлтая, сбой и обрыв — красная.
/// Такую строку видно при любой сортировке.
fn alarm_style(view: View) -> Option<Style> {
    match view {
        View::Wait => Some(Style::new().fg(Color::Black).bg(Color::Yellow)),
        View::Err | View::Lost => Some(Style::new().fg(Color::White).bg(Color::Red)),
        _ => None,
    }
}

/// Верхняя полоса: сколько сессий в каком состоянии, справа сортировка, время и версия.
/// Счётчик ждущих, когда он не ноль, стоит плашкой цвета тревоги у левого края.
fn draw_header(frame: &mut Frame, area: Rect, app: &App, clock: &str) {
    let inner = draw_bar(frame, area, bar());
    let total = usize::from(inner.width);
    let counts = app.counts;
    let wide = total >= 74;
    let text = |count: usize, long: &str, short: &str| {
        if wide {
            format!("{long}: {count}")
        } else {
            format!("{short} {count}")
        }
    };
    let part = |count: usize, long: &str, short: &str, view: View| {
        let style = if count > 0 {
            view_style(view).bg(BAR_BG)
        } else {
            bar_muted()
        };
        Span::styled(text(count, long, short), style)
    };
    let gap = || Span::styled(if wide { " · " } else { "  " }, bar_muted());
    let waiting = text(counts.attention, "ждут тебя", "!");
    let alarm = alarm_style(View::Wait).filter(|_| counts.attention > 0);
    let mut spans = match alarm {
        // Плашка начинается от самого края полосы; после неё разделитель не нужен.
        Some(alarm) => vec![
            Span::styled(format!(" {waiting} "), alarm.add_modifier(Modifier::BOLD)),
            Span::styled(" ", bar()),
        ],
        None => vec![Span::styled(waiting, bar_muted()), gap()],
    };
    spans.extend([
        part(counts.done, "готово", "✓", View::Done),
        gap(),
        part(counts.working, "работают", "●", View::Work),
        gap(),
        part(counts.quiet, "тихо", "?", View::Quiet),
    ]);
    if app.show_all {
        spans.push(gap());
        spans.push(Span::styled("все", bar_key()));
    }
    let left = Line::from(spans);
    // Плашка занимает и отступ слева, поэтому строка с ней рисуется от края полосы.
    let (spot, used) = match alarm {
        Some(_) => (area, left.width().saturating_sub(1)),
        None => (inner, left.width()),
    };
    frame.render_widget(left, spot);

    // Сортировка не по умолчанию видна всегда: в узкой панели у списка нет заголовка колонок.
    let sort = (app.sort != Sort::default()).then(|| format!("сорт: {}", app.sort.label()));
    let brand = format!("disp {VERSION}");
    let candidates: Vec<Vec<(&str, Style)>> = match &sort {
        Some(sort) => vec![
            vec![(sort, bar()), (clock, bar_muted()), (&brand, bar_muted())],
            vec![(sort, bar()), (clock, bar_muted())],
            vec![(sort, bar())],
            vec![(clock, bar_muted())],
        ],
        None => vec![
            vec![(clock, bar_muted()), (&brand, bar_muted())],
            vec![(clock, bar_muted())],
        ],
    };
    let cells_of = |parts: &[(&str, Style)]| {
        parts.iter().map(|(text, _)| width(text)).sum::<usize>() + 2 * (parts.len() - 1)
    };
    let right = candidates
        .into_iter()
        .find(|parts| used + 2 + cells_of(parts) <= total);
    if let Some(parts) = right {
        let cells = cells_of(&parts) as u16;
        let mut spans = Vec::new();
        for (index, (text, style)) in parts.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  ", bar()));
            }
            spans.push(Span::styled(text.to_string(), style));
        }
        let spot = Rect {
            x: inner.right() - cells,
            width: cells,
            ..inner
        };
        frame.render_widget(Line::from(spans), spot);
    }
}

/// Список сессий и, если открыты, подробности выбранной.
fn draw_board(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.rows.is_empty() {
        app.page = 1;
        let hint = if app.show_all {
            "Сессий нет."
        } else {
            "Незакрытых сессий нет.  Tab покажет и убранные."
        };
        frame.render_widget(Line::styled(hint, dim()), area);
        return;
    }
    let cells = usize::from(area.width);
    let details = match app.selected_row() {
        Some(row) if app.details => detail_lines(row, app.now, cells),
        _ => Vec::new(),
    };
    // Подробностям — сколько нужно, но не в ущерб списку.
    let wanted = details.len() as u16 + 1;
    let detail_height = wanted.min(area.height.saturating_sub(LIST_MIN_HEIGHT));
    let detail_height = if details.is_empty() || detail_height < 3 {
        0
    } else {
        detail_height
    };
    let list = Rect {
        height: area.height - detail_height,
        ..area
    };
    draw_list(frame, list, app);
    if detail_height > 0 {
        let below = Rect {
            y: list.bottom(),
            height: detail_height,
            ..area
        };
        draw_details(frame, below, app, details);
    }
}

/// Какие колонки помещаются в таблицу.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Columns {
    /// Показывать ли короткий id.
    id: bool,
    /// Ширина колонки «ЗАДАЧА».
    task: usize,
    /// Ширина колонки «ПОДЗ»; ноль — колонки нет.
    subtasks: usize,
    /// Ширина колонки «ГДЕ»; ноль — колонки нет.
    place: usize,
    /// Писать ли место коротко, без слова `zellij:`.
    short_place: bool,
    /// Ширина колонки «ЧТО».
    what: usize,
}

impl Columns {
    /// Сколько знакомест занимают все колонки до «ЧТО» вместе с пробелами после них.
    fn fixed(self) -> usize {
        let optional = |cells: usize| if cells > 0 { cells + 1 } else { 0 };
        let id = if self.id { 7 } else { 0 };
        id + 11 + 7 + (self.task + 1) + optional(self.subtasks) + optional(self.place) + 5
    }
}

/// Как показывать список при данной ширине.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    /// Таблица: одна строка на сессию.
    Table(Columns),
    /// Карточки: две строки на сессию, для узкой панели.
    Cards,
}

impl Shape {
    /// Выбирает самый подробный вид, при котором на «ЧТО» остаётся достаточно места.
    /// Колонки не шире своего содержимого, поэтому в них нет пустот.
    fn pick(cells: usize, rows: &[Row]) -> Shape {
        let widest = |text: &dyn Fn(&Row) -> String, min: usize, max: usize| {
            rows.iter()
                .map(|row| width(&text(row)))
                .max()
                .unwrap_or(0)
                .clamp(min, max)
        };
        // Наименьшие ширины — под заголовок колонки со стрелкой сортировки.
        let natural = widest(&|row| row.session.label(), 7, 22);
        let subtasks = widest(&|row| row.subtasks(), 4, 7);
        let place = widest(&|row| row.place().to_string(), 4, 18);
        let short = widest(&|row| short_place(row.place()).to_string(), 4, 12);
        // Ширина задачи при данных остальных колонках: длинное имя сжимается, но не
        // сильнее чем до `TASK_MIN`. `None` — места не хватает и на это.
        let floor = natural.min(TASK_MIN);
        let task_for = |others: usize| {
            let room = cells.checked_sub(others + WHAT_MIN)?;
            (room >= floor).then(|| natural.min(room))
        };
        // Каждая колонка занимает свою ширину и пробел после неё.
        let minimal = 11 + 7 + 1 + 5;
        let full = 7 + minimal + (subtasks + 1) + (place + 1);
        if cells >= full + natural + WHAT_MIN {
            return Shape::Table(Columns {
                id: true,
                task: natural,
                subtasks,
                place,
                short_place: false,
                what: cells - full - natural,
            });
        }
        // Номер панели нужнее, чем хвост длинного имени задачи: сначала сжимается задача,
        // и только потом пропадают «ID» и «ГДЕ».
        let medium = 7 + minimal + (short + 1);
        if let Some(task) = task_for(medium) {
            return Shape::Table(Columns {
                id: true,
                task,
                subtasks: 0,
                place: short,
                short_place: true,
                what: cells - medium - task,
            });
        }
        if let Some(task) = task_for(minimal) {
            return Shape::Table(Columns {
                id: false,
                task,
                subtasks: 0,
                place: 0,
                short_place: true,
                what: cells - minimal - task,
            });
        }
        Shape::Cards
    }

    /// Сколько строк экрана занимает одна сессия.
    fn row_height(self) -> u16 {
        match self {
            Shape::Table(_) => 1,
            Shape::Cards => 2,
        }
    }
}

/// Место без слова `zellij:`: в узкой колонке важны сессия и номер панели.
fn short_place(place: &str) -> &str {
    place.strip_prefix("zellij:").unwrap_or(place)
}

/// Колонка «ЧТО» с пометкой о приёмке. У сессии без названия показывается
/// последний промпт, а если и его нет — начало последнего ответа.
fn what(row: &Row) -> String {
    let s = &row.session;
    let filled = |text: &Option<String>| text.clone().filter(|text| !text.is_empty());
    let mut what = row.what();
    if s.title.is_empty() {
        let fallback = filled(&s.last_prompt)
            .or_else(|| filled(&s.last_reply).map(|reply| format!("ответ: {}", unmarked(&reply))));
        what.push_str(&fallback.unwrap_or_default());
    }
    if s.accepted {
        format!("[убрано] {what}")
    } else {
        what
    }
}

/// Текст без разметки Markdown: в одной строке таблицы звёздочки и решётки только мешают.
fn unmarked(text: &str) -> String {
    let cleaned: String = text.chars().filter(|ch| !matches!(ch, '*' | '`')).collect();
    cleaned.trim_start_matches(['#', ' ']).to_string()
}

/// Строка таблицы или её заголовок: значения колонок в порядке показа.
fn table_line(columns: Columns, values: [&str; 8]) -> String {
    let [id, status, agent, task, subtasks, place, age, what] = values;
    let mut out = String::new();
    let mut put = |text: &str, cells: usize| {
        out.push_str(&fit(text, cells));
        out.push(' ');
    };
    if columns.id {
        put(id, 6);
    }
    put(status, 10);
    put(agent, 6);
    put(task, columns.task);
    if columns.subtasks > 0 {
        put(subtasks, columns.subtasks);
    }
    if columns.place > 0 {
        put(place, columns.place);
    }
    put(age, 4);
    out.push_str(&clip_cells(what, columns.what));
    out
}

/// Строки экрана для одной сессии.
fn row_lines(shape: Shape, row: &Row, now: i64, cells: usize, lane: bool) -> Vec<String> {
    let s = &row.session;
    let age = age(now - s.updated);
    let agent = or_question(&s.agent);
    // Когда показана лента, она занимает место названия.
    let what = if lane { String::new() } else { what(row) };
    match shape {
        Shape::Table(columns) => {
            let place = if columns.short_place {
                short_place(row.place())
            } else {
                row.place()
            };
            vec![table_line(
                columns,
                [
                    &row.sid,
                    row.view.label(),
                    agent,
                    &s.label(),
                    &row.subtasks(),
                    place,
                    &age,
                    &what,
                ],
            )]
        }
        Shape::Cards => {
            let label = s.label();
            let with_place = format!("{agent} {} {age}", short_place(row.place()));
            // Место показываем, только если оно не теснит имя задачи.
            let right = if cells >= 11 + width(&label).min(16) + 1 + width(&with_place) {
                with_place
            } else {
                format!("{agent} {age}")
            };
            let left_cells = cells.saturating_sub(width(&right) + 1);
            let left = format!("{} {label}", fit(row.view.label(), 10));
            vec![
                format!("{} {right}", fit(&left, left_cells)),
                clip_cells(&what, cells),
            ]
        }
    }
}

/// Место ленты времени в строке списка и окно, которое она показывает.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneFrame {
    /// Отступ ленты от левого края списка.
    x: usize,
    /// Ширина ленты в знакоместах.
    cells: usize,
    /// Окно времени.
    window: Window,
    /// Моменты делений шкалы.
    ticks: Vec<i64>,
}

impl LaneFrame {
    /// Лента для списка вида `shape` шириной `cells`; `None`, если лента выключена
    /// или ей не хватает места.
    fn pick(app: &App, shape: Shape, cells: usize) -> Option<LaneFrame> {
        if !app.timeline {
            return None;
        }
        let (x, lane_cells) = match shape {
            Shape::Table(columns) => (columns.fixed(), columns.what),
            Shape::Cards => (0, cells),
        };
        if lane_cells < LANE_MIN {
            return None;
        }
        // Картинка точнее символов: делений в ленте столько, сколько в ней пикселей.
        let slots = lane_cells * app.cell_px.map_or(1, usize::from);
        let window = Window::ending_at(app.now, app.span, slots);
        Some(LaneFrame {
            x,
            cells: lane_cells,
            window,
            ticks: window.ticks(app.utc_offset),
        })
    }

    /// Шкала над лентами: длина окна и подписи делений там, где они помещаются.
    fn axis(&self, utc_offset: i64) -> String {
        let mut cells: Vec<char> = vec![' '; self.cells];
        let mut put = |at: usize, text: &str| {
            for (index, ch) in text.chars().enumerate() {
                cells[at + index] = ch;
            }
        };
        let span = span_label(self.window.span);
        put(0, &span);
        let mut free = span.chars().count() + 1;
        for tick in &self
            .window
            .marks(self.window.label_step(self.cells), utc_offset)
        {
            let local = (tick + utc_offset).rem_euclid(86_400);
            let label = format!("{:02}:{:02}", local / 3_600, local % 3_600 / 60);
            let at = self.window.slot(*tick, self.cells);
            if at >= free as i64 && at as usize + label.len() <= self.cells {
                put(at as usize, &label);
                free = at as usize + label.len() + 1;
            }
        }
        cells.into_iter().collect()
    }
}

/// Длина окна ленты словами: `30м`, `3ч`, `24ч`.
fn span_label(span: i64) -> String {
    if span < 3_600 {
        format!("{}м", span / 60)
    } else {
        format!("{}ч", span / 3_600)
    }
}

/// Цвет состояния на ленте: тот же, что у строки в этом состоянии.
fn phase_style(phase: Phase) -> Style {
    view_style(match phase {
        Phase::Idle => View::Idle,
        Phase::Done => View::Done,
        Phase::Quiet => View::Quiet,
        Phase::Working => View::Work,
        Phase::Error => View::Err,
        Phase::Waiting => View::Wait,
    })
}

/// Рисует ленту символами: знакоместо — деление, в нём самое важное состояние.
fn draw_lane_text(buffer: &mut Buffer, x: u16, y: u16, lane: &LaneFrame, segments: &[Segment]) {
    let phases = timeline::slots(segments, lane.window, lane.cells);
    let ticks: Vec<i64> = lane
        .ticks
        .iter()
        .map(|tick| lane.window.slot(*tick, lane.cells))
        .collect();
    for (index, phase) in phases.iter().enumerate() {
        // Как и на картинке, готовое и простой — тонкой линией, остальное — жирной.
        let (glyph, style) = match phase {
            Some(Phase::Idle | Phase::Done) => ("─", phase_style(phase.unwrap_or(Phase::Idle))),
            Some(phase) => ("━", phase_style(*phase)),
            None if ticks.contains(&(index as i64)) => ("┊", dim()),
            None => continue,
        };
        buffer.set_string(x + index as u16, y, glyph, style);
    }
}

/// Заголовки колонок таблицы; у колонки, по которой отсортирован список, — стрелка.
fn column_titles(sort: Sort, what: &str) -> [String; 8] {
    let arrow = sort.arrow();
    let mark = |key: SortKey, title: &str| {
        if sort.key == key {
            format!("{title}{arrow}")
        } else {
            title.to_string()
        }
    };
    // Колонка возраста шириной в свой заголовок: стрелка встаёт на место последней буквы.
    let age = if sort.key == SortKey::Age {
        format!("ВОЗ{arrow}")
    } else {
        "ВОЗР".to_string()
    };
    [
        "ID".to_string(),
        mark(SortKey::Status, "СТАТУС"),
        mark(SortKey::Agent, "АГЕНТ"),
        mark(SortKey::Task, "ЗАДАЧА"),
        "ПОДЗ".to_string(),
        mark(SortKey::Place, "ГДЕ"),
        age,
        what.to_string(),
    ]
}

/// Список сессий. Следит, чтобы выбранная строка была на экране.
fn draw_list(frame: &mut Frame, full: Rect, app: &mut App) {
    // Слева поле под отметку выбранной строки; сам список — правее.
    let area = Rect {
        x: full.x + GUTTER,
        width: full.width.saturating_sub(GUTTER),
        ..full
    };
    let cells = usize::from(area.width);
    let shape = Shape::pick(cells, &app.rows);
    let lane = LaneFrame::pick(app, shape, cells);
    let mut top = area.y;
    let header = Rect { height: 1, ..area };
    match shape {
        Shape::Table(columns) => {
            let titles = column_titles(app.sort, if lane.is_some() { "" } else { "ЧТО" });
            let titles = titles.each_ref().map(String::as_str);
            frame.render_widget(Line::styled(table_line(columns, titles), bold()), header);
            top += 1;
        }
        // У карточек заголовка нет; строка нужна только под шкалу ленты.
        Shape::Cards if lane.is_some() => top += 1,
        Shape::Cards => {}
    }
    if let Some(lane) = &lane {
        let axis = lane.axis(app.utc_offset);
        frame
            .buffer_mut()
            .set_string(area.x + lane.x as u16, header.y, axis, dim());
    }
    let row_height = shape.row_height();
    let visible = usize::from((area.bottom().saturating_sub(top) / row_height).max(1));
    app.page = visible;
    if app.selected < app.offset {
        app.offset = app.selected;
    }
    if app.selected >= app.offset + visible {
        app.offset = app.selected + 1 - visible;
    }
    app.offset = app.offset.min(app.rows.len().saturating_sub(visible));

    let buffer = frame.buffer_mut();
    let shown = app.rows.iter().enumerate().skip(app.offset).take(visible);
    for (slot, (index, row)) in shown.enumerate() {
        let spot = Rect {
            y: top + slot as u16 * row_height,
            height: row_height,
            ..area
        }
        .intersection(area);
        if spot.height == 0 {
            continue;
        }
        let selected = index == app.selected;
        let head = if row.session.accepted {
            dim()
        } else {
            view_style(row.view)
        };
        // Вторая строка карточки — название: обычным цветом, чтобы читалось.
        let tail = if row.session.accepted || row.view == View::Idle {
            dim()
        } else {
            Style::new()
        };
        // Строка, требующая человека, залита целиком. Заливка не снимается и у выбранной
        // строки, поэтому выбор показывает ещё и отметка в поле слева.
        let alarm = alarm_style(row.view).filter(|_| !row.session.accepted);
        if selected {
            let colour = if row.view == View::Idle {
                Style::new()
            } else {
                head
            };
            buffer.set_string(full.x, spot.y, "▶", colour.add_modifier(Modifier::BOLD));
        }
        let mut lines = row_lines(shape, row, app.now, cells, lane.is_some());
        // Лента занимает последнюю строку: у таблицы место после колонок, у карточки — вторую.
        let lane_line = lines.len().saturating_sub(1);
        if lane.is_some() && shape == Shape::Cards {
            lines[lane_line].clear();
        }
        for (line, text) in lines.iter().enumerate().take(usize::from(spot.height)) {
            let style = match (alarm, selected) {
                (Some(alarm), true) => alarm.add_modifier(Modifier::BOLD),
                (Some(alarm), false) => alarm,
                (None, true) => head.add_modifier(Modifier::REVERSED),
                (None, false) if line == 0 => head,
                (None, false) => tail,
            };
            // Заливка идёт на всю ширину текста; лента не закрашивается.
            let text_cells = match (&lane, shape) {
                (Some(lane), Shape::Table(_)) => lane.x.saturating_sub(1),
                (Some(_), Shape::Cards) if line == lane_line => 0,
                _ => cells,
            };
            let strip = Rect {
                y: spot.y + line as u16,
                height: 1,
                width: text_cells as u16,
                ..spot
            };
            buffer.set_style(strip, style);
            buffer.set_stringn(strip.x, strip.y, text, text_cells, style);
        }
        let lane_y = spot.y + lane_line as u16;
        if let Some(lane) = &lane
            && lane_y < spot.bottom()
        {
            let segments = app.history.lane(row, app.now);
            let x = area.x + lane.x as u16;
            if app.cell_px.is_some() {
                app.lanes.push(LaneBox {
                    x,
                    y: lane_y,
                    cells: lane.cells as u16,
                    segments,
                    window: lane.window,
                    ticks: lane.ticks.clone(),
                });
            } else {
                draw_lane_text(buffer, x, lane_y, lane, &segments);
            }
        }
    }
}

/// Строки подробностей выбранной сессии, уже разбитые по ширине.
fn detail_lines(row: &Row, now: i64, cells: usize) -> Vec<Line<'static>> {
    let s = &row.session;
    let value_cells = cells.saturating_sub(DETAIL_KEY).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut field = |key: &str, value: &str, style: Style| {
        for (index, part) in wrap(value, value_cells).into_iter().enumerate() {
            let key = if index == 0 { key } else { "" };
            lines.push(Line::from(vec![
                Span::styled(fit(key, DETAIL_KEY), dim()),
                Span::styled(part, style),
            ]));
        }
    };
    let plain = Style::new();

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
    field("статус", &status, view_style(row.view));
    field("что", &s.title, plain);
    if let Some(prompt) = s.last_prompt.as_deref().filter(|text| *text != s.title) {
        field("промпт", prompt, plain);
    }
    if let Some(reply) = s.last_reply.as_deref() {
        field("ответ", reply, plain);
    }
    field("где", or_question(&s.place), plain);
    // «Каталог» — тот, по которому названа задача: проект, над которым агент работает,
    // а если отдельного проекта нет — каталог запуска. Остальное показываем, когда отличается.
    let root = s.root.as_deref().unwrap_or(&s.cwd);
    let home = s.project.as_deref().unwrap_or(root);
    field("каталог", &home_relative(home), plain);
    if home != root {
        field("запущен в", &home_relative(root), plain);
    }
    if s.cwd != home && s.cwd != root {
        field("сейчас в", &home_relative(&s.cwd), plain);
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
    field("время", &times.join(" · "), plain);

    let mut extra = Vec::new();
    if s.subagents > 0 {
        extra.push(format!("субагентов в работе: {}", s.subagents));
    }
    if s.bg > 0 {
        extra.push(format!("фоновых задач: {}", s.bg));
    }
    if !extra.is_empty() {
        field("ещё", &extra.join(" · "), plain);
    }
    field("сессия", &s.session_id, dim());
    if !s.subtasks.is_empty() {
        field("подзадачи", &row.subtasks(), plain);
        for task in s.subtasks.values() {
            let (mark, style) = if task.d {
                ("[x]", dim())
            } else {
                ("[ ]", plain)
            };
            field("", &format!("{mark} {}", task.s), style);
        }
    }
    lines
}

/// Путь с `~` вместо домашнего каталога.
fn home_relative(path: &str) -> String {
    match env_nonempty("HOME") {
        Some(home) if path == home => "~".to_string(),
        Some(home) => match path.strip_prefix(&format!("{home}/")) {
            Some(rest) => format!("~/{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Подробности выбранной сессии под списком.
fn draw_details(frame: &mut Frame, area: Rect, app: &App, mut lines: Vec<Line<'static>>) {
    let Some(row) = app.selected_row() else {
        return;
    };
    let cells = usize::from(area.width);
    let title = format!(
        "── {} · {} · {} ",
        row.sid,
        row.session.label(),
        or_question(&row.session.agent)
    );
    let title = clip_cells(&title, cells);
    let rule = "─".repeat(cells.saturating_sub(width(&title)));
    frame.render_widget(
        Line::styled(format!("{title}{rule}"), dim()),
        Rect { height: 1, ..area },
    );
    let room = usize::from(area.height) - 1;
    if lines.len() > room {
        lines.truncate(room.saturating_sub(1));
        lines.push(Line::styled(fit("", DETAIL_KEY) + "…", dim()));
    }
    let below = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    frame.render_widget(Paragraph::new(lines), below);
}

/// Журнал смен статуса: новые записи сверху.
fn draw_journal(frame: &mut Frame, area: Rect, app: &mut App, scroll: usize) {
    let cells = usize::from(area.width);
    let title = format!(
        "Журнал: смены статуса, новые сверху ({})",
        app.journal.len()
    );
    frame.render_widget(
        Line::styled(clip_cells(&title, cells), bold()),
        Rect { height: 1, ..area },
    );
    let room = usize::from(area.height.saturating_sub(1)).max(1);
    app.page = room;
    if app.journal.is_empty() {
        let spot = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        frame.render_widget(Line::styled("Журнал пуст.", dim()), spot.intersection(area));
        return;
    }
    let buffer = frame.buffer_mut();
    let shown = app.journal.iter().rev().skip(scroll).take(room);
    for (slot, event) in shown.enumerate() {
        let style = match event.to.as_str() {
            "waiting" => view_style(View::Wait),
            "error" => view_style(View::Err),
            "done" => view_style(View::Done),
            "working" => view_style(View::Work),
            _ => dim(),
        };
        let when = local_time(event.ts, "%d.%m %H:%M");
        let change = format!("{} → {}", status_name(&event.from), status_name(&event.to));
        let text = if cells >= JOURNAL_WIDE {
            format!(
                "{when}  {} {}  {change}  ({})",
                fit(&event.agent, 6),
                event.task,
                short_place(&event.place),
            )
        } else {
            format!("{when} {}  {change}", event.task)
        };
        let y = area.y + 1 + slot as u16;
        if y < area.bottom() {
            buffer.set_stringn(area.x, y, clip_cells(&text, cells), cells, style);
        }
    }
}

/// Подсказка в нижней полосе: клавиша и что она делает.
type Hint = (&'static str, String);

fn hint(key: &'static str, text: &str) -> Hint {
    (key, text.to_string())
}

/// Ширина подсказок, записанных в строку через два пробела.
fn hints_width(hints: &[Hint]) -> usize {
    let texts: usize = hints
        .iter()
        .map(|(key, text)| width(key) + usize::from(!text.is_empty()) + width(text))
        .sum();
    texts + 2 * hints.len().saturating_sub(1)
}

/// Подсказки одной строкой: клавиши яркие, пояснения обычные.
fn hints_line(hints: &[Hint]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, text)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", bar()));
        }
        spans.push(Span::styled(*key, bar_key()));
        if !text.is_empty() {
            spans.push(Span::styled(format!(" {text}"), bar()));
        }
    }
    Line::from(spans)
}

/// Самый подробный набор подсказок, который помещается в `cells` знакомест.
fn pick_hints(variants: Vec<Vec<Hint>>, cells: usize) -> Vec<Hint> {
    variants
        .into_iter()
        .find(|hints| hints_width(hints) <= cells)
        .unwrap_or_default()
}

/// Нижняя полоса: ввод названия, вопрос, сообщение о действии или подсказка по клавишам.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let confirm = app.mode == Mode::Confirm && app.selected_row().is_some();
    // Вопрос о приёмке работающей сессии — на всю полосу цветом тревоги.
    let base = match alarm_style(View::Wait) {
        Some(alarm) if confirm => alarm.add_modifier(Modifier::BOLD),
        _ => bar(),
    };
    let inner = draw_bar(frame, area, base);
    let cells = usize::from(inner.width);
    match &app.mode {
        Mode::Rename { input } => {
            let prompt = "название: ";
            // Длинный ввод показываем с конца: курсор всегда на экране.
            let room = cells.saturating_sub(width(prompt) + 1);
            let mut tail: Vec<char> = Vec::new();
            let mut used = 0;
            for ch in input.chars().rev() {
                let ch_cells = width(ch.encode_utf8(&mut [0; 4]));
                if used + ch_cells > room {
                    break;
                }
                tail.push(ch);
                used += ch_cells;
            }
            let tail: String = tail.into_iter().rev().collect();
            frame.render_widget(
                Line::from(vec![
                    Span::styled(prompt, bar_key()),
                    Span::styled(tail, bar()),
                ]),
                inner,
            );
            let column = (width(prompt) + used).min(cells.saturating_sub(1)) as u16;
            frame.set_cursor_position((inner.x + column, inner.y));
        }
        Mode::Confirm => {
            let Some(row) = app.selected_row() else {
                return;
            };
            let state = if row.session.status == Status::Waiting {
                "ждёт тебя"
            } else {
                "работает"
            };
            let text = format!(
                "{} ещё {state}. Убрать из списка всё равно? y — да",
                row.sid
            );
            frame.render_widget(Line::styled(clip_cells(&text, cells), base), inner);
        }
        Mode::Journal { .. } => {
            let hints = pick_hints(
                vec![
                    vec![
                        hint("↑↓ PgUp PgDn", "прокрутка"),
                        hint("l", "назад к сессиям"),
                    ],
                    vec![hint("↑↓", ""), hint("l", "назад")],
                ],
                cells,
            );
            frame.render_widget(hints_line(&hints), inner);
        }
        Mode::Board | Mode::Help => {
            if let Some(notice) = &app.notice {
                let colour = if notice.error { View::Err } else { View::Done };
                let style = view_style(colour).bg(BAR_BG);
                frame.render_widget(Line::styled(clip_cells(&notice.text, cells), style), inner);
                return;
            }
            // Справа — где мы в списке, если он не помещается целиком.
            let position = if app.rows.len() > app.page {
                format!(" {}/{}", app.selected + 1, app.rows.len())
            } else {
                String::new()
            };
            let lane = if app.timeline {
                vec![
                    hint("t", &format!("лента {}", span_label(app.span))),
                    hint("+/-", "масштаб"),
                ]
            } else {
                vec![hint("t", "лента")]
            };
            let with_lane = |before: Vec<Hint>, after: Vec<Hint>| -> Vec<Hint> {
                before
                    .into_iter()
                    .chain(lane.iter().cloned())
                    .chain(after)
                    .collect()
            };
            let (up, enter) = (hint("↑↓", "выбор"), hint("Enter", "подробности"));
            let (approve, remove) = (hint("a", "принять"), hint("x", "убрать"));
            let go = hint("g", "перейти");
            let (help, quit) = (hint("?", "справка"), hint("q", "выход"));
            let variants = vec![
                with_lane(
                    vec![
                        up.clone(),
                        enter.clone(),
                        approve.clone(),
                        remove.clone(),
                        go.clone(),
                        hint("n", "назвать"),
                        hint("l", "журнал"),
                        hint("s", "сортировка"),
                    ],
                    vec![hint("Tab", "все"), help.clone(), quit.clone()],
                ),
                with_lane(
                    vec![
                        up,
                        enter.clone(),
                        approve.clone(),
                        remove.clone(),
                        go.clone(),
                        hint("s", "сортировка"),
                    ],
                    vec![help.clone(), quit.clone()],
                ),
                with_lane(
                    vec![
                        enter,
                        approve.clone(),
                        remove.clone(),
                        go.clone(),
                        hint("s", "сорт."),
                    ],
                    vec![help.clone(), quit],
                ),
                with_lane(
                    vec![approve.clone(), remove.clone(), go.clone()],
                    vec![help.clone()],
                ),
                if app.timeline {
                    with_lane(Vec::new(), vec![help.clone()])
                } else {
                    vec![approve, remove, go, help.clone()]
                },
                vec![help],
            ];
            let hints = pick_hints(variants, cells.saturating_sub(width(&position)));
            frame.render_widget(hints_line(&hints), inner);
            if !position.is_empty() {
                let position_cells = width(&position) as u16;
                let spot = Rect {
                    x: inner.right() - position_cells,
                    width: position_cells,
                    ..inner
                };
                frame.render_widget(Line::styled(position, bar_muted()), spot);
            }
        }
    }
}

/// Справка по клавишам поверх панели.
fn draw_help(frame: &mut Frame, area: Rect) {
    let keys = [
        ("↑ ↓  j k", "выбор строки"),
        ("Enter  d", "подробности выбранной сессии"),
        ("a", "принять: ответить агенту «да» (Enter в его панели)"),
        ("x", "убрать из списка: результат посмотрел"),
        ("u", "вернуть убранную в список"),
        ("g", "перейти в панель агента (zellij)"),
        ("n", "дать строке название"),
        ("l", "журнал смен статуса"),
        ("s  S", "сортировка: поле и направление"),
        ("t", "лента времени вместо «ЧТО»"),
        ("+  -", "масштаб ленты: от 30 минут до суток"),
        ("Tab", "показать и убранные"),
        ("r", "перечитать"),
        ("q  Ctrl-C", "выход"),
    ];
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {}", fit(key, 11)), bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " Буквы работают и в русской раскладке.",
        dim(),
    ));

    let box_width = area.width.min(56);
    let box_height = (lines.len() as u16 + 2).min(area.height);
    let spot = Rect {
        x: area.x + (area.width - box_width) / 2,
        y: area.y + (area.height - box_height) / 2,
        width: box_width,
        height: box_height,
    };
    frame.render_widget(Clear, spot);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Клавиши ")),
        spot,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::model::Subtask;
    use crate::store::JournalEvent;
    use crate::tui::app::tests::row;

    /// Рисует панель в память и возвращает строки экрана.
    fn screen(app: &mut App, cols: u16, lines: u16) -> Vec<String> {
        let mut terminal = match Terminal::new(TestBackend::new(cols, lines)) {
            Ok(terminal) => terminal,
            Err(err) => panic!("терминал в памяти не создан: {err}"),
        };
        if let Err(err) = terminal.draw(|frame| draw(frame, app, "18:06:12")) {
            panic!("отрисовка не удалась: {err}");
        }
        let buffer = terminal.backend().buffer();
        (0..lines)
            .map(|y| {
                (0..cols)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn app() -> App {
        let mut waiting = row("claude", "site-31", Status::Waiting, 940);
        waiting.session.detail = "нужно разрешение: Bash".to_string();
        waiting.session.last_prompt = Some("Почини импорт цен из CSV".to_string());
        let mut done = row("codex", "nbx-142", Status::Done, 400);
        done.session.subtasks.insert(
            "1".to_string(),
            Subtask {
                s: "Шаг первый".to_string(),
                d: true,
            },
        );
        let working = row("claude", "trading-system", Status::Working, 997);
        let mut app = App::default();
        app.set_rows(vec![waiting, done, working], 1000);
        app
    }

    #[test]
    fn wide_panel_is_a_full_table() {
        let mut app = app();
        let lines = screen(&mut app, 110, 8);
        assert_eq!(
            lines[0],
            format!(
                " ждут тебя: 1  готово: 1 · работают: 1 · тихо: 0{}18:06:12  disp {VERSION}",
                " ".repeat(110 - 48 - 15 - VERSION.len() - 1)
            )
        );
        // Слева поле с отметкой выбранной строки; у колонки сортировки стрелка.
        assert_eq!(
            lines[1],
            "  ID     СТАТУС▲    АГЕНТ  ЗАДАЧА         ПОДЗ ГДЕ          ВОЗР ЧТО"
        );
        // Порядок по статусу: ждущая, работающая, готовая.
        assert_eq!(
            lines[2],
            "▶ site31 ! ЖДЁТ     claude site-31        —    zellij:dev#3 1м   нужно разрешение: Bash · Задача site-31"
        );
        assert_eq!(
            lines[3],
            "  system ● работает claude trading-system —    zellij:dev#3 3с   Задача trading-system"
        );
        assert_eq!(
            lines[4],
            "  nbx142 ✓ ГОТОВО   codex  nbx-142        1/1  zellij:dev#3 10м  Задача nbx-142"
        );
        assert!(
            lines[7].starts_with(" ↑↓ выбор  Enter подробности  a принять  x убрать  g перейти"),
            "{}",
            lines[7]
        );
    }

    #[test]
    fn medium_panel_drops_columns() {
        let mut app = app();
        let lines = screen(&mut app, 80, 6);
        assert_eq!(
            lines[0],
            format!(
                " ждут тебя: 1  готово: 1 · работают: 1 · тихо: 0{}18:06:12  disp {VERSION}",
                " ".repeat(80 - 48 - 15 - VERSION.len() - 1)
            )
        );
        assert_eq!(
            lines[1],
            "  ID     СТАТУС▲    АГЕНТ  ЗАДАЧА         ГДЕ   ВОЗР ЧТО"
        );
        assert_eq!(
            lines[2],
            "▶ site31 ! ЖДЁТ     claude site-31        dev#3 1м   нужно разрешение: Bash · З…"
        );
    }

    #[test]
    fn narrow_panel_shows_cards() {
        let mut app = app();
        let lines = screen(&mut app, 50, 9);
        assert_eq!(
            lines[0],
            format!(
                " ! 1  ✓ 1  ● 1  ? 0{}18:06:12  disp {VERSION}",
                " ".repeat(50 - 19 - 15 - VERSION.len() - 1)
            )
        );
        // Карточка: слева статус и задача, справа агент, место и возраст; ниже название.
        let card = |mark: &str, left: &str, right: &str| format!("{mark} {left:<32} {right}");
        assert_eq!(lines[1], card("▶", "! ЖДЁТ     site-31", "claude dev#3 1м"));
        assert_eq!(lines[2], "  нужно разрешение: Bash · Задача site-31");
        assert_eq!(
            lines[3],
            card(" ", "● работает trading-system", "claude dev#3 3с")
        );
        assert_eq!(lines[4], "  Задача trading-system");
        assert_eq!(lines[5], card(" ", "✓ ГОТОВО   nbx-142", "codex dev#3 10м"));
        assert_eq!(lines[8], " a принять  x убрать  g перейти  ? справка");
    }

    #[test]
    fn selected_row_is_highlighted_and_kept_on_screen() {
        let mut app = app();
        app.selected = 2;
        let mut terminal =
            Terminal::new(TestBackend::new(50, 6)).unwrap_or_else(|err| panic!("{err}"));
        terminal
            .draw(|frame| draw(frame, &mut app, ""))
            .unwrap_or_else(|err| panic!("{err}"));
        // На четырёх строках помещаются две карточки: список прокручен к выбранной.
        assert_eq!(app.page, 2);
        assert_eq!(app.offset, 1);
        let buffer = terminal.backend().buffer();
        // Обе строки выбранной карточки закрашены цветом статуса до правого края,
        // а в поле слева стоит отметка.
        for (x, y) in [(2, 3), (49, 3), (2, 4), (49, 4)] {
            assert!(buffer[(x, y)].modifier.contains(Modifier::REVERSED));
            assert_eq!(buffer[(x, y)].fg, Color::Green, "ячейка {x},{y}");
        }
        assert_eq!(buffer[(0, 3)].symbol(), "▶");
        assert!(!buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buffer[(0, 1)].symbol(), " ");
        assert!(!buffer[(2, 1)].modifier.contains(Modifier::REVERSED));
        let lines = screen(&mut app, 50, 6);
        assert!(lines[5].ends_with(" 3/3"));
    }

    #[test]
    fn status_and_hint_lines_are_bars() {
        let mut app = app();
        let mut terminal =
            Terminal::new(TestBackend::new(80, 6)).unwrap_or_else(|err| panic!("{err}"));
        terminal
            .draw(|frame| draw(frame, &mut app, "18:06:12"))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        // Обе полосы залиты от края до края; список между ними — нет.
        for x in [0, 40, 79] {
            assert_eq!(buffer[(x, 5)].bg, BAR_BG, "нижняя полоса, колонка {x}");
        }
        assert_eq!(buffer[(40, 0)].bg, BAR_BG);
        assert_eq!(buffer[(79, 0)].bg, BAR_BG);
        assert_eq!(buffer[(40, 4)].bg, Color::Reset);
        // Кто-то ждёт: счётчик стоит жёлтой плашкой от самого края.
        assert_eq!(buffer[(0, 0)].bg, Color::Yellow);
        assert_eq!(buffer[(13, 0)].bg, Color::Yellow);
        assert_eq!(buffer[(14, 0)].bg, BAR_BG);
        // Ненулевой счётчик своего цвета, нулевой и время — приглушённые.
        assert_eq!(buffer[(15, 0)].fg, Color::Green);
        assert_eq!(buffer[(78, 0)].fg, BAR_MUTED);
        // В подсказке клавиша яркая и жирная, пояснение обычное.
        // При 80 колонках первая подсказка — «a принять».
        assert_eq!(buffer[(1, 5)].symbol(), "a");
        assert_eq!(buffer[(1, 5)].fg, BAR_KEY);
        assert!(buffer[(1, 5)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(3, 5)].symbol(), "п");
        assert_eq!(buffer[(3, 5)].fg, BAR_FG);
        assert!(!buffer[(3, 5)].modifier.contains(Modifier::BOLD));

        // Никто не ждёт: плашки нет, счётчик приглушён.
        app.rows[0].view = View::Work;
        let rows = std::mem::take(&mut app.rows);
        app.set_rows(rows, 1000);
        terminal
            .draw(|frame| draw(frame, &mut app, "18:06:12"))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, BAR_BG);
        assert_eq!(buffer[(1, 0)].fg, BAR_MUTED);
        let top: String = (0..80).map(|x| buffer[(x, 0)].symbol()).collect();
        assert!(
            top.starts_with(" ждут тебя: 0 · готово: 1 · работают: 2 · тихо: 0"),
            "{top}"
        );

        // Вопрос о приёмке работающей сессии занимает всю нижнюю полосу цветом тревоги.
        app.mode = Mode::Confirm;
        terminal
            .draw(|frame| draw(frame, &mut app, "18:06:12"))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 5)].bg, Color::Yellow);
        assert_eq!(buffer[(79, 5)].bg, Color::Yellow);
    }

    #[test]
    fn rows_that_need_the_human_are_filled() {
        let mut app = app();
        app.rows[1].view = View::Err;
        app.selected = 2;
        let mut terminal =
            Terminal::new(TestBackend::new(80, 6)).unwrap_or_else(|err| panic!("{err}"));
        terminal
            .draw(|frame| draw(frame, &mut app, ""))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        // Ждущая строка залита жёлтым, сбойная — красным, от поля слева до правого края.
        for x in [2, 40, 79] {
            assert_eq!(buffer[(x, 2)].bg, Color::Yellow, "ждущая, колонка {x}");
            assert_eq!(buffer[(x, 2)].fg, Color::Black);
            assert_eq!(buffer[(x, 3)].bg, Color::Red, "сбойная, колонка {x}");
        }
        assert_eq!(buffer[(0, 2)].bg, Color::Reset, "поле слева не заливается");
        // Обычная строка без заливки: выбранная показана обращением цветов.
        assert_eq!(buffer[(2, 4)].bg, Color::Reset);
        assert!(buffer[(2, 4)].modifier.contains(Modifier::REVERSED));

        // Выбранная ждущая строка остаётся залитой: выбор показывает отметка слева.
        app.selected = 0;
        terminal
            .draw(|frame| draw(frame, &mut app, ""))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 2)].symbol(), "▶");
        assert_eq!(buffer[(2, 2)].bg, Color::Yellow);
        assert!(!buffer[(2, 2)].modifier.contains(Modifier::REVERSED));
        assert!(buffer[(2, 2)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn header_shows_the_sort_column() {
        let mut app = app();
        app.sort = Sort {
            key: SortKey::Age,
            reversed: true,
        };
        let rows = std::mem::take(&mut app.rows);
        app.set_rows(rows, 1000);
        let lines = screen(&mut app, 110, 8);
        assert!(
            lines[0].ends_with(&format!("сорт: возраст ▼  18:06:12  disp {VERSION}")),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "  ID     СТАТУС     АГЕНТ  ЗАДАЧА         ПОДЗ ГДЕ          ВОЗ▼ ЧТО"
        );
        // Обратный порядок по возрасту: самое давнее событие сверху.
        // Выбор остался на той же сессии, хотя она переехала на вторую строку.
        assert!(lines[2].starts_with("  nbx142"), "{}", lines[2]);
        assert!(lines[3].starts_with("▶ site31"), "{}", lines[3]);
        assert!(lines[4].starts_with("  system"), "{}", lines[4]);
        app.sort = Sort {
            key: SortKey::Task,
            reversed: false,
        };
        let lines = screen(&mut app, 110, 8);
        assert!(lines[1].starts_with("  ID     СТАТУС     АГЕНТ  ЗАДАЧА▲ "));
        // В карточках заголовка колонок нет: сортировка видна в верхней строке.
        let lines = screen(&mut app, 50, 9);
        assert!(lines[0].contains("сорт: задача ▲"), "{}", lines[0]);
    }

    #[test]
    fn details_show_the_selected_session() {
        let mut app = app();
        app.details = true;
        let lines = screen(&mut app, 60, 16);
        let at = lines
            .iter()
            .position(|line| line.starts_with("── site31 · site-31 · claude ─"))
            .unwrap_or_else(|| panic!("нет заголовка подробностей: {lines:#?}"));
        assert_eq!(lines[at + 1], "статус    ! ЖДЁТ · нужно разрешение: Bash");
        assert_eq!(lines[at + 2], "что       Задача site-31");
        assert_eq!(lines[at + 3], "промпт    Почини импорт цен из CSV");
        assert_eq!(lines[at + 4], "где       zellij:dev#3");
        assert!(lines.iter().any(|line| line == "сессия    0000-site-31"));
    }

    #[test]
    fn details_never_squeeze_the_list_out() {
        let mut app = app();
        app.details = true;
        let lines = screen(&mut app, 60, 8);
        // Шесть строк между шапкой и подсказкой: пять остаются списку, подробностям не хватает.
        assert!(lines.iter().all(|line| !line.starts_with("──")));
        assert!(lines[2].starts_with("▶ ! ЖДЁТ"), "{lines:#?}");
    }

    #[test]
    fn empty_board_and_tiny_window() {
        let mut app = App::default();
        let lines = screen(&mut app, 60, 5);
        assert_eq!(lines[1], "Незакрытых сессий нет.  Tab покажет и убранные.");
        let lines = screen(&mut app, 20, 3);
        assert_eq!(lines[0], "disp: мало места");
    }

    #[test]
    fn footer_shows_prompt_question_and_notice() {
        let mut app = app();
        app.mode = Mode::Rename {
            input: "Релиз 1.4".to_string(),
        };
        assert_eq!(screen(&mut app, 60, 6)[5], " название: Релиз 1.4");
        app.mode = Mode::Confirm;
        assert_eq!(
            screen(&mut app, 60, 6)[5],
            " site31 ещё ждёт тебя. Убрать из списка всё равно? y — да"
        );
        app.mode = Mode::Board;
        app.notify("принято: nbx-142 [codex] Задача nbx-142", false);
        assert_eq!(
            screen(&mut app, 60, 6)[5],
            " принято: nbx-142 [codex] Задача nbx-142"
        );
    }

    #[test]
    fn journal_lists_newest_first() {
        let mut app = app();
        let event = |ts: i64, to: &str| JournalEvent {
            ts,
            agent: "codex".to_string(),
            task: "nbx-142".to_string(),
            place: "zellij:dev#3".to_string(),
            from: "working".to_string(),
            to: to.to_string(),
            ..JournalEvent::default()
        };
        app.journal = vec![event(1_000_000, "waiting"), event(1_000_600, "done")];
        app.mode = Mode::Journal { scroll: 0 };
        let lines = screen(&mut app, 80, 6);
        assert_eq!(lines[1], "Журнал: смены статуса, новые сверху (2)");
        assert!(
            lines[2].ends_with("codex  nbx-142  работает → ГОТОВО  (dev#3)"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].ends_with("codex  nbx-142  работает → ЖДЁТ  (dev#3)"),
            "{}",
            lines[3]
        );
        app.mode = Mode::Journal { scroll: 1 };
        let lines = screen(&mut app, 80, 6);
        assert!(lines[2].ends_with("работает → ЖДЁТ  (dev#3)"));
        // В узкой панели остаются время, задача и сама смена статуса.
        let lines = screen(&mut app, 50, 6);
        assert!(
            lines[2].ends_with(" nbx-142  работает → ЖДЁТ"),
            "{}",
            lines[2]
        );
    }

    #[test]
    fn help_is_drawn_over_the_board() {
        let mut app = app();
        app.mode = Mode::Help;
        let lines = screen(&mut app, 70, 18);
        assert!(lines.iter().any(|line| line.contains("┌ Клавиши ")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("убрать из списка: результат посмотрел"))
        );
    }

    #[test]
    fn untitled_session_shows_its_prompt_or_reply() {
        let mut row = row("claude", "site-31", Status::Done, 900);
        row.session.title.clear();
        row.session.last_reply = Some("## **Готово:** цены `обновлены`".to_string());
        assert_eq!(what(&row), "ответ: Готово: цены обновлены");
        row.session.last_prompt = Some("Поправь цены".to_string());
        assert_eq!(what(&row), "Поправь цены");
        row.session.detail = "ход прерван".to_string();
        row.session.accepted = true;
        assert_eq!(what(&row), "[убрано] ход прерван · Поправь цены");
    }

    /// Панель с включённой лентой: получасовое окно и журнал трёх сессий.
    fn timeline_app() -> App {
        let mut app = app();
        let event = |ts: i64, agent: &str, task: &str, to: &str| JournalEvent {
            ts,
            agent: agent.to_string(),
            sid: format!("0000-{task}"),
            to: to.to_string(),
            ..JournalEvent::default()
        };
        app.history = timeline::History::from_events(&[
            event(100, "claude", "site-31", "working"),
            event(100, "codex", "nbx-142", "working"),
            event(400, "codex", "nbx-142", "done"),
            event(700, "claude", "trading-system", "working"),
            event(940, "claude", "site-31", "waiting"),
        ]);
        app.timeline = true;
        app.span = 1_800;
        app
    }

    #[test]
    fn timeline_replaces_the_title_column() {
        let mut app = timeline_app();
        let mut terminal =
            Terminal::new(TestBackend::new(80, 6)).unwrap_or_else(|err| panic!("{err}"));
        terminal
            .draw(|frame| draw(frame, &mut app, ""))
            .unwrap_or_else(|err| panic!("{err}"));
        let buffer = terminal.backend().buffer();
        let line = |y: u16| -> String {
            (0..80)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        // Окно 30 минут на 27 знакомест: деления каждые пять минут, подписи каждые десять.
        assert_eq!(
            line(1),
            "  ID     СТАТУС▲    АГЕНТ  ЗАДАЧА         ГДЕ   ВОЗР 30м        00:00    00:10"
        );
        assert_eq!(
            line(2),
            format!(
                "▶ site31 ! ЖДЁТ     claude site-31        dev#3 1м     ┊   ┊    ┊{}",
                "━".repeat(15)
            )
        );
        assert_eq!(
            line(3),
            format!(
                "  system ● работает claude trading-system dev#3 3с     ┊   ┊    ┊   ┊    ┊{}",
                "━".repeat(6)
            )
        );
        assert_eq!(
            line(4),
            format!(
                "  nbx142 ✓ ГОТОВО   codex  nbx-142        dev#3 10м    ┊   ┊    ┊{}{}",
                "━".repeat(6),
                "─".repeat(9)
            )
        );
        // Лента начинается в колонке 53; работа голубая, ожидание жёлтое, готовое зелёное.
        assert_eq!(buffer[(53 + 12, 2)].fg, Color::Cyan);
        assert_eq!(buffer[(53 + 24, 2)].fg, Color::Cyan);
        assert_eq!(buffer[(53 + 25, 2)].fg, Color::Yellow);
        assert_eq!(buffer[(53 + 17, 4)].fg, Color::Cyan);
        assert_eq!(buffer[(53 + 18, 4)].fg, Color::Green);
        // Ждущая строка залита до ленты, сама лента — нет.
        assert_eq!(buffer[(2, 2)].bg, Color::Yellow);
        assert_eq!(buffer[(51, 2)].bg, Color::Yellow);
        assert_eq!(buffer[(53 + 12, 2)].bg, Color::Reset);
        assert!(app.lanes.is_empty(), "символьной ленте картинки не нужны");
        let hint = line(5);
        assert!(hint.contains("t лента 30м  +/- масштаб"), "{hint}");
    }

    #[test]
    fn timeline_in_cards_takes_the_second_line() {
        let mut app = timeline_app();
        let lines = screen(&mut app, 50, 9);
        assert!(lines[1].starts_with("  30м "), "{}", lines[1]);
        let card = |mark: &str, left: &str, right: &str| format!("{mark} {left:<32} {right}");
        assert_eq!(lines[2], card("▶", "! ЖДЁТ     site-31", "claude dev#3 1м"));
        assert!(lines[3].ends_with(&"━".repeat(20)), "{}", lines[3]);
        assert!(!lines[3].contains("Задача"));
        assert_eq!(
            lines[4],
            card(" ", "● работает trading-system", "claude dev#3 3с")
        );
    }

    #[test]
    fn pixel_timeline_leaves_room_for_images() {
        let mut app = timeline_app();
        app.cell_px = Some(10);
        let lines = screen(&mut app, 80, 6);
        assert_eq!(
            lines[2],
            "▶ site31 ! ЖДЁТ     claude site-31        dev#3 1м"
        );
        assert_eq!(app.lanes.len(), 3);
        let first = &app.lanes[0];
        assert_eq!((first.x, first.y, first.cells), (53, 2, 27));
        assert_eq!(app.lanes[2].y, 4);
        // Окно выровнено по пикселям: 30 минут на 270 пикселей — шаг шесть секунд.
        assert_eq!(first.window.end(), 1_002);
        assert_eq!(first.segments.len(), 2);
        // Поверх справки и в журнале картинок нет.
        app.mode = Mode::Help;
        screen(&mut app, 80, 20);
        assert!(app.lanes.is_empty());
        app.mode = Mode::Journal { scroll: 0 };
        screen(&mut app, 80, 20);
        assert!(app.lanes.is_empty());
    }

    #[test]
    fn shape_follows_the_width() {
        let rows = app().rows;
        assert!(matches!(
            Shape::pick(110, &rows),
            Shape::Table(Columns {
                id: true,
                subtasks: 4,
                short_place: false,
                ..
            })
        ));
        assert!(matches!(
            Shape::pick(80, &rows),
            Shape::Table(Columns {
                id: true,
                subtasks: 0,
                place: 5,
                ..
            })
        ));
        assert!(matches!(
            Shape::pick(60, &rows),
            Shape::Table(Columns {
                id: false,
                place: 0,
                ..
            })
        ));
        assert_eq!(Shape::pick(50, &rows), Shape::Cards);
    }

    #[test]
    fn long_task_name_shrinks_before_columns_disappear() {
        let mut rows = app().rows;
        rows[0].session.branch = "perf/hourly-recover".to_string();
        assert_eq!(rows[0].session.label().chars().count(), 27);
        // 78 знакомест: имя задачи сжато с 22 до 21, «ID» и «ГДЕ» на месте.
        assert!(matches!(
            Shape::pick(78, &rows),
            Shape::Table(Columns {
                id: true,
                task: 21,
                place: 5,
                ..
            })
        ));
        // При 69 задача сжата до наименьшей ширины. При 68 места уже нет: уходят «ID»
        // и «ГДЕ», зато задача снова помещается целиком.
        assert!(matches!(
            Shape::pick(69, &rows),
            Shape::Table(Columns {
                id: true,
                task: 12,
                ..
            })
        ));
        assert!(matches!(
            Shape::pick(68, &rows),
            Shape::Table(Columns {
                id: false,
                task: 22,
                ..
            })
        ));
    }
}
