//! Что диспетчер узнаёт об окружении сессии сам: где она идёт, над какой задачей
//! и как агент её назвал.

use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::config::{Settings, env_nonempty};
use crate::panes;

/// Сколько байт с конца транскрипта просматривать сначала и, если названия там нет, потом.
const TRANSCRIPT_WINDOWS: [u64; 2] = [256 * 1024, 4 * 1024 * 1024];
/// Записи с названием короткие; длинные строки транскрипта не разбираем.
const TITLE_LINE_MAX: usize = 8192;
/// Сколько ждать ответа zellij.
const ZELLIJ_TIMEOUT: Duration = Duration::from_secs(2);
/// Сколько байт с конца списка сессий Codex просматривать в поисках названия.
const CODEX_INDEX_WINDOW: u64 = 1024 * 1024;
/// Сколько байт с начала транскрипта просматривать в поисках каталога запуска.
const TRANSCRIPT_HEAD: u64 = 256 * 1024;
/// Сколько байт с конца транскрипта просматривать в поисках записи о продолжении разговора:
/// она последняя, за ней ничего не пишется.
const TRANSCRIPT_TAIL: u64 = 16 * 1024;
/// Служебные каталоги внутри проекта: сами проектом не считаются.
const SERVICE_DIRS: [&str; 18] = [
    "src",
    "lib",
    "test",
    "tests",
    "docs",
    "doc",
    "bin",
    "scripts",
    "assets",
    "include",
    "examples",
    "dist",
    "build",
    "target",
    "out",
    "tmp",
    "node_modules",
    "vendor",
];
/// Манифесты, по которым каталог признаётся корнем проекта, когда он не репозиторий.
const PROJECT_MARKERS: [&str; 11] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "CMakeLists.txt",
    "Makefile",
    "setup.py",
    "composer.json",
    "Gemfile",
];

/// Где идёт сессия: `zellij:сессия#панель`, `tmux:панель`, `vscode`, имя терминала.
pub fn place() -> String {
    place_from(&|key: &str| env_nonempty(key))
}

/// Место по переменным окружения, которые отдаёт `var`.
fn place_from(var: &dyn Fn(&str) -> Option<String>) -> String {
    if var("ZELLIJ").is_some() || var("ZELLIJ_SESSION_NAME").is_some() {
        let session = var("ZELLIJ_SESSION_NAME").unwrap_or_else(|| "?".to_string());
        let pane = var("ZELLIJ_PANE_ID").unwrap_or_else(|| "?".to_string());
        return format!("zellij:{session}#{pane}");
    }
    if var("TMUX").is_some() {
        return format!(
            "tmux:{}",
            var("TMUX_PANE").unwrap_or_else(|| "?".to_string())
        );
    }
    let term_program = var("TERM_PROGRAM");
    let in_vscode = [
        "VSCODE_PID",
        "VSCODE_IPC_HOOK",
        "VSCODE_IPC_HOOK_CLI",
        "VSCODE_CWD",
    ]
    .iter()
    .any(|key| var(key).is_some());
    if term_program.as_deref() == Some("vscode") || in_vscode {
        return "vscode".to_string();
    }
    if let Some(program) = term_program {
        return program;
    }
    if var("GHOSTTY_RESOURCES_DIR").is_some() {
        return "ghostty".to_string();
    }
    "tty".to_string()
}

/// Каталог, в котором агент был запущен. `cwd` — текущий каталог из события: агент
/// уходит из каталога запуска в подкаталоги и во временные каталоги, поэтому задача
/// определяется не по нему.
///
/// Claude Code передаёт хуку каталог проекта в `CLAUDE_PROJECT_DIR`; если переменной нет,
/// он берётся из первой записи транскрипта. У остальных агентов это каталог первого
/// события сессии.
pub fn launch_dir(agent: &str, transcript: &str, cwd: &str) -> String {
    if agent == "claude" {
        if let Some(dir) = env_nonempty("CLAUDE_PROJECT_DIR") {
            return dir;
        }
        if !transcript.is_empty()
            && let Some(dir) = transcript_first_cwd(Path::new(transcript))
        {
            return dir;
        }
    }
    cwd.to_string()
}

/// Каталог запуска сессии Claude Code по её id: из транскрипта, который Claude Code
/// хранит как `projects/<проект>/<id>.jsonl` в своём каталоге (`$CLAUDE_CONFIG_DIR`
/// или `~/.claude`). Нужен для записей, сделанных до того, как каталог запуска
/// стал запоминаться в сессии.
pub fn claude_launch_dir(sid: &str) -> Option<String> {
    if sid.is_empty() {
        return None;
    }
    let home = env_nonempty("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env_nonempty("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    let name = format!("{sid}.jsonl");
    fs::read_dir(home.join("projects"))
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path().join(&name)))
        .find(|transcript| transcript.is_file())
        .and_then(|transcript| transcript_first_cwd(&transcript))
}

