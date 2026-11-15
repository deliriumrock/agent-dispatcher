//! Подписи панелей zellij: что хук и команды просят у zellij.
//!
//! Настоящий zellij заменён скриптом, который записывает свои аргументы в файл.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Отдельный каталог со своим состоянием диспетчера и поддельным zellij.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("disp-labels-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("каталог песочницы");
        let zellij = dir.join("bin/zellij");
        // list-panes отвечает двумя панелями: одна с подписью диспетчера, другая без.
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$*\" in\n  *list-panes*) printf '%s' \
             '[{{\"id\":3,\"is_plugin\":false,\"title\":\"#3 Старая подпись\"}},\
             {{\"id\":5,\"is_plugin\":false,\"title\":\"PR #38 code review\"}}]' ;;\nesac\n",
            log = dir.join("zellij.log").display()
        );
        fs::write(&zellij, script).expect("поддельный zellij");
        fs::set_permissions(&zellij, fs::Permissions::from_mode(0o755)).expect("права");
        fs::write(dir.join("env"), "").expect("файл настроек");
        Sandbox { dir }
    }

    /// Команда disp в песочнице: своё состояние, свой zellij, панель номер 4 сессии `dev`.
    fn disp(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_disp"));
        let path = std::env::var("PATH").unwrap_or_default();
        command
            .args(args)
            .env("PATH", format!("{}:{path}", self.dir.join("bin").display()))
            .env("DISP_STATE_DIR", self.dir.join("state"))
            .env("DISP_CONFIG", self.dir.join("env"))
            .env("DISP_CODEX_INDEX", self.dir.join("codex-index.jsonl"))
            .env("DISP_DESKTOP", "0")
            .env("DISP_TASK", "site-31")
            .env("ZELLIJ", "0")
            .env("ZELLIJ_SESSION_NAME", "dev")
            .env("ZELLIJ_PANE_ID", "4")
            .env_remove("DISP_NOTIFY_CMD")
            .env_remove("DISP_TG_TOKEN")
            .env_remove("DISP_PANE_LABEL");
        command
    }

    /// Отправляет хуку событие агента.
    fn event(&self, agent: &str, json: &str, labels: bool) {
        let mut command = self.disp(&["hook", agent]);
        if labels {
            command.env("DISP_PANE_LABEL", "1");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("запуск хука");
        child
            .stdin
            .take()
            .expect("stdin хука")
            .write_all(json.as_bytes())
            .expect("событие");
        assert!(child.wait().expect("хук завершился").success());
    }

    /// Что просили у zellij; подпись ставится отдельным процессом, поэтому ждём,
    /// пока в журнале появится `lines` строк.
    fn zellij_calls(&self, lines: usize) -> Vec<String> {
        let log = self.dir.join("zellij.log");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let calls = read_lines(&log);
            if calls.len() >= lines || Instant::now() > deadline {
                return calls;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn hook_labels_the_pane_at_turn_boundaries_only() {
    let sandbox = Sandbox::new("hook");
    let prompt = r#"{"session_id":"s-1","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"Почини импорт цен"}"#;
    let tool =
        r#"{"session_id":"s-1","cwd":"/tmp","hook_event_name":"PostToolUse","tool_name":"Bash"}"#;
    let end = r#"{"session_id":"s-1","cwd":"/tmp","hook_event_name":"SessionEnd","reason":"exit"}"#;

    sandbox.event("claude", prompt, true);
    assert_eq!(
        sandbox.zellij_calls(1),
        ["--session dev action rename-pane -p terminal_4 #4 Почини импорт цен"]
    );
    // События инструментов частые: пока название то же, zellij не трогаем.
    sandbox.event("claude", tool, true);
    sandbox.event("claude", tool, true);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(sandbox.zellij_calls(1).len(), 1);

    // Закрытие сессии возвращает панели заголовок программы.
    sandbox.event("claude", end, true);
    assert_eq!(
        sandbox.zellij_calls(2).last().map(String::as_str),
        Some("--session dev action undo-rename-pane -p terminal_4")
    );
}

#[test]
fn labels_are_off_unless_enabled() {
    let sandbox = Sandbox::new("off");
    let prompt =
        r#"{"session_id":"s-2","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"x"}"#;
    sandbox.event("claude", prompt, false);
    thread::sleep(Duration::from_millis(300));
    assert!(sandbox.zellij_calls(0).is_empty());
}

#[test]
fn codex_title_is_read_from_its_index_and_goes_into_the_label() {
    let sandbox = Sandbox::new("codex");
    fs::write(
        sandbox.dir.join("codex-index.jsonl"),
        "{\"id\":\"c-1\",\"thread_name\":\"Запустить систему за 21 сентября\",\"updated_at\":\"x\"}\n",
    )
    .expect("список сессий Codex");
    let prompt = r#"{"session_id":"c-1","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"запусти"}"#;
    sandbox.event("codex", prompt, true);
    // Название взято из списка Codex: заголовок панели zellij не запрашивался.
    assert_eq!(
        sandbox.zellij_calls(1),
        ["--session dev action rename-pane -p terminal_4 #4 Запустить систему за 21 сентября"]
    );
}

#[test]
fn label_takes_a_missing_title_from_the_pane() {
    let sandbox = Sandbox::new("untitled");
    // Сессия замечена без промпта: названия у диспетчера нет. Она идёт в панели 5,
    // у которой в поддельном zellij заголовок «PR #38 code review».
    let stop = r#"{"session_id":"s-4","cwd":"/tmp","hook_event_name":"Stop"}"#;
    let mut hook = sandbox.disp(&["hook", "claude"]);
    hook.env("ZELLIJ_PANE_ID", "5")
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let mut child = hook.spawn().expect("запуск хука");
    child
        .stdin
        .take()
        .expect("stdin хука")
        .write_all(stop.as_bytes())
        .expect("событие");
    assert!(child.wait().expect("хук завершился").success());

    let output = sandbox.disp(&["label"]).output().expect("disp label");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().next(),
        Some("подписано панелей: 1")
    );
    assert_eq!(
        sandbox.zellij_calls(3),
        [
            "--session dev action undo-rename-pane -p terminal_5",
            "--session dev action list-panes -j",
            "--session dev action rename-pane -p terminal_5 #5 PR #38 code review"
        ]
    );
    // Название сохранено и в самой сессии: оно появится в таблице.
    let shown = sandbox
        .disp(&["show", "site-31"])
        .output()
        .expect("disp show");
    assert!(String::from_utf8_lossy(&shown.stdout).contains("что:       PR #38 code review"));
}

#[test]
fn label_and_unlabel_commands() {
    let sandbox = Sandbox::new("commands");
    let prompt = r#"{"session_id":"s-3","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"Сверстай страницу"}"#;
    sandbox.event("claude", prompt, false);

    let output = sandbox.disp(&["label"]).output().expect("disp label");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with("подписано панелей: 1\n"), "{text}");
    assert_eq!(
        sandbox.zellij_calls(1),
        ["--session dev action rename-pane -p terminal_4 #4 Сверстай страницу"]
    );

    // Переименование строки сразу меняет подпись, если подписи включены.
    let status = sandbox
        .disp(&["name", "site-31", "Лендинг"])
        .env("DISP_PANE_LABEL", "1")
        .status()
        .expect("disp name");
    assert!(status.success());
    assert_eq!(
        sandbox.zellij_calls(2).last().map(String::as_str),
        Some("--session dev action rename-pane -p terminal_4 #4 Лендинг")
    );

    // unlabel снимает только подписи диспетчера: панель «PR #38 …» не трогает.
    let output = sandbox.disp(&["unlabel"]).output().expect("disp unlabel");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "снято подписей: 1\n"
    );
    let calls = sandbox.zellij_calls(4);
    assert_eq!(
        calls[2..],
        [
            "--session dev action list-panes -j",
            "--session dev action undo-rename-pane -p terminal_3"
        ]
    );
}

