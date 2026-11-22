//! Агенты в контейнерах: их видно без хуков и без настройки самих контейнеров.
//!
//! Хуки сообщают о сессии изнутри агента. В контейнере их обычно нет, а если и есть, то
//! пишут в каталог состояния контейнера. Наблюдатель смотрит в контейнер снаружи и по
//! файлам агента узнаёт, что тот делает:
//!
//! - Claude Code ведёт на каждый процесс `~/.claude/sessions/<pid>.json`: номер сессии,
//!   каталог и `status` — `busy`, `waiting` (причина — в `waitingFor`) или `idle`;
//! - Codex пишет журнал сессии `~/.codex/sessions/…/rollout-*.jsonl`: ход начался
//!   (`task_started`) и закончился (`task_complete`). Запросы разрешения в журнал не
//!   попадают, поэтому Codex, ждущий разрешения, выглядит работающим.
//!
//! Заглянуть в контейнер можно двумя путями. На Linux ядро у контейнера общее с хостом:
//! его процессы видны в `/proc`, а файлы — через `/proc/<pid>/root`, если агент запущен
//! от того же пользователя (dev-контейнеры так и делают). Контейнер узнаётся по cgroup
//! движка (Docker, Podman, containerd, CRI-O), так что агенты в песочницах вроде snap
//! и flatpak, у которых свои хуки, сюда не попадают. Где так не видно — на macOS, где
//! контейнеры живут в виртуальной машине, или в контейнере под другим пользователем, —
//! смотрит сам движок: `docker exec` (или `podman exec`), см. `exec`.
//!
//! Замеченные перемены становятся теми же событиями, что присылают хуки (`hook::step`):
//! уведомления, журнал смен статуса и приёмка работают как обычно.

mod exec;

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::config::{self, Settings};
use crate::detect;
use crate::error::Result;
use crate::hook::{self, Action, Context};
use crate::model::{Session, Status, SubAgent};
use crate::notify;
use crate::procs::Procs;
use crate::store::{self, Store};
use crate::text::oneline;
use crate::tr;

/// Как часто смотреть в контейнеры через `/proc` из одного процесса.
const EVERY: Duration = Duration::from_secs(2);
/// Как часто смотреть командой движка: каждый взгляд — запуск `docker exec`.
const EXEC_EVERY: Duration = Duration::from_secs(5);
/// Раз в сколько секунд обновлять время у сессии, которая всё ещё работает: иначе через
/// `DISP_STALE_MIN` минут без событий она числилась бы молчащей.
const TOUCH_SEC: i64 = 60;
/// Сколько байт с конца журнала читать в поисках промпта, ответа и смены хода.
const TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Сколько байт с конца списка сессий Codex читать в поисках названия.
const INDEX_BYTES: u64 = 256 * 1024;
/// События журнала Codex, которые отмечают границы хода.
const TURN_EVENTS: [&str; 3] = ["task_started", "task_complete", "turn_aborted"];
/// Префиксы cgroup, по которым узнаётся контейнер: Docker, Podman, containerd, CRI-O.
const ENGINES: [&str; 4] = ["docker-", "libpod-", "cri-containerd-", "crio-"];

/// Что делает агент, замеченный в контейнере.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Идёт ход.
    Busy,
    /// Ждёт человека; причина словами агента (`permission prompt`, `input needed`).
    Waiting(String),
    /// Хода нет: закончен или ещё не начинался.
    Idle,
}

/// Что агент пишет о сессии; пути — внутри контейнера.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Log {
    /// Транскрипт Claude Code.
    Claude { transcript: String },
    /// Журнал сессии Codex и список названий его сессий.
    Codex { rollout: String, index: String },
}

/// Как добраться до файлов контейнера.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Access {
    /// Напрямую: корень файловой системы контейнера, как его видно с хоста
    /// (`/proc/<pid>/root`).
    Root(PathBuf),
    /// Командой движка: программа `docker` или `podman`.
    Exec(PathBuf),
}

/// Сессия агента в контейнере.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// Агент: `claude` или `codex`.
    pub agent: &'static str,
    /// Идентификатор сессии.
    pub sid: String,
    /// Каталог сессии внутри контейнера.
    pub cwd: String,
    /// Контейнер: первые 12 знаков его идентификатора, как в `docker ps`.
    pub container: String,
    /// Что агент делает.
    pub state: State,
    /// Когда агент последний раз писал журнал сессии, unix-секунды. У Codex только это
    /// и говорит, что ход ещё идёт: журнал не связан с процессом. `None` — за сессией
    /// стоит проверенный процесс (Claude Code).
    pub activity: Option<i64>,
    access: Access,
    log: Log,
}

