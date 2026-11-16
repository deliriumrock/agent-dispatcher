//! Взгляд в контейнер командой движка: `docker exec` или `podman exec`.
//!
//! Так видно контейнеры, чьи файлы с хоста не прочитать: на macOS они живут в виртуальной
//! машине Docker Desktop, а на Linux агент мог быть запущен в контейнере другим
//! пользователем. В контейнер ничего не ставится: короткий сценарий `sh` читает `/proc`
//! контейнера и файлы агентов и печатает строки, которые разбираются здесь. От контейнера
//! нужны только `sh` и обычные утилиты (`sed`, `grep`, `tail`, `stat`, `find`) — они есть
//! и в busybox.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    Access, Details, Log, Look, Seen, TAIL_BYTES, TURN_EVENTS, claude_details, claude_file,
    claude_log, codex_details, turn_state,
};
use crate::detect::{self, which};
use crate::hook;

/// Сколько ждать ответа движка: контейнер мог зависнуть или стоять на паузе.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Разделитель частей ответа сценария подробностей (ASCII RS).
const PART: &str = "\n\u{1e}\n";

/// Сценарий взгляда: агенты в `/proc` контейнера и их файлы. Печатает строки через табуляцию:
///
/// - `C <время запуска> <каталог Claude> <файл sessions/<pid>.json одной строкой>`;
/// - `R <каталог Codex> <журнал> <время изменения> <размер> <"id":…> <"cwd":…>
///   <"source":…> <последнее событие хода>` — для журналов, изменённых с запуска
///   процесса Codex. Событие ищется как `"type":"…"`: упоминание в тексте сообщения
///   записано с экранированными кавычками и под образец не подходит.
const SCAN: &str = r#"
tck=$(getconf CLK_TCK 2>/dev/null) || tck=100
[ -n "$tck" ] || tck=100
bt=$(sed -n 's/^btime //p' /proc/stat)
for d in /proc/[0-9]*; do
  c=$(cat "$d/comm" 2>/dev/null) || continue
  case $c in claude|codex) ;; *) continue ;; esac
  p=${d#/proc/}
  st=$(sed 's/^.*) //' "$d/stat" 2>/dev/null | cut -d ' ' -f 20)
  [ -n "$st" ] || continue
  env=$(tr '\000' '\n' < "$d/environ" 2>/dev/null)
  home=$(printf '%s\n' "$env" | sed -n 's/^HOME=//p' | head -n 1)
  if [ "$c" = claude ]; then
    cfg=$(printf '%s\n' "$env" | sed -n 's/^CLAUDE_CONFIG_DIR=//p' | head -n 1)
    [ -n "$cfg" ] || cfg=$home/.claude
    f=$cfg/sessions/$p.json
    [ -r "$f" ] || continue
    printf 'C\t%s\t%s\t' "$st" "$cfg"
    tr -d '\n\t' < "$f"
    printf '\n'
  else
    ch=$(printf '%s\n' "$env" | sed -n 's/^CODEX_HOME=//p' | head -n 1)
    [ -n "$ch" ] || ch=$home/.codex
    since=$((bt + st / tck))
    find "$ch/sessions" -name 'rollout-*.jsonl' 2>/dev/null | while IFS= read -r f; do
      m=$(stat -c %Y "$f" 2>/dev/null) || continue
      [ "$m" -ge "$since" ] || continue
      first=$(head -n 1 "$f")
      id=$(printf '%s' "$first" | grep -o '"id":"[^"]*"' | head -n 1)
      cwd=$(printf '%s' "$first" | grep -o '"cwd":"[^"]*"' | head -n 1)
      src=$(printf '%s' "$first" | grep -o '"source":[^,}]*' | head -n 1)
      size=$(stat -c %s "$f" 2>/dev/null)
      last=$(tail -c 2097152 "$f" | grep -oE '"type":"(task_started|task_complete|turn_aborted)"' | tail -n 1)
      printf 'R\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$ch" "$f" "$m" "$size" "$id" "$cwd" "$src" "$last"
    done
  fi
done
"#;

/// Сценарий подробностей: хвост журнала `$1`, хвост списка сессий `$2` (если задан)
/// и корень git над каталогом `$3` с первой строкой его `HEAD`, части — через `PART`.
const DETAILS: &str = r#"
tail -c 2097152 "$1" 2>/dev/null
printf '\n\036\n'
[ -n "$2" ] && tail -c 262144 "$2" 2>/dev/null
printf '\n\036\n'
d=$3
while [ -n "$d" ]; do
  if [ -f "$d/.git/HEAD" ] || [ -f "$d/.git" ]; then
    printf '%s\n' "$d"
    head -n 1 "$d/.git/HEAD" 2>/dev/null
    break
  fi
  [ "$d" = / ] && break
  d=$(dirname "$d")
done
"#;

/// Программа движка: `docker` или `podman` из `PATH`.
pub(super) fn engine() -> Option<PathBuf> {
    which("docker").or_else(|| which("podman"))
}

/// Взгляд во все запущенные контейнеры. Если движок не ответил, о контейнерах ничего не
/// известно, и ни одна сессия не закрывается.
pub(super) fn look(engine: &Path) -> Look {
    let Some(running) = run(engine, &["ps", "-q"]) else {
        return Look::default();
    };
    let mut look = Look {
        everywhere: true,
        ..Look::default()
    };
    for container in running.lines().map(str::trim).filter(|id| !id.is_empty()) {
        let container = container.chars().take(12).collect::<String>();
        match inspect(engine, &container) {
            Some(seen) => {
                look.seen.extend(seen);
                look.inspected.insert(container);
            }
            None => {
                look.blind.insert(container);
            }
        }
    }
    look
}

/// Сессии агентов в контейнере; `None` — заглянуть не удалось.
pub(super) fn inspect(engine: &Path, container: &str) -> Option<Vec<Seen>> {
    let output = run(engine, &["exec", "-u", "0", container, "sh", "-c", SCAN])?;
    Some(parse_scan(&output, engine, container))
}

/// Разбирает строки сценария взгляда.
fn parse_scan(output: &str, engine: &Path, container: &str) -> Vec<Seen> {
    let seen = |agent, sid, cwd, state, activity, log| Seen {
        agent,
        sid,
        cwd,
        container: container.to_string(),
        state,
        activity,
        access: Access::Exec(engine.to_path_buf()),
        log,
    };
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["C", start, config, file] => {
                    let (sid, cwd, state) = claude_file(file, start.parse().ok()?)?;
                    let log = claude_log(config, &cwd, &sid);
                    Some(seen("claude", sid, cwd, state, None, log))
                }
                ["R", home, rollout, modified, size, id, cwd, source, last] => {
                    // Служебные потоки (Guardian, субагенты) записаны с источником-объектом.
                    if !source.starts_with(r#""source":""#) {
                        return None;
                    }
                    let sid = hook::sanitize(quoted(id, "id")?);
                    if sid.is_empty() {
                        return None;
                    }
                    let last = TURN_EVENTS.into_iter().find(|kind| last.contains(kind));
                    let whole = size.parse::<u64>().is_ok_and(|size| size <= TAIL_BYTES);
                    let log = Log::Codex {
                        rollout: (*rollout).to_string(),
                        index: format!("{home}/session_index.jsonl"),
                    };
                    let cwd = quoted(cwd, "cwd").unwrap_or_default().to_string();
                    let state = turn_state(last, whole);
                    Some(seen("codex", sid, cwd, state, modified.parse().ok(), log))
                }
                _ => None,
            }
        })
        .collect()
}

