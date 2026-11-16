//! Тесты наблюдателя: поддельный `/proc`, файлы агентов во временном каталоге и
//! хранилище, где уведомления уходят строками в файл.

use super::*;
use crate::config::Paths;
use crate::fakeproc::FakeProc;
use std::os::unix::fs::PermissionsExt;

const ID: &str = "8c0233b8edde2a38477728c1396f581a8ccca3ccb7f2240111003ba04a7a3475";

/// Временный каталог, удаляется в `Drop`.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("disp-observe-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        Dir(dir)
    }

    fn write(&self, path: &str, text: &str) -> PathBuf {
        let path = self.0.join(path);
        assert!(fs::create_dir_all(path.parent().unwrap_or(&self.0)).is_ok());
        assert!(fs::write(&path, text).is_ok());
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_string).collect()
}

#[test]
fn containers_are_known_by_engine_cgroups() {
    let short = Some(ID[..12].to_string());
    assert_eq!(container_id(&format!("1:net_cls:/\n0::/system.slice/docker-{ID}.scope\n")), short);
    assert_eq!(container_id(&format!("0::/user.slice/user-1000.slice/libpod-{ID}.scope")), short);
    assert_eq!(container_id(&format!("0::/kubepods/pod1/cri-containerd-{ID}.scope")), short);
    assert_eq!(container_id(&format!("12:memory:/docker/{ID}")), short);
    // Песочница приложения и обычный сеанс — не контейнер.
    assert_eq!(container_id("0::/user.slice/user-1000.slice/app.slice/snap.code.code-1.scope"), None);
    assert_eq!(container_id("0::/user.slice/user-1000.slice/session-2.scope"), None);
}

#[test]
fn claude_transcripts_live_under_the_project_slug() {
    let transcript = |cwd: &str| match claude_log("/home/u/.claude", cwd, "s1") {
        Log::Claude { transcript } => transcript,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        transcript("/workspaces/nebularix/app-desktop"),
        "/home/u/.claude/projects/-workspaces-nebularix-app-desktop/s1.jsonl"
    );
    assert_eq!(transcript("/home/u/my.site"), "/home/u/.claude/projects/-home-u-my-site/s1.jsonl");
}

