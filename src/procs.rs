//! Что на самом деле с сессией, которая давно молчит: идёт ли у агента команда и жив ли
//! он сам. Ответ берётся из `/proc`.
//!
//! Агент присылает событие на каждый вызов инструмента, а долгая команда (сборка, тесты
//! в контейнере) событий не даёт. По одному времени последнего события не отличить долгую
//! команду от зависшего агента и от агента, которого закрыли без события конца. Процессы
//! отличают:
//!
//! - команда Claude Code — оболочка, которую запустил сам `claude`; в её окружении
//!   `CLAUDE_CODE_SESSION_ID` и `CLAUDE_PID`. Серверы MCP получают ту же переменную,
//!   но они не оболочки;
//! - команды Codex запускает не окно сессии, а общий фоновый процесс `codex app-server`
//!   через свою песочницу; связать команду с сессией можно только по `CODEX_THREAD_ID`
//!   в её окружении;
//! - жив ли Claude Code — по процессу, который хук записал в сессию (`Session::process`);
//!   жив ли Codex — по процессу его окна с тем же рабочим каталогом.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::detect::{self, base_name};
use crate::model::{AgentProcess, Session};
use crate::text;

/// Сколько секунд верить прошлой проверке. Обход `/proc` занимает миллисекунды, а панель
/// перечитывает сессии каждую секунду; за полминуты молчащая сессия не изменится.
const CACHE_SEC: i64 = 30;

/// Оболочки: команда агента запускается в одной из них.
const SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "fish", "ksh", "mksh"];

/// Обёртки песочницы Codex: процессы между фоновым процессом Codex и самой командой.
/// Имена обрезаны до 15 байт, как в `/proc/<pid>/stat`.
const CODEX_WRAPPERS: [&str; 3] = ["codex", "codex-linux-san", "bwrap"];

/// Что делает молчащая сессия.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    /// Идёт команда: её текст и когда она запущена, в unix-секундах.
    Command {
        /// Команда одной строкой.
        text: String,
        /// Время запуска.
        since: i64,
    },
    /// Процесса агента больше нет.
    Gone,
    /// Агент жив, а команд у него нет.
    Stuck,
    /// Проверить нельзя: процесс агента неизвестен.
    Unknown,
}

/// Что делают молчащие сессии. Результат держится `CACHE_SEC` секунд для той же
/// сессии с тем же временем последнего события; `/proc` обходится, только если для
/// какой-то из сессий свежего результата нет.
pub fn activities(sessions: &[&Session], now: i64) -> Vec<Activity> {
    static CACHE: LazyLock<Mutex<HashMap<String, (i64, Activity)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let key_of = |s: &Session| format!("{}-{}-{}", s.agent, s.session_id, s.updated);
    // Отравленный замок значит лишь, что другой поток упал посреди записи: кэш тогда
    // просто не используется.
    let mut cache = CACHE.lock().ok();
    if let Some(cache) = cache.as_mut() {
        cache.retain(|_, (at, _)| (now - *at).abs() < CACHE_SEC);
    }
    let mut procs: Option<Procs> = None;
    sessions
        .iter()
        .map(|&session| {
            let key = key_of(session);
            if let Some((_, known)) = cache.as_ref().and_then(|cache| cache.get(&key)) {
                return known.clone();
            }
            let activity = procs.get_or_insert_with(Procs::scan).activity(session);
            if let Some(cache) = cache.as_mut() {
                cache.insert(key, (now, activity.clone()));
            }
            activity
        })
        .collect()
}

/// Процесс агента, запустившего хук, если хук пришёл от Claude Code этой самой сессии.
///
/// Claude Code передаёт хуку `CLAUDE_PID` и `CLAUDE_CODE_SESSION_ID`. Номер сессии
/// сверяется: хук, запущенный вручную или из тестов внутри другой сессии Claude Code,
/// унаследовал бы чужой процесс.
pub fn claude_process(sid: &str) -> Option<AgentProcess> {
    claude_process_from(
        Path::new("/proc"),
        sid,
        std::env::var("CLAUDE_CODE_SESSION_ID").ok().as_deref(),
        std::env::var("CLAUDE_PID").ok().as_deref(),
    )
}

