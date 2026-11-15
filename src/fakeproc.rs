//! Поддельный `/proc` для тестов: процессы с родителем, именем, временем запуска,
//! командной строкой, каталогом и окружением.

use std::fs;
use std::path::{Path, PathBuf};

/// Каталог во временном каталоге системы, устроенный как `/proc`. Удаляется в `Drop`.
pub struct FakeProc {
    /// Корень: то, что в настоящей системе `/proc`.
    pub root: PathBuf,
}

impl FakeProc {
    /// Пустой `/proc`; время загрузки системы в `/proc/stat` — 1 000 000 секунд.
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("disp-proc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(fs::create_dir_all(&root).is_ok());
        assert!(fs::write(root.join("stat"), "cpu 1 2 3\nbtime 1000000\n").is_ok());
        FakeProc { root }
    }

    /// Процесс, запущенный в момент загрузки системы.
    pub fn add(&self, pid: u32, parent: u32, args: &[&str], cwd: &str, env: &[&str]) {
        self.add_at(pid, parent, args, cwd, env, 0);
    }

    /// Процесс, запущенный через `start` тиков после загрузки. Имя процесса — имя файла
    /// программы, обрезанное до 15 байт, как у ядра.
    pub fn add_at(
        &self,
        pid: u32,
        parent: u32,
        args: &[&str],
        cwd: &str,
        env: &[&str],
        start: u64,
    ) {
        let dir = self.root.join(pid.to_string());
        assert!(fs::create_dir_all(&dir).is_ok());
        let program = args.first().copied().unwrap_or_default();
        let comm: String = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .chars()
            .take(15)
            .collect();
        // Поля после имени: состояние, родитель, ещё 17 полей и время запуска (поле 22).
        let middle = ["0"; 17].join(" ");
        let stat = format!("{pid} ({comm}) S {parent} {middle} {start} 0 0\n");
        assert!(fs::write(dir.join("stat"), stat).is_ok());
        assert!(fs::write(dir.join("cmdline"), args.join("\0") + "\0").is_ok());
        assert!(fs::write(dir.join("environ"), env.join("\0") + "\0").is_ok());
        assert!(std::os::unix::fs::symlink(cwd, dir.join("cwd")).is_ok());
    }
}

impl Drop for FakeProc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
