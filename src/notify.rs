//! Уведомления: своя команда, рабочий стол и Telegram.
//!
//! Каждое уведомление уходит отдельным процессом, которого хук не ждёт: агент не должен
//! стоять, пока отвечает сеть.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::config::Settings;
use crate::detect::which;

/// Уведомление о событии сессии.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    /// Строка статуса: задача, агент, длительность, место. Без текста промптов.
    pub text: String,
    /// Название сессии; уходит только локальным получателям.
    pub title: String,
}

/// Рассылает уведомление всем настроенным получателям.
pub fn send(note: &Note, settings: &Settings) {
    if note.text.is_empty() {
        return;
    }
    if let Some(command) = settings.get("DISP_NOTIFY_CMD") {
        spawn(Command::new(command).arg(&note.text).arg(&note.title), None);
    }
    if settings.get("DISP_DESKTOP") != Some("0") {
        desktop(&note.text, &note.title);
    }
    // В Telegram уходит только строка статуса. Токен передаётся через stdin,
    // чтобы его не было видно в списке процессов.
    if let (Some(token), Some(chat)) = (settings.get("DISP_TG_TOKEN"), settings.get("DISP_TG_CHAT"))
        && which("curl").is_some()
    {
        let config = format!("url = \"https://api.telegram.org/bot{token}/sendMessage\"\n");
        spawn(
            Command::new("curl")
                .args(["-sS", "--max-time", "8", "-o", "/dev/null", "-K", "-"])
                .arg("--data-urlencode")
                .arg(format!("chat_id={chat}"))
                .arg("--data-urlencode")
                .arg(format!("text={}", note.text)),
            Some(config.as_bytes()),
        );
    }
}

/// Программа, которой показываются уведомления на рабочем столе: `notify-send` на Linux,
/// `osascript` на macOS.
pub const DESKTOP_TOOL: &str = if cfg!(target_os = "macos") {
    "osascript"
} else {
    "notify-send"
};

/// Показывает уведомление на рабочем столе: `text` — заголовок, `title` — текст под ним.
/// Возвращает, нашлась ли программа для показа.
pub fn desktop(text: &str, title: &str) -> bool {
    if which(DESKTOP_TOOL).is_none() {
        return false;
    }
    let mut command = Command::new(DESKTOP_TOOL);
    if cfg!(target_os = "macos") {
        // Строки уходят сценарию аргументами, а не внутрь его исходника: кавычки
        // в названии сессии не ломают сценарий.
        command
            .args(["-e", "on run argv"])
            .args([
                "-e",
                "display notification (item 2 of argv) with title \"disp\" \
                 subtitle (item 1 of argv)",
            ])
            .args(["-e", "end run"]);
    } else {
        command.args(["-a", "disp", "--"]);
    }
    spawn(command.arg(text).arg(title), None);
    true
}

/// Запускает процесс и не ждёт его. Ошибка запуска не мешает обработке события.
pub fn spawn(command: &mut Command, stdin: Option<&[u8]>) {
    command.stdout(Stdio::null()).stderr(Stdio::null());
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(bytes);
    }
}