/// Промпт, ответ, название сессии и задача: их читаем, только когда что-то изменилось.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Details {
    prompt: Option<String>,
    reply: Option<String>,
    title: Option<String>,
    /// Задача и ветка git по каталогу сессии.
    project: Option<(String, String)>,
}

/// Что удалось увидеть за один взгляд в контейнеры.
#[derive(Clone, Debug, Default)]
pub struct Look {
    /// Замеченные сессии.
    pub seen: Vec<Seen>,
    /// Смотрели во все контейнеры, кроме `blind`; иначе — только в `inspected`.
    pub everywhere: bool,
    /// Контейнеры с агентами, в которые заглянуть не удалось.
    pub blind: HashSet<String>,
    /// Контейнеры, в которые заглянули командой движка.
    pub inspected: HashSet<String>,
}

impl Look {
    /// Известно ли о контейнере всё: тогда его сессия, которой нет среди замеченных,
    /// закончилась — агент вышел или контейнер остановлен.
    fn covers(&self, container: &str) -> bool {
        self.inspected.contains(container) || (self.everywhere && !self.blind.contains(container))
    }
}

/// Смотрит в контейнеры в фоновом потоке, пока жив процесс: значок, панель и окно
/// не ждут `docker exec`, а перемены видят по файлам сессий, как от хуков. Поток один
/// на процесс, повторные вызовы ничего не делают.
pub fn watch(store: Store) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let _ = thread::Builder::new()
        .name("disp-observe".to_string())
        .spawn(move || {
            loop {
                sync(&store);
                thread::sleep(EVERY);
            }
        });
}

/// Сверяет сессии в контейнерах с файлами диспетчера: перемены записывает и рассылает
/// о них уведомления, как хук. Из одного процесса — не чаще раза в `EVERY` (командой
/// движка — раз в `EXEC_EVERY`). `DISP_CONTAINERS=0` выключает наблюдение, `exec` —
/// смотрит только командой движка. Перед этим закрывает сессии Claude Code, ушедшие
/// в фон (`crate::handover`), и обновляет списки агентов, которых запустили сессии
/// (`crate::agents`): об этом хуки тоже не сообщают.
pub fn sync(store: &Store) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    static LAST_EXEC: Mutex<Option<Instant>> = Mutex::new(None);
    if !due(&LAST, EVERY) {
        return;
    }
    crate::handover::sweep(store);
    crate::agents::sweep(store);
    let settings = Settings::load(&store.paths.conf_file);
    let mode = settings.get("DISP_CONTAINERS");
    if mode == Some("0") {
        return;
    }
    let own = fs::read_link("/proc/self/ns/mnt")
        .ok()
        .filter(|_| mode != Some("exec"));
    let look = match own {
        Some(own) => {
            let mut look = scan(&Procs::scan(), &own.to_string_lossy());
            // Агенты, запущенные в контейнере другим пользователем: туда — командой движка.
            if !look.blind.is_empty()
                && due(&LAST_EXEC, EXEC_EVERY)
                && let Some(engine) = exec::engine()
            {
                let blind: Vec<String> = look.blind.iter().cloned().collect();
                for container in blind {
                    if let Some(seen) = exec::inspect(&engine, &container) {
                        look.blind.remove(&container);
                        look.inspected.insert(container);
                        look.seen.extend(seen);
                    }
                }
            }
            look
        }
        // macOS: контейнеры в виртуальной машине, их процессов на хосте нет.
        None => {
            if !due(&LAST_EXEC, EXEC_EVERY) {
                return;
            }
            let Some(engine) = exec::engine() else {
                return;
            };
            exec::look(&engine)
        }
    };
    apply(store, &settings, &look, config::now());
}

