//! Строковые помощники: те же правила обрезки, выравнивания и подписи времени,
//! что были в bash-версии. Длина везде считается в символах, а не в байтах.

use std::io::{self, Write};

use chrono::{DateTime, Local};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::env_nonempty;

/// Текст значения JSON: строка как есть, всё остальное — компактным JSON.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Сводит текст к одной строке: переводы строк и табуляции становятся одним пробелом,
/// пробелы по краям убираются.
pub fn oneline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_break = false;
    for ch in text.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
        } else {
            out.push(ch);
            in_break = false;
        }
    }
    out.trim_matches(' ').to_string()
}

/// Обрезает текст до `max` символов; обрезанный заканчивается многоточием.
pub fn clip(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Приводит текст ровно к `width` символам: лишнее отрезает, недостающее добивает пробелами.
pub fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.chars().take(width).collect()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Ширина текста в ячейках терминала: широкие символы занимают две.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Обрезает текст до `max` ячеек терминала; обрезанный заканчивается многоточием.
pub fn clip_cells(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > max - 1 {
            break;
        }
        out.push(ch);
        used += cells;
    }
    out.push('…');
    out
}

/// Приводит текст ровно к `cells` ячейкам терминала: лишнее заменяет многоточием,
/// недостающее добивает пробелами.
pub fn fit(text: &str, cells: usize) -> String {
    let mut out = clip_cells(text, cells);
    let used = width(&out);
    out.push_str(&" ".repeat(cells.saturating_sub(used)));
    out
}

/// Разбивает текст на строки не шире `cells` ячеек. Переносит по пробелам;
/// слово длиннее строки режется.
pub fn wrap(text: &str, cells: usize) -> Vec<String> {
    let cells = cells.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        let word_cells = width(word);
        if used > 0 && used + 1 + word_cells <= cells {
            line.push(' ');
            line.push_str(word);
            used += 1 + word_cells;
            continue;
        }
        if used > 0 {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        for ch in word.chars() {
            let ch_cells = ch.width().unwrap_or(0);
            if used + ch_cells > cells {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(ch);
            used += ch_cells;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Путь с `~` вместо домашнего каталога.
pub fn home_relative(path: &str) -> String {
    match env_nonempty("HOME") {
        Some(home) if path == home => "~".to_string(),
        Some(home) => match path.strip_prefix(&format!("{home}/")) {
            Some(rest) => format!("~/{rest}"),
            None => path.to_string(),
        },
        None => path.to_string(),
    }
}

/// Текст без разметки Markdown: в одной строке таблицы звёздочки и решётки только мешают.
pub fn unmarked(text: &str) -> String {
    let cleaned: String = text.chars().filter(|ch| !matches!(ch, '*' | '`')).collect();
    cleaned.trim_start_matches(['#', ' ']).to_string()
}

/// Местное время в заданном формате; пусто, если метка времени некорректна.
pub fn local_time(ts: i64, format: &str) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|time| time.with_timezone(&Local).format(format).to_string())
        .unwrap_or_default()
}

/// Латинская буква той же клавиши в русской раскладке: панелью можно управлять,
/// не переключая раскладку.
pub fn latin_key(ch: char) -> char {
    match ch {
        'й' | 'Й' => 'q',
        'ф' => 'a',
        'ч' | 'Ч' => 'x',
        'Ф' => 'A',
        'п' | 'П' => 'g',
        'т' | 'Т' => 'n',
        'д' | 'Д' => 'l',
        'о' | 'О' => 'j',
        'л' | 'Л' => 'k',
        'в' | 'В' => 'd',
        'к' | 'К' => 'r',
        'г' | 'Г' => 'u',
        'р' | 'Р' => 'h',
        'е' | 'Е' => 't',
        'ы' => 's',
        'Ы' => 'S',
        other => other,
    }
}

/// Возраст для колонки таблицы: `45с`, `12м`, `3ч`, `2д`.
pub fn age(seconds: i64) -> String {
    let d = seconds.max(0);
    if d < 60 {
        format!("{d}с")
    } else if d < 3600 {
        format!("{}м", d / 60)
    } else if d < 86400 {
        format!("{}ч", d / 3600)
    } else {
        format!("{}д", d / 86400)
    }
}

/// Длительность хода для уведомления: `45с`, `12м`, `1ч 5м`.
pub fn dur(seconds: i64) -> String {
    let d = seconds.max(0);
    if d < 60 {
        format!("{d}с")
    } else if d < 3600 {
        format!("{}м", d / 60)
    } else {
        format!("{}ч {}м", d / 3600, (d % 3600) / 60)
    }
}

/// Печатает текст в stdout. Ошибку записи (например, закрытый канал в `disp ls | head`)
/// молча игнорирует: упасть из-за неё было бы хуже.
pub fn emit(text: &str) {
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// Печатает строку в stderr, игнорируя ошибку записи.
pub fn warn(text: &str) {
    let mut err = io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.write_all(b"\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oneline_collapses_breaks_and_trims() {
        assert_eq!(
            oneline("Собери билд под macOS\nи подпиши его"),
            "Собери билд под macOS и подпиши его"
        );
        assert_eq!(oneline("  a\r\n\tb  "), "a b");
    }

    #[test]
    fn clip_counts_characters_not_bytes() {
        assert_eq!(clip("привет", 6), "привет");
        assert_eq!(clip("привет", 4), "при…");
        assert_eq!(clip("привет", 0), "");
    }

    #[test]
    fn pad_truncates_and_fills() {
        assert_eq!(pad("ЖДЁТ", 6), "ЖДЁТ  ");
        assert_eq!(pad("работает", 4), "рабо");
    }

    #[test]
    fn cells_account_for_wide_characters() {
        assert_eq!(width("задача"), 6);
        assert_eq!(width("日本"), 4);
        assert_eq!(clip_cells("日本語のタイトル", 7), "日本語…");
        assert_eq!(clip_cells("привет", 4), "при…");
        assert_eq!(fit("ok", 4), "ok  ");
        assert_eq!(fit("日本語", 4), "日… ");
        assert_eq!(fit("привет", 0), "");
    }

    #[test]
    fn wrap_breaks_on_spaces_and_cuts_long_words() {
        assert_eq!(
            wrap("Собери билд под macOS и подпиши его", 12),
            ["Собери билд", "под macOS и", "подпиши его"]
        );
        assert_eq!(wrap("абвгдежз", 3), ["абв", "где", "жз"]);
        assert_eq!(wrap("  ", 5), Vec::<String>::new());
    }

    #[test]
    fn age_and_dur_use_the_same_units_as_the_script() {
        assert_eq!(age(59), "59с");
        assert_eq!(age(360), "6м");
        assert_eq!(age(7200), "2ч");
        assert_eq!(age(3 * 86400), "3д");
        assert_eq!(dur(900), "15м");
        assert_eq!(dur(3900), "1ч 5м");
    }
}