/// Каталог из первой записи транскрипта Claude Code, в которой он указан.
fn transcript_first_cwd(path: &Path) -> Option<String> {
    let mut head = Vec::new();
    File::open(path)
        .ok()?
        .take(TRANSCRIPT_HEAD)
        .read_to_end(&mut head)
        .ok()?;
    head.split(|byte| *byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter(|line| line.contains("\"cwd\":"))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|record| {
            record
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|dir| !dir.is_empty())
                .map(str::to_string)
        })
}

/// Какой процесс Codex исполняет хук.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexHost {
    /// Сам агент, или неизвестно какой: место — по окружению хука.
    Agent,
    /// Общий фоновый процесс `codex app-server`. Codex 0.169 исполняет хуки в нём, а он
    /// один на все сессии и запущен когда-то из какой-то одной панели. Переменные
    /// окружения хука — его, и по ним нельзя судить, где идёт сессия: все сессии Codex
    /// оказались бы в той первой панели. Место ищется по процессу интерфейса (`codex_place`).
    Daemon,
    /// Сервер расширения Codex в VS Code (и в его ветвях): у такой сессии нет окна
    /// терминала, она идёт в редакторе.
    Editor,
}

/// Какой процесс Codex исполняет этот хук: смотрит предков в `/proc`.
pub fn codex_host() -> CodexHost {
    codex_host_in(Path::new("/proc"), std::process::id())
}

fn codex_host_in(proc: &Path, pid: u32) -> CodexHost {
    let server = ancestor_in(proc, pid, |args| {
        is_codex(args) && args.iter().any(|arg| arg == "app-server")
    });
    let Some(args) = server else {
        return CodexHost::Agent;
    };
    // Программа — тот из первых двух аргументов, что `codex` (см. `is_codex`).
    let program = args.iter().take(2).find(|arg| base_name(arg) == "codex");
    if program.is_some_and(|program| in_editor_extension(program)) {
        CodexHost::Editor
    } else {
        CodexHost::Daemon
    }
}

/// Запущен ли хук запасным процессом Claude Code (`claude bg-spare --bg-spare <сокет>`).
///
/// Такие процессы Claude Code держит наготове для фоновых сессий. Запасной процесс
/// сообщает о начале своей сессии сразу при запуске, а работу может так и не получить:
/// тогда он завершается молча, и в списке осталась бы пустая сессия.
pub fn under_claude_spare() -> bool {
    ancestor_in(Path::new("/proc"), std::process::id(), is_claude_spare).is_some()
}

/// Запасной процесс Claude Code: у него в командной строке есть `--bg-spare`.
fn is_claude_spare(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--bg-spare")
}

/// Аргументы ближайшего предка процесса `pid`, для которого `wanted` верно. Хук
/// запускается через оболочку, поэтому смотрим несколько предков.
fn ancestor_in(proc: &Path, pid: u32, wanted: impl Fn(&[String]) -> bool) -> Option<Vec<String>> {
    let mut pid = pid;
    for _ in 0..6 {
        let parent = parent_pid(proc, pid).filter(|parent| *parent > 1)?;
        let args = cmdline(proc, parent);
        if wanted(&args) {
            return Some(args);
        }
        pid = parent;
    }
    None
}

/// Сессия, в которой продолжен разговор из транскрипта Claude Code `transcript`.
///
/// Когда сессию отправляют в фон, Claude Code продолжает разговор в новой сессии, а в
/// транскрипт прежней дописывает последней записью
/// `{"type":"continued-in","continuedInSessionId":"<новая>"}`. Ни Stop, ни SessionEnd
/// прежняя сессия при этом не присылает. Если после этой записи разговор в прежней
/// сессии шёл дальше (есть запись `user` или `assistant`), он в ней и продолжается.
pub fn claude_continued_in(transcript: &Path) -> Option<String> {
    let (lines, _) = crate::observe::tail_lines(transcript, TRANSCRIPT_TAIL);
    for line in lines.iter().rev() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("continued-in") => {
                return record
                    .get("continuedInSessionId")
                    .and_then(Value::as_str)
                    .filter(|sid| !sid.is_empty())
                    .map(str::to_string);
            }
            Some("user" | "assistant") => return None,
            _ => {}
        }
    }
    None
}

/// Программа из расширения Codex для редактора: `~/.vscode/extensions/openai.chatgpt-…`.
/// Так же лежит она у `~/.vscode-server` и у ветвей VS Code (`~/.cursor/extensions/…`).
fn in_editor_extension(program: &str) -> bool {
    let parts: Vec<&std::ffi::OsStr> = Path::new(program)
        .components()
        .map(|part| part.as_os_str())
        .collect();
    parts.windows(2).any(|pair| {
        pair[0] == "extensions"
            && pair[1]
                .to_str()
                .is_some_and(|name| name.starts_with("openai.chatgpt"))
    })
}

/// Где идёт сессия Codex с рабочим каталогом `cwd`, если хук запущен его фоновым
/// процессом: место берётся из окружения процесса интерфейса Codex с тем же текущим
/// каталогом. Если такие процессы есть в разных местах, выбирается `prefer`, когда оно
/// среди них, а иначе место не определено: гадать нельзя, по месту нажимается Enter.
pub fn codex_place(cwd: &str, prefer: Option<&str>) -> Option<String> {
    codex_place_in(Path::new("/proc"), cwd, prefer)
}

