//! Подключение хуков к настройкам агентов: `disp config` и `disp install`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::env_nonempty;
use crate::error::{Error, Result};
use crate::store::stage;
use crate::text::{emit, warn};
use crate::{t, tr};

/// How many copies of a settings file `disp install` keeps. The newest is
/// `<file>.bak-disp`, the older ones are `<file>.1.bak-disp`, `<file>.2.bak-disp`, ….
const BACKUPS: usize = 5;

/// События Claude Code, на которые подписывается диспетчер.
const CLAUDE_EVENTS: [&str; 12] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "Notification",
    "PostToolUse",
    "TaskCreated",
    "TaskCompleted",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
/// События Codex CLI и их таймауты в секундах: для Interrupt и SessionEnd предел Codex — 3.
const CODEX_EVENTS: [(&str, u32); 9] = [
    ("SessionStart", 5),
    ("UserPromptSubmit", 5),
    ("PermissionRequest", 5),
    ("PostToolUse", 5),
    ("SubagentStart", 5),
    ("SubagentStop", 5),
    ("Stop", 5),
    ("Interrupt", 3),
    ("SessionEnd", 3),
];
/// События Gemini CLI; таймаут у него задаётся в миллисекундах.
const GEMINI_EVENTS: [&str; 6] = [
    "SessionStart",
    "BeforeAgent",
    "AfterTool",
    "Notification",
    "AfterAgent",
    "SessionEnd",
];

/// Куда подключаются хуки агента и что сказать пользователю после подключения.
struct Target {
    file: PathBuf,
    hint: &'static str,
}

/// The running program as `disp install` needs it: what to write into a hook and how
/// to tell its own hooks from foreign ones.
struct Me {
    /// Canonical path of the executable.
    exe: PathBuf,
    /// Home directory, for `~` and `$HOME` in a hook command.
    home: Option<String>,
    /// Search path, for a hook that calls the program by its bare name.
    path: Option<OsString>,
}

/// What a hook that calls disp points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// This very program, whichever way the command names it.
    Current,
    /// A disp that is not this program: an earlier install or another copy.
    Stale,
}

impl Me {
    fn current() -> Self {
        Me {
            exe: env::current_exe()
                .and_then(fs::canonicalize)
                .unwrap_or_else(|_| PathBuf::from("disp")),
            home: env_nonempty("HOME"),
            path: env::var_os("PATH"),
        }
    }

    /// The program path as it goes into a hook command, quoted when the shell needs it.
    fn quoted(&self) -> String {
        shell_quote(&self.exe.display().to_string())
    }

    /// The hook command for an agent.
    fn command(&self, agent: &str) -> String {
        format!("{} hook {agent}", self.quoted())
    }

    /// What the program word of a hook command points at; `None` when it is not disp.
    ///
    /// A stable name that leads to this program — a symbolic link kept by a package
    /// manager, or the bare name found in `PATH` — is this program too: such a hook
    /// survives an update and is left as it is.
    fn kind(&self, program: &str) -> Option<Kind> {
        let path = Path::new(program);
        let found = if program.contains('/') {
            path.is_absolute().then(|| path.to_path_buf())
        } else {
            self.lookup(program)
        };
        let mine = path == self.exe
            || found
                .and_then(|found| fs::canonicalize(found).ok())
                .is_some_and(|real| real == self.exe);
        if mine {
            return Some(Kind::Current);
        }
        let name = path.file_name()?;
        (name == "disp" || Some(name) == self.exe.file_name()).then_some(Kind::Stale)
    }

    /// The file a bare program name stands for, as a shell would find it.
    fn lookup(&self, name: &str) -> Option<PathBuf> {
        env::split_paths(self.path.as_ref()?)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    }
}

/// The hook block for an agent, with the absolute path to this program. `None`: the
/// agent is unknown.
pub fn config_block(agent: &str) -> Option<Value> {
    block(agent, &Me::current().command(agent))
}

/// The hook block for an agent with the given hook command.
fn block(agent: &str, command: &str) -> Option<Value> {
    let mut hooks = Map::new();
    match agent {
        "claude" => {
            for event in CLAUDE_EVENTS {
                hooks.insert(
                    event.to_string(),
                    json!([{ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }]),
                );
            }
        }
        "codex" => {
            for (event, timeout) in CODEX_EVENTS {
                hooks.insert(
                    event.to_string(),
                    json!([{ "hooks": [{ "type": "command", "command": command, "timeout": timeout }] }]),
                );
            }
        }
        "gemini" => {
            for event in GEMINI_EVENTS {
                hooks.insert(
                    event.to_string(),
                    json!([{ "matcher": "*", "hooks": [{
                        "name": "disp", "type": "command", "command": command, "timeout": 5000
                    }] }]),
                );
            }
        }
        _ => return None,
    }
    Some(json!({ "hooks": hooks }))
}

/// `disp config <агент>`: напечатать блок хуков.
pub fn config(args: &[String]) -> u8 {
    match args.first().and_then(|agent| config_block(agent)) {
        Some(block) => {
            emit(&format!("{}\n", pretty(&block)));
            0
        }
        None => {
            warn(t!(
                "usage: disp config claude|codex|gemini",
                "использование: disp config claude|codex|gemini",
                "використання: disp config claude|codex|gemini"
            ));
            2
        }
    }
}

