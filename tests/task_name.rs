//! Имя задачи: определяется по каталогу, где агент запущен, и не меняется,
//! когда агент переходит в другие каталоги.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Отдельный каталог со своим состоянием диспетчера и «проектами».
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("disp-task-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("каталог песочницы");
        fs::write(dir.join("env"), "").expect("файл настроек");
        Sandbox { dir }
    }

    /// Каталог проекта — репозиторий git: так имя задачи не зависит от того,
    /// что лежит выше временного каталога.
    fn project(&self, name: &str) -> String {
        let project = self.dir.join(name);
        fs::create_dir_all(project.join(".git")).expect("каталог проекта");
        fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        project.to_string_lossy().into_owned()
    }

    /// Команда disp в песочнице: своё состояние, никаких настроек пользователя.
    fn disp(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_disp"));
        command
            .args(args)
            // Ожидания тестов написаны по-русски.
            .env("DISP_LANG", "ru")
            .env("DISP_STATE_DIR", self.dir.join("state"))
            .env("DISP_CONFIG", self.dir.join("env"))
            .env("DISP_CODEX_INDEX", self.dir.join("codex-index.jsonl"))
            .env("DISP_DESKTOP", "0")
            // Как у git: репозитории выше песочницы не в счёт.
            .env("GIT_CEILING_DIRECTORIES", &self.dir)
            .env_remove("DISP_TASK")
            .env_remove("DISP_PANE_LABEL")
            .env_remove("DISP_NOTIFY_CMD")
            .env_remove("DISP_TG_TOKEN")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("ZELLIJ_PANE_ID");
        command
    }

    /// Общий каталог без признаков проекта: в таком запускают агента «для всего».
    fn plain(&self, name: &str) -> String {
        let dir = self.dir.join(name);
        fs::create_dir_all(&dir).expect("каталог");
        dir.to_string_lossy().into_owned()
    }

    /// Отправляет хуку событие агента с дополнительными переменными окружения.
    fn event(&self, agent: &str, json: &Value, envs: &[(&str, &str)]) {
        let mut command = self.disp(&["hook", agent]);
        command
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        let mut child = command.spawn().expect("запуск хука");
        child
            .stdin
            .take()
            .expect("stdin хука")
            .write_all(json.to_string().as_bytes())
            .expect("событие");
        assert!(child.wait().expect("хук завершился").success());
    }

    fn session_file(&self, agent: &str, sid: &str) -> PathBuf {
        self.dir.join(format!("state/sessions/{agent}-{sid}.json"))
    }

    /// Поле записи сессии как текст.
    fn field(&self, agent: &str, sid: &str, name: &str) -> String {
        let text = fs::read_to_string(self.session_file(agent, sid)).expect("файл сессии");
        let session: Value = serde_json::from_str(&text).expect("запись сессии");
        session
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn event(sid: &str, cwd: &str, name: &str) -> Value {
    serde_json::json!({"session_id": sid, "cwd": cwd, "hook_event_name": name, "prompt": "сделай"})
}

#[test]
fn task_stays_with_the_directory_of_the_first_event() {
    let sandbox = Sandbox::new("first");
    let home = sandbox.project("site-31");
    let elsewhere = sandbox.project("scratchpad");

    sandbox.event("codex", &event("c-1", &home, "UserPromptSubmit"), &[]);
    assert_eq!(sandbox.field("codex", "c-1", "task"), "site-31");

    // Агент ушёл в другой каталог и получил новый промпт: задача прежняя.
    sandbox.event("codex", &event("c-1", &elsewhere, "PostToolUse"), &[]);
    sandbox.event("codex", &event("c-1", &elsewhere, "UserPromptSubmit"), &[]);
    assert_eq!(sandbox.field("codex", "c-1", "task"), "site-31");
    assert_eq!(sandbox.field("codex", "c-1", "root"), home);
    assert_eq!(sandbox.field("codex", "c-1", "cwd"), elsewhere);
}

#[test]
fn claude_task_comes_from_its_project_directory() {
    let sandbox = Sandbox::new("claude");
    let project = sandbox.project("trading-system");
    let scratch = sandbox.project("scratchpad");

    // Первое событие застало агента уже во временном каталоге.
    sandbox.event(
        "claude",
        &event("s-1", &scratch, "UserPromptSubmit"),
        &[("CLAUDE_PROJECT_DIR", &project)],
    );
    assert_eq!(sandbox.field("claude", "s-1", "task"), "trading-system");
    assert_eq!(sandbox.field("claude", "s-1", "root"), project);
}

#[test]
fn claude_task_falls_back_to_the_start_of_the_transcript() {
    let sandbox = Sandbox::new("transcript");
    let project = sandbox.project("nbx-142");
    let scratch = sandbox.project("scratchpad");
    let transcript = sandbox.dir.join("transcript.jsonl");
    fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            serde_json::json!({"type": "user", "cwd": project, "message": "x"}),
            serde_json::json!({"type": "assistant", "cwd": scratch, "message": "y"}),
        ),
    )
    .expect("транскрипт");

    let mut prompt = event("s-2", &scratch, "UserPromptSubmit");
    prompt["transcript_path"] = Value::from(transcript.to_string_lossy().into_owned());
    sandbox.event("claude", &prompt, &[]);
    assert_eq!(sandbox.field("claude", "s-2", "task"), "nbx-142");
}

