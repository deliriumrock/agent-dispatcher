# Сборка, проверка и установка disp.
#
#   make           собрать
#   make test      модульные тесты и приёмочный набор test.sh
#   make install   собрать и поставить в ~/.local/bin/disp
#   make gui           собрать графическую оболочку disp-gui
#   make install-gui   собрать её и поставить в ~/.local/bin/disp-gui

PREFIX ?= $(HOME)/.local
BIN := $(PREFIX)/bin/disp
BUILT := target/release/disp

.PHONY: build test install gui install-gui

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
