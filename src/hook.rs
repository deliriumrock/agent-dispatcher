//! Приём событий от хуков агентов: `disp hook <агент>`.
//!
//! Хук никогда не мешает агенту: код возврата всегда 0, в stdout ничего не пишется
//! (Gemini CLI ждёт там JSON и получает пустой объект), ошибки уходят в `hook.log`.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::parent_id;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use chrono::Local;
use serde_json::{Map, Value};

use crate::config::{self, Paths, Settings};
use crate::detect;
use crate::error::Result;
use crate::model::{Session, Status, Subtask, task_label};
use crate::notify::{self, Note};
use crate::panes;
use crate::store::{self, JournalEvent, Store};
use crate::text::{clip, dur, oneline, value_text};

/// Типы уведомлений агента, которые означают «жду человека».
const WAITING_NOTICES: [&str; 5] = [
    "permission_prompt",
    "elicitation_dialog",
    "elicitation_url_dialog",
    "agent_needs_input",
    "toolpermission",
];
/// Причины закрытия сессии, которые считаются явным выходом.
const EXPLICIT_EXITS: [&str; 3] = ["prompt_input_exit", "logout", "exit"];

/// Что событие хука означает для состояния сессии.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Сессия открыта.
    Start,
    /// Пользователь отправил промпт: начался ход.
    Prompt,
    /// Агент выполнил инструмент.
    Tool,
    /// Агент запросил разрешение.
    Ask,
    /// Уведомление агента (ожидание ввода, простой).
    Notice,
    /// Создана подзадача.
    TaskNew,
    /// Подзадача выполнена.
    TaskDone,
    /// Запущен субагент.
    SubagentUp,
    /// Субагент завершился.
    SubagentDown,
    /// Ход закончен.
    Stop,
    /// Ход оборвался ошибкой.
    Fail,
    /// Ход прерван пользователем.
    Interrupt,
    /// Сессия закрыта.
    End,
}

impl Action {
    /// Действие по имени события хука; `None` — событие диспетчеру не интересно.
    pub fn from_event(name: &str) -> Option<Self> {
        Some(match name {
            "SessionStart" => Action::Start,
            "UserPromptSubmit" | "BeforeAgent" => Action::Prompt,
            "PostToolUse" | "PostToolUseFailure" | "PostToolBatch" | "AfterTool" => Action::Tool,
            "PermissionRequest" => Action::Ask,
            "Notification" => Action::Notice,
            "TaskCreated" => Action::TaskNew,
            "TaskCompleted" => Action::TaskDone,
            "SubagentStart" => Action::SubagentUp,
            "SubagentStop" => Action::SubagentDown,
            "Stop" | "AfterAgent" => Action::Stop,
            "StopFailure" => Action::Fail,
            "Interrupt" => Action::Interrupt,
            "SessionEnd" => Action::End,
            _ => return None,
        })
    }
}

/// Обстоятельства события, которые не лежат в его данных.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Агент.
    pub agent: String,
    /// Идентификатор сессии.
    pub sid: String,
    /// Рабочий каталог на момент события.
    pub cwd: String,
    /// Каталог, в котором агент был запущен; запоминается в сессии один раз.
    pub root: String,
    /// Каталог проекта, в который агент перешёл на этом событии.
    pub project: Option<String>,
    /// Имя задачи; пусто, если на этом событии оно не определялось.
    pub task: String,
    /// Ветка git; учитывается только при `fresh`.
    pub branch: String,
    /// Определялись ли задача и ветка на этом событии.
    pub fresh: bool,
    /// Где идёт сессия.
    pub place: String,
    /// Текущее время.
    pub now: i64,
    /// Минимальная длительность хода, о конце которого стоит уведомлять.
    pub notify_min_sec: i64,
    /// Принимать ли законченную сессию при явном выходе.
    pub auto_ok_on_exit: bool,
    /// Название, которое агент сам дал сессии.
    pub agent_title: Option<String>,
}

/// Итог обработки события.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Новое состояние сессии.
    pub state: Session,
    /// Уведомление, если оно нужно.
    pub note: Option<Note>,
    /// Запись в журнал, если статус сменился.
    pub event: Option<JournalEvent>,
}