/// `disp install <agent>`: connect the hooks to the agent settings.
///
/// Adds the hooks that are missing. A hook left by an earlier install — the program has
/// moved since, as it does on every update under Nix or Homebrew — gets the current path
/// in place, and a repeated disp hook is removed. Foreign hooks and all other keys stay
/// as they are. The previous file is kept next to the settings as `<file>.bak-disp`.
pub fn install(args: &[String]) -> u8 {
    let agent = args.first().map(String::as_str).unwrap_or_default();
    let me = Me::current();
    let (Some(target), Some(block)) = (target(agent), block(agent, &me.command(agent))) else {
        warn(t!(
            "usage: disp install claude|codex|gemini",
            "использование: disp install claude|codex|gemini",
            "використання: disp install claude|codex|gemini"
        ));
        return 2;
    };
    match apply(&target.file, agent, &block, &me) {
        Ok((message, changed)) => {
            emit(&format!("{message}\n"));
            if changed {
                emit(&format!("{}\n", target.hint));
            }
            0
        }
        Err(err) => {
            warn(&format!("disp: {err}"));
            1
        }
    }
}

fn target(agent: &str) -> Option<Target> {
    let home = PathBuf::from(env_nonempty("HOME").unwrap_or_default());
    let path = |var: &str, default: &str| {
        env_nonempty(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    Some(match agent {
        "claude" => Target {
            file: path("DISP_CLAUDE_SETTINGS", ".claude/settings.json"),
            hint: t!(
                "Open Claude Code sessions pick up the hooks after /hooks or a restart.",
                "Открытые сессии Claude Code подхватят хуки после /hooks или перезапуска.",
                "Відкриті сесії Claude Code підхоплять хуки після /hooks або перезапуску."
            ),
        },
        "codex" => Target {
            file: path("DISP_CODEX_HOOKS", ".codex/hooks.json"),
            hint: t!(
                "Open /hooks in Codex and confirm trust: without it Codex skips the hooks.",
                "Открой /hooks в Codex и подтверди доверие: без этого он хуки пропускает.",
                "Відкрий /hooks у Codex і підтверди довіру: без цього він пропускає хуки."
            ),
        },
        "gemini" => Target {
            file: path("DISP_GEMINI_SETTINGS", ".gemini/settings.json"),
            hint: t!(
                "New Gemini CLI sessions pick up the hooks at start.",
                "Новые сессии Gemini CLI подхватят хуки при запуске.",
                "Нові сесії Gemini CLI підхоплять хуки під час запуску."
            ),
        },
        _ => return None,
    })
}

/// Brings the disp hooks in a settings file in line with the block. Returns what to
/// tell the user and whether the file changed.
fn apply(file: &Path, agent: &str, block: &Value, me: &Me) -> Result<(String, bool)> {
    let shown = file.display();
    let not_written = |err: &dyn std::fmt::Display| {
        Error::Msg(tr!(
            "could not write {shown}: {err}",
            "не удалось записать {shown}: {err}",
            "не вдалося записати {shown}: {err}"
        ))
    };
    let existing = fs::read(file).unwrap_or_default();
    if existing.is_empty() {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|err| not_written(&err))?;
        }
        fs::write(file, format!("{}\n", pretty(block))).map_err(|err| not_written(&err))?;
        return Ok((
            tr!("created {shown}", "создан {shown}", "створено {shown}"),
            true,
        ));
    }

    let unreadable = || {
        Error::Msg(tr!(
            "{shown} does not read as a JSON object, nothing changed",
            "{shown} не читается как JSON-объект, ничего не изменено",
            "{shown} не читається як JSON-об'єкт, нічого не змінено"
        ))
    };
    let mut doc: Value = serde_json::from_slice(&existing).map_err(|_| unreadable())?;
    let change = merge(&mut doc, block, agent, me).ok_or_else(unreadable)?;
    if change == Change::None {
        let message = tr!(
            "disp hooks are already in {shown}, nothing to change",
            "хуки disp уже подключены в {shown}, изменений нет",
            "хуки disp уже підключено в {shown}, змін немає"
        );
        return Ok((message, false));
    }

    // A settings file kept in a dotfiles repository is a symbolic link. The new text
    // goes to the file behind it: a rename over the link would leave a regular file
    // there and detach the settings from the repository.
    let real = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let meta = fs::metadata(&real)?;
    let mode = meta.permissions().mode() & 0o777;
    // Staged before the copies are rotated: if the file cannot be written, nothing changes.
    let text = format!("{}\n", pretty(&doc));
    let staged = stage(&real, text.as_bytes(), mode).map_err(|err| {
        let linked = fs::symlink_metadata(file).is_ok_and(|meta| meta.file_type().is_symlink());
        if !linked {
            return not_written(&err);
        }
        let real = real.display();
        Error::Msg(tr!(
            "{shown} is a link to {real}, which cannot be written: {err}. If another \
             program manages that file, add the block from `disp config {agent}` to its source",
            "{shown} — ссылка на {real}, а туда записать нельзя: {err}. Если этим файлом \
             управляет другая программа, добавь в её настройки блок из `disp config {agent}`",
            "{shown} — посилання на {real}, а туди записати не можна: {err}. Якщо цим файлом \
             керує інша програма, додай до її налаштувань блок із `disp config {agent}`"
        ))
    })?;
    let backup = match keep_copy(file, &real, &meta) {
        Ok(backup) => backup,
        Err(err) => {
            let _ = fs::remove_file(&staged);
            return Err(err);
        }
    };
    if let Err(err) = fs::rename(&staged, &real) {
        let _ = fs::remove_file(&staged);
        return Err(not_written(&err));
    }
    let backup = backup.display();
    let message = match change {
        Change::Updated => tr!(
            "disp hooks updated in {shown} (copy: {backup})",
            "хуки disp обновлены в {shown} (копия: {backup})",
            "хуки disp оновлено в {shown} (копія: {backup})"
        ),
        _ => tr!(
            "disp hooks added to {shown} (copy: {backup})",
            "хуки disp добавлены в {shown} (копия: {backup})",
            "хуки disp додано в {shown} (копія: {backup})"
        ),
    };
    Ok((message, true))
}

