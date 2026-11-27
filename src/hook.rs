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

use crate::agents;
use crate::config::{self, Paths, Settings};
use crate::detect;
use crate::error::Result;
use crate::model::{AgentProcess, Session, Status, Subtask, task_label};
use crate::notify::{self, Note};
use crate::panes;
use crate::procs;
use crate::store::{self, JournalEvent, Store};
use crate::text::{clip, dur, oneline, value_text};
use crate::{t, tr};

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
    /// Процесс агента, если его удалось узнать на этом событии.
    pub process: Option<AgentProcess>,
    /// Транскрипт сессии из события (`transcript_path`), если агент его назвал.
    pub transcript: String,
    /// Сколько фоновых агентов Claude Code ещё работают на конце хода (`agents::waiting_for`).
    pub waiting_agents: usize,
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
    if !ctx.transcript.is_empty() {
        s.transcript = Some(ctx.transcript.clone());
    }
    if ctx.process.is_some() {
        s.process = ctx.process;
    }
    if s.started == 0 {
        s.started = ctx.now;
    }
    s.updated = ctx.now;
    s.ended = false;

    // Ход, который ждал фоновой работы, продолжился или кончился: ждать больше нечего.
    // Инструменты и субагенты ожидание не прерывают: это работа тех же агентов. Не прерывает
    // его и напоминание о простое: пока идёт фоновая работа, Claude Code ждёт ввода, но
    // ход продолжит сам.
    let paused = s.awaiting.is_some();
    let idle_notice =
        action == Action::Notice && text_of(payload, &["notification_type"]) == "idle_prompt";
    if !matches!(
        action,
        Action::Tool
            | Action::SubagentUp
            | Action::SubagentDown
            | Action::TaskNew
            | Action::TaskDone
    ) && !idle_notice
    {
        s.awaiting = None;
        s.held = 0;
    }
    // The thread behind the event: the `agent_id` of a child agent, or empty for the
    // session itself.
    let thread = text_of(payload, &["agent_id"]);
    // The turn ended in failure, by interrupt or with the session: nobody waits for an
    // answer any more.
    if matches!(action, Action::Fail | Action::Interrupt | Action::End) {
        s.asks.clear();
    }
    // The session itself goes on: a turn it ended while a child agent waited is no longer
    // its last word. A request from the agent itself continues the turn, too.
    if action == Action::Prompt
        || (thread.is_empty() && matches!(action, Action::Tool | Action::Ask))
        || s.asks.is_empty()
    {
        s.turn_over = false;
    }
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
            // Запрос не от человека: так Claude Code сам продолжает ход, когда кончилась его
            // фоновая работа, — приходит уведомление о задаче.
            let system = text_of(payload, &["origin"]) == "system"
                || prompt.starts_with("<task-notification>");
            let session_title = text_of(payload, &["session_title"]);
            if !session_title.is_empty() {
                s.title = clip(&oneline(&session_title), 80);
            } else if s.title.is_empty() && !system {
                s.title = clip(&prompt, 80);
            }
            if !system {
                s.last_prompt = Some(clip(&prompt, 300));
            }
            // Запрос не от человека посреди хода — ждавшего фоновой работы, уже продолженного
            // ею или начатого новым запросом человека — ход продолжает, а не начинает заново:
            // время хода и уведомление о его конце считаются от запроса человека. Фоновых задач
            // бывает несколько, и о каждой приходит своё уведомление. Без идущего хода и
            // такой запрос начинает новый.
            let active = matches!(before, Status::Working | Status::Waiting);
            if !(system && active && s.turn_started.is_some()) {
                s.turn_started = Some(ctx.now);
            }
            // A person typed the request, so they are at the keyboard and no question is
            // left open. A request from the agent itself leaves a child agent asking.
            if !system {
                s.asks.clear();
            }
            s.accepted = false;
            s.bg = 0;
            match s.asks.values().last() {
                Some(asked) => s.detail = asked.clone(),
                None => {
                    s.status = Status::Working;
                    s.detail.clear();
                }
            }
        }
        Action::Tool => {
            // Инструмент фонового субагента не оживляет законченный ход.
            let own_tool = thread.is_empty();
            // A tool ran, so the thread that ran it got its answer. Another thread may
            // still be waiting for one.
            let answered = s.asks.shift_remove(&thread).is_some();
            if let Some(asked) = s.asks.values().last() {
                s.detail = asked.clone();
            } else if answered && !own_tool && before == Status::Waiting {
                go_on(&mut s);
            } else if matches!(before, Status::Waiting | Status::Idle | Status::Error)
                || (before == Status::Done && own_tool)
            {
                s.status = Status::Working;
                s.detail.clear();
            }
            // Сам Claude Code снова работает: ход, ждавший агентов, продолжился.
            if own_tool {
                s.awaiting = None;
            }
        }
        Action::Ask => {
            let tool = pick(payload, &["tool_name"]).map_or_else(|| "?".to_string(), value_text);
            let tool = clip(&oneline(&tool), 40);
            let detail = if thread.is_empty() {
                tr!("permission: {tool}", "разрешение: {tool}", "дозвіл: {tool}")
            } else {
                let name = asker_name(&s, &thread, payload);
                tr!(
                    "permission for {name}: {tool}",
                    "разрешение для {name}: {tool}",
                    "дозвіл для {name}: {tool}"
                )
            };
            if before != Status::Waiting {
                note_text = Some(waiting_note(&label, who, &detail, place));
            }
            // A child agent that asks after the session ended its turn does not start a
            // new one: the session is done again once the child goes on.
            if !thread.is_empty() && matches!(before, Status::Done | Status::Error | Status::Idle) {
                s.turn_over = true;
            }
            s.asks.insert(thread.clone(), detail.clone());
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
                    note_text = Some(waiting_note(&label, who, &detail, place));
                    s.detail = detail;
                }
                s.status = Status::Waiting;
                s.accepted = false;
            } else if kind == "idle_prompt" && before == Status::Working && !paused {
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
        Action::SubagentDown => {
            s.subagents = (s.subagents - 1).max(0);
            // A child agent that ended its turn is not waiting for an answer.
            if s.asks.shift_remove(&thread).is_some() {
                match s.asks.values().last() {
                    Some(asked) => s.detail = asked.clone(),
                    None if before == Status::Waiting => go_on(&mut s),
                    None => {}
                }
            }
        }
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
            s.detail.clear();
            s.bg = background;
            s.subagents = 0;
            // The session ended its turn, so it does not wait for an answer itself.
            s.asks.shift_remove("");
            let held = held_tasks(payload);
            let holding = ctx.waiting_agents > 0 || !held.is_empty();
            if holding {
                // Claude Code ждёт своей фоновой работы и, когда она кончится, сам продолжит
                // ход: он не окончен, и сообщать о конце рано.
                s.status = Status::Working;
                s.awaiting = Some(ctx.now);
                s.held = i64::try_from(held.len()).unwrap_or(i64::MAX);
                if let Some(first) = held.first() {
                    s.detail = clip(&oneline(first), 80);
                }
            }
            if let Some(asked) = s.asks.values().last() {
                // A child agent still waits for an answer: the session waits with it. The
                // turn of the session itself is over unless it holds for background work,
                // and the session is done once the child agent goes on.
                s.status = Status::Waiting;
                s.detail = asked.clone();
                s.turn_over = !holding;
                if !holding {
                    s.done_at = Some(ctx.now);
                }
            } else if !holding {
                s.status = Status::Done;
                s.done_at = Some(ctx.now);
                s.accepted = false;
            }
            if s.status == Status::Done && lasted >= ctx.notify_min_sec {
                let in_background = if background > 0 {
                    tr!(
                        " · in background: {background}",
                        " · в фоне: {background}",
                        " · у фоні: {background}"
                    )
                } else {
                    String::new()
                };
                let took = dur(lasted);
                note_text = Some(tr!(
                    "✅ {label} · {who} finished the turn ({took}){in_background} · {place}",
                    "✅ {label} · {who} закончил ход ({took}){in_background} · {place}",
                    "✅ {label} · {who} завершив хід ({took}){in_background} · {place}"
                ));
            }
        }
        Action::Fail => {
            let reason = pick(payload, &["error", "message", "reason"])
                .map_or_else(|| tr!("failure", "сбой", "збій"), value_text);
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
                s.detail = tr!("turn interrupted", "ход прерван", "хід перервано");
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

/// Уведомление «агент ждёт тебя».
fn waiting_note(label: &str, who: &str, detail: &str, place: &str) -> String {
    tr!(
        "✋ {label} · {who} is waiting for you · {detail} · {place}",
        "✋ {label} · {who} ждёт тебя · {detail} · {place}",
        "✋ {label} · {who} чекає на тебе · {detail} · {place}"
    )
}

/// The last open permission request of a child agent got its answer: the session goes
/// on as it would have without the request. It is done if its own turn ended while the
/// child agent waited and it has not gone on since (`Session::turn_over`), and working
/// otherwise.
fn go_on(s: &mut Session) {
    s.status = if s.turn_over {
        Status::Done
    } else {
        Status::Working
    };
    s.turn_over = false;
    s.detail.clear();
}

/// What to call the child agent that asks for permission: its name from the list of the
/// session agents (`Newton` for Codex, the agent type for Claude Code), else the role the
/// agent reports, else just "an agent".
fn asker_name(s: &Session, thread: &str, payload: &Map<String, Value>) -> String {
    let known = s
        .agents
        .iter()
        .find(|agent| agent.id == thread)
        .map(|agent| agent.kind.clone());
    let role = text_of(payload, &["agent_type"]);
    let name = known
        .filter(|name| !name.is_empty())
        .or_else(|| (!role.is_empty() && role != "default").then_some(role))
        .unwrap_or_else(|| tr!("an agent", "агента", "агента"));
    clip(&oneline(&name), 30)
}

/// Первое из полей события, которое задано (не `null` и не `false`).
fn pick<'a>(payload: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| payload.get(*key))
        .find(|value| !matches!(value, Value::Null | Value::Bool(false)))
}