#[test]
fn session_recorded_by_an_older_version_is_repaired_on_any_event() {
    let sandbox = Sandbox::new("repair");
    let project = sandbox.project("trading-system");
    let scratch = sandbox.project("scratchpad");
    // Запись прежней версии: каталога запуска нет, задача названа по временному каталогу.
    let file = sandbox.session_file("claude", "s-3");
    fs::create_dir_all(file.parent().expect("каталог сессий")).expect("каталог сессий");
    let old = serde_json::json!({
        "agent": "claude", "session_id": "s-3", "cwd": scratch, "task": "scratchpad",
        "status": "working", "title": "PR #38 code review", "tsrc": "agent",
        "started": 100, "updated": 200,
    });
    fs::write(&file, old.to_string()).expect("прежняя запись");

    // Достаточно события инструмента, промпта ждать не нужно.
    sandbox.event(
        "claude",
        &event("s-3", &scratch, "PostToolUse"),
        &[("CLAUDE_PROJECT_DIR", &project)],
    );
    assert_eq!(sandbox.field("claude", "s-3", "task"), "trading-system");
    assert_eq!(sandbox.field("claude", "s-3", "root"), project);
    assert_eq!(
        sandbox.field("claude", "s-3", "title"),
        "PR #38 code review"
    );
}