fn codex_place_in(proc: &Path, cwd: &str, prefer: Option<&str>) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    for entry in fs::read_dir(proc).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let args = cmdline(proc, pid);
        if !is_codex(&args) || args.iter().any(|arg| arg == "app-server") {
            continue;
        }
        let dir = proc.join(pid.to_string());
        if fs::read_link(dir.join("cwd")).ok().as_deref() != Some(Path::new(cwd)) {
            continue;
        }
        let Ok(environ) = fs::read(dir.join("environ")) else {
            continue;
        };
        let vars: Vec<(String, String)> = environ
            .split(|byte| *byte == 0)
            .filter_map(|item| {
                let text = String::from_utf8_lossy(item);
                let (key, value) = text.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect();
        let var = |key: &str| {
            vars.iter()
                .find(|(name, value)| name == key && !value.is_empty())
                .map(|(_, value)| value.clone())
        };
        let place = place_from(&var);
        if !found.contains(&place) {
            found.push(place);
        }
    }
    match found.as_slice() {
        [one] => Some(one.clone()),
        many => prefer
            .filter(|wanted| many.iter().any(|place| place == wanted))
            .map(str::to_string),
    }
}

/// Родитель процесса по `/proc/<pid>/stat`.
fn parent_pid(proc: &Path, pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    // Имя процесса в скобках может содержать пробелы: поля считаем после последней «)».
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Аргументы командной строки процесса.
pub(crate) fn cmdline(proc: &Path, pid: u32) -> Vec<String> {
    fs::read(proc.join(pid.to_string()).join("cmdline"))
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Процесс Codex: сама программа или её обёртка на node (`node …/bin/codex`).
pub(crate) fn is_codex(args: &[String]) -> bool {
    args.iter().take(2).any(|arg| base_name(arg) == "codex")
}

/// Имя задачи и ветка для рабочего каталога: имя каталога git-репозитория (или worktree),
/// а вне репозитория — имя самого каталога.
pub fn project(cwd: &str) -> (String, String) {
    match git_toplevel(Path::new(cwd)) {
        Some(top) => (base_name(&top.to_string_lossy()), git_branch(&top)),
        None => (base_name(cwd), String::new()),
    }
}

/// Последний компонент пути, как у утилиты `basename`.
pub fn base_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() {
            String::new()
        } else {
            "/".to_string()
        };
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

/// Ближайший каталог вверх по дереву, в котором есть `.git`. Как и git, поиск не поднимается
/// в каталоги из `GIT_CEILING_DIRECTORIES`; сам исходный каталог проверяется всегда.
fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let ceilings: Vec<PathBuf> = env::var_os("GIT_CEILING_DIRECTORIES")
        .map(|list| env::split_paths(&list).collect())
        .unwrap_or_default();
    cwd.ancestors()
        .filter(|dir| !dir.as_os_str().is_empty())
        .take_while(|dir| *dir == cwd || !ceilings.iter().any(|ceiling| ceiling == dir))
        .find(|dir| is_repository(&dir.join(".git")))
        .map(Path::to_path_buf)
}

/// Настоящий ли это `.git`: каталог с `HEAD` или файл-указатель (worktree, подмодуль).
/// Пустой каталог `.git`, оставшийся от чего-то, git пропускает и ищет выше — так же
/// и здесь.
fn is_repository(dot_git: &Path) -> bool {
    dot_git.is_file() || dot_git.join("HEAD").is_file()
}

/// Файл, который агент записал или изменил инструментом из события. `None`, если
/// событие не о записи файла.
///
/// Claude Code и Gemini CLI передают путь в данных инструмента, Codex — в тексте правки
/// `apply_patch`. Команды оболочки не разбираются: по ним не понять, читали файл или писали.
pub fn written_path(payload: &Map<String, Value>, cwd: &str) -> Option<PathBuf> {
    let tool = payload.get("tool_name")?.as_str()?;
    let input = payload.get("tool_input")?;
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    let raw = match tool {
        "Write" | "Edit" | "MultiEdit" | "write_file" | "replace" => text("file_path")?,
        "NotebookEdit" => text("notebook_path")?,
        "apply_patch" => patched_file(input.get("command")?)?,
        _ => return None,
    };
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        Path::new(cwd).join(path)
    })
}

