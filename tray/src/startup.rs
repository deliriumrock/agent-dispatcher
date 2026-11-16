//! Как `disp-tray --detach` узнаёт, показался ли значок.
//!
//! `--detach` запускает значок отдельным процессом с ключом `CHILD` и каналом вместо
//! stdout. Значок пишет в канал одну строку — `READY`, `RUNNING` или текст ошибки — и
//! отпускает его: на месте stdout остаётся `/dev/null`. `--detach` читает канал до конца,
//! но не дольше `TIMEOUT`, и только тогда отвечает `disp`.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::Child;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Ключ, с которым `--detach` запускает сам значок. В справке его нет: человеку он не нужен.
pub const CHILD: &str = "--detached-child";

/// Сколько `--detach` ждёт ответа значка. Подключение к D-Bus или к строке меню занимает
/// доли секунды; значок, который молчит дольше, считается неудавшимся.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// Строка в канале: значок показан.
const READY: &str = "ready";
/// Строка в канале: значок уже был в трее.
const RUNNING: &str = "running";

/// Итог запуска значка.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Значок показан.
    Shown,
    /// Значок уже был в трее.
    Running,
    /// Значок не показан; текст для человека.
    Failed(String),
}

/// Сообщает итог запуска в канал `--detach` и отпускает канал.
///
/// Ошибки записи не важны: закрытый канал значит, что `--detach` уже не ждёт. Канал
/// отпускается, чтобы `--detach` увидел его конец и вышел, пока значок работает дальше,
/// и чтобы поздняя запись в stdout не упала на закрытом канале.
pub fn report(outcome: &Outcome) {
    let line = match outcome {
        Outcome::Shown => READY,
        Outcome::Running => RUNNING,
        Outcome::Failed(text) => text,
    };
    {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
    if let Ok(null) = File::options().write(true).open("/dev/null") {
        let _ = rustix::stdio::dup2_stdout(&null);
    }
}

/// Ждёт итог от значка `child`, запущенного с каналом вместо stdout: читает канал до
/// конца, но не дольше `timeout`. `log` — куда значок пишет свои ошибки; на него
/// ссылается ответ, если значок вышел молча.
///
/// Значок, который не ответил вовремя, останавливается: иначе он мог бы появиться
/// позже, когда `disp` уже сообщил о неудаче.
pub fn wait(child: &mut Child, timeout: Duration, log: &Path) -> Outcome {
    let Some(mut pipe) = child.stdout.take() else {
        return Outcome::Failed("значок запущен без канала для ответа".to_string());
    };
    let (sender, answer) = mpsc::channel();
    // Поток не держит выход: если ответа нет, `--detach` выйдет и без него.
    thread::spawn(move || {
        let mut text = String::new();
        let _ = sender.send(pipe.read_to_string(&mut text).map(|_| text));
    });
    match answer.recv_timeout(timeout) {
        Ok(Ok(text)) => match text.trim() {
            READY => Outcome::Shown,
            RUNNING => Outcome::Running,
            said => {
                // Значок не показался и выходит; забрать его — узнать, с чем он вышел.
                let status = child
                    .wait()
                    .map_or_else(|err| err.to_string(), |status| status.to_string());
                Outcome::Failed(if said.is_empty() {
                    format!("значок вышел ({status}); подробности — в {}", log.display())
                } else {
                    said.to_string()
                })
            }
        },
        Ok(Err(err)) => Outcome::Failed(format!("не прочитан ответ значка: {err}")),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Outcome::Failed(format!(
                "значок не ответил за {} с и остановлен; подробности — в {}",
                timeout.as_secs(),
                log.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    /// Запускает `sh -c script` с каналом вместо stdout, как `--detach` запускает значок.
    fn child(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh")
    }

    fn log() -> &'static Path {
        Path::new("/state/tray.log")
    }

    #[test]
    fn shown_icon_answers_while_it_keeps_running() {
        // Ответил, отпустил канал и работает дальше: ответ приходит сразу, без таймаута.
        let mut icon = child("echo ready; exec >/dev/null; exec sleep 30");
        let begun = Instant::now();
        assert_eq!(wait(&mut icon, TIMEOUT, log()), Outcome::Shown);
        assert!(begun.elapsed() < Duration::from_secs(5));
        assert!(matches!(icon.try_wait(), Ok(None)), "значок должен работать дальше");
        let _ = icon.kill();
        let _ = icon.wait();
    }

    #[test]
    fn running_and_failure_answers_are_told_apart() {
        assert_eq!(wait(&mut child("echo running"), TIMEOUT, log()), Outcome::Running);
        assert_eq!(
            wait(&mut child("echo 'значок не создан: нет D-Bus'; exit 1"), TIMEOUT, log()),
            Outcome::Failed("значок не создан: нет D-Bus".to_string())
        );
    }

    #[test]
    fn silent_exit_points_to_the_log() {
        let Outcome::Failed(text) = wait(&mut child("exit 7"), TIMEOUT, log()) else {
            panic!("молчаливый выход — это неудача");
        };
        assert!(text.contains('7') && text.contains("/state/tray.log"), "{text}");
    }

    #[test]
    fn silent_icon_is_stopped_after_the_timeout() {
        let mut icon = child("exec sleep 30");
        let outcome = wait(&mut icon, Duration::from_millis(200), log());
        assert!(matches!(&outcome, Outcome::Failed(text) if text.contains("не ответил")));
        assert!(matches!(icon.try_wait(), Ok(Some(_))), "значок должен быть остановлен");
    }
}
