//! Модель данных: запись о сессии агента и то, как она выглядит на доске.
//!
//! Формат файла сессии общий с bash-версией: одна строка JSON, времена в unix-секундах.

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::t;

/// Статус сессии, как он хранится в файле.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", from = "String")]
pub enum Status {
    /// Сессия открыта, заданий не было.
    #[default]
    Idle,
    /// Идёт ход.
    Working,
    /// Агент ждёт разрешения или ответа.
    Waiting,
    /// Ход закончен.
    Done,
    /// Ход оборвался ошибкой.
    Error,
}

impl From<String> for Status {
    fn from(name: String) -> Self {
        match name.as_str() {
            "working" => Status::Working,
            "waiting" => Status::Waiting,
            "done" => Status::Done,
            "error" => Status::Error,
            _ => Status::Idle,
        }
    }
}

impl Status {
    /// Имя статуса в файле сессии и в журнале.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Working => "working",
            Status::Waiting => "waiting",
            Status::Done => "done",
            Status::Error => "error",
        }
    }
}

/// Как сессия выглядит на доске. Порядок вариантов — порядок сортировки таблицы
/// по статусу: сверху то, что требует человека, затем работающие, в конце готовые.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum View {
    /// Агент ждёт человека.
    Wait,
    /// Ход оборвался ошибкой.
    Err,
    /// Сессию закрыли посреди работы.
    Lost,
    /// Числится работающей и давно молчит, а по процессам видно: агент жив, но команды
    /// у него не идут.
    Stuck,
    /// Идёт ход.
    Work,
    /// Числится работающей и давно молчит, а проверить, что с агентом, нельзя.
    Quiet,
    /// Заданий не было.
    Idle,
    /// Ход закончен, результат не принят.
    Done,
}

impl View {
    /// Короткое имя для `disp ls --json`.
    pub fn key(self) -> &'static str {
        match self {
            View::Wait => "wait",
            View::Err => "err",
            View::Lost => "lost",
            View::Stuck => "stuck",
            View::Done => "done",
            View::Quiet => "quiet",
            View::Work => "work",
            View::Idle => "idle",
        }
    }

    /// Подпись в колонке «СТАТУС»: знак и не больше девяти букв.
    pub fn label(self) -> &'static str {
        match self {
            View::Wait => t!("! WAITING", "! ЖДЁТ", "! ЧЕКАЄ"),
            View::Err => t!("x FAILED", "x СБОЙ", "x ЗБІЙ"),
            View::Lost => t!("x LOST", "x ПРЕРВАНА", "x ОБІРВАНА"),
            View::Done => t!("✓ DONE", "✓ ГОТОВО", "✓ ГОТОВО"),
            View::Stuck => t!("? stuck", "? завис", "? завис"),
            View::Quiet => t!("? quiet", "? молчит", "? мовчить"),
            View::Work => t!("● working", "● работает", "● працює"),
            View::Idle => t!("○ idle", "○ простой", "○ простій"),
        }
    }

    /// Код цвета SGR для строки таблицы.
    pub fn color(self) -> &'static str {
        match self {
            View::Wait => "1;33",
            View::Err | View::Lost => "1;31",
            View::Done => "32",
            View::Stuck => "1;35",
            View::Quiet => "35",
            View::Work => "36",
            View::Idle => "2",
        }
    }
}

/// Процесс агента: номер и время запуска. Номера процессов переиспользуются, поэтому
/// процесс считается тем же, только если совпало и время запуска.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProcess {
    /// Номер процесса.
    pub pid: u32,
    /// Время запуска в тиках с загрузки системы: поле 22 `/proc/<pid>/stat`.
    pub start: u64,
}

/// Агент, которого запустила сессия: субагент Claude Code или дочерний агент Codex.
/// Список ведёт сверка (`crate::agents`) по файлам самих агентов.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgent {
    /// Идентификатор агента.
    pub id: String,
    /// Тип агента (`general-purpose`, `Explore`), у Codex — его имя (`Newton`).
    #[serde(default)]
    pub kind: String,
    /// Что ему поручено: описание у Claude Code, имя задачи у Codex.
    #[serde(default)]
    pub what: String,
    /// Фоновый ли агент: ход родителя его не ждёт.
    #[serde(default)]
    pub background: bool,
    /// Работает ли агент сейчас.
    #[serde(default)]
    pub running: bool,
    /// Когда агент запущен, unix-секунды.
    #[serde(default)]
    pub started: i64,
    /// Когда агент последний раз что-то записал, unix-секунды.
    #[serde(default)]
    pub updated: i64,
}

