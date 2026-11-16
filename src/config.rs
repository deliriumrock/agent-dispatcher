//! Пути к состоянию, настройки из окружения и файла `env`, текущее время.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Ключи, которые читаются из файла настроек. Остальные строки файла игнорируются:
/// файл не исполняется и не может задать произвольную переменную.
const CONF_KEYS: [&str; 16] = [
    "DISP_TG_TOKEN",
    "DISP_TG_CHAT",
    "DISP_NOTIFY_CMD",
    "DISP_NOTIFY_MIN_SEC",
    "DISP_DESKTOP",
    "DISP_AUTO_OK_ON_EXIT",
    "DISP_STALE_MIN",
    "DISP_DEBUG",
    "DISP_PANE_TITLE",
    "DISP_GFX",
    "DISP_PANE_LABEL",
    "DISP_TERM_EVERYTHING",
    "DISP_GUI_BIN",
    "DISP_TRAY",
    "DISP_TRAY_BIN",
    "DISP_CONTAINERS",
];

/// Непустое значение переменной окружения. Пустая строка считается отсутствием,
/// как `${VAR:-}` в shell.
pub fn env_nonempty(key: &str) -> Option<String> {
    env::var_os(key)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
}

/// Текущее время в unix-секундах. `DISP_NOW` подменяет его в тестах.
pub fn now() -> i64 {
    env_nonempty("DISP_NOW")
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() as i64)
                .unwrap_or(0)
        })
}

/// Где лежат файлы диспетчера.
#[derive(Clone, Debug)]
pub struct Paths {
    /// Каталог состояния: `$DISP_STATE_DIR` или `~/.local/state/disp`.
    pub state_dir: PathBuf,
    /// Каталог с файлами сессий, по одному JSON-файлу на сессию.
    pub sessions: PathBuf,
    /// Файл настроек: `$DISP_CONFIG` или `~/.config/disp/env`.
    pub conf_file: PathBuf,
}

impl Paths {
    /// Определяет пути по переменным окружения, как это делала bash-версия.
    pub fn from_env() -> Self {
        let home = PathBuf::from(env_nonempty("HOME").unwrap_or_default());
        let state_dir = env_nonempty("DISP_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env_nonempty("XDG_STATE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join("disp")
            });
        let conf_file = env_nonempty("DISP_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                env_nonempty("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"))
                    .join("disp/env")
            });
        let sessions = state_dir.join("sessions");
        Paths {
            state_dir,
            sessions,
            conf_file,
        }
    }

    /// Журнал смен статуса.
    pub fn events(&self) -> PathBuf {
        self.state_dir.join("events.jsonl")
    }

    /// Журнал ошибок хука.
    pub fn hook_log(&self) -> PathBuf {
        self.state_dir.join("hook.log")
    }

    /// Файл блокировки, общий с bash-версией.
    pub fn lock(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    /// Замок запущенного значка в трее.
    pub fn tray_lock(&self) -> PathBuf {
        self.state_dir.join("tray.lock")
    }

    /// Журнал ошибок значка в трее, запущенного через `disp`.
    pub fn tray_log(&self) -> PathBuf {
        self.state_dir.join("tray.log")
    }

    /// Сырые события хуков; пишутся только при `DISP_DEBUG=1`.
    pub fn payloads(&self) -> PathBuf {
        self.state_dir.join("payloads.tsv")
    }
}

/// Настройки: переменные окружения, а для неустановленных — значения из файла.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    values: HashMap<&'static str, String>,
}

impl Settings {
    /// Читает настройки. Окружение важнее файла; в файле учитываются только известные ключи.
    pub fn load(conf_file: &Path) -> Self {
        let mut values = HashMap::new();
        for key in CONF_KEYS {
            if let Some(value) = env_nonempty(key) {
                values.insert(key, value);
            }
        }
        if let Ok(bytes) = fs::read(conf_file) {
            for line in String::from_utf8_lossy(&bytes).lines() {
                let Some((name, value)) = parse_line(line) else {
                    continue;
                };
                if value.is_empty() {
                    continue;
                }
                if let Some(key) = CONF_KEYS.iter().copied().find(|known| *known == name) {
                    values.entry(key).or_insert(value);
                }
            }
        }
        Settings { values }
    }

    /// Значение настройки, если оно задано и не пусто.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Целочисленная настройка; при отсутствии или нечисловом значении — `default`.
    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.get(key)
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(default)
    }
}

/// Разбирает строку `КЛЮЧ=значение`. Комментарии и пустые строки дают `None`.
fn parse_line(line: &str) -> Option<(String, String)> {
    let (name, value) = line.split_once('=').unwrap_or((line, ""));
    let name = name.trim();
    if name.is_empty() || name.starts_with('#') {
        return None;
    }
    let name = name.strip_prefix("export ").unwrap_or(name).trim();
    let value = value.strip_suffix('\r').unwrap_or(value);
    let value = strip_pair(strip_pair(value, '"'), '\'');
    Some((name.to_string(), value.to_string()))
}

/// Снимает по одной кавычке `quote` с каждого края, если они там есть.
fn strip_pair(text: &str, quote: char) -> &str {
    let text = text.strip_prefix(quote).unwrap_or(text);
    text.strip_suffix(quote).unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lines_are_data_not_shell() {
        assert_eq!(parse_line("# комментарий"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(
            parse_line("DISP_STALE_MIN=\"1\""),
            Some(("DISP_STALE_MIN".to_string(), "1".to_string()))
        );
        assert_eq!(
            parse_line("export DISP_TG_CHAT='-100=x'\r"),
            Some(("DISP_TG_CHAT".to_string(), "-100=x".to_string()))
        );
        let (name, value) = parse_line("EVIL=$(touch /tmp/pwned)").unwrap_or_default();
        assert_eq!(name, "EVIL");
        assert_eq!(value, "$(touch /tmp/pwned)");
    }
}
