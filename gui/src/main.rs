//! Графическая оболочка диспетчера сессий AI-агентов.
//!
//! Обычное окно Wayland: его можно открыть на рабочем столе или показать в панели
//! терминала через term-everything. Данные и действия — те же, что у `disp watch`:
//! оболочка читает файлы сессий и пишет в них через ядро диспетчера.

mod state;

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::Key;
use slint::{ComponentHandle, PhysicalSize, SharedString, Timer, TimerMode};

use crate::state::{Outcome, State};
use disp::{t, tr};

slint::include_modules!();

/// Как часто оболочка проверяет файлы сессий.
const POLL: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        let version = disp::VERSION;
        println!(
            "{}",
            tr!(
                "disp-gui {version} — graphical shell of the session dispatcher\n\n  \
                 disp-gui               open the window\n  \
                 disp-gui --size WxH    a window of the given size in pixels\n",
                "disp-gui {version} — графическая оболочка диспетчера сессий\n\n  \
                 disp-gui               открыть окно\n  \
                 disp-gui --size ШxВ    окно заданного размера в пикселях\n",
                "disp-gui {version} — графічна оболонка диспетчера сесій\n\n  \
                 disp-gui               відкрити вікно\n  \
                 disp-gui --size ШxВ    вікно заданого розміру в пікселях\n"
            )
        );
        return ExitCode::SUCCESS;
    }
    match run(window_size(&args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("disp-gui: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Размер окна из аргумента `--size ШxВ`.
fn window_size(args: &[String]) -> Option<PhysicalSize> {
    let value = args
        .iter()
        .position(|arg| arg == "--size")
        .and_then(|at| args.get(at + 1))?;
    let (width, height) = value.split_once(['x', 'X', 'х'])?;
    Some(PhysicalSize::new(
        width.trim().parse().ok()?,
        height.trim().parse().ok()?,
    ))
}

/// Ставит надписи окна на языке интерфейса.
fn set_texts(texts: &Texts) {
    texts.set_waiting(t!("waiting", "ждут тебя", "чекають на тебе").into());
    texts.set_working(t!("working", "работают", "працюють").into());
    texts.set_quiet(t!("quiet", "молчат", "мовчать").into());
    texts.set_done(t!("done", "готово", "готово").into());
    texts.set_all(t!("all", "все", "усі").into());
    texts.set_sort(t!("sort", "сорт", "сорт.").into());
    texts.set_no_sessions(t!("No sessions.", "Сессий нет.", "Сесій немає.").into());
    texts.set_no_open_sessions(
        t!(
            "No open sessions.  Tab shows removed ones too.",
            "Незакрытых сессий нет.  Tab покажет и убранные.",
            "Незакритих сесій немає.  Tab покаже й прибрані."
        )
        .into(),
    );
    texts.set_approve(t!("Approve", "Принять", "Прийняти").into());
    texts.set_remove(t!("Remove", "Убрать", "Прибрати").into());
    texts.set_focus(t!("Go to", "Перейти", "Перейти").into());
    texts.set_rename(t!("Name", "Назвать", "Назвати").into());
    texts.set_name_prompt(t!("name:", "название:", "назва:").into());
    texts.set_name_placeholder(
        t!(
            "Enter — save, empty line — cancel",
            "Enter — сохранить, пустая строка — отмена",
            "Enter — зберегти, порожній рядок — скасувати"
        )
        .into(),
    );
}

/// Создаёт окно, связывает его с состоянием и крутит цикл событий.
fn run(size: Option<PhysicalSize>) -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    if let Some(size) = size {
        app.window().set_size(size);
    }
    app.set_version(disp::VERSION.into());
    set_texts(&app.global::<Texts>());

    let state = Rc::new(RefCell::new(State::new()));
    state.borrow_mut().reload();
    publish(&app, &state.borrow());

    app.on_key({
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        move |text, ctrl| {
            let Some(app) = weak.upgrade() else {
                return false;
            };
            let Some(key) = Pressed::from_text(&text, ctrl) else {
                return false;
            };
            let outcome = state.borrow_mut().key(key);
            apply(&app, &state.borrow(), outcome);
            true
        }
    });
    app.on_select({
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        move |index| {
            let Some(app) = weak.upgrade() else { return };
            state
                .borrow_mut()
                .select(usize::try_from(index).unwrap_or(0));
            publish(&app, &state.borrow());
        }
    });
    app.on_activate({
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        move |index| {
            let Some(app) = weak.upgrade() else { return };
            let outcome = {
                let mut state = state.borrow_mut();
                state.select(usize::try_from(index).unwrap_or(0));
                state.command("focus")
            };
            apply(&app, &state.borrow(), outcome);
        }
    });
    app.on_command({
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        move |name| {
            let Some(app) = weak.upgrade() else { return };
            let outcome = state.borrow_mut().command(name.as_str());
            apply(&app, &state.borrow(), outcome);
        }
    });
    app.on_rename({
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        move |title| {
            let Some(app) = weak.upgrade() else { return };
            state.borrow_mut().rename(title.as_str());
            publish(&app, &state.borrow());
            app.invoke_focus_list();
        }
    });

    // Файлы сессий меняют хуки агентов: перечитываем, когда изменился каталог, и раз
    // в секунду — чтобы шли возрасты и лента времени.
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, POLL, {
        let weak = app.as_weak();
        let state = Rc::clone(&state);
        let mut width = 0.0;
        move || {
            let Some(app) = weak.upgrade() else { return };
            // Ширина окна определяет, сколько места отдать лентам и подсказкам.
            let now = logical_width(&app);
            let resized = now != width;
            width = now;
            if state.borrow_mut().refresh() || resized {
                publish(&app, &state.borrow());
            }
        }
    });

    app.run()
}

/// Клавиша, как её понимает оболочка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressed {
    /// Стрелка вверх.
    Up,
    /// Стрелка вниз.
    Down,
    /// Страница вверх.
    PageUp,
    /// Страница вниз.
    PageDown,
    /// В начало списка.
    Home,
    /// В конец списка.
    End,
    /// Enter.
    Enter,
    /// Tab.
    Tab,
    /// Буква или знак, как набрано; к латинской раскладке её приводит состояние.
    Char(char),
}

