//! Подключение хуков к настройкам агентов: `disp config` и `disp install`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::env_nonempty;
use crate::error::{Error, Result};
use crate::store::write_atomic;
use crate::text::{emit, warn};
use crate::{t, tr};

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

/// Блок хуков для агента с абсолютным путём к этой программе. `None` — агент неизвестен.
pub fn config_block(agent: &str) -> Option<Value> {
    let exe = env::current_exe()
        .and_then(fs::canonicalize)
        .unwrap_or_else(|_| PathBuf::from("disp"));
    let command = format!("{} hook {agent}", exe.display());
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

/// `disp install <агент>`: подключить хуки к настройкам агента. Добавляет недостающие
/// записи и заменяет хуки прежней установки disp (другой путь к программе), чужие хуки
/// и остальные ключи не трогает, перед записью сохраняет прежний файл рядом как
/// `<файл>.bak-disp`.
pub fn install(args: &[String]) -> u8 {
    let agent = args.first().map(String::as_str).unwrap_or_default();
    let (Some(target), Some(block)) = (target(agent), config_block(agent)) else {
        warn(t!(
            "usage: disp install claude|codex|gemini",
            "использование: disp install claude|codex|gemini",
            "використання: disp install claude|codex|gemini"
        ));
        return 2;
    };
    match apply(&target.file, &block) {
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

/// Вносит блок хуков в файл настроек. Возвращает сообщение о том, что сделано, и
/// изменился ли файл.
fn apply(file: &Path, block: &Value) -> Result<(String, bool)> {
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
        return Ok((tr!("created {shown}", "создан {shown}", "створено {shown}"), true));
    }

    let unreadable = || {
        Error::Msg(tr!(
            "{shown} does not read as a JSON object, nothing changed",
            "{shown} не читается как JSON-объект, ничего не изменено",
            "{shown} не читається як JSON-об'єкт, нічого не змінено"
        ))
    };
    let mut doc: Value = serde_json::from_slice(&existing).map_err(|_| unreadable())?;
    if !merge(&mut doc, block).ok_or_else(unreadable)? {
        let message = tr!(
            "disp hooks are already in {shown}, nothing to change",
            "хуки disp уже подключены в {shown}, изменений нет",
            "хуки disp уже підключено в {shown}, змін немає"
        );
        return Ok((message, false));
    }

    let backup = PathBuf::from(format!("{shown}.bak-disp"));
    let meta = fs::metadata(file)?;
    fs::copy(file, &backup).map_err(|err| {
        let backup = backup.display();
        Error::Msg(tr!(
            "could not make a copy {backup}: {err}",
            "не удалось сделать копию {backup}: {err}",
            "не вдалося зробити копію {backup}: {err}"
        ))
    })?;
    if let (Ok(modified), Ok(copy)) = (
        meta.modified(),
        fs::File::options().write(true).open(&backup),
    ) {
        let _ = copy.set_modified(modified);
    }
    let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777;
    write_atomic(file, format!("{}\n", pretty(&doc)).as_bytes(), mode)
        .map_err(|err| not_written(&err))?;
    let backup = backup.display();
    let message = tr!(
        "disp hooks added to {shown} (copy: {backup})",
        "хуки disp добавлены в {shown} (копия: {backup})",
        "хуки disp додано в {shown} (копія: {backup})"
    );
    Ok((message, true))
}

/// Приводит хуки disp в документе к блоку: на каждое событие ровно одна запись с
/// текущей командой. Хуки disp с другим путём к программе (прежняя установка,
/// например версия из Nix store или Homebrew Cellar, которую сменило обновление)
/// и повторы убираются; чужие хуки и остальные ключи не трогаются. Возвращает,
/// изменился ли документ; `None`, если его структура не похожа на настройки с хуками.
fn merge(doc: &mut Value, block: &Value) -> Option<bool> {
    let root = doc.as_object_mut()?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    let mut changed = false;
    for (event, entries) in block.get("hooks")?.as_object()? {
        let command = entries.pointer("/0/hooks/0/command")?.as_str()?;
        let (_, agent) = split_hook_command(command)?;
        let present = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()?;

        // Все хуки disp этого агента в событии: сколько из них уже с нужной командой
        // и есть ли среди них устаревшие.
        let mut current = 0;
        let mut stale = false;
        for hook in present.iter().filter_map(|entry| entry.get("hooks")?.as_array()).flatten() {
            let Some(other) = hook.get("command").and_then(Value::as_str) else {
                continue;
            };
            if other == command {
                current += 1;
            } else if is_disp_hook(other, agent) {
                stale = true;
            }
        }
        if current == 1 && !stale {
            continue;
        }

        // Убрать все хуки disp этого агента, а запись, в которой не осталось ни одного
        // хука, — целиком. Запись, где рядом лежат чужие хуки, остаётся с ними.
        present.retain_mut(|entry| {
            let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = list.len();
            list.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|other| other == command || is_disp_hook(other, agent))
            });
            !(before > 0 && list.is_empty())
        });
        present.extend(entries.as_array()?.iter().cloned());
        changed = true;
    }
    Some(changed)
}

