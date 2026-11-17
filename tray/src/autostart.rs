//! Запуск значка при входе в систему: файл `.desktop` в `~/.config/autostart` на Linux,
//! агент launchd в `~/Library/LaunchAgents` на macOS.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use disp::tr;

/// Имя агента launchd и файла `.desktop`.
const NAME: &str = "disp-tray";

/// Где лежит файл автозапуска на этой системе.
fn location() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other(tr!("HOME is not set", "не задан HOME", "не задано HOME")))?;
    Ok(if cfg!(target_os = "macos") {
        home.join(format!("Library/LaunchAgents/local.{NAME}.plist"))
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join(format!("autostart/{NAME}.desktop"))
    })
}

/// `disp-tray --autostart`: запускать значок при входе в систему.
pub fn enable() -> u8 {
    let result = (|| -> io::Result<PathBuf> {
        let exe = env::current_exe()?.canonicalize()?;
        let path = location()?;
        let text = if cfg!(target_os = "macos") {
            // У агентов launchd свой короткий PATH, без Homebrew: без нынешнего PATH
            // значок не нашёл бы zellij.
            launch_agent(&exe, &env::var("PATH").unwrap_or_default())
        } else {
            desktop_path_problem(&exe).map_or(Ok(()), |problem| Err(io::Error::other(problem)))?;
            desktop_entry(&exe)
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, text)?;
        Ok(path)
    })();
    match result {
        Ok(path) => {
            let path = path.display();
            println!(
                "{}",
                tr!(
                    "disp-tray: the icon will start at login ({path})",
                    "disp-tray: значок будет запускаться при входе в систему ({path})",
                    "disp-tray: значок запускатиметься під час входу в систему ({path})"
                )
            );
            0
        }
        Err(err) => {
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: autostart not set up: {err}",
                    "disp-tray: автозапуск не настроен: {err}",
                    "disp-tray: автозапуск не налаштовано: {err}"
                )
            );
            1
        }
    }
}

/// `disp-tray --no-autostart`: больше не запускать значок при входе.
pub fn disable() -> u8 {
    let path = match location() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("disp-tray: {err}");
            return 1;
        }
    };
    match fs::remove_file(&path) {
        Ok(()) => {
            let path = path.display();
            println!(
                "{}",
                tr!(
                    "disp-tray: autostart removed ({path})",
                    "disp-tray: автозапуск снят ({path})",
                    "disp-tray: автозапуск знято ({path})"
                )
            );
            0
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            println!(
                "{}",
                tr!(
                    "disp-tray: autostart was not set up anyway",
                    "disp-tray: автозапуск и не был настроен",
                    "disp-tray: автозапуск і не було налаштовано"
                )
            );
            0
        }
        Err(err) => {
            let path = path.display();
            eprintln!(
                "{}",
                tr!(
                    "disp-tray: could not delete {path}: {err}",
                    "disp-tray: не удалось удалить {path}: {err}",
                    "disp-tray: не вдалося видалити {path}: {err}"
                )
            );
            1
        }
    }
}

/// Файл автозапуска рабочего стола Linux.
fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=disp tray\n\
         Comment=Tray icon of the AI agent session dispatcher\n\
         Comment[ru]=Значок диспетчера сессий AI-агентов\n\
         Comment[uk]=Значок диспетчера сесій AI-агентів\n\
         Exec={}\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_quote(&exe.to_string_lossy())
    )
}

/// Почему программу по пути `exe` нельзя запускать из файла `.desktop`.
///
/// Знак `%` начинает полевой код (`%f`, `%u`). По спецификации сам знак пишется `%%`,
/// но GNOME (Gio) такой файл не загружает вовсе: он проверяет, что программа из `Exec=`
/// существует, не раскрывая `%%`. А без экранирования Gio вырезает `%f` из пути
/// и запускает несуществующую программу. Надёжного способа нет — путь отвергается.
fn desktop_path_problem(exe: &Path) -> Option<String> {
    exe.to_string_lossy().contains('%').then(|| {
        let exe = exe.display();
        tr!(
            "the path {exe} has a % sign, and the desktop will not start the program from \
             autostart with it; put it in another directory (make install-tray — into ~/.local/bin)",
            "в пути {exe} есть знак %, а с ним рабочий стол программу из автозапуска не \
             запустит; поставь её в другой каталог (make install-tray — в ~/.local/bin)",
            "у шляху {exe} є знак %, а з ним робочий стіл програму з автозапуску не \
             запустить; постав її в інший каталог (make install-tray — у ~/.local/bin)"
        )
    })
}

/// Путь программы для строки `Exec=`: в кавычках, с экранированием по спецификации
/// Desktop Entry, если в нём есть пробелы или особые знаки.
fn exec_quote(path: &str) -> String {
    let plain = |ch: char| ch.is_alphanumeric() || "/._-+".contains(ch);
    if path.chars().all(plain) {
        return path.to_string();
    }
    let mut out = String::from("\"");
    // Внутри кавычек знак экранируется обратной косой чертой, а сама черта в значении
    // файла `.desktop` удваивается ещё раз. `%` начинает полевой код (`%f`, `%u`) даже
    // в кавычках, поэтому сам знак пишется как `%%`.
    for ch in path.chars() {
        match ch {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(ch);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Агент launchd: запустить значок при входе, с заданным `PATH`.
fn launch_agent(exe: &Path, path_var: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>local.{NAME}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.to_string_lossy()),
        xml_escape(path_var)
    )
}

/// Экранирует текст для XML.
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_runs_the_program() {
        let text = desktop_entry(Path::new("/home/u/.local/bin/disp-tray"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("\nExec=/home/u/.local/bin/disp-tray\n"));
    }

    #[test]
    fn exec_paths_with_spaces_are_quoted() {
        assert_eq!(exec_quote("/a b/disp-tray"), "\"/a b/disp-tray\"");
        assert_eq!(exec_quote("/a$b/x"), "\"/a\\\\$b/x\"");
    }

    #[test]
    fn percent_is_not_a_field_code() {
        assert_eq!(exec_quote("/tmp/build%f/disp-tray"), "\"/tmp/build%%f/disp-tray\"");
        assert_eq!(exec_quote("/a%%b"), "\"/a%%%%b\"");
    }

    #[test]
    fn paths_with_percent_are_refused() {
        let problem = desktop_path_problem(Path::new("/tmp/build%f/disp-tray"));
        assert!(problem.is_some_and(|text| text.contains("build%f")));
        assert!(desktop_path_problem(Path::new("/home/u/.local/bin/disp-tray")).is_none());
    }

    #[test]
    fn launch_agent_keeps_path_and_escapes_xml() {
        let text = launch_agent(Path::new("/Users/u/bin/disp-tray"), "/opt/homebrew/bin:/a&b");
        assert!(text.contains("<string>/Users/u/bin/disp-tray</string>"));
        assert!(text.contains("<string>/opt/homebrew/bin:/a&amp;b</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
    }
}
