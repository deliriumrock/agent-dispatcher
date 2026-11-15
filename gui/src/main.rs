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

slint::include_modules!();

/// Как часто оболочка проверяет файлы сессий.
const POLL: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!(
            "disp-gui {} — графическая оболочка диспетчера сессий\n\n  \
             disp-gui               открыть окно\n  \
             disp-gui --size ШxВ    окно заданного размера в пикселях\n",
            env!("CARGO_PKG_VERSION")
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

/// Создаёт окно, связывает его с состоянием и крутит цикл событий.
fn run(size: Option<PhysicalSize>) -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    if let Some(size) = size {
        app.window().set_size(size);
    }
    app.set_version(disp::VERSION.into());

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
}
