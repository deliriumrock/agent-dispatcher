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
//! Claude Code смотрит, не ждёт ли тот своих фоновых агентов (`waiting_for`).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::{self, Settings, env_nonempty};
use crate::model::{Session, SubAgent};
use crate::observe;
use crate::store::{self, Store};

/// Сколько агентов одной сессии помнить: у ревью кода их бывает несколько десятков.
const KEEP: usize = 20;

/// Обновляет списки агентов у открытых сессий на этой машине.
pub fn sweep(store: &Store) {
    let settings = Settings::load(&store.paths.conf_file);
    let stale = settings.int("DISP_STALE_MIN", 30) * 60;
    sweep_at(store, config::now(), stale, &codex_home());
}

fn sweep_at(store: &Store, now: i64, stale: i64, codex_home: &Path) {
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
                    claude(Path::new(transcript), now, stale)
                }
                _ => continue,
            },
            "codex" => codex.remove(&session.session_id).unwrap_or_default(),
            _ => continue,
        };
        let agents = keep(found, turn_start(&session));
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

/// Сессия, за агентами которой следим: открытая и не в контейнере.
fn tracked(session: &Session) -> bool {
    !session.ended && !session.accepted && session.container.is_none()
}

/// Начало текущего хода сессии, а без него — начало самой сессии.
fn turn_start(session: &Session) -> i64 {
    session.turn_started.unwrap_or(session.started)
}

/// Агенты для показа: работающие и те, что работали с начала хода `turn`, по времени
/// запуска; не больше `KEEP` последних.
fn keep(mut found: Vec<SubAgent>, turn: i64) -> Vec<SubAgent> {
    found.retain(|agent| agent.running || agent.updated >= turn);
    found.sort_by(|a, b| a.started.cmp(&b.started).then_with(|| a.id.cmp(&b.id)));
    let extra = found.len().saturating_sub(KEEP);
    found.drain(..extra);
    found
}

/// Сколько фоновых агентов Claude Code ещё работают у сессии с транскриптом
/// `transcript`. Пока они работают, ход не окончен: когда они закончат, Claude Code
/// продолжит его сам.
pub fn waiting_for(transcript: &str, now: i64, stale: i64) -> usize {
    if transcript.is_empty() {
        return 0;
    }
    claude(Path::new(transcript), now, stale)
        .iter()
        .filter(|agent| agent.background && agent.running)
        .count()
}

/// Субагенты сессии Claude Code с транскриптом `transcript`. Агент без отметки об
/// окончании считается работающим, пока его транскрипт менялся не дольше `stale`
/// секунд назад: так старые версии Claude Code, которые отметку не ставят, и упавший
/// Claude Code не оставляют вечно работающих агентов.
pub fn claude(transcript: &Path, now: i64, stale: i64) -> Vec<SubAgent> {
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
fn claude_agent(dir: &Path, id: String, now: i64, stale: i64) -> Option<SubAgent> {
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
    Some(SubAgent {
        kind: text("agentType").unwrap_or_default(),
        what: text("description")
            .or_else(|| text("name"))
            .unwrap_or_default(),
        background: text("requestShape").as_deref() == Some("background"),
        running: !finished && now - updated <= stale,
        started,
        updated,
        id,
    })
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
    use crate::model::Status;

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
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Транскрипт сессии `s1` с тремя субагентами: фоновый работает, обычный закончил,
    /// а у третьего, от старой версии Claude Code, отметки об окончании нет вовсе.
    fn claude_session(scratch: &Scratch) -> PathBuf {
        let transcript = scratch.write("proj/s1.jsonl", "{}\n");
        let meta = |id: &str, body: &str| {
            scratch.write(&format!("proj/s1/subagents/agent-{id}.meta.json"), body);
            scratch.write(&format!("proj/s1/subagents/agent-{id}.jsonl"), "{}\n");
        };
        meta(
            "a1",
            r#"{"agentType":"general-purpose","description":"Survey wiki","requestShape":"background"}"#,
        );
        meta(
            "a2",
            r#"{"agentType":"Explore","description":"Find icons","requestShape":"foreground","finished":true}"#,
        );
        meta(
            "a3",
            r#"{"agentType":"general-purpose","name":"/code-review 61","requestShape":"background"}"#,
        );
        scratch.write("proj/s1/subagents/agent-bad.meta.json", "{не json");
        transcript
    }

    #[test]
    fn claude_agents_come_from_their_meta_files() {
        let scratch = Scratch::new("claude");
        let transcript = claude_session(&scratch);
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
            ]
        );
        assert_eq!(waiting_for(&transcript.to_string_lossy(), now, 1800), 2);
        // Давно молчащий агент без отметки об окончании уже не работает.
        assert_eq!(
            waiting_for(&transcript.to_string_lossy(), now + 3600, 1800),
            0
        );
        assert_eq!(waiting_for("", now, 1800), 0);
        assert!(claude(&scratch.0.join("proj/none.jsonl"), now, 1800).is_empty());
    }

    #[test]
    fn list_keeps_this_turn_and_running_agents() {
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
        let many: Vec<SubAgent> = (0..30)
            .map(|n| agent(&format!("{n:02}"), true, n, n))
            .collect();
        let kept = keep(many, 0);
        assert_eq!(kept.len(), KEEP);
        assert_eq!(kept.first().map(|a| a.id.as_str()), Some("10"));
    }

    #[test]
    fn sweep_updates_open_sessions_only() {
        let scratch = Scratch::new("sweep");
        let transcript = claude_session(&scratch);
        let now = modified(&transcript).unwrap_or(0);
        let state_dir = scratch.0.join("state");
        let store = Store::new(Paths {
            sessions: state_dir.join("sessions"),
            state_dir,
            conf_file: scratch.0.join("env"),
        });
        assert!(store.ensure_dirs().is_ok());
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
        sweep_at(&store, now, 1800, &scratch.0.join("codex"));
        let read = |sid: &str| store::read_session(&store.session_file("claude", sid));
        let open = read("open").map(|s| s.agents.len());
        assert_eq!(open, Some(3));
        assert_eq!(read("closed").map(|s| s.agents.len()), Some(0));
    }
}