/// Подзадача сессии (у Claude Code — события TaskCreated и TaskCompleted).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtask {
    /// Текст подзадачи.
    #[serde(default, deserialize_with = "nullable")]
    pub s: String,
    /// Выполнена ли.
    #[serde(default, deserialize_with = "nullable")]
    pub d: bool,
}

/// Запись о сессии агента. Неизвестные поля сохраняются как есть.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Session {
    /// Какой агент ведёт сессию: `claude`, `codex`, `gemini`.
    #[serde(default, deserialize_with = "nullable")]
    pub agent: String,
    /// Идентификатор сессии, как его сообщает агент.
    pub session_id: String,
    /// Рабочий каталог сессии на момент последнего события: агент ходит по каталогам.
    #[serde(default, deserialize_with = "nullable")]
    pub cwd: String,
    /// Каталог, в котором агент был запущен. По нему определяется задача: он не меняется,
    /// пока идёт сессия.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Каталог проекта, над которым агент работает сейчас, если он глубже каталога запуска:
    /// агента запустили в общем каталоге, а пишет он файлы в одном из подкаталогов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Имя задачи: каталог проекта, над которым работает агент, или `DISP_TASK`.
    #[serde(default, deserialize_with = "nullable")]
    pub task: String,
    /// Ветка git на момент последнего промпта.
    #[serde(default, deserialize_with = "nullable")]
    pub branch: String,
    /// Где идёт сессия: `zellij:сессия#панель`, `vscode`, имя терминала.
    #[serde(default, rename = "where", deserialize_with = "nullable")]
    pub place: String,
    /// Текущий статус.
    #[serde(default, deserialize_with = "nullable")]
    pub status: Status,
    /// Уточнение к статусу: чего ждёт агент, какая ошибка.
    #[serde(default, deserialize_with = "nullable")]
    pub detail: String,
    /// Краткое название для колонки «ЧТО».
    #[serde(default, deserialize_with = "nullable")]
    pub title: String,
    /// Откуда название: `agent` — от самого агента, `name` — задано через `disp name`,
    /// нет поля — из первого промпта.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tsrc: Option<String>,
    /// Последний промпт, обрезанный до 300 символов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    /// Последний ответ агента, обрезанный до 300 символов.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reply: Option<String>,
    /// Когда сессия впервые попала в диспетчер.
    #[serde(default, deserialize_with = "nullable")]
    pub started: i64,
    /// Время последнего события.
    #[serde(default, deserialize_with = "nullable")]
    pub updated: i64,
    /// Когда начался текущий ход.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started: Option<i64>,
    /// Когда закончился последний ход.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_at: Option<i64>,
    /// Закрыта ли сессия.
    #[serde(default, deserialize_with = "nullable")]
    pub ended: bool,
    /// Причина закрытия, как её сообщил агент.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// Принят ли результат человеком.
    #[serde(default, deserialize_with = "nullable")]
    pub accepted: bool,
    /// Когда результат принят.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<i64>,
    /// Подзадачи по идентификатору, в порядке появления.
    #[serde(default, deserialize_with = "nullable")]
    pub subtasks: IndexMap<String, Subtask>,
    /// Сколько субагентов сейчас работает.
    #[serde(default, deserialize_with = "nullable")]
    pub subagents: i64,
    /// Сколько фоновых задач осталось после остановки хода.
    #[serde(default, deserialize_with = "nullable")]
    pub bg: i64,
    /// Транскрипт сессии, как его назвал агент (`transcript_path`). По концу транскрипта
    /// Claude Code видно, что разговор продолжен в другой сессии (`crate::handover`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    /// Процесс агента, если хук смог его узнать: по нему видно, жив ли агент, когда
    /// сессия давно молчит.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<AgentProcess>,
    /// Агенты, которых сессия запустила в текущем ходе, и те, что ещё работают.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<SubAgent>,
    /// Контейнер, в котором идёт сессия (начало его идентификатора), если её ведёт
    /// наблюдатель (`crate::observe`), а не хуки.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Поля, которых эта версия не знает.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Session {
    /// Короткий идентификатор для таблицы и команд.
    pub fn short_id(&self) -> String {
        short_id(&self.session_id)
    }

    /// Подпись задачи: имя, а для неосновной ветки — `имя@ветка`.
    pub fn label(&self) -> String {
        task_label(&self.task, &self.branch)
    }

    /// Последнее, что известно о работе сессии: её событие или запись её работающего
    /// агента. Дочерние агенты Codex хуков в сессию не присылают, а работа идёт.
    pub fn last_activity(&self) -> i64 {
        self.agents
            .iter()
            .filter(|agent| agent.running)
            .map(|agent| agent.updated)
            .fold(self.updated, i64::max)
    }

    /// Вид на доске в момент `now`; `stale_min` — через сколько минут молчания
    /// работающая сессия считается молчащей. Что с ней на самом деле, уточняет
    /// проверка процессов (`crate::procs`).
    pub fn view(&self, now: i64, stale_min: i64) -> View {
        match self.status {
            Status::Waiting => View::Wait,
            Status::Error => View::Err,
            Status::Done => View::Done,
            Status::Working if self.ended => View::Lost,
            Status::Working if now - self.last_activity() > stale_min * 60 => View::Quiet,
            Status::Working => View::Work,
            Status::Idle => View::Idle,
        }
    }
}

