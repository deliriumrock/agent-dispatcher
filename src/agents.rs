//! Агенты, которых запускает сессия: субагенты Claude Code и дочерние агенты Codex.
//!
//! Claude Code кладёт рядом с транскриптом сессии каталог `<сессия>/subagents/`. В нём на
//! каждого агента `agent-<id>.meta.json` (тип, описание, фоновый ли, а по окончании —
//! `"finished": true`) и транскрипт агента `agent-<id>.jsonl`. Codex пишет журнал
//! дочернего агента как журнал отдельной сессии со ссылкой на родительскую
//! (`observe::codex_children`).
//!
//! Сверка (`observe::sync`) раз в несколько секунд обновляет у открытых сессий список
//! агентов: те, что работали в текущем ходе, и те, что ещё работают. Хук на конце хода
//! Claude Code смотрит, не ждёт ли тот своих фоновых агентов (`waiting_for`): тогда ход
//! не окончен. Когда агенты закончат, Claude Code сам продолжит ход; если же он молчит,
//! а его транскрипт кончается концом ответа, ход завершает сверка (`finish`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::{self, Settings, env_nonempty};
use crate::hook::{self, Action, Context};
use crate::model::{Session, Status, SubAgent};
use crate::notify;
use crate::observe;
use crate::store::{self, Store};

/// Сколько законченных агентов одной сессии помнить: у ревью кода их бывает несколько
/// десятков. Работающие помнятся все.
const KEEP: usize = 20;
/// Сколько секунд после конца последнего фонового агента ждать, что Claude Code сам
/// продолжит ход, прежде чем завершить его сверкой.
const GRACE_SEC: i64 = 60;
/// Сколько байт с конца транскрипта агента просматривать в поисках его последней записи.
const LOG_TAIL: u64 = 64 * 1024;
/// То же для транскрипта самой сессии: после конца ответа Claude Code дописывает в него
/// служебные записи на десятки килобайт.
const TRANSCRIPT_TAIL: u64 = 512 * 1024;

/// Обновляет списки агентов у открытых сессий на этой машине.
pub fn sweep(store: &Store) {
    let settings = Settings::load(&store.paths.conf_file);
    sweep_at(store, &settings, config::now(), &codex_home());
}

fn sweep_at(store: &Store, settings: &Settings, now: i64, codex_home: &Path) {
    let stale = settings.int("DISP_STALE_MIN", 30) * 60;
    let open: Vec<(PathBuf, Session)> = store
        .all()
        .into_iter()
        .filter(|(_, session)| tracked(session))
        .collect();
    let parents: HashMap<String, i64> = open
        .iter()
        .filter(|(_, session)| session.agent == "codex")
        .map(|(_, session)| (session.session_id.clone(), turn_start(session)))
        .collect();
    let mut codex = if parents.is_empty() {
        HashMap::new()
    } else {
        observe::codex_children(&codex_home.join("sessions"), &parents, now, stale)
    };
    for (file, session) in open {
        let found = match session.agent.as_str() {
            "claude" => match session.transcript.as_deref() {
                Some(transcript) if !transcript.is_empty() => {
                    claude_found(Path::new(transcript), now, stale)
                }
                _ => continue,
            },
            "codex" => codex
                .remove(&session.session_id)
                .unwrap_or_default()
                .into_iter()
                .map(|agent| Found {
                    agent,
                    marked: false,
                })
                .collect(),
            _ => continue,
        };
        if held_too_long(&session, &found, now) {
            finish(store, settings, &file, &session, now, stale);
        }
        let agents = keep(
            found.into_iter().map(|found| found.agent).collect(),
            turn_start(&session),
        );
        if agents == session.agents {
            continue;
        }
        let _lock = store.lock();
        // Под блокировкой перечитываем: сессию мог только что обновить хук.
        if let Some(mut fresh) = store::read_session(&file).filter(tracked) {
            fresh.agents = agents;
            // Не удалось записать — попробуем на следующей сверке.
            let _ = store::write_session(&file, &fresh);
        }
    }
}

/// Агент, каким его видно по файлам, и ставит ли ему Claude Code отметку об окончании.
#[derive(Clone, Debug)]
struct Found {
    agent: SubAgent,
    marked: bool,
}