/// Переход состояния: прежняя запись сессии, данные события и действие дают новую запись.
/// Функция чистая: ничего не читает и не пишет.
pub fn step(
    old: Option<&Session>,
    payload: &Map<String, Value>,
    action: Action,
    ctx: &Context,
) -> Outcome {
    let from = old.map_or("new", |session| session.status.as_str());
    let mut s = old.cloned().unwrap_or_default();
    s.agent = ctx.agent.clone();
    s.session_id = ctx.sid.clone();
    if !ctx.cwd.is_empty() {
        s.cwd = ctx.cwd.clone();
    }
    if s.root.is_none() && !ctx.root.is_empty() {
        s.root = Some(ctx.root.clone());
    }
    if let Some(project) = &ctx.project {
        s.project = Some(project.clone());
    }
    if !ctx.task.is_empty() {
        s.task = ctx.task.clone();
    }
    if ctx.fresh {
        s.branch = ctx.branch.clone();
    }
    s.place = ctx.place.clone();
    if s.started == 0 {
        s.started = ctx.now;
    }
    s.updated = ctx.now;
    s.ended = false;

    let label = task_label(&s.task, &s.branch);
    let who = match ctx.agent.as_str() {
        "claude" => "Claude",
        "codex" => "Codex",
        "gemini" => "Gemini",
        other => other,
    };
    let place = &ctx.place;
    let before = s.status;
    let title_before = s.title.clone();
    let mut note_text = None;

    match action {
        Action::Start => {}
        Action::Prompt => {
            let prompt = oneline(&text_of(payload, &["prompt"]));
            let session_title = text_of(payload, &["session_title"]);
            if !session_title.is_empty() {
                s.title = clip(&oneline(&session_title), 80);
            } else if s.title.is_empty() {
                s.title = clip(&prompt, 80);
            }
            s.last_prompt = Some(clip(&prompt, 300));
            s.status = Status::Working;
            s.turn_started = Some(ctx.now);
            s.accepted = false;
            s.detail.clear();
            s.bg = 0;
        }
        Action::Tool => {
            // Инструмент фонового субагента не оживляет законченный ход.
            let own_tool = matches!(payload.get("agent_id"), None | Some(Value::Null));
            let resumes = matches!(before, Status::Waiting | Status::Idle | Status::Error)
                || (before == Status::Done && own_tool);
            if resumes {
                s.status = Status::Working;
                s.detail.clear();
            }
        }
        Action::Ask => {
            let tool = pick(payload, &["tool_name"]).map_or_else(|| "?".to_string(), value_text);
            let detail = format!("разрешение: {}", clip(&oneline(&tool), 40));
            if before != Status::Waiting {
                note_text = Some(format!("✋ {label} · {who} ждёт тебя · {detail} · {place}"));
            }
            s.status = Status::Waiting;
            s.detail = detail;
            s.accepted = false;
        }
        Action::Notice => {
            let kind = text_of(payload, &["notification_type"]);
            let message = pick(payload, &["message"]).map_or_else(|| kind.clone(), value_text);
            let detail = clip(&oneline(&message), 80);
            let asks_human = WAITING_NOTICES
                .iter()
                .any(|known| kind.eq_ignore_ascii_case(known));
            if asks_human || (kind.is_empty() && before == Status::Working) {
                if before != Status::Waiting {
                    note_text = Some(format!("✋ {label} · {who} ждёт тебя · {detail} · {place}"));
                    s.detail = detail;
                }
                s.status = Status::Waiting;
                s.accepted = false;
            } else if kind == "idle_prompt" && before == Status::Working {
                s.status = Status::Done;
                s.done_at = Some(ctx.now);
                s.detail.clear();
                s.accepted = false;
            }
        }
        Action::TaskNew => {
            if let Some(id) = pick(payload, &["task_id"]).map(value_text) {
                let subject = clip(&oneline(&text_of(payload, &["task_subject"])), 80);
                s.subtasks.insert(
                    id,
                    Subtask {
                        s: subject,
                        d: false,
                    },
                );
            }
        }
        Action::TaskDone => {
            if let Some(id) = pick(payload, &["task_id"]).map(value_text) {
                let subject = match s.subtasks.get(&id) {
                    Some(known) => known.s.clone(),
                    None => text_of(payload, &["task_subject"]),
                };
                s.subtasks.insert(
                    id,
                    Subtask {
                        s: clip(&oneline(&subject), 80),
                        d: true,
                    },
                );
            }
        }
        Action::SubagentUp => s.subagents += 1,
        Action::SubagentDown => s.subagents = (s.subagents - 1).max(0),
        Action::Stop => {
            let lasted = ctx.now - s.turn_started.unwrap_or(ctx.now);
            let background = match payload.get("background_tasks") {
                Some(Value::Array(tasks)) => tasks.len() as i64,
                Some(Value::Number(count)) => count
                    .as_i64()
                    .or_else(|| count.as_f64().map(|value| value as i64))
                    .unwrap_or(0),
                _ => 0,
            };
            let reply = oneline(&text_of(
                payload,
                &["last_assistant_message", "prompt_response"],
            ));
            if !reply.is_empty() {
                s.last_reply = Some(clip(&reply, 300));
            }
            s.status = Status::Done;
            s.done_at = Some(ctx.now);
            s.detail.clear();
            s.bg = background;
            s.subagents = 0;
            s.accepted = false;
            if lasted >= ctx.notify_min_sec {
                let in_background = if background > 0 {
                    format!(" · в фоне: {background}")
                } else {
                    String::new()
                };
                note_text = Some(format!(
                    "✅ {label} · {who} закончил ход ({}){in_background} · {place}",
                    dur(lasted)
                ));
            }
        }
        Action::Fail => {
            let reason = pick(payload, &["error", "message", "reason"])
                .map_or_else(|| "сбой".to_string(), value_text);
            let detail = clip(&oneline(&reason), 80);
            note_text = Some(format!("⛔ {label} · {who}: {detail} · {place}"));
            s.status = Status::Error;
            s.done_at = Some(ctx.now);
            s.detail = detail;
            s.accepted = false;
        }
        Action::Interrupt => {
            // Пользователь сам у клавиатуры, уведомление не нужно,
            // но строка остаётся в списке до явной приёмки.
            if matches!(before, Status::Working | Status::Waiting) {
                s.status = Status::Done;
                s.done_at = Some(ctx.now);
                s.detail = "ход прерван".to_string();
                s.subagents = 0;
                s.accepted = false;
            }
        }
        Action::End => {
            let reason = text_of(payload, &["reason"]);
            if ctx.auto_ok_on_exit
                && s.status == Status::Done
                && EXPLICIT_EXITS.contains(&reason.as_str())
            {
                s.accepted = true;
                s.accepted_at = Some(ctx.now);
            }
            s.ended = true;
            s.end_reason = Some(reason);
        }
    }

    if let Some(title) = ctx.agent_title.as_deref().filter(|title| !title.is_empty())
        && s.tsrc.as_deref() != Some("name")
    {
        s.title = clip(&oneline(title), 80);
        s.tsrc = Some("agent".to_string());
    }

    let ended = action == Action::End;
    let event = (from != s.status.as_str() || ended).then(|| JournalEvent {
        ts: ctx.now,
        agent: ctx.agent.clone(),
        sid: ctx.sid.clone(),
        task: label.clone(),
        place: ctx.place.clone(),
        from: from.to_string(),
        to: if ended {
            "ended".to_string()
        } else {
            s.status.as_str().to_string()
        },
    });
    let note = note_text.map(|text| Note {
        text,
        title: title_before,
    });
    Outcome {
        state: s,
        note,
        event,
    }
}