/// The name of a kept copy of the settings file: generation 0 is the newest.
fn backup_path(file: &Path, generation: usize) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    if generation > 0 {
        name.push(format!(".{generation}"));
    }
    name.push(".bak-disp");
    PathBuf::from(name)
}

/// Keeps the settings file as it is now in `<file>.bak-disp`. Earlier copies move one
/// generation up and the oldest of `BACKUPS` is dropped, so a second run does not
/// overwrite the only copy of the original settings. Returns the name of the new copy.
fn keep_copy(file: &Path, real: &Path, meta: &fs::Metadata) -> Result<PathBuf> {
    let backup = backup_path(file, 0);
    if let Err(err) = rotate(file).and_then(|()| fs::copy(real, &backup)) {
        let backup = backup.display();
        return Err(Error::Msg(tr!(
            "could not make a copy {backup}: {err}",
            "не удалось сделать копию {backup}: {err}",
            "не вдалося зробити копію {backup}: {err}"
        )));
    }
    if let (Ok(modified), Ok(copy)) = (
        meta.modified(),
        fs::File::options().write(true).open(&backup),
    ) {
        let _ = copy.set_modified(modified);
    }
    Ok(backup)
}

/// Moves the kept copies one generation up, dropping the oldest.
fn rotate(file: &Path) -> io::Result<()> {
    for generation in (0..BACKUPS - 1).rev() {
        let older = backup_path(file, generation + 1);
        match fs::rename(backup_path(file, generation), older) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

/// What `merge` did to the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// The hooks were already in order.
    None,
    /// Hooks were added and nothing else changed.
    Added,
    /// A hook of an earlier install was repaired or a repeat was removed.
    Updated,
}

/// Brings the disp hooks of one agent in line with the block.
///
/// Every event in the document is put in order first, the ones this version does not
/// subscribe to included: a hook of an earlier install would otherwise keep calling a
/// program that is gone. Then each event of the block that has no disp hook gets the
/// entry from the block. Foreign hooks and all other keys are left alone. `None` when
/// the document does not look like settings with hooks.
fn merge(doc: &mut Value, block: &Value, agent: &str, me: &Me) -> Option<Change> {
    let hooks = doc
        .as_object_mut()?
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    let wanted = block.get("hooks")?.as_object()?;
    let mut change = Change::None;
    for (event, entries) in hooks.iter_mut() {
        match entries.as_array_mut() {
            Some(entries) => {
                if tidy(entries, agent, me) {
                    change = Change::Updated;
                }
            }
            None if wanted.contains_key(event) => return None,
            None => {}
        }
    }
    for (event, entries) in wanted {
        let present = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()?;
        if !calls_disp(present, agent, me) {
            present.extend(entries.as_array()?.iter().cloned());
            if change == Change::None {
                change = Change::Added;
            }
        }
    }
    Some(change)
}

/// Puts the disp hooks of one event in order and returns whether anything changed.
///
/// Hooks that repeat one another — the same matcher and the same command apart from
/// the program path — are reduced to one. A run of earlier installs leaves such
/// repeats. The one that stays is the first that already calls this program, so a
/// stable name the user chose (a link kept by a package manager, `disp` from `PATH`)
/// wins over a versioned path; when none does, the first one stays and gets the path of
/// this program in place. Only the program word changes, so the matcher, the timeout,
/// other keys and the position the user gave it all stay.
///
/// Disp hooks that differ in anything else — the matcher, a `NAME=value` prefix,
/// redirections — are separate on purpose, and all of them stay.
fn tidy(entries: &mut Vec<Value>, agent: &str, me: &Me) -> bool {
    let found = disp_hooks(entries, agent, me);
    let mut kept: Vec<&Found> = Vec::new();
    for hook in &found {
        match kept.iter_mut().find(|kept| kept.group == hook.group) {
            None => kept.push(hook),
            Some(kept) if kept.call.kind == Kind::Stale && hook.call.kind == Kind::Current => {
                *kept = hook;
            }
            Some(_) => {}
        }
    }

    let mut changed = false;
    let mut entry_index = 0;
    entries.retain_mut(|entry| {
        let this_entry = entry_index;
        entry_index += 1;
        let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let before = list.len();
        let mut hook_index = 0;
        list.retain_mut(|hook| {
            let at = (this_entry, hook_index);
            hook_index += 1;
            let Some(this) = found.iter().find(|found| found.at == at) else {
                return true;
            };
            if !kept.iter().any(|kept| kept.at == at) {
                changed = true;
                return false;
            }
            if this.call.kind == Kind::Stale {
                let (before, after) = (&this.group.1, &this.group.2);
                hook["command"] = Value::String(format!("{before}{}{after}", me.quoted()));
                changed = true;
            }
            true
        });
        // An entry that held nothing but repeats goes with them.
        !(before > 0 && list.is_empty())
    });
    changed
}

/// A disp hook found in the list of an event.
struct Found {
    /// The index of its entry, and its index in the entry's hooks.
    at: (usize, usize),
    /// What another hook must share to be a repeat of this one: everything in the entry
    /// but its hooks (the matcher and the like), and the command before and after the
    /// program word.
    group: (Value, String, String),
    /// What the command is.
    call: Call,
}

/// Every disp hook of the agent among the entries of one event, in order.
fn disp_hooks(entries: &[Value], agent: &str, me: &Me) -> Vec<Found> {
    let mut found = Vec::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        let Some(list) = fields.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        let mut scope = fields.clone();
        scope.remove("hooks");
        let scope = Value::Object(scope);
        for (hook_index, hook) in list.iter().enumerate() {
            let Some(command) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            let Some(call) = disp_call(command, agent, me) else {
                continue;
            };
            found.push(Found {
                at: (entry_index, hook_index),
                group: (
                    scope.clone(),
                    command[..call.program.start].to_string(),
                    command[call.program.end..].to_string(),
                ),
                call,
            });
        }
    }
    found
}

