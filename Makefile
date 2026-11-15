# Сборка, проверка и установка disp.
#
#   make           собрать
#   make test      модульные тесты и приёмочный набор test.sh
#   make install   собрать и поставить в ~/.local/bin/disp
#   make bump      повысить номер сборки: 0.8.001 → 0.8.002

PREFIX ?= $(HOME)/.local
BIN := $(PREFIX)/bin/disp
BUILT := target/release/disp

.PHONY: build test install bump

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

# Номер сборки — третья цифра версии в первой строке `version = "…"` в Cargo.toml.
# Повышается перед каждой сборкой, которая уходит в работу, чтобы по шапке панели
# и `disp --version` было видно, что запущена новая.
bump:
	@awk 'BEGIN { FS = OFS = "\"" } \
	     !done && /^version = "/ { split($$2, v, "."); $$2 = v[1] "." v[2] "." v[3] + 1; \
	       printf "сборка %s.%s.%03d\n", v[1], v[2], v[3] + 1 > "/dev/stderr"; done = 1 } \
	     { print }' Cargo.toml > Cargo.toml.new
	@mv Cargo.toml.new Cargo.toml
