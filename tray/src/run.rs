//! Жизнь значка: показывает его, раз в секунду смотрит на каталог сессий, обновляет
//! значок и меню, выполняет нажатия.
//!
//! Цикл событий у систем свой. На Linux значок обслуживает поток D-Bus (`ksni`), и
//! главному потоку достаточно ждать сигналов из канала. На macOS меню живёт только при
//! цикле событий AppKit в главном потоке; его ведёт `tao`.

use std::sync::Arc;
use std::time::Duration;

use disp::board::Board;
use disp::config;
use disp::notify;
use disp::observe;
use disp::store::{self, Signature, Store};
use tray_icon::menu::MenuEvent;
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::actions;
use crate::icon;
use crate::menu;
use crate::model::{Action, Tray};
use crate::startup::Outcome;

/// Как часто смотреть, не изменился ли каталог сессий.
const POLL: Duration = Duration::from_secs(1);
/// Как часто пересчитывать значок, даже если файлы не менялись: сессия становится
/// молчащей со временем, а проверка процессов видит смену процессов.
const RECHECK_SEC: i64 = 30;

/// Сигнал от трея главному потоку.
#[derive(Clone, Debug)]
pub enum Signal {
    /// Нажат пункт меню с этим идентификатором.
    Menu(String),
    /// Значок активирован: двойной щелчок в GNOME, щелчок в KDE.
    Activate,
}

/// Продолжать ли работу после сигнала.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Работать дальше.
    Go,
    /// Убрать значок и выйти.
    Quit,
}

/// Значок и то, что он сейчас показывает.
pub struct App {
    store: Store,
    tray: Tray,
    seen: Option<(Signature, i64)>,
    icon: Option<TrayIcon>,
}

impl App {
    /// Значок по файлам диспетчера; на экране он появится после `show`.
    pub fn new(store: Store) -> Self {
        // Агенты в контейнерах: фоновый поток записывает перемены у них в файлы сессий,
        // а значок видит их, как события хуков, не дожидаясь `docker exec`.
        observe::watch(store.clone());
        App {
            store,
            tray: Tray::default(),
            seen: None,
            icon: None,
        }
    }

    /// Показывает значок. На macOS вызывается только из запущенного цикла событий.
    pub fn show(&mut self) -> Result<(), String> {
        let tray = self.read();
        let builder = TrayIconBuilder::new().with_id("disp");
        // На Linux заголовок — имя значка для оболочки и подсказки. На macOS он стал бы
        // надписью рядом со значком в строке меню.
        let builder = if cfg!(target_os = "macos") {
            builder
        } else {
            builder.with_title("disp")
        };
        let icon = builder
            .with_icon(picture(&tray)?)
            .with_tooltip(&tray.summary)
            .with_menu(Box::new(menu::build(&tray)?))
            .build()
            .map_err(|err| format!("значок не создан: {err}"))?;
        self.icon = Some(icon);
        self.tray = tray;
        Ok(())
    }

    /// Перечитывает сессии, если изменился их каталог или пришло время, и обновляет
    /// значок, если изменилось то, что он показывает.
    pub fn refresh(&mut self) {
        if self.seen.as_ref() == Some(&self.stamp()) {
            return;
        }
        let next = self.read();
        if next == self.tray {
            return;
        }
        if let Some(icon) = self.icon.as_mut() {
            if (next.level, next.count) != (self.tray.level, self.tray.count) {
                match picture(&next) {
                    Ok(picture) => {
                        if let Err(err) = icon.set_icon(Some(picture)) {
                            warn(&format!("значок не обновлён: {err}"));
                        }
                    }
                    Err(err) => warn(&err),
                }
            }
            if let Err(err) = icon.set_tooltip(Some(&next.summary)) {
                warn(&format!("подсказка не обновлена: {err}"));
            }
            match menu::build(&next) {
                Ok(menu) => icon.set_menu(Some(Box::new(menu))),
                Err(err) => warn(&err),
            }
        }
        self.tray = next;
    }