/// Прошло ли с прошлого раза `every`; если да, отмечает этот раз.
fn due(last: &Mutex<Option<Instant>>, every: Duration) -> bool {
    let Ok(mut last) = last.lock() else {
        return true;
    };
    if last.is_some_and(|at| at.elapsed() < every) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// Сессии агентов во всех контейнерах снимка `procs`. `own_mnt` — своё пространство
/// имён файловой системы (`mnt:[…]`): процессы в нём — не в контейнере.
pub fn scan(procs: &Procs, own_mnt: &str) -> Look {
    let mut look = Look {
        everywhere: true,
        ..Look::default()
    };
    // Процессы Codex одного контейнера с общим каталогом Codex: сессии у них общие,
    // а смотреть их стоит с запуска самого раннего.
    let mut codex: HashMap<(String, String), (i64, PathBuf)> = HashMap::new();
    for proc in procs.list() {
        if proc.comm != "claude" && proc.comm != "codex" {
            continue;
        }
        let dir = procs.dir(proc.pid);
        // cgroup читается у любого процесса, поэтому контейнер узнаём по ней первой.
        let Some(container) = fs::read_to_string(dir.join("cgroup"))
            .ok()
            .as_deref()
            .and_then(container_id)
        else {
            continue;
        };
        match fs::read_link(dir.join("ns/mnt")) {
            Ok(mnt) if mnt.as_os_str() == own_mnt => continue,
            Ok(_) => {}
            // Агент другого пользователя: о нём ничего не известно, но он есть.
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                look.blind.insert(container);
                continue;
            }
            // Процесс успел завершиться.
            Err(_) => continue,
        }
        let root = dir.join("root");
        if fs::read(dir.join("environ")).is_err() || fs::read_dir(&root).is_err() {
            look.blind.insert(container);
            continue;
        }
        let env = procs.environ(proc.pid);
        let home = env.get("HOME").cloned().unwrap_or_default();
        let setting = |key: &str, default: &str| {
            env.get(key)
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("{home}/{default}"))
        };
        if proc.comm == "claude" {
            let config = setting("CLAUDE_CONFIG_DIR", ".claude");
            let pid = ns_pid(&dir).unwrap_or(proc.pid);
            let file = inside(&root, &format!("{config}/sessions/{pid}.json"));
            let Some((sid, cwd, state)) = fs::read_to_string(file)
                .ok()
                .and_then(|text| claude_file(&text, proc.start))
            else {
                continue;
            };
            look.seen.push(Seen {
                agent: "claude",
                log: claude_log(&config, &cwd, &sid),
                sid,
                cwd,
                container,
                state,
                activity: None,
                access: Access::Root(root),
            });
        } else {
            let since = procs.unix_start(proc);
            codex
                .entry((container, setting("CODEX_HOME", ".codex")))
                .and_modify(|(first, _)| *first = (*first).min(since))
                .or_insert((since, root));
        }
    }
    for ((container, home), (since, root)) in codex {
        look.seen
            .extend(codex_sessions(&root, &home, since, &container));
    }
    look
}

/// Контейнер процесса по его cgroup: первые 12 знаков идентификатора. `None` — процесс
/// не в контейнере известного движка.
fn container_id(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let path = line.splitn(3, ':').nth(2)?;
        path.split('/').find_map(|part| {
            let id = ENGINES
                .iter()
                .find_map(|engine| part.strip_prefix(engine))
                .map(|rest| rest.trim_end_matches(".scope"))
                // cgroup v1: `/docker/<id>`.
                .unwrap_or(part);
            is_container_id(id).then(|| id[..12].to_string())
        })
    })
}