/// Первый файл, который добавляет или меняет правка в формате `apply_patch`.
fn patched_file(command: &Value) -> Option<String> {
    let patch = match command {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    patch.lines().find_map(|line| {
        line.strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "))
            .map(|path| path.trim().to_string())
    })
}

/// Каталог проекта, к которому относится `path`, для агента, запущенного в `root`.
/// `is_dir` — сам `path` является каталогом (например, текущим каталогом агента);
/// `known` — проект, уже определённый для этой сессии. `None` — менять нечего.
///
/// Если агент запущен внутри git-репозитория, проект — этот репозиторий, и подкаталоги
/// отдельными проектами не считаются. Иначе на пути от `root` к `path` ищется каталог
/// с признаком проекта: сначала репозиторий (`.git`), затем манифест (`Cargo.toml`,
/// `package.json` и т. п.); из нескольких берётся ближайший к `root`. Если признаков нет,
/// проектом считается ближайший к `path` каталог, не считая служебных вроде `src` и `tests`.
/// Пути вне `root` (временные каталоги, настройки в домашнем каталоге) и файлы прямо
/// в `root` проекта не дают.
pub fn project_dir(root: &str, path: &Path, is_dir: bool, known: Option<&str>) -> Option<String> {
    if git_toplevel(Path::new(root)).is_some() {
        return None;
    }
    subproject(Path::new(root), path, is_dir, known.map(Path::new))
}

/// Подкаталог `root`, который выглядит проектом, на пути к `path`; см. [`project_dir`].
fn subproject(root: &Path, path: &Path, is_dir: bool, known: Option<&Path>) -> Option<String> {
    let inside = path.strip_prefix(root).ok()?;
    if inside
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return None;
    }
    // Каталоги от первого подкаталога `root` до каталога, где лежит `path`.
    let depth = inside.components().count() - usize::from(!is_dir);
    let mut dirs = Vec::with_capacity(depth);
    let mut dir = root.to_path_buf();
    for part in inside.components().take(depth) {
        dir.push(part);
        dirs.push(dir.clone());
    }
    let has = |dir: &PathBuf, marker: &str| dir.join(marker).symlink_metadata().is_ok();
    let marked = dirs.iter().find(|dir| has(dir, ".git")).or_else(|| {
        dirs.iter()
            .find(|dir| PROJECT_MARKERS.iter().any(|marker| has(dir, marker)))
    });
    if let Some(project) = marked {
        return Some(project.to_string_lossy().into_owned());
    }
    // Признаков проекта нет: это догадка, и уже известный проект она не перебивает,
    // пока агент пишет внутри него или в каталог над ним.
    if known.is_some_and(|known| path.starts_with(known)) {
        return None;
    }
    let guess = dirs.iter().rev().find(|dir| {
        dir.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.starts_with('.') && !SERVICE_DIRS.contains(&name))
    })?;
    if known.is_some_and(|known| known.starts_with(guess)) {
        return None;
    }
    Some(guess.to_string_lossy().into_owned())
}

/// Текущая ветка репозитория; пусто, если HEAD не на ветке.
fn git_branch(top: &Path) -> String {
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // Worktree и подмодуль: `.git` — файл со строкой `gitdir: <путь>`.
        let Ok(pointer) = fs::read_to_string(&dot_git) else {
            return String::new();
        };
        let Some(target) = pointer.trim().strip_prefix("gitdir:") else {
            return String::new();
        };
        let target = Path::new(target.trim());
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            top.join(target)
        }
    };
    let Ok(head) = fs::read_to_string(git_dir.join("HEAD")) else {
        return String::new();
    };
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .unwrap_or("")
        .to_string()
}

/// Краткое название сессии `sid`, которое агент придумал сам.
///
/// Claude Code пишет его в транскрипт (записи `ai-title`, после `/rename` — `custom-title`).
/// Codex ведёт список своих сессий с названиями. Если там названия нет, как и у остальных
/// агентов, берётся заголовок панели zellij: Codex ставит туда «тема | проект».
/// Опрос zellij стоит десятки миллисекунд, поэтому делается только при `full`.
pub fn agent_title(
    agent: &str,
    sid: &str,
    transcript: &str,
    full: bool,
    dir: &str,
    place: &str,
    settings: &Settings,
) -> Option<String> {
    if agent == "claude" {
        if transcript.is_empty() {
            return None;
        }
        return transcript_title(Path::new(transcript));
    }
    if agent == "codex"
        && let Some(title) = codex_index().and_then(|index| codex_title(&index, sid))
    {
        return Some(title);
    }
    if !full || settings.get("DISP_PANE_TITLE") == Some("0") {
        return None;
    }
    let (zellij, pane) = crate::panes::target(place)?;
    let raw = pane_title(zellij, pane)?;
    clean_pane_title(&raw, agent, dir)
}

/// Список сессий Codex: `$DISP_CODEX_INDEX`, иначе `session_index.jsonl` в каталоге
/// Codex (`$CODEX_HOME` или `~/.codex`).
fn codex_index() -> Option<PathBuf> {
    if let Some(path) = env_nonempty("DISP_CODEX_INDEX") {
        return Some(PathBuf::from(path));
    }
    let home = env_nonempty("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env_nonempty("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
    Some(home.join("session_index.jsonl"))
}

/// Название сессии `sid` из списка сессий Codex: поле `thread_name` последней записи
/// с этим `id`.
fn codex_title(index: &Path, sid: &str) -> Option<String> {
    if sid.is_empty() {
        return None;
    }
    let tail = read_tail(index, CODEX_INDEX_WINDOW)?;
    let text = String::from_utf8_lossy(&tail);
    text.lines()
        .rev()
        .filter(|line| line.contains(sid))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record.get("id").and_then(Value::as_str) == Some(sid))
        .and_then(|record| {
            record
                .get("thread_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|title| !title.is_empty())
}

/// Название из транскрипта Claude Code: последнее заданное через `/rename`,
/// а если такого нет — последнее придуманное самим агентом.
fn transcript_title(path: &Path) -> Option<String> {
    for window in TRANSCRIPT_WINDOWS {
        let tail = read_tail(path, window)?;
        if let Some(title) = scan_titles(&tail) {
            return Some(title);
        }
        if (tail.len() as u64) < window {
            break;
        }
    }
    None
}

/// Последние `window` байт файла.
fn read_tail(path: &Path, window: u64) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(window)))
        .ok()?;
    let mut tail = Vec::new();
    file.take(window).read_to_end(&mut tail).ok()?;
    Some(tail)
}

