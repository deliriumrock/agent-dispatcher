#!/usr/bin/env python3
"""Поддельный терминал для zellij: клиент в псевдотерминале, который отвечает на запросы
как терминал с Kitty graphics protocol и пишет в файл всё, что zellij ему присылает.

Так видно без настоящего экрана, какие картинки и куда zellij передаёт наружу.
Использование: faketerm.py <сессия> <колонок> <строк> <секунд> <файл-журнал>
Клиент отключается раньше, если рядом с журналом появляется файл <журнал>.stop.
"""
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

CELL_W, CELL_H = 10, 20


def main() -> int:
    session, cols, rows = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    seconds, log_path = float(sys.argv[4]), sys.argv[5]
    stop_path = log_path + ".stop"
    pid, fd = pty.fork()
    if pid == 0:
        for key in ("ZELLIJ", "ZELLIJ_SESSION_NAME", "ZELLIJ_PANE_ID"):
            os.environ.pop(key, None)
        os.environ["TERM"] = "xterm-256color"
        os.execvp("zellij", ["zellij", "attach", session])
    size = struct.pack("HHHH", rows, cols, cols * CELL_W, rows * CELL_H)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, size)

    replies = [
        (re.compile(rb"\x1b\[0?c"), b"\x1b[?62;4c"),
        (re.compile(rb"\x1b\[14t"), b"\x1b[4;%d;%dt" % (rows * CELL_H, cols * CELL_W)),
        (re.compile(rb"\x1b\[16t"), b"\x1b[6;%d;%dt" % (CELL_H, CELL_W)),
        (re.compile(rb"\x1b\[18t"), b"\x1b[8;%d;%dt" % (rows, cols)),
        (re.compile(rb"\x1b\]10;\?(\x07|\x1b\\)"), b"\x1b]10;rgb:dddd/dddd/dddd\x1b\\"),
        (re.compile(rb"\x1b\]11;\?(\x07|\x1b\\)"), b"\x1b]11;rgb:1111/1111/1111\x1b\\"),
        (re.compile(rb"\x1b\[\?2026\$p"), b"\x1b[?2026;2$y"),
    ]
    graphics = re.compile(rb"\x1b_G([^;\x1b]*)(;[^\x1b]*)?\x1b\\")
    deadline = time.monotonic() + seconds
    pending = b""
    with open(log_path, "wb") as log:
        while time.monotonic() < deadline and not os.path.exists(stop_path):
            ready, _, _ = select.select([fd], [], [], 0.2)
            if not ready:
                continue
            try:
                data = os.read(fd, 1 << 16)
            except OSError:
                break
            if not data:
                break
            log.write(data)
            log.flush()
            pending = (pending + data)[-(1 << 20):]
            consumed = 0
            for pattern, reply in replies:
                for match in pattern.finditer(pending):
                    os.write(fd, reply)
                    consumed = max(consumed, match.end())
            for match in graphics.finditer(pending):
                consumed = max(consumed, match.end())
                keys = dict(
                    item.split(b"=", 1) for item in match.group(1).split(b",") if b"=" in item
                )
                quiet = keys.get(b"q", b"0")
                # Ответ положен на запрос и на последний кусок передачи, если его не заглушили.
                final = keys.get(b"m", b"0") == b"0"
                if quiet == b"0" and final and (b"i" in keys or keys.get(b"a") == b"q"):
                    os.write(fd, b"\x1b_Gi=%s;OK\x1b\\" % keys.get(b"i", b"0"))
            pending = pending[consumed:]
    os.kill(pid, 15)
    os.waitpid(pid, 0)
    return 0


if __name__ == "__main__":
    sys.exit(main())