fn claude_process_from(
    proc: &Path,
    sid: &str,
    env_sid: Option<&str>,
    env_pid: Option<&str>,
) -> Option<AgentProcess> {
    if sid.is_empty() || env_sid != Some(sid) {
        return None;
    }
    let pid: u32 = env_pid?.trim().parse().ok()?;
    let stat = read_stat(proc, pid)?;
    Some(AgentProcess {
        pid,
        start: stat.start,
    })
}

/// Процесс из `/proc/<pid>/stat`.
#[derive(Clone, Debug)]
pub(crate) struct Proc {
    /// Номер процесса.
    pub(crate) pid: u32,
    /// Номер родителя.
    pub(crate) ppid: u32,
    /// Имя программы, обрезанное ядром до 15 байт.
    pub(crate) comm: String,
    /// Время запуска в тиках с загрузки системы.
    pub(crate) start: u64,
}

/// Снимок процессов системы.
pub struct Procs {
    root: PathBuf,
    list: Vec<Proc>,
    by_pid: HashMap<u32, usize>,
    /// Время загрузки системы, unix-секунды.
    boot: i64,
    /// Тиков в секунде.
    ticks: u64,
}

impl Procs {
    /// Снимок настоящего `/proc`.
    pub fn scan() -> Self {
        Self::scan_in(Path::new("/proc"), rustix::param::clock_ticks_per_second())
    }