/// Ход ждал фоновых агентов, они закончили, а Claude Code уже `GRACE_SEC` молчит и
/// сам ход не продолжил.
///
/// Хуков, по которым видно, что Claude Code продолжил ход, может не быть долго: о начале
/// своего инструмента он не сообщает, а о конце сообщит, только когда, например, тесты
/// доработают. Поэтому молчание проверяется и по транскрипту сессии: он не менялся
/// `GRACE_SEC`, и последняя запись разговора в нём — конец ответа, а не вызов
/// инструмента или начатый ответ.
fn held_too_long(session: &Session, found: &[Found], now: i64) -> bool {
    if session.awaiting.is_none() || session.status != Status::Working {
        return false;
    }
    if found.iter().any(Found::holds) {
        return false;
    }
    let Some(transcript) = session
        .transcript
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(Path::new)
    else {
        return false;
    };
    let quiet_since = found
        .iter()
        .filter(|found| found.agent.background)
        .map(|found| found.agent.updated)
        .chain(modified(transcript))
        .fold(session.updated, i64::max);
    now - quiet_since >= GRACE_SEC && answered(transcript, TRANSCRIPT_TAIL)
}

/// Завершает ход, который ждал фоновых агентов, как это сделал бы Stop от Claude Code:
/// статус «готово» и уведомление о конце хода. Возвращает, завершён ли ход.
///
/// Пока сверка ждала блокировку, Claude Code мог продолжить ход. Начало своего
/// инструмента он пишет только в транскрипт, хука нет, так что одной записи сессии мало:
/// под блокировкой заново проверяется всё, на чём сверка основала решение (`held_too_long`
/// по свежим файлам агентов и транскрипта), и что сессию никто не трогал с тех пор, как её
/// видела сверка.
fn finish(
    store: &Store,
    settings: &Settings,
    file: &Path,
    seen: &Session,
    now: i64,
    stale: i64,
) -> bool {
    let lock = store.lock();
    let Some(old) = store::read_session(file) else {
        return false;
    };
    let found = old
        .transcript
        .as_deref()
        .filter(|path| !path.is_empty())
        .map(|transcript| claude_found(Path::new(transcript), now, stale))
        .unwrap_or_default();
    if old.updated != seen.updated || !held_too_long(&old, &found, now) {
        return false;
    }
    let ctx = Context {
        agent: old.agent.clone(),
        sid: old.session_id.clone(),
        place: old.place.clone(),
        now,
        notify_min_sec: settings.int("DISP_NOTIFY_MIN_SEC", 120),
        ..Context::default()
    };
    let outcome = hook::step(Some(&old), &serde_json::Map::new(), Action::Stop, &ctx);
    if store::write_session(file, &outcome.state).is_err() {
        return false;
    }
    if let Some(event) = &outcome.event {
        let _ = store.append_event(event);
    }
    drop(lock);
    if let Some(note) = &outcome.note {
        notify::send(note, settings);
    }
    true
}

impl Found {
    /// Держит ли агент ход родителя: фоновый, работает, и Claude Code ставит ему отметку
    /// об окончании. Остальным (агентам воркфлоу, старым версиям) отметку не ставят, и
    /// их «работает» — только догадка по свежести журнала: ход по ней не держим.
    fn holds(&self) -> bool {
        self.marked && self.agent.background && self.agent.running
    }
}

/// Сессия, за агентами которой следим: открытая и не в контейнере.
fn tracked(session: &Session) -> bool {
    !session.ended && !session.accepted && session.container.is_none()
}

/// Начало текущего хода сессии, а без него — начало самой сессии.
fn turn_start(session: &Session) -> i64 {
    session.turn_started.unwrap_or(session.started)
}

/// Агенты для показа: работающие и те, что работали с начала хода `turn`, по времени
/// запуска. Работающие остаются все: по ним видно, что работа идёт. Законченных — не
/// больше, чем помещается до `KEEP`, раньше уходят старые.
fn keep(mut found: Vec<SubAgent>, turn: i64) -> Vec<SubAgent> {
    found.retain(|agent| agent.running || agent.updated >= turn);
    found.sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
    let running = found.iter().filter(|agent| agent.running).count();
    let finished = found.len() - running;
    let mut extra = finished.saturating_sub(KEEP.saturating_sub(running));
    found.retain(|agent| {
        let drop = !agent.running && extra > 0;
        if drop {
            extra -= 1;
        }
        !drop
    });
    found
}