/// Whether any hook of the event calls disp for the agent.
fn calls_disp(entries: &[Value], agent: &str, me: &Me) -> bool {
    entries
        .iter()
        .filter_map(|entry| entry.get("hooks")?.as_array())
        .flatten()
        .filter_map(|hook| hook.get("command")?.as_str())
        .any(|command| disp_call(command, agent, me).is_some())
}

/// A hook command that is a plain call of disp for one agent.
struct Call {
    /// Where the program word sits in the command.
    program: Range<usize>,
    /// What that word points at.
    kind: Kind,
}

/// Reads a hook command as `[NAME=value …] <program> hook <agent>`, redirections
/// allowed anywhere. `None` for everything else.
///
/// The command is taken apart the way a shell would, so the program is the word the
/// shell runs. Looking only at how the text ends would also catch a foreign command
/// that merely has a disp path among its arguments, or one that runs something else
/// first. Such commands, wrappers and anything with substitutions are not disp hooks
/// here: they are left exactly as they are.
fn disp_call(command: &str, agent: &str, me: &Me) -> Option<Call> {
    // An install made before paths were quoted wrote the path as it is. With a blank
    // in it that command never ran and does not read as a call, yet it is this
    // program's own hook and gets the quotes.
    let raw = me.exe.display().to_string();
    if raw != me.quoted() && command == format!("{raw} hook {agent}") {
        return Some(Call {
            program: 0..raw.len(),
            kind: Kind::Stale,
        });
    }
    let words = shell_words(command, me.home.as_deref())?;
    let assignments = words
        .iter()
        .take_while(|word| is_assignment(&command[word.span.clone()]))
        .count();
    let [program, verb, target] = &words[assignments..] else {
        return None;
    };
    if verb.text != "hook" || target.text != agent {
        return None;
    }
    Some(Call {
        program: program.span.clone(),
        kind: me.kind(&program.text)?,
    })
}