/// Ищет записи с названием среди строк транскрипта.
fn scan_titles(data: &[u8]) -> Option<String> {
    let (mut custom, mut ai) = (None, None);
    for line in data.split(|byte| *byte == b'\n') {
        if line.len() > TITLE_LINE_MAX {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        if !text.contains("\"type\":\"ai-title\"") && !text.contains("\"type\":\"custom-title\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let field = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") => custom = field("customTitle").or(custom),
            Some("ai-title") => ai = field("aiTitle").or(ai),
            _ => {}
        }
    }
    custom.or(ai).filter(|title| !title.is_empty())
}

/// Заголовок панели zellij с номером `pane` в сессии `zellij` из вывода
/// `zellij action list-panes`.
fn pane_title(zellij: &str, pane: &str) -> Option<String> {
    let listing = run_with_timeout(Command::new("zellij").args([
        "--session",
        zellij,
        "action",
        "list-panes",
    ]))?;
    let wanted = format!("terminal_{pane}");
    listing.lines().find_map(|line| {
        let (id, rest) = line.trim_start().split_once(char::is_whitespace)?;
        if id != wanted {
            return None;
        }
        let rest = rest.trim_start();
        let (_kind, title) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        Some(title.trim().to_string())
    })
}

/// Запускает команду и возвращает её stdout; `None`, если она не запустилась,
/// завершилась ошибкой или не уложилась в отведённое время.
pub fn run_with_timeout(command: &mut Command) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + ZELLIJ_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    Some(output)
}

/// Приводит заголовок панели к названию задачи: убирает спиннер в начале и « | проект»
/// в конце. Заголовок оболочки (`user@host:путь`), имя агента, имя каталога и подпись,
/// которую на панель поставил сам диспетчер, названием задачи не считаются.
pub fn clean_pane_title(raw: &str, agent: &str, dir: &str) -> Option<String> {
    if panes::is_label(raw.trim()) {
        return None;
    }
    let mut title = raw.trim_start_matches(|ch: char| !ch.is_alphanumeric());
    let suffix = format!(" | {dir}");
    if !dir.is_empty()
        && let Some(head) = title.strip_suffix(suffix.as_str())
    {
        title = head;
    }
    let title = title.trim_end();
    if title.is_empty()
        || title == dir
        || title.eq_ignore_ascii_case(agent)
        || looks_like_shell_prompt(title)
    {
        return None;
    }
    Some(title.to_string())
}

/// Похоже ли на заголовок, который ставит оболочка: `user@host:путь` без пробелов.
fn looks_like_shell_prompt(title: &str) -> bool {
    if title.contains(char::is_whitespace) {
        return false;
    }
    title.split_once('@').is_some_and(|(user, rest)| {
        !user.is_empty()
            && rest
                .split_once(':')
                .is_some_and(|(host, _)| !host.is_empty())
    })
}

