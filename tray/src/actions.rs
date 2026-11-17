//! Что делают кнопки меню. Каждая перечитывает файл сессии под блокировкой хранилища:
//! между тем, как меню было построено, и нажатием хук мог сессию изменить.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use disp::board::{self, Accepted, Board, Row};
use disp::{cli, panes, tr};

use crate::model::Action;

/// Выполняет действие над сессией из файла `path`. `Ok` — что сделано, `Err` — почему
/// сделать нельзя; обе строки для человека.
pub fn on_session(action: &Action, path: &Path) -> Result<String, String> {
    let board = Board::load();
    let Some((lock, mut row)) = board.fresh(path) else {
        return Err(gone());
    };
    match action {
        // Enter уходит в панель агента: решение принимается по только что перечитанной
        // записи и под блокировкой, как в `disp yes`.
        Action::Approve(_) => panes::approve(&row.session),
        Action::Focus(_) => {
            drop(lock);
            panes::focus(&row.session)
        }
        Action::Remove(_) => remove(&board, &mut row),
        Action::Open | Action::Quit => Ok(String::new()),
    }
}

/// Убирает сессию из списка. Работающую не убирает: это решается в панели, где видно,
/// что с ней.
fn remove(board: &Board, row: &mut Row) -> Result<String, String> {
    match board::accept(row, board.now, false) {
        Ok(Accepted::Yes) => {
            let summary = row.summary();
            Ok(tr!("removed: {summary}", "убрано: {summary}", "прибрано: {summary}"))
        }
        Ok(Accepted::StillActive) => {
            Err(tr!(
            "the session is working again — it can be removed in the panel (disp watch)",
            "сессия снова работает — убрать её можно в панели (disp watch)",
            "сесія знову працює — прибрати її можна в панелі (disp watch)"
        ))
        }
        Err(err) => Err(tr!("not saved: {err}", "не записано: {err}", "не записано: {err}")),
    }
}

/// Открывает графическую оболочку отдельным окном, независимым от значка.
pub fn open_panel() -> Result<String, String> {
    let board = Board::load();
    let Some(program) = cli::gui_program(&board) else {
        return Err(tr!(
            "disp-gui not found (build and install: make install-gui)",
            "не найдена программа disp-gui (собрать и поставить: make install-gui)",
            "не знайдено програму disp-gui (зібрати й поставити: make install-gui)"
        ));
    };
    let mut child = Command::new(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| {
            let program = program.display();
            tr!(
                "could not start {program}: {err}",
                "не удалось запустить {program}: {err}",
                "не вдалося запустити {program}: {err}"
            )
        })?;
    // Значок живёт долго: закрытое окно надо забрать, иначе останется процесс-зомби.
    std::thread::spawn(move || child.wait());
    Ok(tr!("panel opened", "открыта панель", "панель відкрито"))
}

/// «Этой сессии уже нет в списке» на языке интерфейса.
pub fn gone() -> String {
    tr!(
        "this session is no longer in the list",
        "этой сессии уже нет в списке",
        "цієї сесії вже немає в списку"
    )
}