/// Короткий идентификатор: последние шесть букв и цифр идентификатора сессии.
/// Хвост, а не начало: у UUIDv7 общий префикс времени.
pub fn short_id(session_id: &str) -> String {
    let clean: Vec<char> = session_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    clean[clean.len().saturating_sub(6)..].iter().collect()
}

/// Подпись задачи: имя, а для неосновной ветки — `имя@ветка`.
pub fn task_label(task: &str, branch: &str) -> String {
    if !branch.is_empty() && branch != task && branch != "main" && branch != "master" {
        format!("{task}@{branch}")
    } else {
        task.to_string()
    }
}

/// Читает поле, считая `null` значением по умолчанию: файл мог записать другой инструмент.
fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_id_takes_the_tail() {
        assert_eq!(short_id("aaaaaaaa-1111-4111-8111-00000000a1b2"), "00a1b2");
        assert_eq!(short_id("019a0000-0000-7000-8000-0000000C0DE1"), "0c0de1");
        assert_eq!(short_id("gemini-session-d4"), "siond4");
        assert_eq!(short_id("x"), "x");
    }

    #[test]
    fn label_hides_default_branches() {
        assert_eq!(task_label("site-31", "main"), "site-31");
        assert_eq!(task_label("nbx-142", "nbx-142"), "nbx-142");
        assert_eq!(task_label("trading-system", "dev"), "trading-system@dev");
    }

    #[test]
    fn reads_a_file_written_by_the_bash_version() {
        let line = r#"{"agent":"codex","session_id":"s1","where":"vscode","status":"waiting",
            "detail":null,"subtasks":{"1":{"s":"Шаг 1","d":true}},"future_field":7}"#;
        let session: Session = serde_json::from_str(line).unwrap_or_default();
        assert_eq!(session.place, "vscode");
        assert_eq!(session.status, Status::Waiting);
        assert_eq!(session.detail, "");
        assert_eq!(session.subtasks.get("1").map(|task| task.d), Some(true));
        assert_eq!(session.extra.get("future_field"), Some(&Value::from(7)));
    }

    #[test]
    fn status_labels_fit_their_column_in_every_language() {
        use crate::i18n::{Lang, scoped};
        let views = [
            View::Wait,
            View::Err,
            View::Lost,
            View::Stuck,
            View::Work,
            View::Quiet,
            View::Idle,
            View::Done,
        ];
        for lang in Lang::ALL {
            let _lang = scoped(lang);
            for view in views {
                let label = view.label();
                assert!(crate::text::width(label) <= 10, "{lang:?}: {label}");
            }
        }
    }

    #[test]
    fn view_follows_status_and_silence() {
        let mut session = Session {
            status: Status::Working,
            updated: 1000,
            ..Session::default()
        };
        assert_eq!(session.view(1000 + 29 * 60, 30), View::Work);
        assert_eq!(session.view(1000 + 31 * 60, 30), View::Quiet);
        session.ended = true;
        assert_eq!(session.view(1001, 30), View::Lost);
    }
}