/// Первое из полей события, которое задано (не `null` и не `false`).
fn pick<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| payload.get(*key))
        .find(|value| !matches!(value, Value::Null | Value::Bool(false)))
}

/// Текст первого заданного поля; пустая строка, если не задано ни одно.
fn text_of(payload: &Map<String, Value>, keys: &[&str]) -> String {
    pick(payload, keys).map(value_text).unwrap_or_default()
}

/// Оставляет в идентификаторе только безопасные для имени файла символы.
fn sanitize(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(80)
        .collect()
}

/// Обрабатывает одно событие хука: читает его из stdin и обновляет состояние сессии.
/// Никогда не завершается ошибкой и не паникует наружу.
pub fn run(agent: &str, event_arg: &str) {
    panic::set_hook(Box::new(|_| {}));
    let store = Store::new(Paths::from_env());
    let _ = store.ensure_dirs();
    let mut raw = Vec::new();
    let _ = io::stdin().lock().read_to_end(&mut raw);

    let handled = panic::catch_unwind(AssertUnwindSafe(|| handle(&store, agent, event_arg, &raw)));
    match handled {
        Ok(Ok(())) => log(&store, None),
        Ok(Err(err)) => log(&store, Some(&err.to_string())),
        Err(_) => log(&store, Some("внутренняя ошибка при обработке события")),
    }
    if agent == "gemini" {
        let _ = io::stdout().write_all(b"{}\n");
    }
}