#[test]
fn repair_fixes_a_silent_session_from_its_transcript() {
    let sandbox = Sandbox::new("silent");
    let project = sandbox.project("trading-system");
    let scratch = sandbox.project("scratchpad");
    // Запись прежней версии без каталога запуска; сессия молчит, событий больше нет.
    let file = sandbox.session_file("claude", "s-4");
    fs::create_dir_all(file.parent().expect("каталог сессий")).expect("каталог сессий");
    let old = serde_json::json!({
        "agent": "claude", "session_id": "s-4", "cwd": scratch, "task": "scratchpad",
        "status": "done", "title": "PR #38 code review", "started": 100, "updated": 200,
    });
    fs::write(&file, old.to_string()).expect("прежняя запись");
    // Запись другого агента: каталог запуска неизвестен, имя задачи не трогаем.
    let other = sandbox.session_file("codex", "c-9");
    let named = serde_json::json!({
        "agent": "codex", "session_id": "c-9", "cwd": scratch, "task": "релиз-1.4",
        "status": "working", "started": 100, "updated": 200,
    });
    fs::write(&other, named.to_string()).expect("запись Codex");

    // Транскрипт Claude Code лежит в его каталоге под именем сессии.
    let transcripts = sandbox.dir.join("claude/projects/-home-x-trading-system");
    fs::create_dir_all(&transcripts).expect("каталог транскриптов");
    fs::write(
        transcripts.join("s-4.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({"type": "user", "cwd": project, "message": "x"})
        ),
    )
    .expect("транскрипт");

    let run = |sandbox: &Sandbox| {
        let output = sandbox
            .disp(&["repair"])
            .env("CLAUDE_CONFIG_DIR", sandbox.dir.join("claude"))
            .output()
            .expect("disp repair");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    assert_eq!(run(&sandbox), "исправлено записей: 2\n");
    assert_eq!(sandbox.field("claude", "s-4", "task"), "trading-system");
    assert_eq!(sandbox.field("claude", "s-4", "root"), project);
    assert_eq!(sandbox.field("claude", "s-4", "cwd"), scratch);
    assert_eq!(sandbox.field("codex", "c-9", "task"), "релиз-1.4");
    assert_eq!(sandbox.field("codex", "c-9", "root"), scratch);
    // Второй запуск ничего не меняет.
    assert_eq!(run(&sandbox), "исправлено записей: 0\n");
}

/// Событие записи файла инструментом Claude Code.
fn write(sid: &str, cwd: &str, file: &str) -> Value {
    serde_json::json!({
        "session_id": sid, "cwd": cwd, "hook_event_name": "PostToolUse",
        "tool_name": "Write", "tool_input": {"file_path": file, "content": "x"},
    })
}

#[test]
fn task_is_the_project_the_agent_writes_into() {
    let sandbox = Sandbox::new("writes");
    let root = sandbox.plain("tools");
    let env = [("CLAUDE_PROJECT_DIR", root.as_str())];
    let task = |sandbox: &Sandbox| sandbox.field("claude", "s-5", "task");

    // Агент запущен в общем каталоге: пока он ничего не пишет, задача — этот каталог.
    sandbox.event("claude", &event("s-5", &root, "UserPromptSubmit"), &env);
    assert_eq!(task(&sandbox), "tools");

    // Новый проект в подкаталоге: задача — он.
    let file = format!("{root}/model_dispatcher/src/main.rs");
    sandbox.event("claude", &write("s-5", &root, &file), &env);
    assert_eq!(task(&sandbox), "model_dispatcher");
    assert_eq!(
        sandbox.field("claude", "s-5", "project"),
        format!("{root}/model_dispatcher")
    );
    // Следующий промпт, временный файл вне каталога запуска и файл прямо в нём задачу не меняют.
    let scratch = sandbox.plain("scratchpad");
    sandbox.event("claude", &event("s-5", &scratch, "UserPromptSubmit"), &env);
    sandbox.event(
        "claude",
        &write("s-5", &scratch, &format!("{scratch}/probe.py")),
        &env,
    );
    sandbox.event(
        "claude",
        &write("s-5", &root, &format!("{root}/notes.md")),
        &env,
    );
    assert_eq!(task(&sandbox), "model_dispatcher");

    // Агент перешёл к другому проекту.
    sandbox.event(
        "claude",
        &write("s-5", &root, &format!("{root}/term-gfx/gfx.py")),
        &env,
    );
    assert_eq!(task(&sandbox), "term-gfx");
    assert_eq!(sandbox.field("claude", "s-5", "root"), root);
}

#[test]
fn repository_stays_one_task_whatever_is_written_inside() {
    let sandbox = Sandbox::new("repo");
    let repo = sandbox.project("trading-system");
    let env = [("CLAUDE_PROJECT_DIR", repo.as_str())];
    sandbox.event("claude", &event("s-6", &repo, "UserPromptSubmit"), &env);
    sandbox.event(
        "claude",
        &write("s-6", &repo, &format!("{repo}/apps/api/main.rs")),
        &env,
    );
    assert_eq!(sandbox.field("claude", "s-6", "task"), "trading-system");
    assert_eq!(sandbox.field("claude", "s-6", "project"), "");
}

#[test]
fn codex_patch_names_the_project() {
    let sandbox = Sandbox::new("patch");
    let root = sandbox.plain("tools");
    sandbox.event("codex", &event("c-2", &root, "UserPromptSubmit"), &[]);
    assert_eq!(sandbox.field("codex", "c-2", "task"), "tools");
    let patch = serde_json::json!({
        "session_id": "c-2", "cwd": root, "hook_event_name": "PostToolUse",
        "tool_name": "apply_patch",
        "tool_input": {"command": "*** Begin Patch\n*** Add File: newproj/src/a.txt\n+hi\n*** End Patch"},
    });
    sandbox.event("codex", &patch, &[]);
    assert_eq!(sandbox.field("codex", "c-2", "task"), "newproj");
}

#[test]
fn current_directory_is_a_hint_until_the_first_write() {
    let sandbox = Sandbox::new("hint");
    let root = sandbox.plain("tools");
    let inner = sandbox.plain("tools/model_dispatcher");
    fs::write(format!("{inner}/Cargo.toml"), "").expect("признак проекта");
    let env = [("CLAUDE_PROJECT_DIR", root.as_str())];
    sandbox.event("claude", &event("s-7", &root, "UserPromptSubmit"), &env);
    // Агент перешёл в подкаталог и что-то там запускает.
    let shell = serde_json::json!({
        "session_id": "s-7", "cwd": format!("{inner}/src"), "hook_event_name": "PostToolUse",
        "tool_name": "Bash", "tool_input": {"command": "cargo build"},
    });
    sandbox.event("claude", &shell, &env);
    assert_eq!(sandbox.field("claude", "s-7", "task"), "model_dispatcher");
}

#[test]
fn repair_names_a_silent_session_by_its_last_directory() {
    let sandbox = Sandbox::new("lastdir");
    let root = sandbox.plain("projects");
    let inner = sandbox.plain("projects/tools/term-everything");
    fs::create_dir_all(format!("{inner}/.git")).expect("репозиторий");
    fs::write(format!("{inner}/.git/HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    // Запись версии 0.6.2: каталог запуска есть, проекта нет, агент стоит в подкаталоге.
    let file = sandbox.session_file("claude", "s-8");
    fs::create_dir_all(file.parent().expect("каталог сессий")).expect("каталог сессий");
    let old = serde_json::json!({
        "agent": "claude", "session_id": "s-8", "cwd": inner, "root": root, "task": "projects",
        "status": "done", "title": "Графика в Zellij", "started": 100, "updated": 200,
    });
    fs::write(&file, old.to_string()).expect("прежняя запись");

    // То же, но у каталога нет ни репозитория, ни манифеста: проект — сам этот каталог.
    let bare = sandbox.plain("projects/tools/term-gfx");
    let second = sandbox.session_file("claude", "s-9");
    let old = serde_json::json!({
        "agent": "claude", "session_id": "s-9", "cwd": bare, "root": root, "task": "projects",
        "status": "done", "started": 100, "updated": 200,
    });
    fs::write(&second, old.to_string()).expect("вторая запись");

    let output = sandbox.disp(&["repair"]).output().expect("disp repair");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "исправлено записей: 2\n"
    );
    assert_eq!(sandbox.field("claude", "s-8", "task"), "term-everything");
    assert_eq!(sandbox.field("claude", "s-8", "project"), inner);
    assert_eq!(sandbox.field("claude", "s-8", "root"), root);
    assert_eq!(sandbox.field("claude", "s-9", "task"), "term-gfx");
}
