//! `disp-tray --detach` с настоящим значком: отвечает успехом, только когда значок
//! показался. Шина D-Bus — несуществующая или своя, отдельная: значок не появляется в
//! панели того, кто запускает тесты.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Код выхода `--detach`, когда значок уже в трее (`disp::cli::TRAY_RUNNING_EXIT`).
const RUNNING_EXIT: i32 = 3;

/// Свой каталог состояния; при выходе останавливает запущенные из него значки.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("disp-tray-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("каталог песочницы");
        fs::write(dir.join("env"), "").expect("файл настроек");
        Sandbox { dir }
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    /// `disp-tray --detach` на шине `bus`.
    fn detach(&self, bus: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_disp-tray"))
            .arg("--detach")
            .env("DISP_STATE_DIR", self.state())
            .env("DISP_CONFIG", self.dir.join("env"))
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .stdin(Stdio::null())
            .output()
            .expect("disp-tray --detach")
    }

    /// Значки, запущенные с этим каталогом состояния: их узнаём по окружению.
    fn icons(&self) -> Vec<u32> {
        let marker = format!("DISP_STATE_DIR={}", self.state().display());
        let Ok(procs) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        procs
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                    environ
                        .split(|&byte| byte == 0)
                        .any(|var| var == marker.as_bytes())
                })
            })
            .collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for pid in self.icons() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Своя шина D-Bus на время теста.
struct Bus {
    daemon: Child,
    address: String,
}

impl Bus {
    /// Запускает `dbus-daemon`; `None`, если его нет в системе.
    fn start() -> Option<Self> {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut address = String::new();
        let read = daemon
            .stdout
            .take()
            .map(|out| BufReader::new(out).read_line(&mut address));
        let address = address.trim().to_string();
        if !matches!(read, Some(Ok(_))) || address.is_empty() {
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        }
        Some(Bus { daemon, address })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn icon_that_cannot_reach_dbus_is_a_failure() {
    let sandbox = Sandbox::new("no-bus");
    let bus = format!("unix:path={}", sandbox.dir.join("no-bus").display());
    let output = sandbox.detach(&bus);
    let (out, err) = (text(&output.stdout), text(&output.stderr));
    assert_eq!(output.status.code(), Some(1), "{out}{err}");
    assert!(err.contains("значок не создан") && err.contains("D-Bus"), "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(sandbox.icons().is_empty(), "значок не должен остаться");
    // Тот же текст значок оставил в своём журнале.
    let log = fs::read_to_string(sandbox.state().join("tray.log")).unwrap_or_default();
    assert!(log.contains("значок не создан"), "{log}");
}

#[test]
fn icon_on_a_bus_is_shown_and_keeps_running() {
    let Some(bus) = Bus::start() else {
        eprintln!("пропуск: нет dbus-daemon");
        return;
    };
    let sandbox = Sandbox::new("bus");
    let output = sandbox.detach(&bus.address);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(sandbox.icons().len(), 1, "значок должен работать после --detach");
    // Второй запуск видит работающий значок и нового не создаёт.
    let again = sandbox.detach(&bus.address);
    assert_eq!(again.status.code(), Some(RUNNING_EXIT), "{}", text(&again.stderr));
    assert_eq!(sandbox.icons().len(), 1);
    assert!(Path::new(&sandbox.state().join("tray.lock")).exists());
}
