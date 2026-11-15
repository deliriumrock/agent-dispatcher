//! Подписи панелей zellij: номер панели и название сессии в заголовке панели.
//!
//! Номера панелей в zellij идут в порядке создания, а не по расположению, поэтому
//! по строке таблицы (`zellij:dev#4`) панель глазами не найти. Подпись ставит этот же
//! номер на саму панель: `#4 Название сессии`. Имя панели заменяет заголовок, который
//! ставит программа; после `undo-rename-pane` заголовок программы возвращается.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

use crate::config::{Settings, env_nonempty};
use crate::detect::run_with_timeout;
use crate::model::Session;
use crate::notify;
use crate::text::clip;

/// Наибольшая длина названия в подписи: длиннее рамка панели всё равно не покажет.
const TITLE_MAX: usize = 60;

/// Включены ли подписи панелей (`DISP_PANE_LABEL=1`).
pub fn enabled(settings: &Settings) -> bool {
    settings.get("DISP_PANE_LABEL") == Some("1")
}

/// Сессия zellij и номер панели из места вида `zellij:сессия#панель`.
pub fn target(place: &str) -> Option<(&str, &str)> {
    let (session, pane) = place.strip_prefix("zellij:")?.rsplit_once('#')?;
    let known =
        !session.is_empty() && !pane.is_empty() && pane.chars().all(|ch| ch.is_ascii_digit());
    known.then_some((session, pane))
}

/// Подпись панели номер `pane` для сессии: номер и название, а без названия — имя задачи.
pub fn label(pane: &str, session: &Session) -> String {
    let title = if session.title.is_empty() {
        session.label()
    } else {
        clip(&session.title, TITLE_MAX)
    };
    format!("#{pane} {title}").trim_end().to_string()
}

/// Похож ли заголовок панели на подпись диспетчера: решётка, номер и пробел либо конец.
/// Такой заголовок нельзя принимать за название, которое придумал агент.
pub fn is_label(title: &str) -> bool {
    let Some(rest) = title.strip_prefix('#') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && matches!(rest[digits..].chars().next(), None | Some(' '))
}

/// Ставит подпись на панель, где идёт сессия. Если сессия не в zellij, ничего не делает.
///
/// С `wait` команда zellij выполняется до конца: так подписывают команды и панель.
/// Без `wait` она запускается и остаётся сама по себе: хук не должен её ждать.
pub fn apply(session: &Session, wait: bool) {
    let Some((zellij, pane)) = target(&session.place) else {
        return;
    };
    let name = label(pane, session);
    run(
        action(zellij).args(["rename-pane", "-p", &format!("terminal_{pane}"), &name]),
        wait,
    );
}

/// Снимает подпись с панели: в заголовок возвращается то, что ставит программа.
pub fn clear(place: &str, wait: bool) {
    let Some((zellij, pane)) = target(place) else {
        return;
    };
    run(
        action(zellij).args(["undo-rename-pane", "-p", &format!("terminal_{pane}")]),
        wait,
    );
}

/// Заголовки терминальных панелей сессии zellij по номерам панелей. Пусто, если zellij
/// не ответил.
pub fn titles(zellij: &str) -> HashMap<String, String> {
    let Some(listing) = run_with_timeout(action(zellij).args(["list-panes", "-j"])) else {
        return HashMap::new();
    };
    let panes: Vec<Value> = serde_json::from_str(&listing).unwrap_or_default();
    panes
        .iter()
        .filter(|pane| pane.get("is_plugin").and_then(Value::as_bool) != Some(true))
        .filter_map(|pane| {
            let id = pane.get("id").and_then(Value::as_u64)?;
            let title = pane.get("title").and_then(Value::as_str)?;
            Some((id.to_string(), title.to_string()))
        })
        .collect()
}

/// Снимает подписи диспетчера со всех панелей сессии zellij. Возвращает, сколько снято.
pub fn clear_all(zellij: &str) -> usize {
    let labelled: Vec<String> = titles(zellij)
        .into_iter()
        .filter(|(_, title)| is_label(title))
        .map(|(pane, _)| pane)
        .collect();
    for pane in &labelled {
        clear(&format!("zellij:{zellij}#{pane}"), true);
    }
    labelled.len()
}

/// Переводит фокус zellij в панель, где идёт сессия. Возвращает сообщение для человека:
/// `Ok` — что сделано, `Err` — почему перейти нельзя.
pub fn focus(session: &Session) -> Result<String, String> {
    if session.ended {
        return Err("сессия закрыта: её панели уже нет".to_string());
    }
    let Some((zellij, pane)) = target(&session.place) else {
        let place = if session.place.is_empty() {
            "?"
        } else {
            &session.place
        };
        return Err(format!("эта сессия не в zellij: {place}"));
    };
    let here = env_nonempty("ZELLIJ_SESSION_NAME");
    let same_session = here.as_deref() == Some(zellij);
    let mut command = Command::new("zellij");
    if !same_session {
        // Про несуществующую сессию zellij сообщает текстом, а код возврата оставляет нулевым.
        let sessions = run_with_timeout(Command::new("zellij").args(["list-sessions", "-n", "-s"]));
        let exists = sessions.is_some_and(|list| list.lines().any(|name| name.trim() == zellij));
        if !exists {
            return Err(format!("сессии zellij «{zellij}» сейчас нет"));
        }
        command.args(["--session", zellij]);
    }
    command.args(["action", "focus-pane-id", &format!("terminal_{pane}")]);
    match run_with_timeout(&mut command) {
        Some(_) if same_session => Ok(format!("фокус: {}", session.place)),
        Some(_) => Ok(format!("выбрана в другой сессии zellij: {zellij}")),
        None => Err(format!("в сессии «{zellij}» нет панели {pane}")),
    }
}

/// Команда `zellij --session <сессия> action`.
fn action(zellij: &str) -> Command {
    let mut command = Command::new("zellij");
    command.args(["--session", zellij, "action"]);
    command
}

fn run(command: &mut Command, wait: bool) {
    if wait {
        let _ = run_with_timeout(command);
    } else {
        notify::spawn(command, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(title: &str) -> Session {
        Session {
            task: "trading-system".to_string(),
            branch: "dev".to_string(),
            title: title.to_string(),
            ..Session::default()
        }
    }

    #[test]
    fn zellij_place_is_parsed() {
        assert_eq!(target("zellij:dev#4"), Some(("dev", "4")));
        assert_eq!(target("zellij:work#2#17"), Some(("work#2", "17")));
        assert_eq!(target("zellij:dev#?"), None);
        assert_eq!(target("zellij:?#3"), Some(("?", "3")));
        assert_eq!(target("vscode"), None);
        assert_eq!(target("tmux:%3"), None);
    }

    #[test]
    fn label_is_number_and_title() {
        assert_eq!(
            label("4", &session("Графика в Zellij")),
            "#4 Графика в Zellij"
        );
        // Без названия — задача с веткой, как в таблице.
        assert_eq!(label("35", &session("")), "#35 trading-system@dev");
        assert_eq!(label("7", &Session::default()), "#7");
        let long = label("4", &session(&"я".repeat(200)));
        assert_eq!(long.chars().count(), 3 + TITLE_MAX);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn own_labels_are_recognised() {
        assert!(is_label("#4 Графика в Zellij"));
        assert!(is_label("#35"));
        assert!(!is_label("PR #38 code review"));
        assert!(!is_label("#review"));
        assert!(!is_label("#4x"));
        assert!(!is_label("Графика в Zellij"));
    }
}
