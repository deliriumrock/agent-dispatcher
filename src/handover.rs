//! Сессии Claude Code, ушедшие в фон.
//!
//! Когда сессию отправляют в фон, Claude Code продолжает разговор в новой фоновой сессии,
//! а прежняя не присылает ни Stop, ни SessionEnd. В её транскрипт только дописывается
//! последней записью `continued-in` (см. `detect::claude_continued_in`). Процесс прежней
//! сессии при этом жив и показывает уже фоновую, так что иначе она висела бы в списке
//! работающей, а потом «зависшей».
//!
//! В какой момент появится эта запись относительно событий новой сессии, не определено,
//! поэтому её ищут не хуки, а те, кто показывает список: при каждой сверке
//! (`observe::sync`) просматриваются концы транскриптов открытых сессий Claude Code.
//! Транскрипт, который с прошлого просмотра не менялся, второй раз не читается.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::config;
use crate::detect;
use crate::model::Session;
use crate::store::{self, Store};

/// Транскрипты, в которых при прошлом просмотре передачи не было: их время изменения
/// и размер тогда.
static UNCHANGED: Mutex<Option<HashMap<PathBuf, (SystemTime, u64)>>> = Mutex::new(None);

/// Закрывает открытые сессии Claude Code, разговор которых продолжен в другой сессии,
/// и убирает их из списка: та же работа видна в строке новой.
pub fn sweep(store: &Store) {
    sweep_at(store, config::now());
}

fn sweep_at(store: &Store, now: i64) {
    for (file, session) in store.all() {
        let handed = watched(&session).is_some_and(|transcript| handed_over(Path::new(transcript)));
        if !handed {
            continue;
        }
        let _lock = store.lock();
        // Под блокировкой перечитываем: сессию мог только что обновить хук.
        if let Some(fresh) = store::read_session(&file).filter(|fresh| watched(fresh).is_some()) {
            // Не удалось записать — попробуем на следующей сверке.
            let _ = store::write_session(&file, &hand_over(fresh, now));
        }
    }
}

/// Транскрипт открытой сессии Claude Code на этой машине, если хук его сообщил.
fn watched(session: &Session) -> Option<&str> {
    if session.agent != "claude" || session.ended || session.accepted || session.container.is_some()
    {
        return None;
    }
    session
        .transcript
        .as_deref()
        .filter(|path| !path.is_empty())
}

/// Кончается ли транскрипт передачей разговора в другую сессию.
fn handed_over(transcript: &Path) -> bool {
    let stamp = store::file_stamp(transcript);
    let Ok(mut unchanged) = UNCHANGED.lock() else {
        return detect::claude_continued_in(transcript).is_some();
    };
    let unchanged = unchanged.get_or_insert_with(HashMap::new);
    if stamp.is_some() && unchanged.get(transcript) == stamp.as_ref() {
        return false;
    }
    let found = detect::claude_continued_in(transcript).is_some();
    match stamp {
        Some(stamp) if !found => {
            unchanged.insert(transcript.to_path_buf(), stamp);
        }
        _ => {
            unchanged.remove(transcript);
        }
    }
    found
}

/// Прежняя сессия, разговор которой продолжен в другой: закрыта и убрана из списка.
fn hand_over(mut session: Session, now: i64) -> Session {
    session.ended = true;
    session.end_reason = Some("continued".to_string());
    session.accepted = true;
    session.accepted_at = Some(now);
    session.updated = now;
    session
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::Paths;
    use crate::model::Status;

    /// Хранилище и транскрипты во временном каталоге.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("disp-handover-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            assert!(fs::create_dir_all(&dir).is_ok());
            Scratch(dir)
        }

        fn store(&self) -> Store {
            let state_dir = self.0.join("state");
            let store = Store::new(Paths {
                sessions: state_dir.join("sessions"),
                state_dir,
                conf_file: self.0.join("env"),
            });
            assert!(store.ensure_dirs().is_ok());
            store
        }

        /// Сессия Claude Code с транскриптом `<sid>.jsonl` из строк `lines`.
        fn session(&self, store: &Store, sid: &str, lines: &[&str]) -> Session {
            let transcript = self.0.join(format!("{sid}.jsonl"));
            assert!(fs::write(&transcript, lines.join("\n") + "\n").is_ok());
            let session = Session {
                agent: "claude".to_string(),
                session_id: sid.to_string(),
                status: Status::Working,
                transcript: Some(transcript.to_string_lossy().into_owned()),
                ..Session::default()
            };
            assert!(store::write_session(&store.session_file("claude", sid), &session).is_ok());
            session
        }

        fn append(&self, sid: &str, line: &str) {
            let transcript = self.0.join(format!("{sid}.jsonl"));
            let text = fs::read_to_string(&transcript).unwrap_or_default();
            assert!(fs::write(&transcript, format!("{text}{line}\n")).is_ok());
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn read(store: &Store, sid: &str) -> Session {
        store::read_session(&store.session_file("claude", sid)).unwrap_or_default()
    }

    const USER: &str = r#"{"type":"user","message":"поправь иконку"}"#;
    const MARKER: &str =
        r#"{"type":"continued-in","sessionId":"old","continuedInSessionId":"new"}"#;

    #[test]
    fn handed_over_session_leaves_the_list_once_the_marker_appears() {
        let scratch = Scratch::new("late");
        let store = scratch.store();
        scratch.session(&store, "old", &[USER]);
        // Новая сессия успела прислать событие раньше, чем появилась запись о передаче.
        scratch.session(&store, "new", &[USER]);
        sweep_at(&store, 100);
        assert!(!read(&store, "old").ended);

        scratch.append("old", MARKER);
        sweep_at(&store, 200);
        let old = read(&store, "old");
        assert!(old.ended && old.accepted);
        assert_eq!(old.end_reason.as_deref(), Some("continued"));
        assert_eq!((old.accepted_at, old.updated), (Some(200), 200));
        assert!(!read(&store, "new").ended);
    }

    #[test]
    fn only_open_claude_sessions_on_this_machine_are_closed() {
        let scratch = Scratch::new("scope");
        let store = scratch.store();
        let write = |session: &Session| {
            let file = store.session_file(&session.agent, &session.session_id);
            assert!(store::write_session(&file, session).is_ok());
        };
        let mut boxed = scratch.session(&store, "boxed", &[USER, MARKER]);
        boxed.container = Some("8c0233b8edde".to_string());
        write(&boxed);
        let mut closed = scratch.session(&store, "closed", &[USER, MARKER]);
        closed.ended = true;
        closed.end_reason = Some("other".to_string());
        write(&closed);
        let mut codex = scratch.session(&store, "codex", &[USER, MARKER]);
        codex.agent = "codex".to_string();
        write(&codex);
        let _ = fs::remove_file(store.session_file("claude", "codex"));
        // После передачи разговор в прежней сессии пошёл дальше: она снова в работе.
        scratch.session(&store, "back", &[USER, MARKER, USER]);

        sweep_at(&store, 300);
        assert!(!read(&store, "boxed").ended);
        assert_eq!(read(&store, "closed").end_reason.as_deref(), Some("other"));
        let codex = store::read_session(&store.session_file("codex", "codex"));
        assert!(codex.is_some_and(|codex| !codex.ended));
        assert!(!read(&store, "back").ended);
    }
}
