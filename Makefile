# Сборка, проверка и установка disp.
#
#   make           собрать
#   make test      модульные тесты и приёмочный набор test.sh
#   make install   собрать и поставить в ~/.local/bin/disp
#   make gui           собрать графическую оболочку disp-gui
#   make install-gui   собрать её и поставить в ~/.local/bin/disp-gui
#   make tray          собрать значок в трее disp-tray (Linux, macOS)
#   make install-tray  собрать его и поставить в ~/.local/bin/disp-tray
#   make bump      повысить номер сборки: 0.8.001 → 0.8.002

PREFIX ?= $(HOME)/.local
BIN := $(PREFIX)/bin/disp
BUILT := target/release/disp

.PHONY: build test install gui install-gui tray install-tray bump

build:
	cargo build --release

test: build
	cargo test --release
	bash test.sh $(BUILT)

# Хуки агентов вызывают disp постоянно, поэтому файл подменяется переименованием,
# а не перезаписью: в любой момент на месте либо старая версия, либо новая.
install: build
	install -m 755 $(BUILT) $(BIN).new
	mv -f $(BIN).new $(BIN)
	@$(BIN) --version

# Графическая оболочка — отдельный пакет с тяжёлыми зависимостями: собирается минуты две.
gui:
	cargo build --release -p disp-gui

install-gui: gui
	install -m 755 target/release/disp-gui $(PREFIX)/bin/disp-gui.new
	mv -f $(PREFIX)/bin/disp-gui.new $(PREFIX)/bin/disp-gui
	@$(PREFIX)/bin/disp-gui --help | head -1

# Значок в трее — тоже отдельный пакет: на Linux он говорит с оболочкой по D-Bus,
# на macOS — с AppKit. Запущенный значок не мешает замене файла: старый процесс
# работает со старой версией, пока его не перезапустят.
tray:
	cargo build --release -p disp-tray

install-tray: tray
	install -m 755 target/release/disp-tray $(PREFIX)/bin/disp-tray.new
	mv -f $(PREFIX)/bin/disp-tray.new $(PREFIX)/bin/disp-tray
	@$(PREFIX)/bin/disp-tray --version

# Номер сборки — третья цифра версии в первой строке `version = "…"` в Cargo.toml
# (раздел [workspace.package]: версия общая для disp и disp-gui). Повышается перед
# каждой сборкой, которая уходит в работу, чтобы по шапке панели и `disp --version`
# было видно, что запущена новая.
bump:
	@awk 'BEGIN { FS = OFS = "\"" } \
	     !done && /^version = "/ { split($$2, v, "."); $$2 = v[1] "." v[2] "." v[3] + 1; \
	       printf "сборка %s.%s.%03d\n", v[1], v[2], v[3] + 1 > "/dev/stderr"; done = 1 } \
	     { print }' Cargo.toml > Cargo.toml.new
	@mv Cargo.toml.new Cargo.toml