    /// Снимок `/proc` в каталоге `root`; `ticks` — тиков в секунде.
    pub(crate) fn scan_in(root: &Path, ticks: u64) -> Self {
        let list: Vec<Proc> = fs::read_dir(root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
                    .filter_map(|pid| read_stat(root, pid))
                    .collect()
            })
            .unwrap_or_default();
        let by_pid = list
            .iter()
            .enumerate()
            .map(|(index, proc)| (proc.pid, index))
            .collect();
        let boot = fs::read_to_string(root.join("stat"))
            .ok()
            .and_then(|stat| {
                stat.lines()
                    .find_map(|line| line.strip_prefix("btime ")?.trim().parse().ok())
            })
            .unwrap_or(0);
        Procs {
            root: root.to_path_buf(),
            list,
            by_pid,
            boot,
            ticks: ticks.max(1),
        }
    }

    /// Что делает сессия, если её считать молчащей.
    pub fn activity(&self, session: &Session) -> Activity {
        // Снимок пуст, когда `/proc` нет (macOS): отсутствие процесса там ничего не значит.
        // Сессию в контейнере ведёт наблюдатель (`crate::observe`): номера процессов
        // контейнера здесь ничего не значат, а жив ли агент, он проверяет сам.
        if self.list.is_empty() || session.container.is_some() {
            return Activity::Unknown;
        }
        let sid = session.session_id.as_str();
        match session.agent.as_str() {
            "claude" => self.claude_command(sid).unwrap_or(match session.process {
                Some(process) if self.alive(process) => Activity::Stuck,
                Some(_) => Activity::Gone,
                None => Activity::Unknown,
            }),
            "codex" => self
                .codex_command(sid)
                .unwrap_or_else(|| self.codex_window(session)),
            _ => Activity::Unknown,
        }
    }

    /// Жив ли процесс: тот же номер и то же время запуска.
    fn alive(&self, process: AgentProcess) -> bool {
        self.get(process.pid)
            .is_some_and(|proc| proc.start == process.start)
    }

    /// Самая свежая команда сессии Claude Code: оболочка, которую запустил сам агент.
    fn claude_command(&self, sid: &str) -> Option<Activity> {
        self.list
            .iter()
            .filter(|proc| SHELLS.contains(&proc.comm.as_str()))
            .filter(|proc| {
                let env = self.environ(proc.pid);
                env.get("CLAUDE_CODE_SESSION_ID").map(String::as_str) == Some(sid)
                    && env
                        .get("CLAUDE_PID")
                        .and_then(|pid| pid.parse::<u32>().ok())
                        == Some(proc.ppid)
            })
            .max_by_key(|proc| proc.start)
            .map(|proc| self.command(proc))
    }

    /// Самая свежая команда сессии Codex: первый процесс под обёртками песочницы,
    /// у которого в окружении номер этой сессии.
    fn codex_command(&self, sid: &str) -> Option<Activity> {
        self.list
            .iter()
            .filter(|proc| !CODEX_WRAPPERS.contains(&proc.comm.as_str()))
            .filter(|proc| {
                self.get(proc.ppid)
                    .is_some_and(|parent| CODEX_WRAPPERS.contains(&parent.comm.as_str()))
            })
            .filter(|proc| {
                self.environ(proc.pid)
                    .get("CODEX_THREAD_ID")
                    .map(String::as_str)
                    == Some(sid)
            })
            .max_by_key(|proc| proc.start)
            .map(|proc| self.command(proc))
    }

    /// Жив ли Codex этой сессии: процесс его окна (не общий фоновый процесс и не сервер
    /// расширения VS Code) с тем же рабочим каталогом. Окно ищется только у сессий в
    /// zellij: у прочих его может и не быть, и отсутствие ничего не значит.
    fn codex_window(&self, session: &Session) -> Activity {
        let window = self.list.iter().any(|proc| {
            let args = self.cmdline(proc.pid);
            detect::is_codex(&args)
                && !args
                    .iter()
                    .any(|arg| arg == "app-server" || arg == "exec-server")
                && fs::read_link(self.dir(proc.pid).join("cwd"))
                    .ok()
                    .as_deref()
                    == Some(Path::new(&session.cwd))
        });
        if window {
            Activity::Stuck
        } else if session.place.starts_with("zellij:") {
            Activity::Gone
        } else {
            Activity::Unknown
        }
    }

    /// Команда для показа: текст и время запуска.
    fn command(&self, proc: &Proc) -> Activity {
        Activity::Command {
            text: command_text(&self.cmdline(proc.pid)),
            since: self.unix_start(proc),
        }
    }

    /// Процессы снимка.
    pub(crate) fn list(&self) -> &[Proc] {
        &self.list
    }

    /// Время запуска процесса в unix-секундах.
    pub(crate) fn unix_start(&self, proc: &Proc) -> i64 {
        self.boot + i64::try_from(proc.start / self.ticks).unwrap_or(0)
    }

    fn get(&self, pid: u32) -> Option<&Proc> {
        self.by_pid
            .get(&pid)
            .and_then(|index| self.list.get(*index))
    }

    /// Каталог процесса в `/proc`.
    pub(crate) fn dir(&self, pid: u32) -> PathBuf {
        self.root.join(pid.to_string())
    }

    fn cmdline(&self, pid: u32) -> Vec<String> {
        detect::cmdline(&self.root, pid)
    }

    /// Окружение процесса; пустое, если его нельзя прочитать (чужой процесс).
    pub(crate) fn environ(&self, pid: u32) -> HashMap<String, String> {
        fs::read(self.dir(pid).join("environ"))
            .map(|bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter_map(|item| {
                        let text = String::from_utf8_lossy(item);
                        let (key, value) = text.split_once('=')?;
                        Some((key.to_string(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Номер родителя, имя и время запуска из `/proc/<pid>/stat`.
fn read_stat(root: &Path, pid: u32) -> Option<Proc> {
    let stat = fs::read_to_string(root.join(pid.to_string()).join("stat")).ok()?;
    // Имя в скобках может содержать пробелы и скобки: поля считаем после последней «)».
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // После имени: состояние (поле 3), родитель (4), …, время запуска (22).
    Some(Proc {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        comm,
        start: fields
            .get(19)
            .and_then(|start| start.parse().ok())
            .unwrap_or(0),
    })
}

/// Команда одной строкой. У оболочки (`zsh -c 'скрипт'`) — сам скрипт, а у оболочки
/// Claude Code, которая сначала загружает снимок настроек, — то, что стоит после `eval`.
fn command_text(args: &[String]) -> String {
    let script = args
        .first()
        .filter(|program| SHELLS.contains(&base_name(program).as_str()))
        .and_then(|_| {
            let flag = args.iter().position(|arg| {
                arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('c')
            })?;
            args.get(flag + 1)
        });
    let text = match script {
        Some(script) => script
            .find(" eval ")
            .map(|at| shell_word(&script[at + " eval ".len()..]))
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| script.clone()),
        None => args.join(" "),
    };
    text::clip(&text::oneline(&text), 80)
}

/// Первое слово оболочки с раскрытыми кавычками: `'a'"'"'b' c` даёт `a'b`.
fn shell_word(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.trim_start().chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => out.extend(chars.by_ref().take_while(|ch| *ch != '\'')),
            '"' => {
                while let Some(ch) = chars.next() {
                    match ch {
                        '"' => break,
                        '\\' => out.extend(chars.next()),
                        other => out.push(other),
                    }
                }
            }
            '\\' => out.extend(chars.next()),
            ch if ch.is_whitespace() => break,
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakeproc::FakeProc;

    fn session(agent: &str, sid: &str, cwd: &str, place: &str) -> Session {
        Session {
            agent: agent.to_string(),
            session_id: sid.to_string(),
            cwd: cwd.to_string(),
            place: place.to_string(),
            ..Session::default()
        }
    }

    /// Команда Claude Code, как её запускает Bash: снимок настроек, потом `eval`.
    const CLAUDE_SHELL: &str = "source /home/u/.claude/shell-snapshots/snapshot-zsh-1.sh \
        2>/dev/null || true && { \\builtin unalias -- 'which'; } >/dev/null 2>&1 || true \
        && eval 'cargo test 2>&1 | grep '\"'\"'result'\"'\"'' < /dev/null && pwd -P >| /tmp/claude-1-cwd";

    #[test]
    fn claude_command_is_a_shell_started_by_the_agent() {
        let proc = FakeProc::new("claude-cmd");
        let env = ["CLAUDE_CODE_SESSION_ID=s1", "CLAUDE_PID=100"];
        proc.add_at(100, 1, &["claude"], "/w", &[], 50);
        // Сервер MCP с той же переменной — не команда.
        proc.add_at(
            110,
            100,
            &["uv", "tool", "uvx", "mcp-server"],
            "/w",
            &env,
            60,
        );
        proc.add_at(
            120,
            100,
            &["/usr/bin/zsh", "-c", CLAUDE_SHELL],
            "/w",
            &env,
            300,
        );
        // Оболочка другой сессии того же агента.
        proc.add_at(
            130,
            100,
            &["/usr/bin/zsh", "-c", "sleep 1"],
            "/w",
            &["CLAUDE_CODE_SESSION_ID=s2", "CLAUDE_PID=100"],
            400,
        );
        let procs = Procs::scan_in(&proc.root, 100);
        let mut s = session("claude", "s1", "/w", "zellij:dev#4");
        s.process = Some(AgentProcess {
            pid: 100,
            start: 50,
        });
        assert_eq!(
            procs.activity(&s),
            Activity::Command {
                text: "cargo test 2>&1 | grep 'result'".to_string(),
                since: 1_000_003,
            }
        );
    }

    #[test]
    fn claude_without_commands_is_stuck_or_gone() {
        let proc = FakeProc::new("claude-alive");
        proc.add_at(100, 1, &["claude"], "/w", &[], 50);
        proc.add_at(
            110,
            100,
            &["uv", "mcp-server"],
            "/w",
            &["CLAUDE_CODE_SESSION_ID=s1"],
            60,
        );
        let procs = Procs::scan_in(&proc.root, 100);
        let mut s = session("claude", "s1", "/w", "zellij:dev#4");
        assert_eq!(procs.activity(&s), Activity::Unknown);
        s.process = Some(AgentProcess {
            pid: 100,
            start: 50,
        });
        assert_eq!(procs.activity(&s), Activity::Stuck);
        // Тот же номер, но другой процесс: прежнего агента нет.
        s.process = Some(AgentProcess {
            pid: 100,
            start: 49,
        });
        assert_eq!(procs.activity(&s), Activity::Gone);
        s.process = Some(AgentProcess {
            pid: 999,
            start: 50,
        });
        assert_eq!(procs.activity(&s), Activity::Gone);
    }

    #[test]
    fn codex_command_runs_under_the_shared_daemon() {
        let proc = FakeProc::new("codex-cmd");
        let daemon = "/home/u/.codex/packages/app-server-daemon/bin/codex";
        let thread = ["CODEX_THREAD_ID=t1", "CODEX_SESSION_ID=t1"];
        proc.add(
            10,
            1,
            &[daemon, "app-server", "--listen", "unix://"],
            "/",
            &[],
        );
        proc.add(11, 10, &["uv", "mcp-server"], "/", &[]);
        proc.add(
            20,
            10,
            &[
                "/home/u/.codex/tmp/codex-linux-sandbox",
                "--sandbox-policy-cwd",
                "/w",
            ],
            "/w",
            &thread,
        );
        proc.add(21, 20, &["bwrap", "--as-pid-1"], "/w", &thread);
        proc.add(
            22,
            21,
            &["codex-linux-sandbox", "--command-cwd", "/w"],
            "/w",
            &thread,
        );
        proc.add_at(
            23,
            22,
            &["/usr/bin/zsh", "-c", "cd app && pytest -q"],
            "/w",
            &thread,
            500,
        );
        proc.add_at(
            24,
            23,
            &["python", "-m", "pytest", "-q"],
            "/w",
            &thread,
            510,
        );
        let procs = Procs::scan_in(&proc.root, 100);
        assert_eq!(
            procs.activity(&session("codex", "t1", "/w", "zellij:dev#3")),
            Activity::Command {
                text: "cd app && pytest -q".to_string(),
                since: 1_000_005,
            }
        );
    }

    #[test]
    fn codex_window_tells_stuck_from_gone() {
        let proc = FakeProc::new("codex-window");
        let tui = "/home/u/node_modules/@openai/codex-linux-x64/vendor/bin/codex";
        let daemon = "/home/u/.codex/packages/app-server-daemon/bin/codex";
        proc.add(10, 1, &[daemon, "app-server"], "/other", &[]);
        proc.add(20, 1, &[tui, "resume"], "/w", &[]);
        proc.add(30, 1, &["/vscode/codex", "app-server"], "/vs", &[]);
        let procs = Procs::scan_in(&proc.root, 100);
        assert_eq!(
            procs.activity(&session("codex", "t1", "/w", "zellij:dev#3")),
            Activity::Stuck
        );
        // Окна в этом каталоге нет: в zellij это значит, что Codex закрыт.
        assert_eq!(
            procs.activity(&session("codex", "t2", "/other", "zellij:dev#3")),
            Activity::Gone
        );
        // Сервер расширения VS Code — не окно, и судить по нему нельзя.
        assert_eq!(
            procs.activity(&session("codex", "t3", "/vs", "vscode")),
            Activity::Unknown
        );
        assert_eq!(
            procs.activity(&session("gemini", "g1", "/w", "zellij:dev#1")),
            Activity::Unknown
        );
    }

    #[test]
    fn without_proc_nothing_is_known() {
        // Так выглядит система без `/proc`, например macOS: процессов не видно вовсе.
        let procs = Procs::scan_in(Path::new("/nonexistent/disp-proc"), 100);
        let mut claude = session("claude", "s1", "/w", "zellij:dev#4");
        claude.process = Some(AgentProcess {
            pid: 100,
            start: 50,
        });
        assert_eq!(procs.activity(&claude), Activity::Unknown);
        assert_eq!(
            procs.activity(&session("codex", "t1", "/w", "zellij:dev#3")),
            Activity::Unknown
        );
    }

    #[test]
    fn hook_takes_the_claude_process_of_its_own_session_only() {
        let proc = FakeProc::new("claude-hook");
        proc.add_at(100, 1, &["claude"], "/w", &[], 777);
        assert_eq!(
            claude_process_from(&proc.root, "s1", Some("s1"), Some("100")),
            Some(AgentProcess {
                pid: 100,
                start: 777
            })
        );
        assert_eq!(
            claude_process_from(&proc.root, "s1", Some("s2"), Some("100")),
            None
        );
        assert_eq!(
            claude_process_from(&proc.root, "s1", None, Some("100")),
            None
        );
        assert_eq!(
            claude_process_from(&proc.root, "s1", Some("s1"), Some("999")),
            None
        );
    }

    #[test]
    fn command_text_unwraps_shells() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        assert_eq!(command_text(&args(&["sleep", "41"])), "sleep 41");
        assert_eq!(
            command_text(&args(&["/bin/bash", "-lc", "make\ntest"])),
            "make test"
        );
        assert_eq!(
            command_text(&args(&[
                "zsh",
                "-c",
                "x && eval \"echo \\\"hi\\\"\" < /dev/null"
            ])),
            "echo \"hi\""
        );
        assert_eq!(shell_word("'a'\"'\"'b' c"), "a'b");
    }
}
