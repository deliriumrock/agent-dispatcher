//! Язык интерфейса: английский, русский, украинский.
//!
//! Язык — `DISP_LANG` (`en`, `ru`, `uk`) в окружении или в файле настроек, иначе язык
//! системы (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`), иначе английский. Строки
//! переводятся на месте макросами `tr!` и `t!`: все три языка стоят рядом, и забыть
//! один из них не даст компилятор.

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::{Paths, Settings, env_nonempty};

/// Язык интерфейса.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    /// Английский: для всех, чей язык не русский и не украинский.
    #[default]
    En,
    /// Русский.
    Ru,
    /// Украинский.
    Uk,
}

impl Lang {
    /// Все языки по порядку.
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Ru, Lang::Uk];

    /// Язык по коду: `en`, `ru`, `uk` (и `ua`), с регионом и кодировкой или без
    /// (`ru_RU.UTF-8`, `uk-UA`). `None` — язык не из трёх.
    pub fn parse(code: &str) -> Option<Lang> {
        let base = code
            .trim()
            .split(['_', '-', '.', '@'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match base.as_str() {
            "en" => Some(Lang::En),
            "ru" => Some(Lang::Ru),
            "uk" | "ua" => Some(Lang::Uk),
            _ => None,
        }
    }

    /// Код языка: `en`, `ru`, `uk`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
            Lang::Uk => "uk",
        }
    }

    fn index(self) -> u8 {
        match self {
            Lang::En => 1,
            Lang::Ru => 2,
            Lang::Uk => 3,
        }
    }

    fn from_index(index: u8) -> Option<Lang> {
        Lang::ALL.into_iter().find(|lang| lang.index() == index)
    }
}

/// Язык программы; 0 — ещё не определён.
static GLOBAL: AtomicU8 = AtomicU8::new(0);

thread_local! {
    /// Язык текущего потока, если он задан `scoped`.
    static LOCAL: Cell<Option<Lang>> = const { Cell::new(None) };
}

/// Язык интерфейса. Определяется один раз на процесс (см. `detect`).
pub fn lang() -> Lang {
    if let Some(lang) = LOCAL.with(Cell::get) {
        return lang;
    }
    if let Some(lang) = Lang::from_index(GLOBAL.load(Ordering::Relaxed)) {
        return lang;
    }
    let lang = detect();
    GLOBAL.store(lang.index(), Ordering::Relaxed);
    lang
}

/// Задаёт язык всей программы вместо найденного по настройкам.
pub fn set(lang: Lang) {
    GLOBAL.store(lang.index(), Ordering::Relaxed);
}

/// Задаёт язык только текущему потоку, пока жив возвращённый страж: так тесты
/// проверяют разные языки, не мешая друг другу.
pub fn scoped(lang: Lang) -> Scoped {
    Scoped(LOCAL.with(|local| local.replace(Some(lang))))
}

/// Страж `scoped`: возвращает потоку прежний язык.
#[must_use = "язык действует, пока жив страж"]
pub struct Scoped(Option<Lang>);

impl Drop for Scoped {
    fn drop(&mut self) {
        LOCAL.with(|local| local.set(self.0));
    }
}

/// Язык по настройкам: `DISP_LANG`, затем язык системы, затем английский. В модульных
/// тестах этого крейта — русский: на нём написаны их ожидания.
fn detect() -> Lang {
    if cfg!(test) {
        return Lang::Ru;
    }
    let settings = Settings::load(&Paths::from_env().conf_file);
    let system = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(env_nonempty)
        // `LANGUAGE` — список через двоеточие, в порядке предпочтения.
        .flat_map(|value| {
            value
                .split(':')
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    settings
        .get("DISP_LANG")
        .map(str::to_string)
        .into_iter()
        .chain(system)
        .find_map(|code| Lang::parse(&code))
        .unwrap_or_default()
}

/// Строка на языке интерфейса: `tr!("English", "Русский", "Українська")`. Аргументы
/// форматирования — как у `format!`, общие для трёх строк:
/// `tr!("{n} waiting", "ждут: {n}", "чекають: {n}")`.
#[macro_export]
macro_rules! tr {
    ($en:literal, $ru:literal, $uk:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => ::std::format!($en),
            $crate::i18n::Lang::Ru => ::std::format!($ru),
            $crate::i18n::Lang::Uk => ::std::format!($uk),
        }
    };
    ($en:literal, $ru:literal, $uk:literal, $($arg:tt)+) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => ::std::format!($en, $($arg)+),
            $crate::i18n::Lang::Ru => ::std::format!($ru, $($arg)+),
            $crate::i18n::Lang::Uk => ::std::format!($uk, $($arg)+),
        }
    };
}

/// Неизменная строка на языке интерфейса, `&'static str`: `t!("Done", "Готово", "Готово")`.
#[macro_export]
macro_rules! t {
    ($en:literal, $ru:literal, $uk:literal $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => $en,
            $crate::i18n::Lang::Ru => $ru,
            $crate::i18n::Lang::Uk => $uk,
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_with_region_and_encoding_are_understood() {
        assert_eq!(Lang::parse("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::parse("uk_UA.UTF-8"), Some(Lang::Uk));
        assert_eq!(Lang::parse("ua"), Some(Lang::Uk));
        assert_eq!(Lang::parse("en-GB"), Some(Lang::En));
        assert_eq!(Lang::parse("EN"), Some(Lang::En));
        assert_eq!(Lang::parse("de_DE.UTF-8"), None);
        assert_eq!(Lang::parse("C.UTF-8"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn strings_follow_the_thread_language() {
        let pick = || t!("Done", "Готово", "Готово");
        let count = |n: usize| tr!("{n} waiting", "ждут: {n}", "чекають: {n}");
        {
            let _en = scoped(Lang::En);
            assert_eq!((pick(), count(2)), ("Done", "2 waiting".to_string()));
            {
                let _uk = scoped(Lang::Uk);
                assert_eq!(count(3), "чекають: 3");
            }
            assert_eq!(count(1), "1 waiting");
        }
        // Вне стража — язык программы; в тестах ядра это русский.
        assert_eq!(count(4), "ждут: 4");
    }
}