/// Фоновые задачи из события конца хода, которые держат ход, — описание каждой.
///
/// Claude Code перечисляет в `background_tasks` всю идущую фоновую работу сессии: когда
/// она кончится, он сам продолжит ход. Агентов среди неё (`type: subagent`) здесь нет:
/// держат ли они ход, решают их файлы (`crate::agents::waiting_for`). Число вместо списка
/// и задачи без типа (старый вид поля) ход не держат: что это за задачи, неизвестно.
fn held_tasks(payload: &Map<String, Value>) -> Vec<String> {
    let Some(Value::Array(tasks)) = payload.get("background_tasks") else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter_map(Value::as_object)
        .filter(|task| {
            task.get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "subagent")
        })
        .map(|task| {
            let described = text_of(task, &["description"]);
            if described.is_empty() {
                text_of(task, &["command", "name", "type"])
            } else {
                described
            }
        })
        .collect()
}

/// Текст первого заданного поля; пустая строка, если не задано ни одно.
fn text_of(payload: &Map<String, Value>, keys: &[&str]) -> String {
    pick(payload, keys).map(value_text).unwrap_or_default()
}

/// Оставляет в идентификаторе только безопасные для имени файла символы.
pub(crate) fn sanitize(raw: &str) -> String {
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
        Err(_) => log(
            &store,
            Some(t!(
                "internal error while handling the event",
                "внутренняя ошибка при обработке события",
                "внутрішня помилка під час обробки події"
            )),
        ),
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
    // Запасной процесс Claude Code сообщает о начале сессии, ещё не получив работы, и может
    // так её и не получить. Строка такой сессии появится с первым настоящим событием.
    if agent == "claude" && action == Action::Start && detect::under_claude_spare() {
        return Ok(());
    }

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

    // Где идёт сессия. Codex 0.169 исполняет хуки своим фоновым процессом, а у того
    // окружение панели, из которой он однажды запустился; поэтому место сессии Codex
    // ищется по процессу его интерфейса — на границах хода и пока оно неизвестно.
    // Хуки сессии из VS Code исполняет сервер расширения: окна терминала у неё нет.
    let codex_host = if agent == "codex" {
        detect::codex_host()
    } else {
        detect::CodexHost::Agent
    };
    let place = match codex_host {
        detect::CodexHost::Editor => "vscode".to_string(),
        detect::CodexHost::Daemon => {
            let stored = before
                .as_ref()
                .map(|session| session.place.clone())
                .filter(|known| !known.is_empty() && known != "?");
            match stored {
                Some(known) if !boundary => known,
                stored => {
                    detect::codex_place(&cwd, stored.as_deref()).unwrap_or_else(|| "?".to_string())
                }
            }
        }
        detect::CodexHost::Agent => detect::place(),
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
            detect::agent_title(&agent, &sid, &transcript, false, &dir, &place, &settings)
        }
    } else {
        detect::agent_title(&agent, &sid, &transcript, true, &dir, &place, &settings)
    };

    // Процесс агента запоминается, чтобы потом, если сессия замолчит, проверить, жив ли он.
    let process = if agent == "claude" {
        procs::claude_process(&sid)
    } else {
        None
    };
    // Конец хода Claude Code: не ждёт ли он своих фоновых агентов.
    let waiting_agents = if agent == "claude" && action == Action::Stop {
        let stale = settings.int("DISP_STALE_MIN", 30) * 60;
        agents::waiting_for(&transcript, config::now(), stale)
    } else {
        0
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
        place,
        now: config::now(),
        notify_min_sec: settings.int("DISP_NOTIFY_MIN_SEC", 120),
        auto_ok_on_exit: settings.get("DISP_AUTO_OK_ON_EXIT") == Some("1"),
        agent_title,
        process,
        transcript,
        waiting_agents,
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
    let body = payload.map_or_else(|| "\"<not JSON>\"".to_string(), Value::to_string);
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
    use crate::model::View;
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

    /// Codex passes the parent session in `session_id` and the child thread in `agent_id`.
    fn codex(now: i64) -> Context {
        Context {
            agent: "codex".to_string(),
            ..ctx(now)
        }
    }

    fn newton() -> Value {
        json!({"tool_name": "exec_command", "agent_id": "child-1", "agent_type": "default"})
    }

    #[test]
    fn child_agent_keeps_the_session_waiting_while_the_parent_works_and_ends() {
        let mut started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(1000),
        );
        started.state.agents = vec![crate::model::SubAgent {
            id: "child-1".into(),
            kind: "Newton".into(),
            what: "nested_qa".into(),
            running: true,
            ..Default::default()
        }];
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            newton(),
            &codex(1100),
        );
        assert_eq!(asked.state.status, Status::Waiting);
        assert_eq!(asked.state.detail, "разрешение для Newton: exec_command");
        assert!(asked.note.is_some_and(|note| note.text.contains("Newton")));

        // The parent goes on with its own tools: the child still waits.
        let parent = json!({"tool_name": "exec_command"});
        let busy = apply(Some(&asked.state), "PostToolUse", parent, &codex(1102));
        assert_eq!(busy.state.status, Status::Waiting);
        assert_eq!(busy.state.detail, "разрешение для Newton: exec_command");
        assert!(busy.event.is_none());

        // The parent ends its turn: still waiting, and no "finished" notification.
        let stop = json!({"last_assistant_message": "готово"});
        let ended = apply(Some(&busy.state), "Stop", stop, &codex(1300));
        assert_eq!(ended.state.status, Status::Waiting);
        assert_eq!(ended.state.done_at, Some(1300));
        assert!(ended.note.is_none());
        assert!(ended.event.is_none());

        // The child got its answer and ran the command: the turn of the session is over.
        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        let answered = apply(Some(&ended.state), "PostToolUse", child, &codex(1310));
        assert_eq!(answered.state.status, Status::Done);
        assert!(answered.state.asks.is_empty());
        assert!(answered.state.detail.is_empty());
        assert!(answered.note.is_none());
        assert_eq!(
            answered.event.map(|event| (event.from, event.to)),
            Some(("waiting".into(), "done".into()))
        );
    }

    /// A parent that asked nothing itself, a child agent with an open request, and the
    /// parent's turn ended at `stop`.
    fn ended_while_child_asks(start: i64, stop: i64) -> Session {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(start),
        );
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            newton(),
            &codex(start),
        );
        let ended = apply(Some(&asked.state), "Stop", json!({}), &codex(stop));
        assert_eq!(ended.state.status, Status::Waiting);
        assert!(ended.state.turn_over);
        ended.state
    }

    #[test]
    fn parent_that_goes_on_after_its_stop_is_working_when_the_child_is_answered() {
        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        // The parent runs its own tool after the Stop.
        let ended = ended_while_child_asks(1000, 1300);
        let own = json!({"tool_name": "exec_command"});
        let resumed = apply(Some(&ended), "PostToolUse", own, &codex(1400));
        assert_eq!(resumed.state.status, Status::Waiting);
        assert!(!resumed.state.turn_over);
        let answered = apply(
            Some(&resumed.state),
            "PostToolUse",
            child.clone(),
            &codex(1410),
        );
        assert_eq!(answered.state.status, Status::Working);

        // The agent continues the turn itself with a task notification.
        let system = json!({
            "prompt": "<task-notification>done</task-notification>",
            "origin": "system"
        });
        let resumed = apply(Some(&ended), "UserPromptSubmit", system, &codex(1400));
        assert_eq!(resumed.state.status, Status::Waiting);
        assert_eq!(resumed.state.turn_started, Some(1000));
        let answered = apply(Some(&resumed.state), "PostToolUse", child, &codex(1410));
        assert_eq!(answered.state.status, Status::Working);
    }

    #[test]
    fn order_within_one_second_does_not_depend_on_timestamps() {
        // The turn starts and ends in the same second: it is over.
        let ended = ended_while_child_asks(1000, 1000);
        let gone = json!({"agent_id": "child-1", "agent_type": "default"});
        let stopped = apply(Some(&ended), "SubagentStop", gone, &codex(1010));
        assert_eq!(stopped.state.status, Status::Done);
        assert!(!stopped.state.turn_over);

        // A turn ends, and the next one starts in the same second: it is not over.
        let first = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(900),
        );
        let done = apply(Some(&first.state), "Stop", json!({}), &codex(1000));
        let next = apply(
            Some(&done.state),
            "UserPromptSubmit",
            json!({"prompt": "y"}),
            &codex(1000),
        );
        let asked = apply(
            Some(&next.state),
            "PermissionRequest",
            newton(),
            &codex(1000),
        );
        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        let answered = apply(Some(&asked.state), "PostToolUse", child, &codex(1000));
        assert_eq!(answered.state.status, Status::Working);
    }

    #[test]
    fn child_agent_answer_during_the_parent_turn_goes_back_to_working() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(1000),
        );
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            newton(),
            &codex(1100),
        );
        // Neither the list of agents nor a role names it.
        assert_eq!(asked.state.detail, "разрешение для агента: exec_command");
        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        let answered = apply(Some(&asked.state), "PostToolUse", child, &codex(1110));
        assert_eq!(answered.state.status, Status::Working);
        assert!(answered.state.detail.is_empty());
    }

    #[test]
    fn every_thread_that_asked_must_go_on() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(1000),
        );
        let first = apply(
            Some(&started.state),
            "PermissionRequest",
            newton(),
            &codex(1100),
        );
        let other =
            json!({"tool_name": "apply_patch", "agent_id": "child-2", "agent_type": "explorer"});
        let both = apply(Some(&first.state), "PermissionRequest", other, &codex(1101));
        assert_eq!(both.state.detail, "разрешение для explorer: apply_patch");
        assert!(both.note.is_none(), "the session already waits");

        // The second child is done with its turn without running the tool.
        let gone = json!({"agent_id": "child-2", "agent_type": "explorer"});
        let one = apply(Some(&both.state), "SubagentStop", gone, &codex(1102));
        assert_eq!(one.state.status, Status::Waiting);
        assert_eq!(one.state.detail, "разрешение для агента: exec_command");

        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        let none = apply(Some(&one.state), "PostToolUse", child, &codex(1103));
        assert_eq!(none.state.status, Status::Working);
    }

    #[test]
    fn child_agent_that_ends_its_turn_stops_waiting() {
        let done = Session {
            status: Status::Done,
            session_id: "s1".into(),
            turn_started: Some(900),
            done_at: Some(950),
            ..Session::default()
        };
        // A child agent asks after the parent ended its turn.
        let asked = apply(Some(&done), "PermissionRequest", newton(), &codex(1000));
        assert_eq!(asked.state.status, Status::Waiting);
        let gone = json!({"agent_id": "child-1", "agent_type": "default"});
        let stopped = apply(Some(&asked.state), "SubagentStop", gone, &codex(1010));
        assert_eq!(stopped.state.status, Status::Done);
        assert_eq!(stopped.state.done_at, Some(950));
    }

    #[test]
    fn a_person_typing_closes_open_questions() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(1000),
        );
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            newton(),
            &codex(1100),
        );
        // A request from the agent itself does not answer the child agent.
        let system = json!({"prompt": "<task-notification>done</task-notification>"});
        let resumed = apply(Some(&asked.state), "UserPromptSubmit", system, &ctx(1110));
        assert_eq!(resumed.state.status, Status::Waiting);
        assert_eq!(resumed.state.asks.len(), 1);
        let typed = apply(
            Some(&resumed.state),
            "UserPromptSubmit",
            json!({"prompt": "y"}),
            &codex(1120),
        );
        assert_eq!(typed.state.status, Status::Working);
        assert!(typed.state.asks.is_empty());
    }

    #[test]
    fn own_question_is_answered_by_own_tool() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "x"}),
            &codex(1000),
        );
        let asked = apply(
            Some(&started.state),
            "PermissionRequest",
            json!({"tool_name": "exec_command"}),
            &codex(1100),
        );
        assert_eq!(asked.state.detail, "разрешение: exec_command");
        // A child agent's tool does not answer the session's own question.
        let child = json!({"tool_name": "exec_command", "agent_id": "child-1"});
        let still = apply(Some(&asked.state), "PostToolUse", child, &codex(1101));
        assert_eq!(still.state.status, Status::Waiting);
        let own = apply(
            Some(&still.state),
            "PostToolUse",
            json!({"tool_name": "exec_command"}),
            &codex(1102),
        );
        assert_eq!(own.state.status, Status::Working);
        assert!(own.state.asks.is_empty());
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
    fn turn_waiting_for_background_agents_is_not_over() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "собери"}),
            &ctx(1000),
        );
        let mut waiting = ctx(1300);
        waiting.waiting_agents = 2;
        let stop = json!({"last_assistant_message": "жду агентов"});
        let held = apply(Some(&started.state), "Stop", stop.clone(), &waiting);
        assert_eq!(held.state.status, Status::Working);
        assert_eq!(
            (held.state.done_at, held.state.awaiting),
            (None, Some(1300))
        );
        // Инструмент фонового агента ожидание не прерывает, а свой инструмент Claude Code — да.
        let agent_tool = json!({"tool_name": "Read", "agent_id": "a1"});
        let still = apply(Some(&held.state), "PostToolUse", agent_tool, &ctx(1400));
        assert_eq!(still.state.awaiting, Some(1300));
        let own = apply(
            Some(&held.state),
            "PostToolUse",
            json!({"tool_name": "Read"}),
            &ctx(1400),
        );
        assert_eq!(own.state.awaiting, None);
        assert_eq!(held.state.last_reply.as_deref(), Some("жду агентов"));
        assert!(held.note.is_none(), "о конце хода сообщать рано");
        // Агенты закончили, Claude Code продолжил ход и закончил его сам.
        let done = apply(Some(&held.state), "Stop", stop, &ctx(1600));
        assert_eq!(done.state.status, Status::Done);
        assert_eq!(done.state.awaiting, None);
        assert!(done.note.is_some_and(|note| note.text.contains("10м")));
    }

    #[test]
    fn turn_waiting_for_a_background_command_is_paused() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "Прогони проверки и закоммить"}),
            &ctx(1000),
        );
        let tasks = json!([
            {"id": "b2v6", "type": "shell", "status": "running", "description": "Full checks for build 0.1.094", "command": "make test"},
            {"id": "a1", "type": "subagent", "status": "running", "description": "Survey wiki", "agent_type": "Explore"},
        ]);
        let held = apply(
            Some(&started.state),
            "Stop",
            json!({"last_assistant_message": "Идёт прогон, после него закоммичу", "background_tasks": tasks}),
            &ctx(1300),
        );
        assert_eq!(held.state.status, Status::Working);
        assert_eq!(
            (held.state.awaiting, held.state.held, held.state.bg),
            (Some(1300), 1, 2)
        );
        assert_eq!(held.state.detail, "Full checks for build 0.1.094");
        assert_eq!(held.state.view(1300 + 3600, 30), View::Background);
        assert!(held.note.is_none(), "о конце хода сообщать рано");

        // Claude Code минуту ждёт ввода и напоминает об этом: ход всё ещё ждёт команду.
        let idle = apply(
            Some(&held.state),
            "Notification",
            json!({"notification_type": "idle_prompt", "message": "Claude is waiting for your input"}),
            &ctx(1360),
        );
        assert_eq!(
            (idle.state.status, idle.state.awaiting),
            (Status::Working, Some(1300))
        );
        assert!(idle.note.is_none());

        // Команда доработала: Claude Code сам продолжает ход уведомлением о задаче.
        let resumed = apply(
            Some(&idle.state),
            "UserPromptSubmit",
            json!({"prompt": "<task-notification>\n<task-id>b2v6</task-id>\n<status>completed</status>\n</task-notification>", "origin": "system"}),
            &ctx(1540),
        );
        assert_eq!(resumed.state.status, Status::Working);
        assert_eq!((resumed.state.awaiting, resumed.state.held), (None, 0));
        assert_eq!(resumed.state.turn_started, Some(1000), "ход продолжается");
        assert_eq!(
            resumed.state.last_prompt.as_deref(),
            Some("Прогони проверки и закоммить")
        );
        assert_eq!(resumed.state.title, "Прогони проверки и закоммить");
        // Доработала вторая задача, а Claude Code ещё не закончил ответ: ожидания уже нет,
        // но ход тот же.
        let second = apply(
            Some(&resumed.state),
            "UserPromptSubmit",
            json!({"prompt": "<task-notification>\n<task-id>b3k9</task-id>\n</task-notification>", "origin": "system"}),
            &ctx(1570),
        );
        assert_eq!(
            second.state.turn_started,
            Some(1000),
            "и после второго уведомления"
        );

        let done = apply(
            Some(&second.state),
            "Stop",
            json!({"last_assistant_message": "Закоммитил", "background_tasks": []}),
            &ctx(1600),
        );
        assert_eq!(done.state.status, Status::Done);
        assert!(done.note.is_some_and(|note| note.text.contains("(10м)")));
    }

    #[test]
    fn a_new_request_while_paused_starts_a_new_turn() {
        let started = apply(
            None,
            "UserPromptSubmit",
            json!({"prompt": "подними сервер"}),
            &ctx(1000),
        );
        let server =
            json!([{"id": "b1", "type": "shell", "status": "running", "command": "npm run dev"}]);
        let held = apply(
            Some(&started.state),
            "Stop",
            json!({"background_tasks": server}),
            &ctx(1100),
        );
        assert_eq!(
            (held.state.held, held.state.detail.as_str()),
            (1, "npm run dev")
        );
        let asked = apply(
            Some(&held.state),
            "UserPromptSubmit",
            json!({"prompt": "теперь поправь заголовок", "origin": "user"}),
            &ctx(1200),
        );
        assert_eq!((asked.state.awaiting, asked.state.held), (None, 0));
        assert_eq!(asked.state.turn_started, Some(1200));
        assert_eq!(
            asked.state.last_prompt.as_deref(),
            Some("теперь поправь заголовок")
        );
        // Посреди нового хода пришёл результат старой фоновой задачи: ход тот же.
        let late = apply(
            Some(&asked.state),
            "UserPromptSubmit",
            json!({"prompt": "<task-notification>\n<task-id>b0</task-id>\n</task-notification>"}),
            &ctx(1250),
        );
        assert_eq!(late.state.turn_started, Some(1200));
        assert_eq!(
            late.state.last_prompt.as_deref(),
            Some("теперь поправь заголовок")
        );
        // Ход окончен, а потом пришло уведомление: это уже новый ход.
        let finished = apply(Some(&late.state), "Stop", json!({}), &ctx(1260));
        let woken = apply(
            Some(&finished.state),
            "UserPromptSubmit",
            json!({"prompt": "<task-notification>\n<task-id>b0</task-id>\n</task-notification>"}),
            &ctx(1290),
        );
        assert_eq!(
            (woken.state.status, woken.state.turn_started),
            (Status::Working, Some(1290))
        );

        // Старый вид поля — число задач или задачи без типа — ход не держит, как и агент
        // без файлов, по которым видно, что он держит ход.
        for tasks in [
            json!(1),
            json!([{"id": "b1"}]),
            json!([{"id": "a1", "type": "subagent"}]),
        ] {
            let counted = apply(
                Some(&asked.state),
                "Stop",
                json!({"background_tasks": tasks}),
                &ctx(1300),
            );
            assert_eq!((counted.state.status, counted.state.bg), (Status::Done, 1));
        }
        // Без ожидания напоминание о простое завершает работающий ход, как и раньше.
        let idle = apply(
            Some(&asked.state),
            "Notification",
            json!({"notification_type": "idle_prompt"}),
            &ctx(1300),
        );
        assert_eq!(idle.state.status, Status::Done);
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