impl Pressed {
    /// Разбирает текст клавиши из события Slint. `None` — клавиша оболочке не нужна.
    fn from_text(text: &SharedString, ctrl: bool) -> Option<Pressed> {
        let ch = text.chars().next()?;
        let special = |key: Key| SharedString::from(key).starts_with(ch);
        if ctrl {
            // Ctrl+C закрывает окно, как и в текстовой панели.
            return matches!(disp::text::latin_key(ch), 'c' | 'с').then_some(Pressed::Char('q'));
        }
        Some(if special(Key::UpArrow) {
            Pressed::Up
        } else if special(Key::DownArrow) {
            Pressed::Down
        } else if special(Key::PageUp) {
            Pressed::PageUp
        } else if special(Key::PageDown) {
            Pressed::PageDown
        } else if special(Key::Home) {
            Pressed::Home
        } else if special(Key::End) {
            Pressed::End
        } else if special(Key::Return) {
            Pressed::Enter
        } else if special(Key::Tab) {
            Pressed::Tab
        } else if ch.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&ch) {
            // Прочие служебные клавиши (Shift, F-клавиши и т. п.) оболочке не нужны.
            return None;
        } else {
            Pressed::Char(ch)
        })
    }
}

/// Показывает состояние в окне и выполняет то, что нужно сделать с самим окном.
fn apply(app: &AppWindow, state: &State, outcome: Outcome) {
    publish(app, state);
    match outcome {
        Outcome::None => {}
        Outcome::Reveal => app.invoke_reveal(),
        Outcome::StartRename => app.invoke_start_rename(),
        Outcome::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}

/// Ширина окна в логических пикселях.
fn logical_width(app: &AppWindow) -> f32 {
    app.window().size().width as f32 / app.window().scale_factor()
}

/// Переносит состояние в свойства окна.
fn publish(app: &AppWindow, state: &State) {
    let view = state.view(logical_width(app));
    app.set_lane_width(view.lane_width);
    app.set_rows(view.rows);
    app.set_ticks(view.ticks);
    app.set_details(view.details);
    app.set_hints(view.hints);
    app.set_waiting(view.counts[0]);
    app.set_unaccepted(view.counts[1]);
    app.set_working(view.counts[2]);
    app.set_silent(view.counts[3]);
    app.set_clock(view.clock);
    app.set_sort(view.sort);
    app.set_span(view.span);
    app.set_show_all(view.show_all);
    app.set_details_open(view.details_open);
    app.set_selected(view.selected);
    app.set_selected_kind(view.selected_kind);
    app.set_selected_title(view.selected_title);
    app.set_notice(view.notice);
    app.set_notice_error(view.notice_error);
    app.set_question(view.question);
    app.set_renaming(view.renaming);
    let fit = header_fit(
        logical_width(app),
        &Header {
            title: app.get_title_width(),
            badges: app.get_badges_width(),
            compact: app.get_compact_width(),
            right: [
                app.get_sort_width(),
                app.get_clock_width(),
                app.get_version_width(),
            ],
        },
    );
    app.set_compact_badges(fit.compact);
    app.set_show_sort(fit.shown >= 1);
    app.set_show_clock(fit.shown >= 2);
    app.set_show_version(fit.shown >= 3);
}

/// Ширины частей верхней полосы окна, логические пиксели.
struct Header {
    /// Надпись «disp» слева.
    title: f32,
    /// Четыре счётчика с подписями.
    badges: f32,
    /// Те же счётчики со значками вместо подписей.
    compact: f32,
    /// Справа по порядку: сортировка, время, версия.
    right: [f32; 3],
}

/// Что помещается в верхнюю полосу.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fit {
    /// Счётчики со значками вместо подписей.
    compact: bool,
    /// Сколько надписей справа показать: сортировка, время, версия — в этом порядке.
    shown: usize,
}

