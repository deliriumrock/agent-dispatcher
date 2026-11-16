//! Уведомления: своя команда, рабочий стол и Telegram.
//!
//! Каждое уведомление уходит отдельным процессом, которого хук не ждёт: агент не должен
//! стоять, пока отвечает сеть.

use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;

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
///
/// Завершившийся процесс забирает фоновый поток. Хук живёт миллисекунды, и его детей
/// подбирает система, но панель и значок в трее работают часами: без этого каждое
/// уведомление и каждая подпись панели оставались бы у них зомби до самого выхода.
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
    // Если поток не создать, процесс просто останется незабранным, как раньше.
    let _ = thread::Builder::new()
        .name("disp-reap".to_string())
        .spawn(move || child.wait());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    /// Дети этого процесса с именем `name`: номер и состояние из `/proc/<pid>/stat`.
    fn children_named(name: &str) -> Vec<(u32, char)> {
        let Ok(tasks) = fs::read_dir("/proc/self/task") else {
            return Vec::new();
        };
        tasks
            .flatten()
            .filter_map(|task| fs::read_to_string(task.path().join("children")).ok())
            .flat_map(|list| {
                list.split_whitespace()
                    .filter_map(|pid| pid.parse::<u32>().ok())
                    .collect::<Vec<_>>()
            })
            .filter_map(|pid| {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let (head, tail) = stat.split_once(") ")?;
                let comm = head.split_once(" (")?.1;
                (comm == name).then(|| (pid, tail.chars().next().unwrap_or('?')))
            })
            .collect()
    }

    #[test]
    fn finished_processes_are_reaped() {
        // Копия `true` под своим именем: её детей не спутать с детьми других тестов.
        let dir = std::env::temp_dir().join(format!("disp-reap-{}", std::process::id()));
        assert!(fs::create_dir_all(&dir).is_ok());
        let program = dir.join("disp-reap-probe");
        assert!(fs::copy("/bin/true", &program).is_ok());
        for _ in 0..3 {
            spawn(&mut Command::new(&program), None);
        }
        spawn(Command::new("cat").arg("-"), Some(b"stdin"));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut left = children_named("disp-reap-probe");
        while !left.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            left = children_named("disp-reap-probe");
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(left.is_empty(), "не забраны: {left:?}");
    }
}