#[test]
fn claude_session_files_give_the_state() {
    let file = |status: &str| {
        format!(r#"{{"sessionId":"s1","cwd":"/w","status":"{status}","waitingFor":"permission prompt","procStart":"500"}}"#)
    };
    let state = |text: &str, start| claude_file(text, start).map(|(_, _, state)| state);
    assert_eq!(state(&file("busy"), 500), Some(State::Busy));
    assert_eq!(state(&file("waiting"), 500), Some(State::Waiting("permission prompt".to_string())));
    assert_eq!(state(&file("idle"), 500), Some(State::Idle));
    // Файл прежнего процесса с тем же номером.
    assert_eq!(state(&file("busy"), 501), None);
    assert_eq!(state(r#"{"status":"busy"}"#, 500), None);
}

/// Контейнер с Claude Code, который ждёт разрешения, и Codex посреди хода; рядом —
/// агенты, которых наблюдатель видеть не должен.
fn world(proc: &FakeProc, fs_root: &Dir) {
    let docker = format!("/system.slice/docker-{ID}.scope");
    let env = ["HOME=/home/dev"];
    proc.add_at(100, 1, &["/opt/claude"], "/", &env, 500);
    proc.contain(100, "mnt:[2]", &docker, 77, &fs_root.0);
    fs_root.write(
        "home/dev/.claude/sessions/77.json",
        r#"{"pid":77,"sessionId":"s-claude","cwd":"/work/app","status":"waiting","waitingFor":"permission prompt","procStart":"500"}"#,
    );
    // Агент на хосте и агент в песочнице со своей файловой системой — не в контейнере.
    proc.add_at(101, 1, &["/usr/bin/claude"], "/", &env, 600);
    proc.contain(101, "mnt:[1]", &docker, 78, &fs_root.0);
    proc.add_at(102, 1, &["/snap/claude"], "/", &env, 700);
    proc.contain(102, "mnt:[3]", "/user.slice/app.slice/snap.x.scope", 79, &fs_root.0);
    fs_root.write("home/dev/.claude/sessions/79.json", r#"{"sessionId":"s-snap","status":"busy"}"#);
    // Файл сессии от прежнего процесса с тем же номером: время запуска другое.
    proc.add_at(103, 1, &["/opt/claude"], "/", &env, 800);
    proc.contain(103, "mnt:[2]", &docker, 80, &fs_root.0);
    fs_root.write(
        "home/dev/.claude/sessions/80.json",
        r#"{"sessionId":"s-old","status":"busy","procStart":"1"}"#,
    );
    // Codex: сессия человека посреди хода и служебная проверка Guardian.
    proc.add_at(200, 1, &["/opt/codex"], "/", &env, 900);
    proc.contain(200, "mnt:[2]", &docker, 90, &fs_root.0);
    let day = "home/dev/.codex/sessions/2026/11/16";
    fs_root.write(
        &format!("{day}/rollout-2026-11-16T10-00-00-c1.jsonl"),
        concat!(
            r#"{"type":"session_meta","payload":{"id":"c1","cwd":"/work/app","source":"vscode"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
        ),
    );
    fs_root.write(
        &format!("{day}/rollout-2026-11-16T10-01-00-g1.jsonl"),
        r#"{"type":"session_meta","payload":{"id":"g1","cwd":"/work/app","source":{"subagent":{"other":"guardian"}}}}"#,
    );
}

#[test]
fn scan_finds_agents_in_containers_only() {
    let proc = FakeProc::new("observe-scan");
    let fs_root = Dir::new("scan-root");
    world(&proc, &fs_root);
    let look = scan(&Procs::scan_in(&proc.root, 100), "mnt:[1]");
    let found: Vec<(&str, &str, &State)> = look
        .seen
        .iter()
        .map(|one| (one.agent, one.sid.as_str(), &one.state))
        .collect();
    assert_eq!(
        found,
        [
            ("claude", "s-claude", &State::Waiting("permission prompt".to_string())),
            ("codex", "c1", &State::Busy),
        ]
    );
    assert!(look.seen.iter().all(|one| one.container == ID[..12] && one.cwd == "/work/app"));
    assert_eq!(
        look.seen[0].log,
        Log::Claude {
            transcript: "/home/dev/.claude/projects/-work-app/s-claude.jsonl".to_string()
        }
    );
    assert_eq!(
        look.seen[1].log,
        Log::Codex {
            rollout: "/home/dev/.codex/sessions/2026/11/16/rollout-2026-11-16T10-00-00-c1.jsonl"
                .to_string(),
            index: "/home/dev/.codex/session_index.jsonl".to_string(),
        }
    );
    assert!(look.everywhere && look.blind.is_empty());
}

#[test]
fn unreadable_agents_make_their_container_blind() {
    let proc = FakeProc::new("observe-blind");
    let fs_root = Dir::new("blind-root");
    world(&proc, &fs_root);
    // Агент другого пользователя: окружение не прочитать.
    let other = format!("{:0>64}", "ab");
    proc.add_at(300, 1, &["/opt/claude"], "/", &[], 1000);
    proc.contain(300, "mnt:[4]", &format!("/system.slice/docker-{other}.scope"), 5, &fs_root.0);
    assert!(fs::remove_file(proc.root.join("300/environ")).is_ok());
    let look = scan(&Procs::scan_in(&proc.root, 100), "mnt:[1]");
    assert_eq!(look.blind, HashSet::from([other[..12].to_string()]));
    assert!(look.covers(&ID[..12]) && !look.covers(&other[..12]));
}

#[test]
fn codex_turns_follow_the_last_task_event() {
    let dir = Dir::new("codex-state");
    let meta = r#"{"type":"session_meta","payload":{"id":"x","source":"cli"}}"#;
    let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
    let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
    let file = |name: &str, lines: &[&str]| dir.write(name, &(lines.join("\n") + "\n"));
    assert_eq!(codex_state(&file("open.jsonl", &[meta])), State::Idle);
    assert_eq!(codex_state(&file("busy.jsonl", &[meta, started])), State::Busy);
    assert_eq!(codex_state(&file("done.jsonl", &[meta, started, complete])), State::Idle);
    // Упоминание события в тексте ответа — не событие.
    let quoted = r#"{"type":"response_item","payload":{"type":"message","text":"task_started"}}"#;
    assert_eq!(
        codex_state(&file("quoted.jsonl", &[meta, started, complete, quoted])),
        State::Idle
    );
    // Хвост долгого хода, где начала хода уже не видно.
    assert_eq!(turn_state(None, false), State::Busy);
}

#[test]
fn details_come_from_what_the_agents_write() {
    let transcript = lines(concat!(
        r#"{"type":"user","message":{"role":"user","content":"старый промпт"}}"#,
        "\n",
        r#"{"type":"last-prompt","lastPrompt":"Собери билд\nи подпиши"}"#,
        "\n",
        r#"{"type":"ai-title","aiTitle":"Сборка билда"}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Готово, билд подписан."}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
    ));
    assert_eq!(
        claude_details(&transcript),
        Details {
            prompt: Some("Собери билд и подпиши".to_string()),
            reply: Some("Готово, билд подписан.".to_string()),
            title: Some("Сборка билда".to_string()),
            project: None,
        }
    );
    let rollout = lines(concat!(
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Проверь PR #407"}]}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>…</environment_context>"}]}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"Замечаний нет."}}"#,
    ));
    let index = lines(concat!(
        r#"{"id":"c1","thread_name":"Старое имя"}"#,
        "\n",
        r#"{"id":"other","thread_name":"Чужая"}"#,
        "\n",
        r#"{"id":"c1","thread_name":"Review PR #407"}"#,
    ));
    assert_eq!(
        codex_details(&rollout, &index, "c1"),
        Details {
            prompt: Some("Проверь PR #407".to_string()),
            reply: Some("Замечаний нет.".to_string()),
            title: Some("Review PR #407".to_string()),
            project: None,
        }
    );
}

#[test]
fn changes_become_hook_actions() {
    let with = |status: Status, ended: bool| Session {
        status,
        ended,
        ..Session::default()
    };
    let waiting = State::Waiting("permission prompt".to_string());
    assert_eq!(action_for(None, &State::Busy), Some(Action::Prompt));
    assert_eq!(action_for(None, &State::Idle), Some(Action::Start));
    assert_eq!(action_for(None, &waiting), Some(Action::Notice));
    let working = with(Status::Working, false);
    assert_eq!(action_for(Some(&working), &State::Busy), None);
    assert_eq!(action_for(Some(&working), &waiting), Some(Action::Notice));
    assert_eq!(action_for(Some(&working), &State::Idle), Some(Action::Stop));
    let asking = with(Status::Waiting, false);
    assert_eq!(action_for(Some(&asking), &State::Busy), Some(Action::Tool));
    assert_eq!(action_for(Some(&asking), &waiting), None);
    let done = with(Status::Done, false);
    assert_eq!(action_for(Some(&done), &State::Busy), Some(Action::Prompt));
    assert_eq!(action_for(Some(&done), &State::Idle), None);
    // Закрытая сессия, которую снова видно: окно VS Code перезагрузили.
    assert_eq!(action_for(Some(&with(Status::Done, true)), &State::Idle), Some(Action::Start));
    assert_eq!(action_for(Some(&with(Status::Working, true)), &State::Busy), Some(Action::Tool));
}

/// Хранилище во временном каталоге; уведомления уходят строками в `notes.log`.
fn store_in(dir: &Dir) -> (Store, Settings) {
    let notes = dir.0.join("notes.log");
    let script = dir.write(
        "note.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n", notes.display()),
    );
    assert!(fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).is_ok());
    let conf = dir.write(
        "env",
        &format!(
            "DISP_DESKTOP=0\nDISP_NOTIFY_MIN_SEC=0\nDISP_NOTIFY_CMD={}\n",
            script.display()
        ),
    );
    let state_dir = dir.0.join("state");
    let paths = Paths {
        sessions: state_dir.join("sessions"),
        state_dir,
        conf_file: conf.clone(),
    };
    (Store::new(paths), Settings::load(&conf))
}

/// Строки `notes.log`, когда их станет `count` (уведомления уходят отдельным процессом).
fn notes(dir: &Dir, count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lines = lines(&fs::read_to_string(dir.0.join("notes.log")).unwrap_or_default());
        if lines.len() >= count || Instant::now() > deadline {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Сессия Claude Code в контейнере с корнем `root`, в каталоге `/app`.
fn claude_seen(root: &Path, sid: &str, state: State) -> Seen {
    Seen {
        agent: "claude",
        sid: sid.to_string(),
        cwd: "/app".to_string(),
        container: ID[..12].to_string(),
        state,
        activity: None,
        access: Access::Root(root.to_path_buf()),
        log: claude_log("/home/dev/.claude", "/app", sid),
    }
}

/// Взгляд во все контейнеры, где видны только `seen`.
fn everywhere(seen: Vec<Seen>) -> Look {
    Look {
        seen,
        everywhere: true,
        ..Look::default()
    }
}

#[test]
fn apply_records_turns_like_hooks_do() {
    let dir = Dir::new("apply");
    let (store, settings) = store_in(&dir);
    assert!(fs::create_dir_all(dir.0.join("app")).is_ok());
    let look = |state: State| everywhere(vec![claude_seen(&dir.0, "s1", state)]);
    let file = store.session_file("claude", "s1");
    let read = || store::read_session(&file).unwrap_or_default();

    assert_eq!(apply(&store, &settings, &look(State::Busy), 1000), 1);
    let session = read();
    assert_eq!((session.status, session.task.as_str()), (Status::Working, "app"));
    assert_eq!(session.place, "контейнер 8c0233b8");
    assert_eq!(session.container.as_deref(), Some(&ID[..12]));
    assert_eq!(session.root.as_deref(), Some("/app"));
    // Ничего не изменилось — ничего не пишется.
    assert_eq!(apply(&store, &settings, &look(State::Busy), 1010), 0);
    // Долгий ход: время сессии обновляется, чтобы она не числилась молчащей.
    assert_eq!(apply(&store, &settings, &look(State::Busy), 1100), 1);
    assert_eq!(read().updated, 1100);

    let waiting = State::Waiting("permission prompt".to_string());
    assert_eq!(apply(&store, &settings, &look(waiting), 1110), 1);
    assert_eq!((read().status, read().detail), (Status::Waiting, "разрешение".to_string()));
    assert_eq!(apply(&store, &settings, &look(State::Busy), 1120), 1);
    assert_eq!(read().status, Status::Working);
    assert_eq!(apply(&store, &settings, &look(State::Idle), 1200), 1);
    assert_eq!(read().status, Status::Done);
    let sent = notes(&dir, 2);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(sent[0].contains("ждёт тебя") && sent[0].contains("контейнер 8c0233b8"), "{sent:?}");
    assert!(sent[1].contains("закончил ход"), "{sent:?}");

    // Агента больше не видно: сессия закрыта, но остаётся в списке до приёмки.
    assert_eq!(apply(&store, &settings, &everywhere(vec![]), 1300), 1);
    let session = read();
    assert!(session.ended && session.status == Status::Done);
    assert_eq!(apply(&store, &settings, &everywhere(vec![]), 1310), 0);
    let journal = fs::read_to_string(store.paths.events()).unwrap_or_default();
    let moves: Vec<String> = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .map(|event| {
            format!(
                "{}→{}",
                event["from"].as_str().unwrap_or(""),
                event["to"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        moves,
        ["new→working", "working→waiting", "waiting→working", "working→done", "done→ended"]
    );
}

#[test]
fn sessions_of_unseen_containers_stay_open() {
    let dir = Dir::new("unseen");
    let (store, settings) = store_in(&dir);
    let look = everywhere(vec![claude_seen(&dir.0, "s3", State::Busy)]);
    assert_eq!(apply(&store, &settings, &look, 1000), 1);
    let file = store.session_file("claude", "s3");
    // В контейнер не удалось заглянуть: о его сессиях ничего не известно.
    let blind = Look {
        everywhere: true,
        blind: HashSet::from([ID[..12].to_string()]),
        ..Look::default()
    };
    assert_eq!(apply(&store, &settings, &blind, 1010), 0);
    // Командой движка смотрели только в другие контейнеры.
    let elsewhere = Look {
        inspected: HashSet::from(["0123456789ab".to_string()]),
        ..Look::default()
    };
    assert_eq!(apply(&store, &settings, &elsewhere, 1020), 0);
    assert!(!store::read_session(&file).unwrap_or_default().ended);
}

#[test]
fn sessions_kept_by_hooks_are_left_alone() {
    let dir = Dir::new("hooked");
    let (store, settings) = store_in(&dir);
    assert!(store.ensure_dirs().is_ok());
    let file = store.session_file("claude", "s2");
    let hooked = Session {
        agent: "claude".to_string(),
        session_id: "s2".to_string(),
        status: Status::Working,
        updated: 1000,
        ..Session::default()
    };
    assert!(store::write_session(&file, &hooked).is_ok());
    let look = everywhere(vec![claude_seen(&dir.0, "s2", State::Idle)]);
    assert_eq!(apply(&store, &settings, &look, 2000), 0);
    assert_eq!(apply(&store, &settings, &everywhere(vec![]), 2000), 0);
    let session = store::read_session(&file).unwrap_or_default();
    assert_eq!(
        (session.status, session.ended, session.updated),
        (Status::Working, false, 1000)
    );
}

#[test]
fn words_in_message_text_are_not_turn_events() {
    let dir = Dir::new("event-text");
    let rollout = dir.write(
        "r.jsonl",
        concat!(
            r#"{"type":"session_meta","payload":{"id":"x","source":"cli"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Checking task_complete handling."}}"#,
            "\n",
        ),
    );
    assert_eq!(codex_state(&rollout), State::Busy);
}

#[test]
fn unreadable_namespace_makes_the_container_blind() {
    let proc = FakeProc::new("observe-ns");
    let fs_root = Dir::new("ns-root");
    let other = format!("{:0>64}", "cd");
    proc.add_at(400, 1, &["/opt/codex"], "/", &["HOME=/root"], 100);
    proc.contain(400, "mnt:[5]", &format!("/system.slice/docker-{other}.scope"), 7, &fs_root.0);
    // Агент другого пользователя: `ns/mnt` не прочитать.
    let ns = proc.root.join("400/ns");
    assert!(fs::set_permissions(&ns, fs::Permissions::from_mode(0o000)).is_ok());
    let denied = fs::read_link(ns.join("mnt"))
        .is_err_and(|err| err.kind() == ErrorKind::PermissionDenied);
    let look = scan(&Procs::scan_in(&proc.root, 100), "mnt:[1]");
    assert!(fs::set_permissions(&ns, fs::Permissions::from_mode(0o755)).is_ok());
    // Под root права не мешают читать: проверять нечего.
    if denied {
        assert_eq!(look.blind, HashSet::from([other[..12].to_string()]));
        assert!(!look.covers(&other[..12]));
    }
}

#[test]
fn resumed_sessions_are_found_in_old_day_directories() {
    let dir = Dir::new("resumed");
    let meta = |id: &str| {
        format!(
            "{}\n{}\n",
            format_args!(r#"{{"type":"session_meta","payload":{{"id":"{id}","cwd":"/w","source":"cli"}}}}"#),
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#
        )
    };
    // Сессию создали год назад и продолжили сейчас; соседнюю не трогали.
    dir.write("home/dev/.codex/sessions/2025/11/01/rollout-2025-11-01T10-00-00-old1.jsonl", &meta("old1"));
    let idle = dir.write(
        "home/dev/.codex/sessions/2025/11/01/rollout-2025-11-01T11-00-00-old2.jsonl",
        &meta("old2"),
    );
    let year_ago = SystemTime::now() - Duration::from_secs(365 * 86_400);
    assert!(File::options().write(true).open(&idle).is_ok_and(|file| file.set_modified(year_ago).is_ok()));
    let seen = codex_sessions(&dir.0, "/home/dev/.codex", config::now() - 60, &ID[..12]);
    let sids: Vec<&str> = seen.iter().map(|one| one.sid.as_str()).collect();
    assert_eq!(sids, ["old1"]);
    assert_eq!(seen[0].state, State::Busy);
}

#[test]
fn codex_turns_stay_alive_only_while_the_rollout_grows() {
    let dir = Dir::new("codex-alive");
    let (store, settings) = store_in(&dir);
    let look = |activity: i64| {
        everywhere(vec![Seen {
            agent: "codex",
            sid: "c1".to_string(),
            cwd: "/app".to_string(),
            container: ID[..12].to_string(),
            state: State::Busy,
            activity: Some(activity),
            access: Access::Root(dir.0.clone()),
            log: Log::Codex {
                rollout: "/none.jsonl".to_string(),
                index: "/none-index.jsonl".to_string(),
            },
        }])
    };
    let updated = || {
        store::read_session(&store.session_file("codex", "c1"))
            .unwrap_or_default()
            .updated
    };
    assert_eq!(apply(&store, &settings, &look(1000), 1000), 1);
    // Журнал не пишется, хотя процесс Codex в контейнере жив: агент этой сессии мог
    // пропасть, и время не продлевается — через `DISP_STALE_MIN` она станет молчащей.
    assert_eq!(apply(&store, &settings, &look(1000), 5000), 0);
    assert_eq!(updated(), 1000);
    // Журнал снова пишется — ход жив.
    assert_eq!(apply(&store, &settings, &look(4900), 5000), 1);
    assert_eq!(updated(), 4900);
}

#[test]
fn half_written_session_meta_is_read_again() {
    let dir = Dir::new("meta-retry");
    let rollout = dir.0.join("rollout-x.jsonl");
    assert!(fs::write(&rollout, r#"{"type":"session_meta","payl"#).is_ok());
    assert!(codex_meta(&rollout).is_none());
    assert!(
        fs::write(
            &rollout,
            concat!(r#"{"type":"session_meta","payload":{"id":"x1","cwd":"/w","source":"cli"}}"#, "\n")
        )
        .is_ok()
    );
    assert_eq!(codex_meta(&rollout).map(|meta| meta.sid), Some("x1".to_string()));
}
