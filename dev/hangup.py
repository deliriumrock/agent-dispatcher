#!/usr/bin/env python3
"""Проверка: `disp watch` выходит, когда пропадает его терминал, и не крутится вхолостую.

Два случая. Терминал управляющий: ядро шлёт панели SIGHUP. Терминал не управляющий
(панель запущена в своём сеансе): сигнала нет, выход — по сторожу терминала.
До 0.8.005 в обоих случаях панель застревала в чтении клавиш и занимала ядро целиком.

Использование: dev/hangup.py <disp> <каталог-состояния> <файл-настроек>
Код выхода 0, если панель вышла в обоих случаях.
"""
import os
import pty
import subprocess
import sys
import time

LIMIT_SEC = 6.0


def cpu_sec(pid: int) -> float:
    """Процессорное время процесса в секундах."""
    with open(f"/proc/{pid}/stat") as stat:
        fields = stat.read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def drain(fd: int, seconds: float) -> None:
    """Читает вывод панели, пока она рисуется."""
    os.set_blocking(fd, False)
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            os.read(fd, 1 << 16)
        except BlockingIOError:
            time.sleep(0.05)
        except OSError:
            return


def wait_exit(pid: int, poll) -> str:
    """Ждёт выхода панели и описывает итог."""
    before = cpu_sec(pid)
    start = time.monotonic()
    while time.monotonic() - start < LIMIT_SEC:
        code = poll()
        if code is not None:
            return f"ok: вышла через {time.monotonic() - start:.1f} с, код {code}"
        time.sleep(0.1)
    used = cpu_sec(pid) - before
    os.kill(pid, 9)
    return f"FAIL: не вышла за {LIMIT_SEC:.0f} с, процессор {used / LIMIT_SEC * 100:.0f}% ядра"


def main() -> int:
    disp, state, conf = sys.argv[1:4]
    env = dict(os.environ, DISP_STATE_DIR=state, DISP_CONFIG=conf, TERM="xterm-256color")
    for key in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
        env.pop(key, None)

    # Управляющий терминал: при его закрытии лидер сеанса получает SIGHUP.
    pid, master = pty.fork()
    if pid == 0:
        os.execve(disp, [disp, "watch"], env)
    drain(master, 2)
    os.close(master)

    def reap():
        done, status = os.waitpid(pid, os.WNOHANG)
        return os.waitstatus_to_exitcode(status) if done else None

    first = wait_exit(pid, reap)
    print(f"с SIGHUP:  {first}")

    # Терминал не управляющий: сигнала не будет.
    master, slave = os.openpty()
    proc = subprocess.Popen(
        [disp, "watch"], stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True
    )
    drain(master, 2)
    os.close(master)
    os.close(slave)
    second = wait_exit(proc.pid, proc.poll)
    print(f"без SIGHUP: {second}")
    return 0 if first.startswith("ok") and second.startswith("ok") else 1


if __name__ == "__main__":
    sys.exit(main())