/// Основная работа хука. Ошибки возвращаются вызывающему и попадают в `hook.log`.
fn handle(store: &Store, agent_arg: &str, event_arg: &str, raw: &[u8]) -> Result<()> {
    let settings = Settings::load(&store.paths.conf_file);
    let agent = match sanitize(agent_arg) {
        name if name.is_empty() => "unknown".to_string(),
        name => name,
    };
    let parsed: Option<Value> = serde_json::from_slice(raw).ok();
    if settings.get("DISP_DEBUG") == Some("1") {
        dump_payload(store, &agent, parsed.as_ref());
    }
    let empty = Map::new();
    let payload = parsed.as_ref().and_then(Value::as_object).unwrap_or(&empty);

    let event = match text_of(payload, &["hook_event_name"]) {
        name if name.is_empty() => event_arg.to_string(),
        name => name,
    };
    let Some(action) = Action::from_event(&event) else {
        return Ok(());
    };

    let sid = match sanitize(&text_of(
        payload,
        &["session_id", "sessionId", "thread_id", "conversation_id"],
    )) {
        id if id.is_empty() => format!("nosid-{}", parent_id()),
        id => id,
    };
    let cwd = match text_of(payload, &["cwd"]) {
        dir if dir.is_empty() => std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
        dir => dir,
    };

    let file = store.session_file(&agent, &sid);
    // Прежнее состояние читаем без блокировки: запись атомарная, а нужно оно здесь только
    // чтобы решить, что стоит определять заново.
    let before = store::read_session(&file);
    let transcript = text_of(payload, &["transcript_path"]);

    // Задача определяется по каталогу, где агент запущен, а не по текущему: иначе она
    // менялась бы всякий раз, когда агент переходит в другой каталог. Каталог запуска
    // запоминается в сессии; у сессии, где его ещё нет, он определяется на любом событии.
    let stored_root = before
        .as_ref()
        .and_then(|session| session.root.clone())
        .filter(|root| !root.is_empty());
    let boundary = stored_root.is_none() || matches!(action, Action::Start | Action::Prompt);
    let root = stored_root.unwrap_or_else(|| detect::launch_dir(&agent, &transcript, &cwd));

    // Агента могли запустить в общем каталоге, а работает он в одном из подкаталогов:
    // тогда задача — этот подкаталог. Его выдаёт файл, который агент только что записал;
    // пока записей не было, годится и текущий каталог, если он внутри каталога запуска.
    let stored_project = before.as_ref().and_then(|session| session.project.clone());
    let written = if action == Action::Tool {
        detect::written_path(payload, &cwd)
    } else {
        None
    };
    let project = match (&written, &stored_project) {
        (Some(file), known) => detect::project_dir(&root, file, false, known.as_deref()),
        (None, None) if cwd != root => detect::project_dir(&root, Path::new(&cwd), true, None),
        _ => None,
    }
    .filter(|found| Some(found) != stored_project.as_ref());

    let fresh = boundary || project.is_some();
    let (task, branch) = if fresh {
        let base = project
            .as_ref()
            .or(stored_project.as_ref())
            .unwrap_or(&root);
        let (task, branch) = detect::project(base);
        (config::env_nonempty("DISP_TASK").unwrap_or(task), branch)
    } else {
        (String::new(), String::new())
    };

    // Название от агента ищем до блокировки: опрос zellij небыстрый. События инструментов
    // самые частые, поэтому на них ищем, только пока название ещё не найдено и не задано вручную.
    let dir = detect::base_name(&cwd);
    let agent_title = if action == Action::Tool {
        let known = before
            .as_ref()
            .is_some_and(|session| matches!(session.tsrc.as_deref(), Some("agent" | "name")));
        if known {
            None
        } else {
            detect::agent_title(&agent, &sid, &transcript, false, &dir, &settings)
        }
    } else {
        detect::agent_title(&agent, &sid, &transcript, true, &dir, &settings)
    };

    let ctx = Context {
        agent,
        sid,
        cwd,
        root,
        project,
        task,
        branch,
        fresh,
        place: detect::place(),
        now: config::now(),
        notify_min_sec: settings.int("DISP_NOTIFY_MIN_SEC", 120),
        auto_ok_on_exit: settings.get("DISP_AUTO_OK_ON_EXIT") == Some("1"),
        agent_title,
    };

    let lock = store.lock();
    let old = store::read_session(&file);
    let outcome = step(old.as_ref(), payload, action, &ctx);
    store::write_session(&file, &outcome.state)?;
    if let Some(event) = &outcome.event {
        store.append_event(event)?;
    }
    drop(lock);

    if let Some(note) = &outcome.note {
        notify::send(note, &settings);
    }
    if panes::enabled(&settings) {
        label_pane(action, old.as_ref(), &outcome.state);
    }
    Ok(())
}

