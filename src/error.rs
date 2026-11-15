//! Единый тип ошибки для команд disp.

use std::fmt;
use std::io;

/// Ошибка выполнения команды.
#[derive(Debug)]
pub enum Error {
    /// Ошибка ввода-вывода.
    Io(io::Error),
    /// Некорректный JSON.
    Json(serde_json::Error),
    /// Ошибка с готовым текстом для пользователя.
    Msg(String),
}

/// Результат с ошибкой [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "{err}"),
            Error::Json(err) => write!(f, "{err}"),
            Error::Msg(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Json(err) => Some(err),
            Error::Msg(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(err)
    }
}