/// Значение из `"key":"value"`, как его находит `grep -o`.
fn quoted<'a>(field: &'a str, key: &str) -> Option<&'a str> {
    field
        .strip_prefix(&format!(r#""{key}":""#))?
        .strip_suffix('"')
}

/// Промпт, ответ, название и задача сессии в контейнере.
pub(super) fn details(engine: &Path, container: &str, log: &Log, sid: &str, cwd: &str) -> Details {
    let (journal, index) = match log {
        Log::Claude { transcript } => (transcript.as_str(), ""),
        Log::Codex { rollout, index } => (rollout.as_str(), index.as_str()),
    };
    let args = [
        "exec", "-u", "0", container, "sh", "-c", DETAILS, "sh", journal, index, cwd,
    ];
    let output = run(engine, &args).unwrap_or_default();
    parse_details(&output, log, sid, cwd)
}

/// Разбирает ответ сценария подробностей.
fn parse_details(output: &str, log: &Log, sid: &str, cwd: &str) -> Details {
    let mut parts = output.splitn(3, PART);
    let lines = |part: Option<&str>| -> Vec<String> {
        part.unwrap_or_default().lines().map(str::to_string).collect()
    };
    let journal = lines(parts.next());
    let index = lines(parts.next());
    let git = lines(parts.next());
    let mut details = match log {
        Log::Claude { .. } => claude_details(&journal),
        Log::Codex { .. } => codex_details(&journal, &index, sid),
    };
    details.project = Some(match git.first().filter(|top| !top.is_empty()) {
        Some(top) => {
            let branch = git
                .get(1)
                .and_then(|head| head.trim().strip_prefix("ref: refs/heads/"))
                .unwrap_or_default();
            (detect::base_name(top), branch.to_string())
        }
        None => (detect::base_name(cwd), String::new()),
    });
    details
}

/// Запускает движок с аргументами и возвращает его вывод; `None` — ошибка, ненулевой
/// код выхода или нет ответа за `TIMEOUT`.
fn run(engine: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new(engine)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    // Вывод читается в своём потоке: иначе большой ответ заполнил бы канал и движок ждал бы.
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::super::State;
    use super::*;

    const ENGINE: &str = "/usr/bin/docker";

    #[test]
    fn scan_lines_become_sessions() {
        let output = concat!(
            "C\t500\t/home/dev/.claude\t",
            r#"{"sessionId":"s1","cwd":"/work/app","status":"busy","procStart":"500"}"#,
            "\n",
            "C\t501\t/home/dev/.claude\t",
            r#"{"sessionId":"old","status":"busy","procStart":"1"}"#,
            "\n",
            "R\t/home/dev/.codex\t/home/dev/.codex/sessions/2026/11/16/rollout-c1.jsonl\t1794830000\t2048\t",
            r#""id":"c1""#,
            "\t",
            r#""cwd":"/work/app""#,
            "\t",
            r#""source":"vscode""#,
            "\t",
            r#""type":"task_complete""#,
            "\n",
            "R\t/home/dev/.codex\t/home/dev/.codex/sessions/2026/11/16/rollout-g1.jsonl\t1794830000\t10\t",
            r#""id":"g1""#,
            "\t",
            r#""cwd":"/work/app""#,
            "\t",
            r#""source":{"subagent":{"other":"guardian""#,
            "\t\n",
            "мусор\n",
        );
        let seen = parse_scan(output, Path::new(ENGINE), "8c0233b8edde");
        let found: Vec<(&str, &str, &str, &State)> = seen
            .iter()
            .map(|one| (one.agent, one.sid.as_str(), one.cwd.as_str(), &one.state))
            .collect();
        assert_eq!(
            found,
            [
                ("claude", "s1", "/work/app", &State::Busy),
                ("codex", "c1", "/work/app", &State::Idle),
            ]
        );
        assert_eq!(seen[0].access, Access::Exec(PathBuf::from(ENGINE)));
        assert_eq!((seen[0].activity, seen[1].activity), (None, Some(1_794_830_000)));
        assert_eq!(
            seen[1].log,
            Log::Codex {
                rollout: "/home/dev/.codex/sessions/2026/11/16/rollout-c1.jsonl".to_string(),
                index: "/home/dev/.codex/session_index.jsonl".to_string(),
            }
        );
    }

    #[test]
    fn long_codex_turns_without_a_visible_start_are_busy() {
        let line = |size: u64| {
            format!(
                "R\t/h\t/h/sessions/r.jsonl\t1794830000\t{size}\t\"id\":\"c1\"\t\"cwd\":\"/w\"\t\"source\":\"cli\"\t\n"
            )
        };
        let state = |size| parse_scan(&line(size), Path::new(ENGINE), "c")[0].state.clone();
        assert_eq!(state(100), State::Idle);
        assert_eq!(state(TAIL_BYTES + 1), State::Busy);
    }

    #[test]
    fn details_come_in_three_parts() {
        let output = [
            concat!(
                "частичная строка\n",
                r#"{"type":"last-prompt","lastPrompt":"Собери билд"}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Готово."}]}}"#,
            ),
            "",
            "/workspaces/nebularix\nref: refs/heads/docs/check\n",
        ]
        .join(PART);
        let log = claude_log("/home/dev/.claude", "/workspaces/nebularix/app", "s1");
        let details = parse_details(&output, &log, "s1", "/workspaces/nebularix/app");
        assert_eq!(details.prompt.as_deref(), Some("Собери билд"));
        assert_eq!(details.reply.as_deref(), Some("Готово."));
        assert_eq!(
            details.project,
            Some(("nebularix".to_string(), "docs/check".to_string()))
        );
        // Без git задача — имя каталога сессии.
        let bare = parse_details(&["", "", ""].join(PART), &log, "s1", "/work/app");
        assert_eq!(bare.project, Some(("app".to_string(), String::new())));
    }
}