/// Сколько фоновых агентов Claude Code держат ход сессии с транскриптом `transcript`
/// (см. `Found::holds`). Пока они работают, ход не окончен: когда они закончат, Claude
/// Code продолжит его сам.
pub fn waiting_for(transcript: &str, now: i64, stale: i64) -> usize {
    if transcript.is_empty() {
        return 0;
    }
    claude_found(Path::new(transcript), now, stale)
        .iter()
        .filter(|found| found.holds())
        .count()
}

/// Субагенты сессии Claude Code с транскриптом `transcript`.
pub fn claude(transcript: &Path, now: i64, stale: i64) -> Vec<SubAgent> {
    claude_found(transcript, now, stale)
        .into_iter()
        .map(|found| found.agent)
        .collect()
}

/// Субагенты сессии Claude Code. Агент без отметки об окончании считается работающим,
/// пока его последняя запись — не конец ответа и транскрипт менялся не дольше `stale`
/// секунд назад: так агенты, которым отметку не ставят, и упавший Claude Code не
/// оставляют вечно работающих агентов.
fn claude_found(transcript: &Path, now: i64, stale: i64) -> Vec<Found> {
    let dir = transcript.with_extension("").join("subagents");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let id = name
                .to_str()?
                .strip_prefix("agent-")?
                .strip_suffix(".meta.json")?
                .to_string();
            claude_agent(&dir, id, now, stale)
        })
        .collect()
}

/// Субагент Claude Code по его файлам в каталоге `dir`.
fn claude_agent(dir: &Path, id: String, now: i64, stale: i64) -> Option<Found> {
    let meta_path = dir.join(format!("agent-{id}.meta.json"));
    let meta: Value = serde_json::from_slice(&fs::read(&meta_path).ok()?).ok()?;
    let text = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let log = dir.join(format!("agent-{id}.jsonl"));
    let updated = modified(&log).or_else(|| modified(&meta_path)).unwrap_or(0);
    let started = created(&log)
        .or_else(|| created(&meta_path))
        .unwrap_or(updated);
    let finished = meta
        .get("finished")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let running = !finished && now - updated <= stale && !answered(&log, LOG_TAIL);
    let agent = SubAgent {
        kind: text("agentType").unwrap_or_default(),
        what: text("description")
            .or_else(|| text("name"))
            .unwrap_or_default(),
        background: text("requestShape").as_deref() == Some("background"),
        running,
        started,
        updated,
        id,
    };
    // Отметку об окончании Claude Code ставит агентам, запущенным инструментом Agent:
    // у них в meta.json есть `toolUseId`.
    let marked = meta.get("toolUseId").is_some_and(Value::is_string);
    Some(Found { agent, marked })
}

/// Закончен ли ответ: последняя запись разговора в последних `tail` байтах транскрипта
/// `log` — ответ модели с `stop_reason: end_turn`, после которого ничего не пришло.
fn answered(log: &Path, tail: u64) -> bool {
    let (lines, _) = observe::tail_lines(log, tail);
    for line in lines.iter().rev() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let stop = record
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str);
                return stop == Some("end_turn");
            }
            Some("user") => return false,
            _ => {}
        }
    }
    false
}

