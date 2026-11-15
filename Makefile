# Сборка, проверка и установка disp.
#
#   make           собрать
#   make test      модульные тесты и приёмочный набор test.sh
#   make install   собрать и поставить в ~/.local/bin/disp

PREFIX ?= $(HOME)/.local
BIN := $(PREFIX)/bin/disp
BUILT := target/release/disp

.PHONY: build test install

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