/// Поля и промежутки полосы: как в разметке (`padding` 12 + 12, `spacing` 8).
const HEADER_PADDING: f32 = 24.0;
const HEADER_SPACING: f32 = 8.0;

/// Решает, что поместится в полосу окна шириной `width`. Счётчики важнее всего:
/// сначала справа налево уходят версия, время и сортировка, а если тесно и без них,
/// подписи счётчиков сжимаются до значков.
fn header_fit(width: f32, header: &Header) -> Fit {
    // Части полосы: «disp», счётчики, растяжка и показанные надписи справа.
    let need = |badges: f32, shown: usize| {
        let right: f32 = header.right[..shown].iter().sum();
        HEADER_PADDING + header.title + badges + right + HEADER_SPACING * (2 + shown) as f32
    };
    for compact in [false, true] {
        let badges = if compact { header.compact } else { header.badges };
        if let Some(shown) = (0..=3).rev().find(|&shown| need(badges, shown) <= width) {
            return Fit { compact, shown };
        }
    }
    Fit {
        compact: true,
        shown: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Полоса, где «disp» — 35, счётчики — 300 (со значками — 150), справа — 90, 50, 80.
    fn header() -> Header {
        Header {
            title: 35.0,
            badges: 300.0,
            compact: 150.0,
            right: [90.0, 50.0, 80.0],
        }
    }

    #[test]
    fn counters_outlive_everything_else_in_the_header() {
        let fit = |width| header_fit(width, &header());
        // Всё: 24 + 35 + 300 + 220 + 8 × 5 = 619.
        assert_eq!(fit(619.0), Fit { compact: false, shown: 3 });
        // Уходят версия, время, сортировка.
        assert_eq!(fit(618.0), Fit { compact: false, shown: 2 });
        assert_eq!(fit(500.0), Fit { compact: false, shown: 1 });
        assert_eq!(fit(400.0), Fit { compact: false, shown: 0 });
        // Подписи не помещаются и без надписей справа (24 + 35 + 300 + 16 = 375): значки,
        // и рядом снова есть место для сортировки (24 + 35 + 150 + 90 + 24 = 323).
        assert_eq!(fit(374.0), Fit { compact: true, shown: 1 });
        assert_eq!(fit(240.0), Fit { compact: true, shown: 0 });
        assert_eq!(fit(100.0), Fit { compact: true, shown: 0 });
    }
}