/// Каталог Codex: `$CODEX_HOME` или `~/.codex`.
fn codex_home() -> PathBuf {
    env_nonempty("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env_nonempty("HOME").unwrap_or_default()).join(".codex"))
}

/// Время изменения файла, unix-секунды.
pub(crate) fn modified(path: &Path) -> Option<i64> {
    unix(fs::metadata(path).ok()?.modified().ok()?)
}

/// Время создания файла, unix-секунды, если файловая система его хранит.
pub(crate) fn created(path: &Path) -> Option<i64> {
    unix(fs::metadata(path).ok()?.created().ok()?)
}

fn unix(time: SystemTime) -> Option<i64> {
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    /// Каталог с файлами во временном каталоге системы.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("disp-agents-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            assert!(fs::create_dir_all(&dir).is_ok());
            Scratch(dir)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                assert!(fs::create_dir_all(parent).is_ok());
            }
            assert!(fs::write(&path, text).is_ok());
            path
        }

        /// Хранилище в этом каталоге; уведомления не отправляются.
        fn store(&self) -> (Store, Settings) {
            let conf = self.write("env", "DISP_DESKTOP=0\n");
            let state_dir = self.0.join("state");
            let store = Store::new(Paths {
                sessions: state_dir.join("sessions"),
                state_dir,
                conf_file: conf.clone(),
            });
            assert!(store.ensure_dirs().is_ok());
            (store, Settings::load(&conf))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const ANSWER: &str = r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#;
    const TOOL: &str = r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#;

    /// Транскрипт сессии `s1`, которая закончила ответ, и её субагенты: `(id, meta.json,
    /// последняя запись журнала)`.
    fn claude_session(scratch: &Scratch, agents: &[(&str, &str, &str)]) -> PathBuf {
        let transcript = scratch.write(
            "proj/s1.jsonl",
            &format!("{{\"type\":\"user\"}}\n{ANSWER}\n"),
        );
        for (id, meta, last) in agents {
            scratch.write(&format!("proj/s1/subagents/agent-{id}.meta.json"), meta);
            scratch.write(
                &format!("proj/s1/subagents/agent-{id}.jsonl"),
                &format!("{{\"type\":\"user\"}}\n{last}\n"),
            );
        }
        scratch.write("proj/s1/subagents/agent-bad.meta.json", "{не json");
        transcript
    }

    /// Фоновый агент инструмента Agent работает; обычный закончил; агент воркфлоу без
    /// отметок работает, а такой же, уже ответивший, — нет.
    const MIXED: [(&str, &str, &str); 4] = [
        (
            "a1",
            r#"{"agentType":"general-purpose","description":"Survey wiki","requestShape":"background","toolUseId":"toolu_1"}"#,
            TOOL,
        ),
        (
            "a2",
            r#"{"agentType":"Explore","description":"Find icons","requestShape":"foreground","toolUseId":"toolu_2","finished":true}"#,
            ANSWER,
        ),
        (
            "a3",
            r#"{"agentType":"general-purpose","name":"/code-review 61","requestShape":"background"}"#,
            TOOL,
        ),
        (
            "a4",
            r#"{"agentType":"general-purpose","name":"/code-review 62","requestShape":"background"}"#,
            ANSWER,
        ),
    ];

    #[test]
    fn claude_agents_come_from_their_meta_files() {
        let scratch = Scratch::new("claude");
        let transcript = claude_session(&scratch, &MIXED);
        let now = modified(&transcript).unwrap_or(0);
        let mut agents = claude(&transcript, now, 1800);
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        let summary: Vec<(&str, &str, &str, bool, bool)> = agents
            .iter()
            .map(|a| {
                (
                    a.id.as_str(),
                    a.kind.as_str(),
                    a.what.as_str(),
                    a.background,
                    a.running,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("a1", "general-purpose", "Survey wiki", true, true),
                ("a2", "Explore", "Find icons", false, false),
                ("a3", "general-purpose", "/code-review 61", true, true),
                ("a4", "general-purpose", "/code-review 62", true, false),
            ]
        );
        // Ход держит только агент, которому Claude Code поставит отметку об окончании.
        let path = transcript.to_string_lossy();
        assert_eq!(waiting_for(&path, now, 1800), 1);
        // Давно молчащий агент уже не работает.
        assert_eq!(waiting_for(&path, now + 3600, 1800), 0);
        assert_eq!(waiting_for("", now, 1800), 0);
        assert!(claude(&scratch.0.join("proj/none.jsonl"), now, 1800).is_empty());
    }

    #[test]
    fn list_keeps_this_turn_and_every_running_agent() {
        let agent = |id: &str, running: bool, started: i64, updated: i64| SubAgent {
            id: id.to_string(),
            running,
            started,
            updated,
            ..SubAgent::default()
        };
        let found = vec![
            agent("late", false, 300, 400),
            agent("old", false, 10, 20),
            agent("long", true, 5, 30),
            agent("early", false, 150, 250),
        ];
        let ids: Vec<String> = keep(found, 200).into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["long", "early", "late"]);

        // Работающий агент среди двадцати законченных, запущенных в ту же секунду.
        let mut crowd: Vec<SubAgent> = (1..=20)
            .map(|n| agent(&format!("{n:03}-done"), false, 100, 150))
            .collect();
        crowd.push(agent("000-running", true, 100, 150));
        let kept = keep(crowd, 0);
        assert_eq!(kept.len(), KEEP);
        assert!(kept.iter().any(|a| a.id == "000-running"));
        assert_eq!(kept.iter().filter(|a| !a.running).count(), KEEP - 1);

        // Работающих больше лимита — остаются все.
        let many: Vec<SubAgent> = (0..30)
            .map(|n| agent(&format!("{n:02}"), true, n, n))
            .collect();
        assert_eq!(keep(many, 0).len(), 30);
    }

    fn held_session(transcript: &Path, now: i64) -> Session {
        Session {
            agent: "claude".to_string(),
            session_id: "s1".to_string(),
            status: Status::Working,
            place: "zellij:dev#4".to_string(),
            started: now - 900,
            turn_started: Some(now - 900),
            updated: now - 600,
            awaiting: Some(now - 600),
            transcript: Some(transcript.to_string_lossy().into_owned()),
            ..Session::default()
        }
    }

    #[test]
    fn held_turn_ends_when_agents_are_done_and_claude_is_silent() {
        let scratch = Scratch::new("held");
        let (store, settings) = scratch.store();
        let finished = r#"{"agentType":"general-purpose","description":"Survey wiki","requestShape":"background","toolUseId":"toolu_1","finished":true}"#;
        let transcript = claude_session(&scratch, &[("a1", finished, ANSWER)]);
        let now = modified(&transcript).unwrap_or(0) + GRACE_SEC;
        let file = store.session_file("claude", "s1");
        assert!(store::write_session(&file, &held_session(&transcript, now)).is_ok());

        sweep_at(&store, &settings, now, &scratch.0.join("codex"));
        let session = store::read_session(&file).unwrap_or_default();
        assert_eq!(session.status, Status::Done);
        assert_eq!((session.done_at, session.awaiting), (Some(now), None));
        assert_eq!(session.place, "zellij:dev#4");
        assert_eq!(session.agents.len(), 1);
    }

    #[test]
    fn held_turn_waits_for_running_agents_and_for_claude() {
        let scratch = Scratch::new("hold");
        let (store, settings) = scratch.store();
        let running =
            r#"{"agentType":"general-purpose","requestShape":"background","toolUseId":"toolu_1"}"#;
        let transcript = claude_session(&scratch, &[("a1", running, TOOL)]);
        let now = modified(&transcript).unwrap_or(0) + GRACE_SEC;
        let file = store.session_file("claude", "s1");
        assert!(store::write_session(&file, &held_session(&transcript, now)).is_ok());
        sweep_at(&store, &settings, now, &scratch.0.join("codex"));
        let status = |file: &Path| store::read_session(file).map(|s| s.status);
        assert_eq!(status(&file), Some(Status::Working));

        // Агент закончил только что: Claude Code ещё может продолжить ход сам.
        let finished = r#"{"agentType":"general-purpose","requestShape":"background","toolUseId":"toolu_1","finished":true}"#;
        claude_session(&scratch, &[("a1", finished, ANSWER)]);
        let just = modified(&transcript).unwrap_or(0) + 5;
        assert!(store::write_session(&file, &held_session(&transcript, just)).is_ok());
        sweep_at(&store, &settings, just, &scratch.0.join("codex"));
        assert_eq!(status(&file), Some(Status::Working));
    }

    /// Ставит файлу `path` время изменения `secs`, unix-секунды.
    fn stamp(path: &Path, secs: i64) {
        let time = UNIX_EPOCH + std::time::Duration::from_secs(secs.unsigned_abs());
        let file = fs::File::options().write(true).open(path);
        assert!(file.and_then(|file| file.set_modified(time)).is_ok());
    }

    #[test]
    fn held_turn_is_not_ended_while_claude_works_on_it() {
        let scratch = Scratch::new("resumed");
        let (store, settings) = scratch.store();
        let finished = r#"{"agentType":"general-purpose","requestShape":"background","toolUseId":"toolu_1","finished":true}"#;
        let transcript = claude_session(&scratch, &[("a1", finished, ANSWER)]);
        let base = modified(&transcript).unwrap_or(0);
        let file = store.session_file("claude", "s1");
        let status =
            |file: &Path| store::read_session(file).map(|s| (s.status, s.awaiting.is_some()));

        // Агент закончил, Claude Code продолжил ход и запустил долгую команду: хуков нет,
        // пока она не доработает.
        scratch.write(
            "proj/s1.jsonl",
            &format!("{{\"type\":\"user\"}}\n{ANSWER}\n{TOOL}\n"),
        );
        stamp(&transcript, base);
        let now = base + 10 * GRACE_SEC;
        assert!(store::write_session(&file, &held_session(&transcript, now)).is_ok());
        sweep_at(&store, &settings, now, &scratch.0.join("codex"));
        assert_eq!(status(&file), Some((Status::Working, true)));

        // Пишет ответ: последняя запись — конец ответа, но транскрипт только что менялся.
        scratch.write(
            "proj/s1.jsonl",
            &format!("{{\"type\":\"user\"}}\n{TOOL}\n{ANSWER}\n"),
        );
        stamp(&transcript, now - 5);
        sweep_at(&store, &settings, now, &scratch.0.join("codex"));
        assert_eq!(status(&file), Some((Status::Working, true)));

        // Ответ закончен, Stop не пришёл, Claude Code молчит: ход завершает сверка.
        let later = now - 5 + GRACE_SEC;
        sweep_at(&store, &settings, later, &scratch.0.join("codex"));
        assert_eq!(status(&file), Some((Status::Done, false)));
    }

    #[test]
    fn held_turn_resumed_while_the_sweep_waited_is_kept() {
        let scratch = Scratch::new("race");
        let (store, settings) = scratch.store();
        let finished = r#"{"agentType":"general-purpose","requestShape":"background","toolUseId":"toolu_1","finished":true}"#;
        let transcript = claude_session(&scratch, &[("a1", finished, ANSWER)]);
        let now = modified(&transcript).unwrap_or(0) + GRACE_SEC;
        let file = store.session_file("claude", "s1");
        let seen = held_session(&transcript, now);
        assert!(store::write_session(&file, &seen).is_ok());
        let status = |file: &Path| store::read_session(file).map(|s| s.status);

        // Сверка решила, что Claude Code молчит.
        let found = claude_found(&transcript, now, 1800);
        assert!(held_too_long(&seen, &found, now));
        // Пока она ждала блокировку, он продолжил ход своей командой: хука нет.
        scratch.write(
            "proj/s1.jsonl",
            &format!("{{\"type\":\"user\"}}\n{ANSWER}\n{TOOL}\n"),
        );
        stamp(&transcript, now);
        assert!(!finish(&store, &settings, &file, &seen, now, 1800));
        assert_eq!(status(&file), Some(Status::Working));

        // Команда доработала, ответ закончен, и Claude Code снова молчит.
        scratch.write(
            "proj/s1.jsonl",
            &format!("{{\"type\":\"user\"}}\n{TOOL}\n{ANSWER}\n"),
        );
        stamp(&transcript, now - GRACE_SEC);
        assert!(finish(&store, &settings, &file, &seen, now, 1800));
        assert_eq!(status(&file), Some(Status::Done));
    }

    #[test]
    fn sweep_updates_open_sessions_only() {
        let scratch = Scratch::new("sweep");
        let (store, settings) = scratch.store();
        let transcript = claude_session(&scratch, &MIXED);
        let now = modified(&transcript).unwrap_or(0);
        let session = |sid: &str, ended: bool| Session {
            agent: "claude".to_string(),
            session_id: sid.to_string(),
            status: Status::Working,
            started: now - 60,
            turn_started: Some(now - 60),
            ended,
            transcript: Some(transcript.to_string_lossy().into_owned()),
            ..Session::default()
        };
        for (sid, ended) in [("open", false), ("closed", true)] {
            let file = store.session_file("claude", sid);
            assert!(store::write_session(&file, &session(sid, ended)).is_ok());
        }
        sweep_at(&store, &settings, now, &scratch.0.join("codex"));
        let read = |sid: &str| store::read_session(&store.session_file("claude", sid));
        assert_eq!(read("open").map(|s| s.agents.len()), Some(4));
        assert_eq!(read("closed").map(|s| s.agents.len()), Some(0));
    }
}
