//! Меню значка из модели `Tray`.

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

use crate::model::{Action, Tray};
use disp::{t, tr};

/// Строит меню: сводка, группы сессий с подменю, внизу — панель и выход.
pub fn build(tray: &Tray) -> Result<Menu, String> {
    build_menu(tray).map_err(|err| {
        tr!("menu not built: {err}", "меню не построено: {err}", "меню не побудовано: {err}")
    })
}

fn build_menu(tray: &Tray) -> tray_icon::menu::Result<Menu> {
    let menu = Menu::new();
    menu.append(&note(&tray.summary))?;
    for section in &tray.sections {
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&note(&section.title))?;
        for entry in &section.entries {
            let sub = Submenu::new(literal(&entry.text), true);
            for line in &entry.info {
                sub.append(&note(line))?;
            }
            if !entry.actions.is_empty() {
                sub.append(&PredefinedMenuItem::separator())?;
            }
            for (label, action) in &entry.actions {
                sub.append(&button(label, action))?;
            }
            menu.append(&sub)?;
        }
        if section.more > 0 {
            let more = section.more;
            menu.append(&note(&tr!(
                "…and {more} more — in the panel",
                "…и ещё {more} — в панели",
                "…і ще {more} — у панелі"
            )))?;
        }
    }
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&button(
        t!("Open the panel", "Открыть панель", "Відкрити панель"),
        &Action::Open,
    ))?;
    menu.append(&button(
        t!("Remove the icon", "Убрать значок", "Прибрати значок"),
        &Action::Quit,
    ))?;
    Ok(menu)
}

/// Неактивная строка со сведениями.
fn note(text: &str) -> MenuItem {
    MenuItem::new(literal(text), false, None)
}

/// Кнопка с действием.
fn button(label: &str, action: &Action) -> MenuItem {
    MenuItem::with_id(action.id(), literal(label), true, None)
}

/// Текст пункта как есть: `&` в меню отмечает клавишу быстрого выбора, а `&&` —
/// это сам знак.
fn literal(text: &str) -> String {
    text.replace('&', "&&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ampersands_are_kept() {
        assert_eq!(literal("R&D && тесты"), "R&&D &&&& тесты");
    }
}
