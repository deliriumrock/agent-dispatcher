//! Подключение хуков к настройкам агентов: `disp config` и `disp install`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::config::env_nonempty;
use crate::error::{Error, Result};
use crate::store::write_atomic;
use crate::text::{emit, warn};

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
            warn("использование: disp config claude|codex|gemini");
            2
        }
    }
}

/// `disp install <агент>`: подключить хуки к настройкам агента. Добавляет только
/// недостающие записи, чужие хуки и остальные ключи не трогает, перед записью
/// сохраняет прежний файл рядом как `<файл>.bak-disp`.
pub fn install(args: &[String]) -> u8 {
    let agent = args.first().map(String::as_str).unwrap_or_default();
    let (Some(target), Some(block)) = (target(agent), config_block(agent)) else {
        warn("использование: disp install claude|codex|gemini");
        return 2;
    };
    match apply(&target.file, &block) {
        Ok(message) => {
            emit(&format!("{message}\n"));
            if !message.contains("изменений нет") {
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
            hint: "Открытые сессии Claude Code подхватят хуки после /hooks или перезапуска.",
        },
        "codex" => Target {
            file: path("DISP_CODEX_HOOKS", ".codex/hooks.json"),
            hint: "Открой /hooks в Codex и подтверди доверие: без этого он хуки пропускает.",
        },
        "gemini" => Target {
            file: path("DISP_GEMINI_SETTINGS", ".gemini/settings.json"),
            hint: "Новые сессии Gemini CLI подхватят хуки при запуске.",
        },
        _ => return None,
    })
}

/// Вносит блок хуков в файл настроек и возвращает сообщение о том, что сделано.
fn apply(file: &Path, block: &Value) -> Result<String> {
    let shown = file.display();
    let existing = fs::read(file).unwrap_or_default();
    if existing.is_empty() {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| Error::Msg(format!("не удалось записать {shown}: {err}")))?;
        }
        fs::write(file, format!("{}\n", pretty(block)))
            .map_err(|err| Error::Msg(format!("не удалось записать {shown}: {err}")))?;
        return Ok(format!("создан {shown}"));
    }

    let unreadable = || {
        Error::Msg(format!(
            "{shown} не читается как JSON-объект, ничего не изменено"
        ))
    };
    let mut doc: Value = serde_json::from_slice(&existing).map_err(|_| unreadable())?;
    if !merge(&mut doc, block).ok_or_else(unreadable)? {
        return Ok(format!("хуки disp уже подключены в {shown}, изменений нет"));
    }

    let backup = PathBuf::from(format!("{shown}.bak-disp"));
    let meta = fs::metadata(file)?;
    fs::copy(file, &backup).map_err(|err| {
        Error::Msg(format!(
            "не удалось сделать копию {}: {err}",
            backup.display()
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
        .map_err(|err| Error::Msg(format!("не удалось записать {shown}: {err}")))?;
    Ok(format!(
        "хуки disp добавлены в {shown} (копия: {})",
        backup.display()
    ))
}

/// Добавляет в документ записи блока, которых там ещё нет. Возвращает, изменился ли
/// документ; `None`, если его структура не похожа на настройки с хуками.
fn merge(doc: &mut Value, block: &Value) -> Option<bool> {
    let root = doc.as_object_mut()?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    let mut changed = false;
    for (event, entries) in block.get("hooks")?.as_object()? {
        let command = entries.pointer("/0/hooks/0/command")?;
        let present = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()?;
        let already = present.iter().any(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|list| list.iter().any(|hook| hook.get("command") == Some(command)))
        });
        if !already {
            present.extend(entries.as_array()?.iter().cloned());
            changed = true;
        }
    }
    Some(changed)
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