/// Держит подпись панели в согласии с сессией: ставит её на границах хода и когда
/// изменилось название, снимает при закрытии сессии. На прочих событиях zellij
/// не вызывается: они слишком частые.
fn label_pane(action: Action, old: Option<&Session>, state: &Session) {
    if action == Action::End {
        panes::clear(&state.place, false);
        return;
    }
    let Some((_, pane)) = panes::target(&state.place) else {
        return;
    };
    let boundary = matches!(
        action,
        Action::Start | Action::Prompt | Action::Stop | Action::Fail | Action::Interrupt
    );
    let changed = old.map(|old| panes::label(pane, old)) != Some(panes::label(pane, state));
    if boundary || changed {
        panes::apply(state, false);
    }
}

/// Дописывает сырое событие в `payloads.tsv` (режим `DISP_DEBUG=1`).
fn dump_payload(store: &Store, agent: &str, payload: Option<&Value>) {
    let body = payload.map_or_else(|| "\"<не JSON>\"".to_string(), Value::to_string);
    let line = format!("{}\t{agent}\t{body}\n", Local::now().format("%F %T"));
    append(&store.paths.payloads(), &line);
}

/// Дописывает сообщение в `hook.log`. Без сообщения только создаёт файл: по нему видно,
/// что хук вызывался.
fn log(store: &Store, message: Option<&str>) {
    let line = message
        .map(|text| format!("{} disp: {text}\n", Local::now().format("%F %T")))
        .unwrap_or_default();
    append(&store.paths.hook_log(), &line);
}

