//! Что делают кнопки меню. Каждая перечитывает файл сессии под блокировкой хранилища:
//! между тем, как меню было построено, и нажатием хук мог сессию изменить.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use disp::board::{self, Accepted, Board, Row};
use disp::{cli, panes};

use crate::model::Action;

/// Выполняет действие над сессией из файла `path`. `Ok` — что сделано, `Err` — почему
/// сделать нельзя; обе строки для человека.
pub fn on_session(action: &Action, path: &Path) -> Result<String, String> {
    let board = Board::load();
    let Some((lock, mut row)) = board.fresh(path) else {
        return Err("этой сессии уже нет в списке".to_string());
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
        Ok(Accepted::Yes) => Ok(format!("убрано: {}", row.summary())),
        Ok(Accepted::StillActive) => {
            Err("сессия снова работает — убрать её можно в панели (disp watch)".to_string())
        }
        Err(err) => Err(format!("не записано: {err}")),
    }
}

/// Открывает графическую оболочку отдельным окном, независимым от значка.
pub fn open_panel() -> Result<String, String> {
    let board = Board::load();
    let Some(program) = cli::gui_program(&board) else {
        return Err("не найдена программа disp-gui (собрать и поставить: make install-gui)".into());
    };
    let mut child = Command::new(&program)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|err| format!("не удалось запустить {}: {err}", program.display()))?;
    // Значок живёт долго: закрытое окно надо забрать, иначе останется процесс-зомби.
    std::thread::spawn(move || child.wait());
    Ok("открыта панель".to_string())
}
