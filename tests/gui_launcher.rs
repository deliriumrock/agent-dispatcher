//! Команда `disp gui`: что и с какими параметрами она запускает.
//!
//! Настоящие disp-gui и term-everything заменены скриптами, которые записывают
//! свои аргументы и нужную переменную окружения в файл.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("disp-gui-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("каталог песочницы");
        fs::write(dir.join("env"), "").expect("файл настроек");
        let sandbox = Sandbox { dir };
        sandbox.script("disp-gui");
        sandbox.script("term-everything");
        sandbox
    }

    /// Скрипт, который записывает своё имя, аргументы и подсказку о порядке цветов.
    fn script(&self, name: &str) {
        let path = self.dir.join("bin").join(name);
        let body = format!(
            "#!/bin/sh\nprintf '%s|%s|%s\\n' '{name}' \"$*\" \"${{TERM_EVERYTHING_PIXEL_TYPE:-}}\" >> '{log}'\n",
            log = self.dir.join("calls.log").display()
        );
        fs::write(&path, body).expect("скрипт");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("права");
    }

    fn disp(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_disp"));
        command
            .args(args)
            .env("DISP_STATE_DIR", self.dir.join("state"))
            .env("DISP_CONFIG", self.dir.join("env"))
            .env("DISP_GUI_BIN", self.dir.join("bin/disp-gui"))
            .env("DISP_TERM_EVERYTHING", self.dir.join("bin/term-everything"))
            .env_remove("DISP_GUI_SIZE")
            .env_remove("TERM_EVERYTHING_PIXEL_TYPE");
        command
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn pane_gets_the_gui_through_term_everything() {
    let sandbox = Sandbox::new("pane");
    let status = sandbox
        .disp(&["gui"])
        .env("DISP_GUI_SIZE", "800x400")
        .status()
        .expect("disp gui");
    assert!(status.success());
    let gui = sandbox.dir.join("bin/disp-gui");
    assert_eq!(
        sandbox.calls(),
        [format!(
            "term-everything|--virtual-monitor-size 800x400 -- {} --size 800x400|BGRA8",
            gui.display()
        )]
    );
}

#[test]
fn window_flag_and_missing_terminal_open_a_desktop_window() {
    let sandbox = Sandbox::new("window");
    // Вывод тестов идёт не в терминал: размера панели нет, окно открывается напрямую.
    assert!(sandbox.disp(&["gui"]).status().expect("disp gui").success());
    let with_flag = sandbox
        .disp(&["gui", "--window"])
        .env("DISP_GUI_SIZE", "800x400")
        .status()
        .expect("disp gui --window");
    assert!(with_flag.success());
    assert_eq!(sandbox.calls(), ["disp-gui||", "disp-gui||"]);
}

#[test]
fn missing_gui_program_is_reported() {
    let sandbox = Sandbox::new("missing");
    let output = sandbox
        .disp(&["gui"])
        .env("DISP_GUI_BIN", sandbox.dir.join("bin/nothing-here"))
        .output()
        .expect("disp gui");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("не удалось запустить"));
}
