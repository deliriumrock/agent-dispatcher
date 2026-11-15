//! Хранилище: файлы сессий, блокировка и журнал смен статуса.
//!
//! Запись атомарная (временный файл и переименование), поэтому читать файлы можно
//! в любой момент. Блокировка общая с bash-версией: тот же файл и тот же `flock`.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::error::Result;
use crate::model::Session;

/// Сколько ждать блокировку, прежде чем работать без неё.
const LOCK_WAIT: Duration = Duration::from_secs(3);

/// Запись журнала: смена статуса сессии.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Время события.
    #[serde(default)]
    pub ts: i64,
    /// Агент.
    #[serde(default)]
    pub agent: String,
    /// Полный идентификатор сессии.
    #[serde(default)]
    pub sid: String,
    /// Подпись задачи.
    #[serde(default)]
    pub task: String,
    /// Где шла сессия.
    #[serde(default, rename = "where")]
    pub place: String,
    /// Прежний статус; `new` для только что появившейся сессии.
    #[serde(default)]
    pub from: String,
    /// Новый статус; `ended` при закрытии сессии.
    #[serde(default)]
    pub to: String,
}

/// Доступ к файлам состояния.
#[derive(Clone, Debug)]
pub struct Store {
    /// Пути к файлам диспетчера.
    pub paths: Paths,
}

impl Store {
    /// Хранилище по заданным путям.
    pub fn new(paths: Paths) -> Self {
        Store { paths }
    }

    /// Создаёт каталоги состояния, закрытые от других пользователей.
    pub fn ensure_dirs(&self) -> Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.paths.sessions)?;
        Ok(())
    }

    /// Файл сессии агента.
    pub fn session_file(&self, agent: &str, sid: &str) -> PathBuf {
        self.paths.sessions.join(format!("{agent}-{sid}.json"))
    }

    /// Все файлы сессий по алфавиту.
    pub fn files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.paths.sessions) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        files
    }

    /// Все читаемые сессии вместе с путями к их файлам. Битые файлы пропускаются.
    pub fn all(&self) -> Vec<(PathBuf, Session)> {
        self.files()
            .into_iter()
            .filter_map(|path| read_session(&path).map(|session| (path, session)))
            .collect()
    }

    /// Берёт общую блокировку состояния. Если за три секунды не удалось, возвращает
    /// файл без блокировки: событие лучше обработать без неё, чем потерять.
    /// Блокировка снимается, когда возвращённый файл закрывается.
    pub fn lock(&self) -> Option<File> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(self.paths.lock())
            .ok()?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(file),
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return Some(file),
            }
        }
    }

    /// Дописывает запись в журнал смен статуса.
    pub fn append_event(&self, event: &JournalEvent) -> Result<()> {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.paths.events())?;
        file.write_all(line.as_bytes())?;
        Ok(())
    }
}

/// Отпечаток каталога сессий: имена файлов, время изменения и размер.
pub type Signature = Vec<(OsString, Option<SystemTime>, u64)>;

/// Отпечаток каталога сессий. Файлы сессий заменяются целиком, поэтому любое событие
/// агента меняет время изменения файла: по отпечатку видно, пора ли перечитывать.
pub fn dir_signature(dir: &Path) -> Signature {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Signature = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            Some((entry.file_name(), meta.modified().ok(), meta.len()))
        })
        .collect();
    files.sort();
    files
}

/// Время изменения и размер файла; `None`, если файла нет.
pub fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Читает файл сессии. `None`, если файла нет или это не запись сессии.
pub fn read_session(path: &Path) -> Option<Session> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Атомарно записывает файл сессии с правами 600.
pub fn write_session(path: &Path, session: &Session) -> Result<()> {
    let mut line = serde_json::to_string(session)?;
    line.push('\n');
    write_atomic(path, line.as_bytes(), 0o600)
}

/// Записывает файл через временный и переименование.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(name);
    let written = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path));
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}