/// 64 шестнадцатеричных знака: так выглядит идентификатор контейнера.
fn is_container_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Путь внутри контейнера, как его видно с хоста.
fn inside(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

/// Номер процесса в его собственном пространстве имён: последний в строке `NSpid`.
fn ns_pid(proc_dir: &Path) -> Option<u32> {
    let status = fs::read_to_string(proc_dir.join("status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("NSpid:"))?;
    line.split_whitespace().last()?.parse().ok()
}

/// Номер, каталог и состояние сессии из файла Claude Code `sessions/<pid>.json`.
/// `start` — время запуска процесса в тиках: файл мог остаться от прежнего процесса
/// с тем же номером.
fn claude_file(text: &str, start: u64) -> Option<(String, String, State)> {
    let file: Map<String, Value> = serde_json::from_str(text).ok()?;
    let recorded = file.get("procStart").and_then(|start| match start {
        Value::String(text) => text.parse::<u64>().ok(),
        other => other.as_u64(),
    });
    if recorded.is_some_and(|recorded| recorded != start) {
        return None;
    }
    let sid = hook::sanitize(file.get("sessionId")?.as_str()?);
    if sid.is_empty() {
        return None;
    }
    let cwd = file
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let state = match file.get("status").and_then(Value::as_str) {
        Some("busy") => State::Busy,
        Some("waiting") => State::Waiting(
            file.get("waitingFor")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => State::Idle,
    };
    Some((sid, cwd, state))
}

/// Транскрипт сессии Claude Code: `<config>/projects/<каталог>/<сессия>.jsonl`, где
/// в каталоге всё, кроме латинских букв и цифр, заменено дефисом.
fn claude_log(config: &str, cwd: &str, sid: &str) -> Log {
    let slug: String = cwd
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    Log::Claude {
        transcript: format!("{config}/projects/{slug}/{sid}.jsonl"),
    }
}

/// Сессии Codex с каталогом `home` (путь внутри контейнера с корнем `root`), чьи журналы
/// менялись с момента `since`.
fn codex_sessions(root: &Path, home: &str, since: i64, container: &str) -> Vec<Seen> {
    let index = format!("{home}/session_index.jsonl");
    rollouts(&inside(root, &format!("{home}/sessions")), since)
        .into_iter()
        .filter_map(|rollout| {
            let meta = codex_meta(&rollout)?;
            let state = codex_state(&rollout);
            let inner = rollout.strip_prefix(root).ok()?.to_string_lossy().into_owned();
            Some(Seen {
                agent: "codex",
                sid: meta.sid,
                cwd: meta.cwd,
                container: container.to_string(),
                state,
                activity: modified(&rollout),
                access: Access::Root(root.to_path_buf()),
                log: Log::Codex {
                    rollout: format!("/{inner}"),
                    index: index.clone(),
                },
            })
        })
        .collect()
}

/// Журналы сессий Codex (`<sessions>/ГГГГ/ММ/ДД/rollout-*.jsonl`), изменённые с `since`.
/// Смотрятся все дни: каталог назван по дню создания сессии, а продолжить можно и
/// давнюю.
fn rollouts(sessions: &Path, since: i64) -> Vec<PathBuf> {
    let since_time = UNIX_EPOCH + Duration::from_secs(u64::try_from(since).unwrap_or(0));
    let numbered = |dir: &Path| -> Vec<(u32, PathBuf)> {
        fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let number = entry.file_name().to_str()?.parse().ok()?;
                        Some((number, entry.path()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut found = Vec::new();
    for (_, year_dir) in numbered(sessions) {
        for (_, month_dir) in numbered(&year_dir) {
            for (_, day_dir) in numbered(&month_dir) {
                let Ok(files) = fs::read_dir(&day_dir) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let name = name.to_string_lossy();
                    let changed = file
                        .metadata()
                        .and_then(|meta| meta.modified())
                        .is_ok_and(|modified| modified >= since_time);
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") && changed {
                        found.push(file.path());
                    }
                }
            }
        }
    }
    found.sort();
    found
}

/// Время изменения файла, unix-секунды.
fn modified(path: &Path) -> Option<i64> {
    let time = fs::metadata(path).ok()?.modified().ok()?;
    i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()
}

/// Первая строка журнала Codex: номер и каталог сессии.
#[derive(Clone, Debug)]
struct Meta {
    sid: String,
    cwd: String,
}

/// Номер и каталог сессии Codex из первой строки журнала. `None` — не журнал сессии или
/// служебный поток (проверка Guardian, субагент): у них источник — объект, у сессий
/// человека — строка (`vscode`, `cli`).
fn codex_meta(rollout: &Path) -> Option<Meta> {
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<Meta>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some(known) = CACHE.lock().ok().and_then(|cache| cache.get(rollout).cloned()) {
        return known;
    }
    let mut line = String::new();
    BufReader::new(File::open(rollout).ok()?)
        .read_line(&mut line)
        .ok()?;
    // Первая строка ещё не дописана: сессия только создаётся, посмотрим в следующий раз.
    let first = serde_json::from_str::<Value>(&line).ok()?;
    let meta = (|| {
        let payload = first.get("payload")?;
        let user = first.get("type")? == "session_meta" && payload.get("source")?.is_string();
        let sid = hook::sanitize(payload.get("id")?.as_str()?);
        (user && !sid.is_empty()).then(|| Meta {
            sid,
            cwd: payload
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    })();
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(rollout.to_path_buf(), meta.clone());
    }
    meta
}

/// Дочерний агент Codex: первая строка его журнала.
#[derive(Clone, Debug)]
struct Child {
    id: String,
    parent: String,
    name: String,
    task: String,
    started: i64,
}

/// Дочерние агенты сессий Codex `parents` (номер сессии → начало её хода) по
/// журналам в каталоге `sessions`. Первая строка журнала агента ссылается на родительскую
/// сессию: `source.subagent.thread_spawn.parent_thread_id`. Служебные потоки Codex
/// (проверки Guardian) агентами сессии не считаются. Агент работает, пока в его журнале
/// идёт ход и журнал менялся не дольше `stale` секунд назад.
pub(crate) fn codex_children(
    sessions: &Path,
    parents: &HashMap<String, i64>,
    now: i64,
    stale: i64,
) -> HashMap<String, Vec<SubAgent>> {
    let mut out: HashMap<String, Vec<SubAgent>> = HashMap::new();
    // Работающий агент мог начаться и до нынешнего хода родителя: смотрим и всё окно,
    // в котором он ещё может считаться работающим.
    let Some(since) = parents.values().min().map(|&turn| turn.min(now - stale)) else {
        return out;
    };
    for rollout in rollouts(sessions, since) {
        let Some(child) = codex_child(&rollout) else {
            continue;
        };
        if !parents.contains_key(&child.parent) {
            continue;
        }
        let updated = modified(&rollout).unwrap_or(0);
        let running = codex_state(&rollout) == State::Busy && now - updated <= stale;
        out.entry(child.parent).or_default().push(SubAgent {
            id: child.id,
            kind: child.name,
            what: child.task,
            background: false,
            running,
            started: if child.started > 0 {
                child.started
            } else {
                updated
            },
            updated,
        });
    }
    out
}

/// Дочерний агент Codex по первой строке журнала; `None` — журнал не дочернего агента.
fn codex_child(rollout: &Path) -> Option<Child> {
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, Option<Child>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some(known) = CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.get(rollout).cloned())
    {
        return known;
    }
    let mut line = String::new();
    BufReader::new(File::open(rollout).ok()?)
        .read_line(&mut line)
        .ok()?;
    // Первая строка ещё не дописана: посмотрим в следующий раз.
    let first = serde_json::from_str::<Value>(&line).ok()?;
    let child = (|| {
        if first.get("type")? != "session_meta" {
            return None;
        }
        let payload = first.get("payload")?;
        let spawn = payload
            .get("source")?
            .get("subagent")?
            .get("thread_spawn")?;
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        // Путь агента в дереве: `/root` — сама сессия, `/root/errors` — агент «errors».
        let path = text(spawn, "agent_path");
        let task = path.strip_prefix("/root/").unwrap_or(&path).to_string();
        let name = [text(spawn, "agent_nickname"), text(spawn, "agent_role")]
            .into_iter()
            .find(|name| !name.is_empty())
            .unwrap_or_default();
        let started = payload
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .map_or(0, |time| time.timestamp());
        Some(Child {
            id: hook::sanitize(&text(payload, "id")),
            parent: hook::sanitize(&text(spawn, "parent_thread_id")),
            name,
            task,
            started,
        })
    })();
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(rollout.to_path_buf(), child.clone());
    }
    child
}

/// Идёт ли ход в сессии Codex: последнее из `task_started`, `task_complete`,
/// `turn_aborted` в журнале. Пока журнал не менялся, ответ берётся из памяти.
fn codex_state(rollout: &Path) -> State {
    type Stamp = (u64, SystemTime);
    static CACHE: LazyLock<Mutex<HashMap<PathBuf, (Stamp, State)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let stamp = fs::metadata(rollout)
        .ok()
        .and_then(|meta| Some((meta.len(), meta.modified().ok()?)));
    if let Some(stamp) = stamp
        && let Some((known, state)) = CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.get(rollout).cloned())
        && known == stamp
    {
        return state;
    }
    let (lines, whole) = tail_lines(rollout, TAIL_BYTES);
    // Слова могут встретиться и в тексте сообщения: событием считается только тип.
    let last = lines.iter().rev().find_map(|line| {
        TURN_EVENTS
            .iter()
            .any(|kind| line.contains(kind))
            .then(|| event_kind(line))
            .flatten()
            .filter(|kind| TURN_EVENTS.contains(&kind.as_str()))
    });
    let state = turn_state(last.as_deref(), whole);
    if let (Some(stamp), Ok(mut cache)) = (stamp, CACHE.lock()) {
        cache.insert(rollout.to_path_buf(), (stamp, state.clone()));
    }
    state
}

/// Состояние сессии Codex по последнему событию хода; `whole` — журнал прочитан целиком.
fn turn_state(last: Option<&str>, whole: bool) -> State {
    match last {
        Some("task_started") => State::Busy,
        Some(_) => State::Idle,
        // Хвост журнала без смены хода: ход идёт так долго, что журнал вырос дальше
        // прочитанного. Журнал без единого хода — сессия только открыта.
        None if !whole => State::Busy,
        None => State::Idle,
    }
}

/// Тип события Codex в строке журнала (`event_msg` с `payload.type`).
fn event_kind(line: &str) -> Option<String> {
    let record: Value = serde_json::from_str(line).ok()?;
    if record.get("type")? != "event_msg" {
        return None;
    }
    Some(record.get("payload")?.get("type")?.as_str()?.to_string())
}

/// Строки из последних `bytes` байт файла, без первой неполной; и прочитан ли файл
/// целиком.
pub(crate) fn tail_lines(path: &Path, bytes: u64) -> (Vec<String>, bool) {
    let Ok(mut file) = File::open(path) else {
        return (Vec::new(), true);
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    let from = len.saturating_sub(bytes);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return (Vec::new(), true);
    }
    let mut data = Vec::new();
    if file.read_to_end(&mut data).is_err() {
        return (Vec::new(), true);
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&data)
        .lines()
        .map(str::to_string)
        .collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    (lines, from == 0)
}

impl Seen {
    /// Промпт, ответ, название и задача сессии из того, что пишет агент.
    fn details(&self) -> Details {
        match &self.access {
            Access::Root(root) => {
                let read = |path: &str, bytes| tail_lines(&inside(root, path), bytes).0;
                let mut details = self.log.details(&self.sid, read);
                details.project = Some(detect::project(
                    &inside(root, &self.cwd).to_string_lossy(),
                ));
                details
            }
            Access::Exec(engine) => {
                exec::details(engine, &self.container, &self.log, &self.sid, &self.cwd)
            }
        }
    }
}

impl Log {
    /// Промпт, ответ и название сессии; `read` отдаёт строки из конца файла контейнера,
    /// не больше заданного числа байт.
    fn details(&self, sid: &str, read: impl Fn(&str, u64) -> Vec<String>) -> Details {
        match self {
            Log::Claude { transcript } => claude_details(&read(transcript, TAIL_BYTES)),
            Log::Codex { rollout, index } => codex_details(
                &read(rollout, TAIL_BYTES),
                &read(index, INDEX_BYTES),
                sid,
            ),
        }
    }
}

/// Последний промпт, последний ответ и название из строк транскрипта Claude Code:
/// записи `last-prompt`, `assistant` и `ai-title`.
fn claude_details(lines: &[String]) -> Details {
    let mut details = Details::default();
    for line in lines.iter().rev() {
        let wanted = (details.prompt.is_none() && line.contains("\"last-prompt\""))
            || (details.title.is_none() && line.contains("\"ai-title\""))
            || (details.reply.is_none() && line.contains("\"assistant\""));
        if !wanted {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("last-prompt") if details.prompt.is_none() => {
                details.prompt = text_field(&record, "lastPrompt");
            }
            Some("ai-title") if details.title.is_none() => {
                details.title = text_field(&record, "aiTitle");
            }
            Some("assistant") if details.reply.is_none() => {
                details.reply = record
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(texts);
            }
            _ => {}
        }
        if details.prompt.is_some() && details.title.is_some() && details.reply.is_some() {
            break;
        }
    }
    details
}

/// Последний промпт и последний ответ из строк журнала Codex, название — из строк списка
/// его сессий.
fn codex_details(rollout: &[String], index: &[String], sid: &str) -> Details {
    let mut details = Details::default();
    for line in rollout.iter().rev() {
        let wanted = (details.reply.is_none() && line.contains("\"task_complete\""))
            || (details.prompt.is_none() && line.contains("\"user\""));
        if !wanted {
            continue;
        }
        let Some(payload) = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|record| record.get("payload").cloned())
        else {
            continue;
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("task_complete") if details.reply.is_none() => {
                details.reply = text_field(&payload, "last_agent_message");
            }
            // Сообщения от имени человека, которые подставляет сам Codex (окружение,
            // AGENTS.md), начинаются с разметки — их пропускаем.
            Some("message")
                if details.prompt.is_none()
                    && payload.get("role").and_then(Value::as_str) == Some("user") =>
            {
                details.prompt = payload
                    .get("content")
                    .and_then(texts)
                    .filter(|text| !text.starts_with('<') && !text.starts_with("# AGENTS.md"));
            }
            _ => {}
        }
        if details.prompt.is_some() && details.reply.is_some() {
            break;
        }
    }
    details.title = index.iter().rev().find_map(|line| {
        let record: Value = serde_json::from_str(line).ok()?;
        (record.get("id")?.as_str()? == sid)
            .then(|| text_field(&record, "thread_name"))
            .flatten()
    });
    details
}

/// Непустое строковое поле одной строкой.
fn text_field(record: &Value, key: &str) -> Option<String> {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(oneline)
        .filter(|text| !text.is_empty())
}

/// Текст сообщения: строка или текстовые части списка.
fn texts(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    Some(oneline(&text)).filter(|text| !text.is_empty())
}

/// Применяет взгляд в контейнеры к хранилищу: перемены записывает как события хуков,
/// сессии, которых больше нет, закрывает. Возвращает, сколько записей изменилось.
pub fn apply(store: &Store, settings: &Settings, look: &Look, now: i64) -> usize {
    // Одна сессия в двух процессах (окно VS Code открыли заново) учитывается раз.
    let mut known = HashSet::new();
    let seen: Vec<&Seen> = look
        .seen
        .iter()
        .filter(|one| known.insert((one.agent, one.sid.as_str())))
        .collect();
    if !seen.is_empty() && store.ensure_dirs().is_err() {
        return 0;
    }
    let mut changed = 0;
    for one in &seen {
        if update(store, settings, one, now).unwrap_or(false) {
            changed += 1;
        }
    }
    for (file, session) in store.all() {
        let vanished = !session.ended
            && session
                .container
                .as_deref()
                .is_some_and(|container| look.covers(container))
            && !known.contains(&(session.agent.as_str(), session.session_id.as_str()));
        if vanished && end(store, settings, &file, now).unwrap_or(false) {
            changed += 1;
        }
    }
    changed
}

/// Какое событие хука означает перемену от записи `old` к состоянию `state`.
/// `None` — перемены нет.
fn action_for(old: Option<&Session>, state: &State) -> Option<Action> {
    let Some(old) = old else {
        return Some(match state {
            State::Busy => Action::Prompt,
            State::Waiting(_) => Action::Notice,
            State::Idle => Action::Start,
        });
    };
    match state {
        // Ожидание кончилось — ход идёт дальше; иначе это новый ход.
        State::Busy => match old.status {
            Status::Working if !old.ended => None,
            Status::Waiting | Status::Working => Some(Action::Tool),
            _ => Some(Action::Prompt),
        },
        State::Waiting(_) => (old.status != Status::Waiting || old.ended).then_some(Action::Notice),
        State::Idle => match old.status {
            Status::Working | Status::Waiting => Some(Action::Stop),
            // Сессию открыли снова (окно перезагрузили): она снова на виду.
            _ => old.ended.then_some(Action::Start),
        },
    }
}

/// Данные события, как их прислал бы хук.
fn payload(action: Action, state: &State, details: &Details) -> Map<String, Value> {
    let mut payload = Map::new();
    let mut put = |key: &str, value: &str| {
        payload.insert(key.to_string(), Value::from(value));
    };
    match action {
        Action::Prompt => put("prompt", details.prompt.as_deref().unwrap_or_default()),
        Action::Notice => {
            let reason = match state {
                State::Waiting(reason) => reason.as_str(),
                _ => "",
            };
            put("notification_type", "permission_prompt");
            put("message", &waiting_detail(reason));
        }
        Action::Stop => {
            if let Some(reply) = &details.reply {
                put("last_assistant_message", reply);
            }
        }
        Action::End => put("reason", "gone"),
        _ => {}
    }
    payload
}

/// Чего ждёт агент, для колонки «ЧТО».
fn waiting_detail(reason: &str) -> String {
    match reason {
        "permission prompt" | "" => tr!("permission", "разрешение", "дозвіл"),
        "input needed" => tr!("needs an answer", "нужен ответ", "потрібна відповідь"),
        "dialog open" => tr!("dialog open", "открыт диалог", "відкрито діалог"),
        other => other.to_string(),
    }
}

/// Где идёт сессия: контейнер по первым восьми знакам идентификатора.
fn place(container: &str) -> String {
    let id = container.get(..8).unwrap_or(container);
    tr!("container {id}", "контейнер {id}", "контейнер {id}")
}

/// Записывает перемену в сессии `seen`, если она есть.
fn update(store: &Store, settings: &Settings, seen: &Seen, now: i64) -> Result<bool> {
    let file = store.session_file(seen.agent, &seen.sid);
    let before = store::read_session(&file);
    // Сессию уже ведут хуки (у контейнера общий с хостом каталог состояния): не вмешиваемся.
    if before.as_ref().is_some_and(|old| old.container.is_none()) {
        return Ok(false);
    }
    let Some(planned) = action_for(before.as_ref(), &seen.state) else {
        return touch(store, &file, before.as_ref(), seen, now);
    };
    // Транскрипт и git читаем до блокировки: это самое долгое.
    let details = seen.details();
    let fresh = before.is_none() || matches!(planned, Action::Start | Action::Prompt);
    let (task, branch) = if fresh {
        details
            .project
            .clone()
            .unwrap_or_else(|| (detect::base_name(&seen.cwd), String::new()))
    } else {
        (String::new(), String::new())
    };
    let ctx = Context {
        agent: seen.agent.to_string(),
        sid: seen.sid.clone(),
        cwd: seen.cwd.clone(),
        root: seen.cwd.clone(),
        project: None,
        task,
        branch,
        fresh,
        place: place(&seen.container),
        now,
        notify_min_sec: settings.int("DISP_NOTIFY_MIN_SEC", 120),
        auto_ok_on_exit: false,
        agent_title: details.title.clone(),
        process: None,
        // Транскрипт лежит внутри контейнера: передачу в фон там не отслеживаем.
        transcript: String::new(),
        waiting_agents: 0,
    };
    let lock = store.lock();
    // Перемену мог уже записать другой процесс: значок и панель смотрят независимо.
    let old = store::read_session(&file);
    let Some(action) = action_for(old.as_ref(), &seen.state) else {
        return Ok(false);
    };
    let mut outcome = hook::step(
        old.as_ref(),
        &payload(action, &seen.state, &details),
        action,
        &ctx,
    );
    outcome.state.container = Some(seen.container.clone());
    store::write_session(&file, &outcome.state)?;
    if let Some(event) = &outcome.event {
        store.append_event(event)?;
    }
    drop(lock);
    if let Some(note) = &outcome.note {
        notify::send(note, settings);
    }
    Ok(true)
}

/// Работающей сессии обновляет время раз в `TOUCH_SEC`: агент занят, хотя перемен нет.
/// Сессия Codex живёт, пока пишется её журнал: время берётся по нему, и сессия, чей
/// агент пропал, со временем становится молчащей, а не работает вечно.
fn touch(
    store: &Store,
    file: &Path,
    before: Option<&Session>,
    seen: &Seen,
    now: i64,
) -> Result<bool> {
    let alive = seen.activity.unwrap_or(now).min(now);
    let stale = |session: &Session| {
        session.status == Status::Working && alive - session.updated >= TOUCH_SEC
    };
    if seen.state != State::Busy || !before.is_some_and(stale) {
        return Ok(false);
    }
    let _lock = store.lock();
    let Some(mut session) = store::read_session(file).filter(stale) else {
        return Ok(false);
    };
    session.updated = alive;
    store::write_session(file, &session)?;
    Ok(true)
}

/// Закрывает сессию из контейнера, которой больше не видно: процесс агента завершился
/// или контейнер остановлен.
fn end(store: &Store, settings: &Settings, file: &Path, now: i64) -> Result<bool> {
    let lock = store.lock();
    let Some(old) = store::read_session(file).filter(|old| !old.ended && old.container.is_some())
    else {
        return Ok(false);
    };
    let ctx = Context {
        agent: old.agent.clone(),
        sid: old.session_id.clone(),
        place: old.place.clone(),
        now,
        notify_min_sec: settings.int("DISP_NOTIFY_MIN_SEC", 120),
        ..Context::default()
    };
    let outcome = hook::step(
        Some(&old),
        &payload(Action::End, &State::Idle, &Details::default()),
        Action::End,
        &ctx,
    );
    store::write_session(file, &outcome.state)?;
    if let Some(event) = &outcome.event {
        store.append_event(event)?;
    }
    drop(lock);
    Ok(true)
}

#[cfg(test)]
mod tests;