#[test]
fn yes_presses_enter_only_for_a_waiting_agent() {
    let sandbox = Sandbox::new("yes");
    let prompt = r#"{"session_id":"s-9","cwd":"/tmp","hook_event_name":"UserPromptSubmit","prompt":"Собери"}"#;
    let ask = r#"{"session_id":"s-9","cwd":"/tmp","hook_event_name":"PermissionRequest","tool_name":"Bash"}"#;
    sandbox.event("claude", prompt, false);

    // Агент работает и ничего не спрашивает: Enter не уходит.
    let output = sandbox
        .disp(&["yes", "site-31"])
        .output()
        .expect("disp yes");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("агент сейчас ничего не спрашивает"));
    assert!(sandbox.zellij_calls(0).is_empty());

    // Агент спросил разрешение: Enter нажимается в его панели.
    sandbox.event("claude", ask, false);
    let output = sandbox
        .disp(&["yes", "site-31"])
        .output()
        .expect("disp yes");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "ответ по умолчанию отправлен: zellij:dev#4\n"
    );
    assert_eq!(sandbox.zellij_calls(1), ["action write -p terminal_4 13"]);

    // Запрос без id и чужой запрос.
    assert_eq!(
        sandbox.disp(&["yes"]).status().ok().and_then(|s| s.code()),
        Some(2)
    );
    let output = sandbox
        .disp(&["yes", "nothing"])
        .output()
        .expect("disp yes");
    assert!(String::from_utf8_lossy(&output.stderr).contains("Не найдено: nothing"));
}