/// Дописывает текст в файл с правами 600, игнорируя ошибки.
fn append(path: &Path, text: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(now: i64) -> Context {
        Context {
            agent: "claude".to_string(),
            sid: "s1".to_string(),
            cwd: "/work/nbx-142".to_string(),
            task: "nbx-142".to_string(),
            fresh: true,
            place: "zellij:main#3".to_string(),
            now,
            notify_min_sec: 120,
            ..Context::default()
        }
    }

    fn apply(old: Option<&Session>, event: &str, payload: Value, ctx: &Context) -> Outcome {
        let empty = Map::new();
        let action = Action::from_event(event).unwrap_or(Action::Start);
        step(old, payload.as_object().unwrap_or(&empty), action, ctx)
    }

    #[test]
    fn launch_directory_is_remembered_once() {
        let mut first = ctx(1000);
        first.root = "/work/nbx-142".to_string();
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "собери"}),
            &first,
        );
        assert_eq!(started.state.root.as_deref(), Some("/work/nbx-142"));

        // Агент ушёл во временный каталог: меняется текущий каталог, но не каталог запуска
        // и не задача.
        let mut later = ctx(1010);
        later.cwd = "/tmp/scratchpad".to_string();
        later.root = "/tmp/scratchpad".to_string();
        later.task = String::new();
        later.fresh = false;
        let moved = apply(Some(&started.state), "PostToolUse", json!({}), &later);
        assert_eq!(moved.state.cwd, "/tmp/scratchpad");
        assert_eq!(moved.state.root.as_deref(), Some("/work/nbx-142"));
        assert_eq!(moved.state.task, "nbx-142");
    }

    #[test]
    fn long_turn_notifies_with_duration_and_background_count() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "Собери билд\nпод macOS"}),
            &ctx(1000),
        );
        assert_eq!(started.state.status, Status::Working);
        assert_eq!(started.state.title, "Собери билд под macOS");
        assert_eq!(
            started.event.map(|event| (event.from, event.to)),
            Some(("new".into(), "working".into()))
        );

        let stopped = apply(
            Some(&started.state),
            "Stop",
            json!({"last_assistant_message": "Билд готов", "background_tasks": [{"id": "b1"}]}),
            &ctx(1900),
        );
        assert_eq!(stopped.state.status, Status::Done);
        assert_eq!(stopped.state.bg, 1);
        assert_eq!(
            stopped.note.map(|note| note.text).as_deref(),
            Some("✅ nbx-142 · Claude закончил ход (15м) · в фоне: 1 · zellij:main#3")
        );
    }

    #[test]
    fn short_turn_stays_quiet() {
        let started = apply(None, "UserPromptSubmit", json!({"prompt": "x"}), &ctx(1000));
        let stopped = apply(Some(&started.state), "Stop", json!({}), &ctx(1030));
        assert!(stopped.note.is_none());
        assert_eq!(stopped.state.status, Status::Done);
    }

    #[test]
    fn permission_request_notifies_once() {
        let started = apply(None, "UserPromptSubmit", json!({"prompt": "x"}), &ctx(1000));
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            json!({"tool_name": "Bash"}),
            &ctx(1100),
        );
        assert_eq!(asked.state.detail, "разрешение: Bash");
        assert!(asked.note.is_some());
        let again = apply(
            Some(&asked.state),
            "Notification",
            json!({"notification_type": "permission_prompt", "message": "Claude needs your permission"}),
            &ctx(1101),
        );
        assert!(again.note.is_none());
        assert_eq!(again.state.detail, "разрешение: Bash");
        assert!(again.event.is_none());
    }

    #[test]
    fn subagent_tool_does_not_revive_a_finished_turn() {
        let done = Session {
            status: Status::Done,
            session_id: "s1".into(),
            ..Session::default()
        };
        let by_subagent = apply(
            Some(&done),
            "PostToolUse",
            json!({"tool_name": "Read", "agent_id": "sub-1"}),
            &ctx(10),
        );
        assert_eq!(by_subagent.state.status, Status::Done);
        let by_agent = apply(
            Some(&done),
            "PostToolUse",
            json!({"tool_name": "Bash"}),
            &ctx(10),
        );
        assert_eq!(by_agent.state.status, Status::Working);
    }

    #[test]
    fn interrupt_keeps_the_row_until_accepted() {
        let started = apply(None, "UserPromptSubmit", json!({"prompt": "x"}), &ctx(1000));
        let interrupted = apply(Some(&started.state), "Interrupt", json!({}), &ctx(1400));
        assert_eq!(interrupted.state.status, Status::Done);
        assert_eq!(interrupted.state.detail, "ход прерван");
        assert!(!interrupted.state.accepted);
        assert!(interrupted.note.is_none());
    }

    #[test]
    fn manual_name_beats_agent_title() {
        let mut context = ctx(1000);
        context.agent_title = Some("Сборка и подпись macOS-билда".to_string());
        let titled = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "ну давай"}),
            &context,
        );
        assert_eq!(titled.state.title, "Сборка и подпись macOS-билда");
        assert_eq!(titled.state.tsrc.as_deref(), Some("agent"));

        let named = Session {
            title: "жду билд из CI".into(),
            tsrc: Some("name".into()),
            ..titled.state
        };
        let after = apply(Some(&named), "Stop", json!({}), &context);
        assert_eq!(after.state.title, "жду билд из CI");
    }

    #[test]
    fn session_end_is_journaled_and_auto_accept_is_opt_in() {
        let done = Session {
            status: Status::Done,
            session_id: "s1".into(),
            ..Session::default()
        };
        let strict = apply(
            Some(&done),
            "SessionEnd",
            json!({"reason": "exit"}),
            &ctx(10),
        );
        assert!(strict.state.ended && !strict.state.accepted);
        assert_eq!(strict.event.map(|event| event.to).as_deref(), Some("ended"));

        let mut lenient = ctx(10);
        lenient.auto_ok_on_exit = true;
        let accepted = apply(
            Some(&done),
            "SessionEnd",
            json!({"reason": "prompt_input_exit"}),
            &lenient,
        );
        assert!(accepted.state.accepted);
    }

    #[test]
    fn session_id_cannot_escape_the_sessions_dir() {
        assert_eq!(sanitize("../../etc/x y"), "....etcxy");
        assert_eq!(sanitize(&"a".repeat(200)).len(), 80);
    }
}