/// Делит команду хука `<программа> hook <агент>` на путь к программе и агента.
fn split_hook_command(command: &str) -> Option<(&str, &str)> {
    command.rsplit_once(" hook ")
}

/// Хук ли это disp для данного агента: команда вида `<…>/disp hook <агент>`.
/// Путь к программе может быть любым: по нему disp узнаёт свои прежние установки.
fn is_disp_hook(command: &str, agent: &str) -> bool {
    split_hook_command(command).is_some_and(|(exe, other)| {
        other == agent && Path::new(exe).file_name().is_some_and(|name| name == "disp")
    })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_foreign_hooks_and_is_idempotent() {
        let block = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/bin/disp hook claude"}]}]}});
        let mut doc = json!({
            "model": "x",
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/true"}]}]}
        });
        assert_eq!(merge(&mut doc, &block), Some(true));
        assert_eq!(doc["hooks"]["Stop"].as_array().map(Vec::len), Some(2));
        assert_eq!(doc["model"], "x");
        assert_eq!(merge(&mut doc, &block), Some(false));
        assert_eq!(doc["hooks"]["Stop"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn merge_replaces_a_previous_install_at_another_path() {
        // Обновление сменило путь к программе (Nix store, Homebrew Cellar): прежний хук
        // заменяется новым, а не остаётся рядом с ним.
        let old = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/nix/store/aaa-disp/bin/disp hook claude"}]}]}});
        let new = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/nix/store/bbb-disp/bin/disp hook claude"}]}]}});
        let mut doc = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "/usr/bin/true"}]}
        ]}});
        assert_eq!(merge(&mut doc, &old), Some(true));
        assert_eq!(merge(&mut doc, &new), Some(true));
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([
                {"hooks": [{"type": "command", "command": "/usr/bin/true"}]},
                {"hooks": [{"type": "command", "command": "/nix/store/bbb-disp/bin/disp hook claude"}]}
            ])
        );
        assert_eq!(merge(&mut doc, &new), Some(false));
    }

    #[test]
    fn merge_heals_duplicates_left_by_earlier_installs() {
        let block = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/c/disp hook claude"}]}]}});
        let entry = |path: &str| json!({"hooks": [{"type": "command", "command": format!("{path} hook claude")}]});
        let mut doc = json!({"hooks": {"Stop": [entry("/a/disp"), entry("/b/disp"), entry("/c/disp"), entry("/c/disp")]}});
        assert_eq!(merge(&mut doc, &block), Some(true));
        assert_eq!(doc["hooks"]["Stop"], json!([entry("/c/disp")]));
    }

    #[test]
    fn merge_leaves_foreign_hooks_that_look_alike() {
        // Не disp, другой агент, чужой хук в одной записи с disp: всё это остаётся.
        let block = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "/new/disp hook claude"}]}]}});
        let mut doc = json!({"hooks": {"Stop": [
            {"hooks": [{"type": "command", "command": "/bin/other hook claude"}]},
            {"hooks": [{"type": "command", "command": "/old/disp hook codex"}]},
            {"matcher": "*", "hooks": [
                {"type": "command", "command": "/usr/bin/true"},
                {"type": "command", "command": "/old/disp hook claude"}
            ]}
        ]}});
        assert_eq!(merge(&mut doc, &block), Some(true));
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([
                {"hooks": [{"type": "command", "command": "/bin/other hook claude"}]},
                {"hooks": [{"type": "command", "command": "/old/disp hook codex"}]},
                {"matcher": "*", "hooks": [{"type": "command", "command": "/usr/bin/true"}]},
                {"hooks": [{"type": "command", "command": "/new/disp hook claude"}]}
            ])
        );
    }

    #[test]
    fn disp_hook_is_recognised_by_program_name_and_agent() {
        assert!(is_disp_hook("/home/u/.local/bin/disp hook claude", "claude"));
        assert!(is_disp_hook("disp hook gemini", "gemini"));
        assert!(is_disp_hook("/Users/u/Application Support/disp hook codex", "codex"));
        assert!(!is_disp_hook("/home/u/.local/bin/disp hook claude", "codex"));
        assert!(!is_disp_hook("/usr/bin/display hook claude", "claude"));
        assert!(!is_disp_hook("/usr/bin/true", "claude"));
    }

    #[test]
    fn merge_refuses_unexpected_shapes() {
        let block = json!({"hooks": {"Stop": [{"hooks": [{"command": "c"}]}]}});
        assert_eq!(merge(&mut json!([1, 2]), &block), None);
        assert_eq!(merge(&mut json!({"hooks": "nope"}), &block), None);
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