/// Whether a word, as written, is a `NAME=value` assignment in front of a command.
fn is_assignment(raw: &str) -> bool {
    raw.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|first: char| first.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// One word of a shell command.
#[derive(Debug, PartialEq, Eq)]
struct Word {
    /// Where the word sits in the command, quotes included.
    span: Range<usize>,
    /// What the shell makes of it: quotes removed, `~` and `$HOME` expanded.
    text: String,
}

/// Splits a simple shell command into its words, leaving the redirections out.
///
/// `None` for everything this does not model: several commands in one line, pipes,
/// groups, substitutions other than `$HOME`, patterns, comments. The caller then
/// treats the command as foreign, which is the safe side.
fn shell_words(command: &str, home: Option<&str>) -> Option<Vec<Word>> {
    let blanks = |at: usize| {
        let rest = &command[at..];
        at + rest.len() - rest.trim_start_matches([' ', '\t']).len()
    };
    let mut words = Vec::new();
    let mut at = blanks(0);
    while at < command.len() {
        let redirected = redirection(&command[at..]);
        if let Some(operator) = redirected {
            at = blanks(at + operator);
        }
        let word = word(command, at, home)?;
        if word.span.is_empty() {
            return None;
        }
        at = blanks(word.span.end);
        // The word after a redirection operator is its target, not part of the command.
        if redirected.is_none() {
            words.push(word);
        }
    }
    Some(words)
}

/// The length of the redirection operator the text starts with, a file descriptor in
/// front included: `>`, `>>`, `2>`, `2>&`, `&>`, `<`. `None` when it starts with none.
fn redirection(rest: &str) -> Option<usize> {
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let after = &rest[digits..];
    let operator = if digits == 0 && after.starts_with("&>>") {
        3
    } else if (digits == 0 && after.starts_with("&>"))
        || [">>", ">&", ">|", "<&"]
            .iter()
            .any(|two| after.starts_with(two))
    {
        2
    } else if after.starts_with(['>', '<']) {
        1
    } else {
        return None;
    };
    Some(digits + operator)
}

/// Reads one word starting at `start`. `None` when the word uses something
/// `shell_words` does not model.
fn word(command: &str, start: usize, home: Option<&str>) -> Option<Word> {
    let mut text = String::new();
    let mut end = command.len();
    let mut chars = command[start..].char_indices().peekable();
    // Skips the rest of a `$HOME` whose `$` has just been read and adds the directory.
    let home_var = |at: usize, text: &mut String| -> Option<usize> {
        let rest = &command[at..];
        let length = if rest.starts_with("${HOME}") {
            7
        } else {
            let after = rest.strip_prefix("$HOME")?;
            if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            5
        };
        text.push_str(home?);
        Some(length - 1)
    };
    while let Some((offset, c)) = chars.next() {
        let here = start + offset;
        match c {
            ' ' | '\t' | '<' | '>' => {
                end = here;
                break;
            }
            '\'' => loop {
                match chars.next()?.1 {
                    '\'' => break,
                    quoted => text.push(quoted),
                }
            },
            '"' => loop {
                match chars.next()? {
                    (_, '"') => break,
                    (_, '\\') => match chars.next()?.1 {
                        escaped @ ('"' | '\\' | '$' | '`') => text.push(escaped),
                        '\n' => {}
                        other => {
                            text.push('\\');
                            text.push(other);
                        }
                    },
                    (offset, '$') => {
                        for _ in 0..home_var(start + offset, &mut text)? {
                            chars.next();
                        }
                    }
                    (_, '`') => return None,
                    (_, quoted) => text.push(quoted),
                }
            },
            '\\' => text.push(chars.next()?.1),
            '$' => {
                for _ in 0..home_var(here, &mut text)? {
                    chars.next();
                }
            }
            '~' if here == start => match chars.peek() {
                None | Some((_, ' ' | '\t' | '/')) => text.push_str(home?),
                Some(_) => return None,
            },
            ';' | '|' | '&' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' | '`' | '\n' | '\r' => {
                return None;
            }
            '#' | '!' if here == start => return None,
            plain => text.push(plain),
        }
    }
    Some(Word {
        span: start..end,
        text,
    })
}

/// Quotes a path for the shell that runs the hook, when it needs quoting at all.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A program at `exe` with `/home/u` as its home and nothing in `PATH`.
    fn me(exe: &str) -> Me {
        Me {
            exe: PathBuf::from(exe),
            home: Some("/home/u".to_string()),
            path: None,
        }
    }

    fn hook(command: &str) -> Value {
        json!({"type": "command", "command": command})
    }

    fn entry(command: &str) -> Value {
        json!({"hooks": [hook(command)]})
    }

    /// Merges the Claude Code block reduced to one event.
    fn merged(doc: &mut Value, event: &str, me: &Me) -> Option<Change> {
        let block = json!({"hooks": {event: [entry(&me.command("claude"))]}});
        merge(doc, &block, "claude", me)
    }

    /// What `command` is as a hook of Claude Code.
    fn kind(command: &str, me: &Me) -> Option<Kind> {
        disp_call(command, "claude", me).map(|found| found.kind)
    }

    /// An empty directory of its own for a test that needs real files.
    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("disp-install-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(fs::create_dir_all(&dir).is_ok());
        dir
    }

    fn read_json(file: &Path) -> Value {
        serde_json::from_slice(&fs::read(file).unwrap_or_default()).unwrap_or_default()
    }

    #[test]
    fn merge_keeps_foreign_hooks_and_is_idempotent() {
        let me = me("/bin/disp");
        let mut doc = json!({"model": "x", "hooks": {"Stop": [entry("/usr/bin/true")]}});
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::Added));
        assert_eq!(
            doc,
            json!({"model": "x", "hooks": {"Stop": [entry("/usr/bin/true"), entry("/bin/disp hook claude")]}})
        );
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::None));
        assert_eq!(doc["hooks"]["Stop"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn merge_repairs_the_path_of_an_earlier_install_in_place() {
        // An update moved the program (Nix store, Homebrew Cellar). The hook keeps its
        // place and everything the user set on it; only the path changes.
        let custom = |path: &str| {
            json!({"matcher": "permission_prompt", "hooks": [
                {"type": "command", "command": format!("{path} hook claude"), "timeout": 30, "async": true}
            ]})
        };
        let mut doc = json!({"hooks": {"Notification": [
            custom("/nix/store/aaa-disp/bin/disp"),
            entry("/usr/bin/true")
        ]}});
        let me = me("/nix/store/bbb-disp/bin/disp");
        assert_eq!(merged(&mut doc, "Notification", &me), Some(Change::Updated));
        assert_eq!(
            doc["hooks"]["Notification"],
            json!([
                custom("/nix/store/bbb-disp/bin/disp"),
                entry("/usr/bin/true")
            ])
        );
        assert_eq!(merged(&mut doc, "Notification", &me), Some(Change::None));
    }

    #[test]
    fn merge_keeps_what_surrounds_the_program_word() {
        let mut doc = json!({"hooks": {"Stop": [
            entry("DISP_DESKTOP=0 '/old dir/disp' hook claude 2>/dev/null")
        ]}});
        assert_eq!(
            merged(&mut doc, "Stop", &me("/new/disp")),
            Some(Change::Updated)
        );
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([entry("DISP_DESKTOP=0 /new/disp hook claude 2>/dev/null")])
        );
    }

    #[test]
    fn merge_heals_duplicates_left_by_earlier_installs() {
        let mut doc = json!({"hooks": {"Stop": [
            entry("/a/disp hook claude"),
            entry("/b/disp hook claude"),
            json!({"hooks": [hook("/usr/bin/true"), hook("/c/disp hook claude")]}),
            entry("/c/disp hook claude")
        ]}});
        assert_eq!(
            merged(&mut doc, "Stop", &me("/c/disp")),
            Some(Change::Updated)
        );
        // The hook that already calls this program stays where it is.
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([{"hooks": [hook("/usr/bin/true"), hook("/c/disp hook claude")]}])
        );
    }

    #[test]
    fn merge_keeps_disp_hooks_that_differ_in_more_than_the_path() {
        // Two state directories, or a hook that keeps quiet: separate on purpose.
        let hooks: Vec<Value> = [
            "DISP_STATE_DIR=/team /old/disp hook claude",
            "DISP_STATE_DIR=/me /old/disp hook claude",
            "/old/disp hook claude 2>/dev/null",
            "/new/disp hook claude",
        ]
        .iter()
        .map(|command| hook(command))
        .collect();
        let mut doc = json!({"hooks": {"Stop": [{"hooks": hooks}]}});
        assert_eq!(
            merged(&mut doc, "Stop", &me("/new/disp")),
            Some(Change::Updated)
        );
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([{"hooks": [
                hook("DISP_STATE_DIR=/team /new/disp hook claude"),
                hook("DISP_STATE_DIR=/me /new/disp hook claude"),
                hook("/new/disp hook claude 2>/dev/null"),
                hook("/new/disp hook claude")
            ]}])
        );
    }

    #[test]
    fn merge_keeps_disp_hooks_under_different_matchers() {
        // The same hook under two matchers is the user's choice, not a repeat.
        let scoped =
            |matcher: &str| json!({"matcher": matcher, "hooks": [hook("/bin/disp hook claude")]});
        let before = json!({"hooks": {"Notification": [scoped("permission_prompt"), scoped("idle_prompt")]}});
        let mut doc = before.clone();
        assert_eq!(
            merged(&mut doc, "Notification", &me("/bin/disp")),
            Some(Change::None)
        );
        assert_eq!(doc, before);
    }

    #[test]
    fn merge_leaves_foreign_hooks_that_look_alike() {
        // Another program, another agent, a disp path among the arguments of a foreign
        // command, several commands in one line, a wrapper: none of these is a disp hook.
        let foreign = [
            "/bin/other hook claude",
            "/old/disp hook codex",
            "/usr/bin/printf '%s\\n' /opt/tools/disp hook claude",
            "/usr/bin/logger turn-finished; /old/disp hook claude",
            "timeout 4 /old/disp hook claude",
        ];
        let mut stop: Vec<Value> = foreign.iter().map(|command| entry(command)).collect();
        stop.push(json!({"matcher": "*", "hooks": [hook("/usr/bin/true"), hook("/old/disp hook claude")]}));
        let mut doc = json!({"hooks": {"Stop": stop}});
        let me = me("/new/disp");
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::Updated));
        let mut expected: Vec<Value> = foreign.iter().map(|command| entry(command)).collect();
        expected.push(json!({"matcher": "*", "hooks": [hook("/usr/bin/true"), hook("/new/disp hook claude")]}));
        assert_eq!(doc["hooks"]["Stop"], json!(expected));
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::None));
    }

    #[test]
    fn merge_adds_its_hook_next_to_a_command_it_cannot_read() {
        let compound = "/usr/bin/logger turn-finished; /old/disp hook claude";
        let mut doc = json!({"hooks": {"Stop": [entry(compound)]}});
        assert_eq!(
            merged(&mut doc, "Stop", &me("/new/disp")),
            Some(Change::Added)
        );
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([entry(compound), entry("/new/disp hook claude")])
        );
    }

    #[test]
    fn merge_repairs_hooks_under_events_outside_the_block() {
        let mut doc =
            json!({"hooks": {"PreCompact": [entry("/nix/store/old/bin/disp hook claude")]}});
        assert_eq!(
            merged(&mut doc, "Stop", &me("/new/disp")),
            Some(Change::Updated)
        );
        assert_eq!(
            doc["hooks"],
            json!({
                "PreCompact": [entry("/new/disp hook claude")],
                "Stop": [entry("/new/disp hook claude")]
            })
        );
    }

    #[test]
    fn merge_refuses_unexpected_shapes() {
        let me = me("/bin/disp");
        assert_eq!(merged(&mut json!([1, 2]), "Stop", &me), None);
        assert_eq!(merged(&mut json!({"hooks": "nope"}), "Stop", &me), None);
        assert_eq!(
            merged(&mut json!({"hooks": {"Stop": "nope"}}), "Stop", &me),
            None
        );
        // The same block is fine for a document of the expected shape.
        assert_eq!(merged(&mut json!({}), "Stop", &me), Some(Change::Added));
    }

    #[test]
    fn a_disp_hook_is_the_program_the_shell_runs() {
        let me = me("/new/disp");
        assert_eq!(kind("/new/disp hook claude", &me), Some(Kind::Current));
        for stale in [
            "/home/u/.local/bin/disp hook claude",
            "disp hook claude",
            "'/old dir/disp' hook claude",
            "\"/old dir/disp\" hook claude",
            "/old\\ dir/disp hook claude",
            "/old/disp hook claude 2>/dev/null",
            "/old/disp hook claude >/dev/null 2>&1",
            "/old/disp hook claude > /dev/null",
            "/old/disp hook claude &>/dev/null",
            "2>/dev/null /old/disp hook claude",
            "A=1 B='x y' /old/disp hook claude",
            "  /old/disp   hook\tclaude  ",
        ] {
            assert_eq!(kind(stale, &me), Some(Kind::Stale), "{stale}");
        }
        for foreign in [
            "",
            "/usr/bin/true",
            "/old/disp hook codex",
            "/usr/bin/display hook claude",
            "/old/disp hook",
            "/old/disp hook claude extra",
            "/usr/bin/printf '%s\\n' /opt/tools/disp hook claude",
            "/usr/bin/logger turn-finished; /old/disp hook claude",
            "/old/disp hook claude && true",
            "/old/disp hook claude | cat",
            "/old/disp hook claude &",
            "timeout 4 /old/disp hook claude",
            "env A=1 /old/disp hook claude",
            "/Users/u/Application Support/disp hook claude",
            "$DISP hook claude",
            "$(command -v disp) hook claude",
            "`command -v disp` hook claude",
            "/old/dis* hook claude",
            "'/old/disp hook claude",
            "/old/disp hook claude <<EOF",
            "/old/disp hook claude >",
            "# /old/disp hook claude",
        ] {
            assert_eq!(kind(foreign, &me), None, "{foreign}");
        }
        let command = "A=1 '/old dir/disp' hook claude";
        let program = disp_call(command, "claude", &me).map(|found| &command[found.program]);
        assert_eq!(program, Some("'/old dir/disp'"));
    }

    #[test]
    fn a_stable_name_of_this_program_is_this_program() {
        // A hook written by hand to survive updates: a link kept by a package manager,
        // the bare name from PATH, a path under the home directory.
        let dir = scratch("stable");
        let (store, bin, other) = (dir.join("store"), dir.join("bin"), dir.join("other"));
        for made in [&store, &bin, &other] {
            assert!(fs::create_dir_all(made).is_ok());
        }
        assert!(fs::write(store.join("disp"), "program").is_ok());
        assert!(fs::write(other.join("disp"), "another copy").is_ok());
        assert!(symlink(store.join("disp"), bin.join("disp")).is_ok());
        let me = Me {
            exe: fs::canonicalize(store.join("disp")).unwrap_or_default(),
            home: Some(dir.display().to_string()),
            path: Some(bin.clone().into_os_string()),
        };
        let link = bin.join("disp").display().to_string();
        for stable in [
            format!("{link} hook claude"),
            "disp hook claude".to_string(),
            "~/bin/disp hook claude".to_string(),
            "$HOME/bin/disp hook claude".to_string(),
            "\"${HOME}/bin/disp\" hook claude".to_string(),
        ] {
            assert_eq!(kind(&stable, &me), Some(Kind::Current), "{stable}");
        }
        let copy = format!("{} hook claude", other.join("disp").display());
        assert_eq!(kind(&copy, &me), Some(Kind::Stale));
        assert_eq!(kind("~/gone/disp hook claude", &me), Some(Kind::Stale));
        assert_eq!(kind("$HOMER/bin/disp hook claude", &me), None);
        assert_eq!(kind("~other/bin/disp hook claude", &me), None);

        let before = json!({"hooks": {"Stop": [entry("disp hook claude")]}});
        let mut doc = before.clone();
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::None));
        assert_eq!(doc, before);

        // A repeat with a versioned path, before or after the stable name: the stable
        // name stays, whatever the order, and the repeat goes.
        let stable = entry(&format!("{link} hook claude"));
        for order in [
            [entry(&copy), stable.clone()],
            [stable.clone(), entry(&copy)],
        ] {
            let mut doc = json!({"hooks": {"Stop": order}});
            assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::Updated));
            assert_eq!(doc["hooks"]["Stop"], json!([stable]));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renamed_program_knows_its_earlier_installs() {
        // A package may wrap the program: the executable is then `.disp-wrapped`.
        let wrapped = "/nix/store/old/bin/.disp-wrapped hook claude";
        assert_eq!(
            kind(wrapped, &me("/nix/store/new/bin/.disp-wrapped")),
            Some(Kind::Stale)
        );
        assert_eq!(kind(wrapped, &me("/new/disp")), None);
    }

    #[test]
    fn the_path_is_quoted_when_the_shell_needs_it() {
        assert_eq!(
            shell_quote("/home/u/.local/bin/disp"),
            "/home/u/.local/bin/disp"
        );
        assert_eq!(
            shell_quote("/Users/u/Application Support/disp"),
            "'/Users/u/Application Support/disp'"
        );
        assert_eq!(shell_quote("/o'neil/disp"), r"'/o'\''neil/disp'");
        // Whatever the path, the command written is read back as this program.
        for path in [
            "/bin/disp",
            "/Users/u/Application Support/disp",
            "/o'neil/$x/disp",
            "/дом/disp",
        ] {
            let me = me(path);
            assert_eq!(
                kind(&me.command("claude"), &me),
                Some(Kind::Current),
                "{path}"
            );
        }
        // An earlier install wrote such a path without quotes: the hook never ran.
        let me = me("/Users/u/Application Support/disp");
        let mut doc =
            json!({"hooks": {"Stop": [entry("/Users/u/Application Support/disp hook claude")]}});
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::Updated));
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([entry("'/Users/u/Application Support/disp' hook claude")])
        );
        assert_eq!(merged(&mut doc, "Stop", &me), Some(Change::None));
    }

    #[test]
    fn apply_writes_through_a_symbolic_link_and_keeps_earlier_copies() {
        // Settings kept in a dotfiles repository: the link stays a link, the text goes
        // to the file behind it and the copies stay next to the link.
        let dir = scratch("link");
        let (repo, conf) = (dir.join("dotfiles"), dir.join("conf"));
        assert!(fs::create_dir_all(&repo).is_ok() && fs::create_dir_all(&conf).is_ok());
        let (real, link) = (repo.join("settings.json"), conf.join("settings.json"));
        assert!(fs::write(&real, "{\"model\":\"x\"}\n").is_ok());
        assert!(symlink(&real, &link).is_ok());
        let install = |exe: &str| {
            let me = me(exe);
            let block = block("claude", &me.command("claude")).unwrap_or_default();
            apply(&link, "claude", &block, &me)
                .map(|(_, changed)| changed)
                .ok()
        };
        let is_link =
            || fs::symlink_metadata(&link).is_ok_and(|meta| meta.file_type().is_symlink());

        assert_eq!(install("/a/disp"), Some(true));
        assert!(is_link());
        assert_eq!(
            read_json(&real)["hooks"].as_object().map(Map::len),
            Some(12)
        );
        assert_eq!(
            read_json(&real)["hooks"]["Stop"],
            json!([{"hooks": [
                {"type": "command", "command": "/a/disp hook claude", "timeout": 5}
            ]}])
        );

        assert_eq!(install("/b/disp"), Some(true));
        assert!(is_link());
        assert_eq!(
            read_json(&real).pointer("/hooks/Stop/0/hooks/0/command"),
            Some(&json!("/b/disp hook claude"))
        );
        assert_eq!(
            read_json(&backup_path(&link, 0)).pointer("/hooks/Stop/0/hooks/0/command"),
            Some(&json!("/a/disp hook claude"))
        );
        assert_eq!(read_json(&backup_path(&link, 1)), json!({"model": "x"}));

        assert_eq!(install("/b/disp"), Some(false));
        assert!(!backup_path(&link, 2).exists());
        // Nothing is left behind next to the real file.
        assert_eq!(fs::read_dir(&repo).map(Iterator::count).ok(), Some(1));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_drops_the_oldest_copy() {
        let dir = scratch("copies");
        let file = dir.join("settings.json");
        assert!(fs::write(&file, "{}\n").is_ok());
        for run in 0..BACKUPS + 2 {
            let me = me(&format!("/v{run}/disp"));
            let block = block("claude", &me.command("claude")).unwrap_or_default();
            assert!(apply(&file, "claude", &block, &me).is_ok_and(|(_, changed)| changed));
        }
        assert!(backup_path(&file, BACKUPS - 1).exists());
        assert!(!backup_path(&file, BACKUPS).exists());
        // The newest copy is the file as the last run found it.
        let previous = format!("/v{BACKUPS}/disp hook claude");
        assert_eq!(
            read_json(&backup_path(&file, 0)).pointer("/hooks/Stop/0/hooks/0/command"),
            Some(&json!(previous))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_block_respects_the_three_second_limit() {
        let block = config_block("codex").unwrap_or_default();
        assert_eq!(
            block.pointer("/hooks/Interrupt/0/hooks/0/timeout"),
            Some(&json!(3))
        );
        assert_eq!(
            block.pointer("/hooks/SessionEnd/0/hooks/0/timeout"),
            Some(&json!(3))
        );
        assert_eq!(block["hooks"].as_object().map(Map::len), Some(9));
        assert!(config_block("roo").is_none());
    }
}