/// Полный путь к программе из `PATH`, если она там есть.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakeproc::FakeProc;

    #[test]
    fn empty_git_directory_is_not_a_repository() {
        // В репозитории `site` подкаталог `app` с пустым `.git`: git его пропускает.
        let root = std::env::temp_dir().join(format!("disp-empty-git-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(fs::create_dir_all(root.join("site/.git")).is_ok());
        assert!(fs::write(root.join("site/.git/HEAD"), "ref: refs/heads/docs/check\n").is_ok());
        assert!(fs::create_dir_all(root.join("site/app/.git")).is_ok());
        let found = project(&root.join("site/app").to_string_lossy());
        let _ = fs::remove_dir_all(&root);
        assert_eq!(found, ("site".to_string(), "docs/check".to_string()));
    }

    #[test]
    fn base_name_matches_the_utility() {
        assert_eq!(base_name("/home/rock/projects/nbx-142"), "nbx-142");
        assert_eq!(base_name("/home/rock/site-31/"), "site-31");
        assert_eq!(base_name("/"), "/");
        assert_eq!(base_name(""), "");
    }

    #[test]
    fn rename_wins_over_generated_title() {
        let data = concat!(
            "{\"type\":\"user\",\"message\":\"x\"}\n",
            "{\"type\":\"custom-title\",\"customTitle\":\"Релиз 1.4 \\\"mac\\\"\",\"sessionId\":\"x\"}\n",
            "{\"type\":\"ai-title\",\"aiTitle\":\"Сборка и подпись macOS-билда\",\"sessionId\":\"x\"}\n",
        );
        assert_eq!(
            scan_titles(data.as_bytes()).as_deref(),
            Some("Релиз 1.4 \"mac\"")
        );
        let only_ai =
            "{\"type\":\"ai-title\",\"aiTitle\":\"Графика в Zellij\",\"sessionId\":\"x\"}\n";
        assert_eq!(
            scan_titles(only_ai.as_bytes()).as_deref(),
            Some("Графика в Zellij")
        );
        assert_eq!(scan_titles(b"{\"type\":\"user\"}\n"), None);
    }

    #[test]
    fn pane_title_is_cleaned() {
        assert_eq!(
            clean_pane_title("⠙ Починить импорт цен | site-31", "codex", "site-31").as_deref(),
            Some("Починить импорт цен")
        );
        assert_eq!(
            clean_pane_title(
                "Запустить систему за 21 сентября | trading-system",
                "codex",
                "trading-system"
            )
            .as_deref(),
            Some("Запустить систему за 21 сентября")
        );
        assert_eq!(
            clean_pane_title("rock@omnibook3:~/projects", "codex", "projects"),
            None
        );
        assert_eq!(clean_pane_title("Codex", "codex", "site-31"), None);
        assert_eq!(clean_pane_title("site-31", "codex", "site-31"), None);
        // Подпись диспетчера — не название от агента.
        assert_eq!(
            clean_pane_title("#3 Починить импорт цен", "gemini", "site-31"),
            None
        );
    }

    #[test]
    fn written_file_is_taken_from_the_tool_event() {
        let event = |value: Value| value.as_object().cloned().unwrap_or_default();
        let write = event(serde_json::json!({
            "tool_name": "Write",
            "tool_input": {"file_path": "/work/tools/disp/src/main.rs", "content": "x"}
        }));
        assert_eq!(
            written_path(&write, "/work/tools"),
            Some(PathBuf::from("/work/tools/disp/src/main.rs"))
        );
        // Codex: путь внутри текста правки, возможно относительный.
        let patch = event(serde_json::json!({
            "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n*** Add File: newproj/src/a.txt\n+hi\n*** End Patch"}
        }));
        assert_eq!(
            written_path(&patch, "/work/tools"),
            Some(PathBuf::from("/work/tools/newproj/src/a.txt"))
        );
        let update = serde_json::json!([
            "apply_patch",
            "*** Begin Patch\n*** Update File: /abs/b.rs\n"
        ]);
        assert_eq!(patched_file(&update).as_deref(), Some("/abs/b.rs"));
        // Чтение и команды оболочки проект не выдают.
        let read = event(serde_json::json!({
            "tool_name": "Read", "tool_input": {"file_path": "/work/other/README.md"}
        }));
        assert_eq!(written_path(&read, "/work/tools"), None);
        let shell = event(serde_json::json!({
            "tool_name": "Bash", "tool_input": {"command": "cat other/README.md"}
        }));
        assert_eq!(written_path(&shell, "/work/tools"), None);
    }

    #[test]
    fn project_is_the_marked_directory_or_the_nearest_one() {
        let root = std::env::temp_dir().join(format!("disp-subproject-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let disp = root.join("tools/model_dispatcher");
        assert!(fs::create_dir_all(disp.join("crates/core/src")).is_ok());
        assert!(fs::write(disp.join("Cargo.toml"), "").is_ok());
        assert!(fs::write(disp.join("crates/core/Cargo.toml"), "").is_ok());
        let web = root.join("soft/cosmo/web");
        assert!(fs::create_dir_all(web.join(".git")).is_ok());
        assert!(fs::write(root.join("soft/Makefile"), "").is_ok());
        let path = |dir: &Path| Some(dir.to_string_lossy().into_owned());
        let find = |file: &Path, known: Option<&Path>| subproject(&root, file, false, known);

        // Манифест: проект — ближайший к каталогу запуска каталог с ним, а не вложенный пакет.
        assert_eq!(
            find(&disp.join("crates/core/src/lib.rs"), None),
            path(&disp)
        );
        assert_eq!(
            subproject(&root, &disp.join("src"), true, None),
            path(&disp)
        );
        // Репозиторий важнее манифеста в каталоге над ним.
        assert_eq!(find(&web.join("src/app.ts"), None), path(&web));

        // Признаков нет: ближайший к файлу каталог, служебные вроде `src` не в счёт.
        let gfx = root.join("tools/term-gfx");
        assert_eq!(find(&gfx.join("gfx.py"), None), path(&gfx));
        assert_eq!(
            find(&root.join("newproj/src/hello.txt"), None),
            path(&root.join("newproj"))
        );
        assert_eq!(subproject(&root, &gfx, true, None), path(&gfx));
        // Такая догадка не перебивает известный проект, пока агент пишет в нём или над ним.
        let newproj = root.join("newproj");
        assert_eq!(
            find(&newproj.join("src/app/models/user.py"), Some(&newproj)),
            None
        );
        assert_eq!(find(&root.join("tools/notes.md"), Some(&gfx)), None);
        // А признак проекта перебивает: агент перешёл в настоящий проект.
        assert_eq!(find(&disp.join("src/main.rs"), Some(&gfx)), path(&disp));

        // Файл прямо в каталоге запуска, сам каталог запуска и пути вне его — не проект.
        assert_eq!(find(&root.join("README.md"), None), None);
        assert_eq!(subproject(&root, &root, true, None), None);
        assert_eq!(find(Path::new("/tmp/scratch/x.rs"), None), None);
        assert_eq!(find(&root.join("../elsewhere/x.rs"), None), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hook_host_tells_the_codex_daemon_from_the_editor() {
        let proc = FakeProc::new("daemon");
        let daemon = "/home/u/.codex/packages/app-server-daemon/bin/codex";
        proc.add(
            100,
            1,
            &[daemon, "app-server", "--listen", "unix"],
            "/w",
            &[],
        );
        proc.add(200, 100, &["sh", "-c", "disp hook codex"], "/w", &[]);
        proc.add(300, 200, &["disp", "hook", "codex"], "/w", &[]);
        // Хук Claude Code: родитель — сам агент.
        proc.add(400, 1, &["claude"], "/w", &[]);
        proc.add(500, 400, &["disp", "hook", "claude"], "/w", &[]);
        // Сервер расширения VS Code: у него своя копия Codex в каталоге расширения.
        let editor = "/home/u/.vscode/extensions/openai.chatgpt-26.1111.21308-linux-x64/bin/linux-x86_64/codex";
        proc.add(600, 1, &[editor, "app-server"], "/home/u", &[]);
        proc.add(700, 600, &["sh", "-c", "disp hook codex"], "/w", &[]);
        proc.add(800, 700, &["disp", "hook", "codex"], "/w", &[]);
        assert_eq!(codex_host_in(&proc.root, 300), CodexHost::Daemon);
        assert_eq!(codex_host_in(&proc.root, 500), CodexHost::Agent);
        assert_eq!(codex_host_in(&proc.root, 800), CodexHost::Editor);
    }

    #[test]
    fn editor_extension_is_found_by_its_directory() {
        assert!(in_editor_extension(
            "/home/vscode/.vscode-server/extensions/openai.chatgpt-26.1111.21308-linux-x64/bin/linux-x86_64/codex"
        ));
        assert!(in_editor_extension(
            "/home/u/.cursor/extensions/openai.chatgpt-1.0.0/bin/codex"
        ));
        assert!(!in_editor_extension(
            "/home/u/.codex/packages/app-server-daemon/bin/codex"
        ));
        assert!(!in_editor_extension(
            "/home/u/.npm-global/lib/node_modules/@openai/codex/bin/codex"
        ));
        assert!(!in_editor_extension("/home/u/extensions/codex"));
    }

    #[test]
    fn codex_place_comes_from_its_interface_process() {
        let proc = FakeProc::new("place");
        let tui = "/home/u/node_modules/@openai/codex-linux-x64/vendor/bin/codex";
        let dev3 = ["ZELLIJ=0", "ZELLIJ_SESSION_NAME=dev", "ZELLIJ_PANE_ID=3"];
        // Фоновый процесс с окружением первой панели не в счёт.
        proc.add(10, 1, &[tui, "app-server"], "/w/site", &dev3);
        proc.add(
            20,
            1,
            &["node", "/home/u/.npm-global/bin/codex"],
            "/w/site",
            &["ZELLIJ=0", "ZELLIJ_SESSION_NAME=dev", "ZELLIJ_PANE_ID=7"],
        );
        proc.add(
            21,
            20,
            &[tui],
            "/w/site",
            &["ZELLIJ=0", "ZELLIJ_SESSION_NAME=dev", "ZELLIJ_PANE_ID=7"],
        );
        proc.add(30, 1, &[tui, "resume"], "/w/other", &dev3);
        proc.add(
            40,
            1,
            &["systemd-inhibit", "--who", "codex"],
            "/w/site",
            &[],
        );
        assert_eq!(
            codex_place_in(&proc.root, "/w/site", None).as_deref(),
            Some("zellij:dev#7")
        );
        assert_eq!(
            codex_place_in(&proc.root, "/w/other", None).as_deref(),
            Some("zellij:dev#3")
        );
        assert_eq!(codex_place_in(&proc.root, "/w/none", None), None);

        // Два интерфейса Codex в одном каталоге: гадать нельзя, кроме уже известного места.
        proc.add(50, 1, &[tui], "/w/other", &["TERM_PROGRAM=vscode"]);
        assert_eq!(codex_place_in(&proc.root, "/w/other", None), None);
        assert_eq!(
            codex_place_in(&proc.root, "/w/other", Some("vscode")).as_deref(),
            Some("vscode")
        );
        assert_eq!(
            codex_place_in(&proc.root, "/w/other", Some("zellij:dev#9")),
            None
        );
    }

    #[test]
    fn hook_from_a_claude_spare_is_recognised() {
        let proc = FakeProc::new("spare");
        let sock = "/tmp/cc-daemon-1000/ab/spare/82b1.claim.sock";
        proc.add(
            100,
            1,
            &["claude", "bg-spare", "--bg-spare", sock],
            "/w",
            &[],
        );
        proc.add(200, 100, &["sh", "-c", "disp hook claude"], "/w", &[]);
        proc.add(300, 200, &["disp", "hook", "claude"], "/w", &[]);
        proc.add(400, 1, &["claude", "--resume"], "/w", &[]);
        proc.add(500, 400, &["disp", "hook", "claude"], "/w", &[]);
        assert!(ancestor_in(&proc.root, 300, is_claude_spare).is_some());
        assert!(ancestor_in(&proc.root, 500, is_claude_spare).is_none());
    }

    #[test]
    fn continued_conversation_is_read_from_the_end_of_the_transcript() {
        let transcript =
            std::env::temp_dir().join(format!("disp-continued-{}", std::process::id()));
        let head = concat!(
            "{\"type\":\"user\",\"cwd\":\"/w\",\"message\":\"continued-in\"}\n",
            "{\"type\":\"system\",\"content\":\"Backgrounding after the current tool finishes…\"}\n",
        );
        assert!(fs::write(&transcript, head).is_ok());
        assert_eq!(claude_continued_in(&transcript), None);
        let last = concat!(
            "{\"type\":\"continued-in\",\"timestamp\":\"2026-11-18T16:35:42.899Z\",",
            "\"sessionId\":\"cf2731ff\",\"continuedInSessionId\":\"f2331f64\"}\n",
        );
        assert!(fs::write(&transcript, format!("{head}{last}")).is_ok());
        assert_eq!(
            claude_continued_in(&transcript).as_deref(),
            Some("f2331f64")
        );
        // Служебные записи после передачи ей не мешают, а новый ход в прежней сессии — да.
        let cost = "{\"type\":\"cost-state\",\"total\":1}\n";
        assert!(fs::write(&transcript, format!("{head}{last}{cost}")).is_ok());
        assert_eq!(
            claude_continued_in(&transcript).as_deref(),
            Some("f2331f64")
        );
        let again = "{\"type\":\"user\",\"message\":\"ещё\"}\n";
        assert!(fs::write(&transcript, format!("{head}{last}{again}")).is_ok());
        assert_eq!(claude_continued_in(&transcript), None);
        let _ = fs::remove_file(&transcript);
        assert_eq!(claude_continued_in(&transcript), None);
    }

    #[test]
    fn launch_dir_is_read_from_the_start_of_the_transcript() {
        let transcript =
            std::env::temp_dir().join(format!("disp-transcript-{}", std::process::id()));
        let lines = concat!(
            "{\"type\":\"summary\",\"summary\":\"без каталога\"}\n",
            "{\"type\":\"user\",\"cwd\":\"/home/rock/projects/tools\",\"message\":\"x\"}\n",
            "{\"type\":\"assistant\",\"cwd\":\"/tmp/scratchpad\",\"message\":\"y\"}\n",
        );
        assert!(fs::write(&transcript, lines).is_ok());
        assert_eq!(
            transcript_first_cwd(&transcript).as_deref(),
            Some("/home/rock/projects/tools")
        );
        let _ = fs::remove_file(&transcript);
        assert_eq!(transcript_first_cwd(&transcript), None);
        // Не Claude Code: каталог запуска — каталог события.
        assert_eq!(
            launch_dir("codex", "", "/home/rock/site-31"),
            "/home/rock/site-31"
        );
    }

    #[test]
    fn codex_title_comes_from_its_session_index() {
        let index = std::env::temp_dir().join(format!("disp-codex-index-{}", std::process::id()));
        let lines = concat!(
            "{\"id\":\"s-1\",\"thread_name\":\"Старое название\",\"updated_at\":\"2026-09-24T20:22:00Z\"}\n",
            "{\"id\":\"s-10\",\"thread_name\":\"Чужая сессия\",\"updated_at\":\"2026-09-24T20:23:00Z\"}\n",
            "не JSON, но с s-1\n",
            "{\"id\":\"s-1\",\"thread_name\":\"Запустить систему за 21 сентября\",\"updated_at\":\"2026-09-24T20:24:00Z\"}\n",
            "{\"id\":\"s-2\",\"thread_name\":\"\",\"updated_at\":\"2026-09-24T20:25:00Z\"}\n",
        );
        assert!(fs::write(&index, lines).is_ok());
        assert_eq!(
            codex_title(&index, "s-1").as_deref(),
            Some("Запустить систему за 21 сентября")
        );
        assert_eq!(codex_title(&index, "s-10").as_deref(), Some("Чужая сессия"));
        assert_eq!(codex_title(&index, "s-2"), None);
        assert_eq!(codex_title(&index, "s-3"), None);
        assert_eq!(codex_title(&index, ""), None);
        let _ = fs::remove_file(&index);
        assert_eq!(codex_title(&index, "s-1"), None);
    }
}