    /// Выполняет нажатие. Неудачу показывает уведомлением: окна, где её написать, нет.
    pub fn handle(&mut self, signal: Signal) -> Flow {
        let action = match signal {
            Signal::Menu(id) => Action::parse(&id),
            Signal::Activate => Some(Action::Open),
        };
        let Some(action) = action else {
            return Flow::Go;
        };
        let (outcome, about) = match (&action, action.key()) {
            (Action::Quit, _) => return Flow::Quit,
            (Action::Open, _) => (actions::open_panel(), String::new()),
            (_, Some(key)) => match self.tray.entry(key) {
                Some(entry) => (actions::on_session(&action, &entry.path), entry.text.clone()),
                None => (Err("этой сессии уже нет в списке".to_string()), String::new()),
            },
            (_, None) => return Flow::Go,
        };
        if let Err(text) = outcome {
            warn(&text);
            notify::desktop(&format!("disp: {text}"), &about);
        }
        self.seen = None;
        self.refresh();
        Flow::Go
    }

    /// Свежая модель значка по файлам.
    fn read(&mut self) -> Tray {
        self.seen = Some(self.stamp());
        Tray::from_rows(&Board::load().visible(false))
    }

    /// Отпечаток того, от чего зависит значок: файлы сессий и время с шагом `RECHECK_SEC`.
    fn stamp(&self) -> (Signature, i64) {
        (
            store::dir_signature(&self.store.paths.sessions),
            config::now().div_euclid(RECHECK_SEC),
        )
    }
}

/// Картинка значка для модели.
fn picture(tray: &Tray) -> Result<Icon, String> {
    Icon::from_rgba(icon::render(tray.level, tray.count), icon::SIZE, icon::SIZE)
        .map_err(|err| format!("картинка значка не годится: {err}"))
}

/// Сообщение в журнал ошибок программы.
fn warn(text: &str) {
    eprintln!("disp-tray: {text}");
}

/// Отдаёт сигналы трея в `sink`. `activate` — передавать ли щелчки по значку: на macOS
/// щелчок открывает меню, и отдельного действия у него нет.
fn listen(sink: impl Fn(Signal) + Send + Sync + 'static, activate: bool) {
    let sink = Arc::new(sink);
    let menu_sink = Arc::clone(&sink);
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        menu_sink(Signal::Menu(event.id.0));
    }));
    if activate {
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                sink(Signal::Activate);
            }
        }));
    }
}

/// Показывает значок, отдаёт итог в `told` — показан или почему нет — и при удаче ведёт
/// значок дальше.
fn begin(app: &mut App, told: impl FnOnce(&Outcome)) -> Result<(), ()> {
    match app.show() {
        Ok(()) => {
            told(&Outcome::Shown);
            Ok(())
        }
        Err(err) => {
            warn(&err);
            told(&Outcome::Failed(err));
            Err(())
        }
    }
}

/// Показывает значок и ведёт его, пока не нажато «Убрать значок». Итог показа отдаёт в
/// `told`. Код выхода программы.
#[cfg(not(target_os = "macos"))]
pub fn run(mut app: App, told: impl FnOnce(&Outcome)) -> u8 {
    use std::sync::mpsc::{self, RecvTimeoutError};

    let (sender, signals) = mpsc::channel();
    listen(
        move |signal| {
            let _ = sender.send(signal);
        },
        true,
    );
    if begin(&mut app, told).is_err() {
        return 1;
    }
    loop {
        match signals.recv_timeout(POLL) {
            Ok(signal) => {
                if app.handle(signal) == Flow::Quit {
                    return 0;
                }
            }
            Err(RecvTimeoutError::Timeout) => app.refresh(),
            Err(RecvTimeoutError::Disconnected) => return 1,
        }
    }
}

/// Показывает значок и ведёт его, пока не нажато «Убрать значок». Итог показа отдаёт в
/// `told`. Цикл событий AppKit не возвращает управление: программа завершается из него.
#[cfg(target_os = "macos")]
pub fn run(mut app: App, told: impl FnOnce(&Outcome) + 'static) -> u8 {
    use std::sync::Mutex;
    use std::time::Instant;

    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

    let mut event_loop = EventLoopBuilder::<Signal>::with_user_event().build();
    // Только значок в строке меню: без значка в Dock и без своего меню программы.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = Mutex::new(event_loop.create_proxy());
    listen(
        move |signal| {
            if let Ok(proxy) = proxy.lock() {
                let _ = proxy.send_event(signal);
            }
        },
        false,
    );
    let mut told = Some(told);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + POLL);
        match event {
            // Значок в строке меню создаётся, когда цикл событий уже запущен.
            Event::NewEvents(StartCause::Init) => {
                if let Some(told) = told.take()
                    && begin(&mut app, told).is_err()
                {
                    *control_flow = ControlFlow::ExitWithCode(1);
                }
            }
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => app.refresh(),
            Event::UserEvent(signal) => {
                if app.handle(signal) == Flow::Quit {
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}
