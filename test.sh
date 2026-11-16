#!/usr/bin/env bash
# Тесты disp на симулированных событиях хуков. Запуск: ./test.sh [путь к disp]
set -u
DISP="${1:-$(dirname "$(readlink -f "$0")")/disp}"
T="$(mktemp -d)"
export DISP_STATE_DIR="$T/state" DISP_CONFIG="$T/env" DISP_DESKTOP=0 DISP_COLOR=0 COLUMNS=140
export DISP_PANE_TITLE=0   # настоящий zellij в тестах не опрашиваем; раздел 12 подставляет свой
export DISP_CONTAINERS=0   # агенты в контейнерах этой машины в тестовый список не попадают
export XDG_STATE_HOME="$T/xdg"
NOTES="$T/notes.log"
cat >"$T/notify.sh" <<EOF
#!/usr/bin/env bash
printf '%s | %s\n' "\$1" "\$2" >>"$NOTES"
EOF
chmod +x "$T/notify.sh"
export DISP_NOTIFY_CMD="$T/notify.sh"
unset ZELLIJ ZELLIJ_SESSION_NAME ZELLIJ_PANE_ID TMUX TERM_PROGRAM VSCODE_PID VSCODE_IPC_HOOK VSCODE_IPC_HOOK_CLI VSCODE_CWD GHOSTTY_RESOURCES_DIR DISP_TASK CLAUDE_PROJECT_DIR DISP_PANE_LABEL

pass=0 fail=0
ok() { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
no() { fail=$((fail + 1)); printf '  FAIL %s\n       %s\n' "$1" "${2:-}"; }
check() { # check <описание> <ожидание> <факт>
  if [ "$2" = "$3" ]; then ok "$1"; else no "$1" "ожидалось [$2], получено [$3]"; fi
}
has() { # has <описание> <подстрока> <текст>
  case "$3" in *"$2"*) ok "$1" ;; *) no "$1" "нет [$2] в: $3" ;; esac
}
hasnt() {
  case "$3" in *"$2"*) no "$1" "лишнее [$2] в: $3" ;; *) ok "$1" ;; esac
}
field() { jq -r "$2" "$DISP_STATE_DIR/sessions/$1.json" 2>/dev/null; }
settle() { sleep 0.3; }

# Рабочие каталоги: два worktree-подобных репозитория.
mkdir -p "$T/nbx-142" "$T/site-31"
git -C "$T/nbx-142" init -q -b nbx-142 2>/dev/null
git -C "$T/site-31" init -q -b main 2>/dev/null

CA="aaaaaaaa-1111-4111-8111-00000000a1b2"   # Claude, задача nbx-142
CB="019a0000-0000-7000-8000-0000000c0de1"   # Codex,  задача nbx-142 (UUIDv7: общий префикс)
CC="019a0000-0000-7000-8000-0000000c0de2"   # Codex,  задача site-31
GD="gemini-session-d4"                       # Gemini, задача site-31

ev() { # ev <agent> <json>  [env...]
  local agent="$1" json="$2"
  shift 2
  printf '%s' "$json" | env "$@" "$DISP" hook "$agent"
}

echo "== 1. Claude Code: полный цикл с подзадачами и разрешением"
export DISP_NOW=1000
Z=(ZELLIJ=0 ZELLIJ_SESSION_NAME=main ZELLIJ_PANE_ID=3)
out="$(ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"SessionStart\",\"source\":\"startup\"}" "${Z[@]}")"
check "SessionStart: stdout пуст" "" "$out"
check "SessionStart: статус idle" "idle" "$(field "claude-$CA" .status)"
check "задача = имя каталога репозитория" "nbx-142" "$(field "claude-$CA" .task)"
check "где = zellij:сессия#панель" "zellij:main#3" "$(field "claude-$CA" .where)"

out="$(ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Собери билд под macOS\\nи подпиши его\"}" "${Z[@]}")"
check "UserPromptSubmit: stdout пуст (иначе попал бы в контекст Claude)" "" "$out"
check "статус working" "working" "$(field "claude-$CA" .status)"
check "название из первого промпта, в одну строку" "Собери билд под macOS и подпиши его" "$(field "claude-$CA" .title)"

for i in 1 2 3; do
  ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"TaskCreated\",\"task_id\":\"$i\",\"task_subject\":\"Шаг $i\"}" "${Z[@]}"
done
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"TaskCompleted\",\"task_id\":\"1\",\"task_subject\":\"Шаг 1\"}" "${Z[@]}"
check "подзадачи: 3 создано" "3" "$(field "claude-$CA" '.subtasks | length')"
check "подзадачи: 1 закрыта" "1" "$(field "claude-$CA" '[.subtasks[] | select(.d)] | length')"

export DISP_NOW=1100
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"PermissionRequest\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"codesign\"}}" "${Z[@]}"
check "PermissionRequest: waiting" "waiting" "$(field "claude-$CA" .status)"
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"Notification\",\"notification_type\":\"permission_prompt\",\"message\":\"Claude needs your permission to use Bash\"}" "${Z[@]}"
settle
check "уведомление «ждёт» ровно одно (PermissionRequest + Notification не дублируют)" "1" "$(grep -c '✋' "$NOTES" 2>/dev/null)"
has "в уведомлении задача, агент и место" "nbx-142 · Claude ждёт тебя · разрешение: Bash · zellij:main#3" "$(cat "$NOTES")"

ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Bash\",\"tool_input\":{},\"tool_response\":{}}" "${Z[@]}"
check "PostToolUse после разрешения: снова working" "working" "$(field "claude-$CA" .status)"

export DISP_NOW=1900
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"Билд готов: dist/Nebularix.dmg\",\"background_tasks\":[{\"id\":\"b1\"}]}" "${Z[@]}"
settle
check "Stop: done" "done" "$(field "claude-$CA" .status)"
check "Stop: фоновые задачи посчитаны" "1" "$(field "claude-$CA" .bg)"
has "длинный ход: уведомление о завершении с длительностью" "✅ nbx-142 · Claude закончил ход (15м) · в фоне: 1" "$(cat "$NOTES")"

export DISP_NOW=2000
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"Notification\",\"notification_type\":\"idle_prompt\",\"message\":\"Claude is waiting for your input\"}" "${Z[@]}"
check "idle_prompt не превращает done в waiting" "done" "$(field "claude-$CA" .status)"

echo "== 2. Codex: вторая сессия той же задачи, ждёт разрешения"
export DISP_NOW=2100
V=(TERM_PROGRAM=vscode)
out="$(ev codex "{\"session_id\":\"$CB\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"SessionStart\",\"source\":\"startup\",\"model\":\"gpt\"}" "${V[@]}")"
check "Codex SessionStart: stdout пуст (иначе стал бы developer-контекстом)" "" "$out"
ev codex "{\"session_id\":\"$CB\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Проверь подпись и нотаризацию\"}" "${V[@]}"
ev codex "{\"session_id\":\"$CB\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"PermissionRequest\",\"tool_name\":\"Bash\"}" "${V[@]}"
check "Codex: waiting" "waiting" "$(field "codex-$CB" .status)"
check "Codex: где = vscode" "vscode" "$(field "codex-$CB" .where)"

echo "== 3. Codex: третья сессия, другая задача, короткий ход"
export DISP_NOW=2200
ev codex "{\"session_id\":\"$CC\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Поправь цены на странице\"}" ZELLIJ=0 ZELLIJ_SESSION_NAME=main ZELLIJ_PANE_ID=7
export DISP_NOW=2230
before="$(wc -l <"$NOTES")"
ev codex "{\"session_id\":\"$CC\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Stop\",\"last_assistant_message\":\"Готово\"}" ZELLIJ=0 ZELLIJ_SESSION_NAME=main ZELLIJ_PANE_ID=7
settle
check "короткий ход (30с): без уведомления" "$before" "$(wc -l <"$NOTES")"
check "событие без SessionStart всё равно создаёт строку" "done" "$(field "codex-$CC" .status)"
check "ветка main не дописывается к имени" "site-31" "$(field "codex-$CC" .task)"

echo "== 4. Gemini CLI: свои имена событий, stdout = JSON"
export DISP_NOW=2300
out="$(ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"SessionStart\",\"source\":\"startup\",\"timestamp\":\"2026-11-14T10:00:00Z\"}")"
check "Gemini: stdout = {}" "{}" "$out"
ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"BeforeAgent\",\"prompt\":\"Сгенерируй варианты заголовка\"}" >/dev/null
check "BeforeAgent: working" "working" "$(field "gemini-$GD" .status)"
ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Notification\",\"notification_type\":\"ToolPermission\",\"message\":\"Allow write_file?\"}" >/dev/null
check "Notification ToolPermission: waiting" "waiting" "$(field "gemini-$GD" .status)"
ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"AfterTool\",\"tool_name\":\"write_file\"}" >/dev/null
check "AfterTool: working" "working" "$(field "gemini-$GD" .status)"
ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"AfterAgent\",\"prompt_response\":\"Вот пять вариантов\"}" >/dev/null
check "AfterAgent: done" "done" "$(field "gemini-$GD" .status)"
ev gemini "{\"session_id\":\"$GD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"SessionEnd\",\"reason\":\"exit\"}" >/dev/null
check "SessionEnd: ended, но не принято (строгий режим по умолчанию)" "true/false" "$(field "gemini-$GD" '"\(.ended)/\(.accepted)"')"

echo "== 5. Таблица"
export DISP_NOW=2400
tbl="$("$DISP" ls)"
echo "$tbl" | sed 's/^/     | /'
check "строки не шире терминала" "0" "$(echo "$tbl" | jq -R --argjson w "$COLUMNS" 'select(length > $w)' | wc -l)"
check "при узкой панели тоже" "0" "$(COLUMNS=96 "$DISP" ls | jq -R 'select(length > 96)' | wc -l)"
has "сводка" "ждут тебя: 1 · работают: 0 · молчат: 0 · готово: 3" "$tbl"
check "первой идёт ждущая сессия" "0c0de1" "$(echo "$tbl" | sed -n 3p | awk '{print $1}')"
has "короткие ID различают сессии с общим префиксом UUIDv7" "0c0de2" "$tbl"
has "подзадачи и фон в таблице" "1/3 ф1" "$tbl"
has "закрытая сессия помечена" "закрыта" "$tbl"
hasnt "без цветовых кодов вне терминала" $'\033' "$tbl"
has "с цветом по запросу" $'\033[1;33m' "$(DISP_COLOR=1 "$DISP" ls)"
check "--json отдаёт массив из 4 строк" "4" "$("$DISP" ls --json | jq length)"

echo "== 6. Приёмка"
"$DISP" ok 00a1b2 >/dev/null
hasnt "принятая сессия ушла из списка" "00a1b2" "$("$DISP" ls)"
has "но видна с -a" "[убрано]" "$("$DISP" ls -a)"
"$DISP" ok --ended >/dev/null
hasnt "ok --ended принял закрытую" "gemini" "$("$DISP" ls)"
"$DISP" ok site-31 >/dev/null
hasnt "ok <задача> принимает все её сессии" "0c0de2" "$("$DISP" ls)"
msg="$("$DISP" ok nbx-142 2>&1)"; rc=$?
has "ok не прячет сессию, которая ждёт тебя" "пропущено (ещё ждёт тебя)" "$msg"
has "ждущая сессия осталась в списке" "0c0de1" "$("$DISP" ls)"
"$DISP" ok nosuch >/dev/null 2>&1
check "ok по несуществующему id: код 1" "1" "$?"
export DISP_NOW=2500
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Теперь залей на R2\"}" "${Z[@]}"
has "новый промпт возвращает принятую сессию в список" "● работает" "$("$DISP" ls | grep 00a1b2)"
check "название не перетирается следующим промптом" "Собери билд под macOS и подпиши его" "$(field "claude-$CA" .title)"

msg="$("$DISP" ok 00a1b2 2>&1)"; rc=$?
check "ok по работающей сессии: отказ" "1" "$rc"
"$DISP" ok -f 00a1b2 >/dev/null 2>&1
hasnt "ok -f прячет работающую" "00a1b2" "$("$DISP" ls)"
export DISP_NOW=2520
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"Stop\"}" "${Z[@]}"
has "завершение хода возвращает строку, даже если её спрятали" "✓ ГОТОВО" "$("$DISP" ls | grep 00a1b2)"
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Read\",\"agent_id\":\"sub-1\"}" "${Z[@]}"
check "инструмент фонового субагента не оживляет законченный ход" "done" "$(field "claude-$CA" .status)"
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"Notification\",\"notification_type\":\"elicitation_complete\"}" "${Z[@]}"
check "посторонние типы уведомлений статус не меняют" "done" "$(field "claude-$CA" .status)"
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Bash\"}" "${Z[@]}"
check "свой инструмент после Stop (автопродолжение): снова working" "working" "$(field "claude-$CA" .status)"
export DISP_NOW=2530

echo "== 7. Молчащие, прерванные, show, name"
export DISP_NOW=$((2530 + 31 * 60))
has "working без событий 31 мин, процесс агента неизвестен = молчит" "? молчит" "$("$DISP" ls | grep 00a1b2)"
ev claude "{\"session_id\":\"$CA\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"SessionEnd\",\"reason\":\"other\"}" "${Z[@]}"
has "сессия закрыта посреди работы = ПРЕРВАНА" "x ПРЕРВАНА" "$("$DISP" ls | grep 00a1b2)"
"$DISP" name 00a1b2 "macOS-билд Nebularix" >/dev/null
sh="$("$DISP" show 00a1b2)"
has "show: название" "macOS-билд Nebularix" "$sh"
has "show: последний ответ агента" "Билд готов: dist/Nebularix.dmg" "$sh"
has "show: подзадачи" "[x] Шаг 1" "$sh"
has "show: последний промпт" "Теперь залей на R2" "$sh"

lg="$("$DISP" log 50)"
has "журнал: ход Claude закончен" "nbx-142  работает → ГОТОВО  (zellij:main#3)" "$lg"
has "журнал: Codex ждёт" "codex  nbx-142  работает → ЖДЁТ  (vscode)" "$lg"
has "журнал: закрытие сессии" "→ закрыта" "$lg"
check "журнал: только смены статуса, без дублей" "0" "$(jq -c 'select(.from == .to)' "$DISP_STATE_DIR/events.jsonl" | wc -l)"
hasnt "в журнале нет текста промптов" "Собери билд" "$(cat "$DISP_STATE_DIR/events.jsonl")"

echo "== 8. StopFailure и авто-приёмка по выходу"
export DISP_NOW=5000
S5="bbbbbbbb-2222-4222-8222-00000000e5e5"
ev claude "{\"session_id\":\"$S5\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Долгая задача\"}"
ev claude "{\"session_id\":\"$S5\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"StopFailure\",\"error\":\"rate_limit\"}"
settle
check "StopFailure: error" "error" "$(field "claude-$S5" .status)"
has "StopFailure: уведомление" "⛔ site-31 · Claude: rate_limit" "$(cat "$NOTES")"
S6="cccccccc-3333-4333-8333-00000000f6f6"
ev claude "{\"session_id\":\"$S6\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"x\"}" DISP_AUTO_OK_ON_EXIT=1
ev claude "{\"session_id\":\"$S6\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Stop\"}" DISP_AUTO_OK_ON_EXIT=1
ev claude "{\"session_id\":\"$S6\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"SessionEnd\",\"reason\":\"prompt_input_exit\"}" DISP_AUTO_OK_ON_EXIT=1
check "DISP_AUTO_OK_ON_EXIT=1: явный выход принимает done-сессию" "true" "$(field "claude-$S6" .accepted)"

echo "== 9. Устойчивость: хук никогда не мешает агенту"
out="$(printf '' | "$DISP" hook claude)"; rc=$?
check "пустой stdin: код 0, stdout пуст" "0/" "$rc/$out"
out="$(printf 'это не json {{{' | "$DISP" hook codex)"; rc=$?
check "битый JSON: код 0, stdout пуст" "0/" "$rc/$out"
out="$(printf '[1,2,3]' | "$DISP" hook gemini)"; rc=$?
check "JSON-массив вместо объекта: код 0, Gemini получает {}" "0/{}" "$rc/$out"
out="$(printf '{"hook_event_name":"PreCompact","session_id":"x"}' | "$DISP" hook claude)"; rc=$?
check "неизвестное событие: игнорируется" "0/" "$rc/$out"
big="$(head -c 400000 /dev/zero | tr '\0' 'a')"
out="$(printf '{"session_id":"%s","cwd":"%s","hook_event_name":"PostToolUse","tool_response":{"content":"%s"}}' "$S5" "$T/site-31" "$big" | "$DISP" hook claude)"; rc=$?
check "payload 400 КБ: код 0" "0/" "$rc/$out"
check "payload 400 КБ: состояние обновлено" "working" "$(field "claude-$S5" .status)"
printf 'мусор' >"$DISP_STATE_DIR/sessions/claude-$S6.json"
ev claude "{\"session_id\":\"$S6\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"после порчи\"}"
check "битый файл состояния: строка пересоздана" "working" "$(field "claude-$S6" .status)"
printf 'мусор\n' >"$DISP_STATE_DIR/sessions/zzz-broken.json"
"$DISP" ls >/dev/null 2>&1
check "битый чужой файл не ломает ls" "0" "$?"
out="$(printf '{"cwd":"%s","hook_event_name":"UserPromptSubmit","prompt":"без id"}' "$T/site-31" | "$DISP" hook codex)"; rc=$?
check "нет session_id: код 0" "0/" "$rc/$out"
out="$(printf '{"session_id":"../../etc/x y","cwd":"/nonexistent/dir","hook_event_name":"SessionStart"}' | "$DISP" hook claude)"; rc=$?
check "опасный session_id и несуществующий cwd: код 0" "0/" "$rc/$out"
check "файлы состояния только внутри каталога сессий" "0" "$(find "$T" -name '*.json' -not -path "$DISP_STATE_DIR/sessions/*" | wc -l)"
out="$(printf '{"session_id":"ro","hook_event_name":"SessionStart"}' | DISP_STATE_DIR=/proc/nope "$DISP" hook claude 2>/dev/null)"; rc=$?
check "недоступный каталог состояния: код 0" "0/" "$rc/$out"
mkdir -p "$T/bin"
for t in cat mkdir date tr cut basename dirname readlink; do ln -s "$(command -v "$t")" "$T/bin/$t"; done
# bash-версию запускаем через bash по полному пути, скомпилированную — напрямую
RUN=("$DISP")
[ "$(head -c 2 "$DISP")" = '#!' ] && RUN=("$(command -v bash)" "$DISP")
out="$(printf '{"session_id":"nojq","hook_event_name":"SessionStart"}' | PATH="$T/bin" "${RUN[@]}" hook gemini 2>/dev/null)"; rc=$?
check "нет jq в PATH: код 0, Gemini получает {}" "0/{}" "$rc/$out"

echo "== 10. Параллельные события одной сессии (flock)"
export DISP_NOW=6000
S7="dddddddd-4444-4444-8444-0000000a7a7a"
ev claude "{\"session_id\":\"$S7\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"много подзадач\"}"
for i in $(seq 1 30); do
  ev claude "{\"session_id\":\"$S7\",\"cwd\":\"$T/nbx-142\",\"hook_event_name\":\"TaskCreated\",\"task_id\":\"t$i\",\"task_subject\":\"п$i\"}" &
done
wait
check "30 одновременных TaskCreated: ничего не потеряно" "30" "$(field "claude-$S7" '.subtasks | length')"

echo "== 11. Codex: прерывание хода (Interrupt) и конец сессии (SessionEnd)"
export DISP_NOW=6500
S9="019a0000-0000-7000-8000-0000000c9c9c"
ev codex "{\"session_id\":\"$S9\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Долгий рефакторинг\"}"
before="$(wc -l <"$NOTES")"
export DISP_NOW=6900
out="$(ev codex "{\"session_id\":\"$S9\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Interrupt\",\"turn_id\":\"t1\"}")"
settle
check "Interrupt: stdout пуст (простой текст для Codex здесь недопустим)" "" "$out"
check "Interrupt: активный ход снят с «работает», но не принят" "done/ход прерван/false" "$(field "codex-$S9" '"\(.status)/\(.detail)/\(.accepted)"')"
check "Interrupt: без уведомления (ты сам у клавиатуры)" "$before" "$(wc -l <"$NOTES")"
has "прерванный ход остаётся в списке с пометкой" "ход прерван · Долгий рефакторинг" "$("$DISP" ls | grep 0c9c9c)"
ev codex "{\"session_id\":\"$S9\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Interrupt\",\"turn_id\":\"t1\"}"
check "повторный Interrupt вне хода ничего не меняет" "6900" "$(field "codex-$S9" .done_at)"
ev codex "{\"session_id\":\"$S9\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"SessionEnd\",\"reason\":\"exit\"}"
check "Codex SessionEnd: сессия закрыта, строка не принята" "true/false" "$(field "codex-$S9" '"\(.ended)/\(.accepted)"')"
check "config codex: подключены Interrupt и SessionEnd" "true" "$("$DISP" config codex | jq '.hooks | has("Interrupt") and has("SessionEnd")')"
check "config codex: таймаут Interrupt и SessionEnd не больше 3 с (предел Codex)" "3" "$("$DISP" config codex | jq '[.hooks.Interrupt[0].hooks[0].timeout, .hooks.SessionEnd[0].hooks[0].timeout] | max')"

echo "== 12. Название сессии от самого агента"
export DISP_NOW=6950
SA="ffffffff-6666-4666-8666-0000000d1d1d"
TR="$T/tr-a.jsonl"
printf '%s\n' '{"type":"user","message":"x"}' '{"type":"ai-title","aiTitle":"Сборка и подпись macOS-билда","sessionId":"x"}' >"$TR"
ev claude "{\"session_id\":\"$SA\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TR\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"ну давай, собери уже как-нибудь\"}"
check "Claude: название из ai-title транскрипта, а не из промпта" "Сборка и подпись macOS-билда" "$(field "claude-$SA" .title)"
printf '%s\n' '{"type":"custom-title","customTitle":"Релиз 1.4 \"mac\"","sessionId":"x"}' '{"type":"ai-title","aiTitle":"Сборка и подпись macOS-билда","sessionId":"x"}' >>"$TR"
ev claude "{\"session_id\":\"$SA\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TR\",\"hook_event_name\":\"Stop\"}"
check "Claude: /rename (custom-title) важнее ai-title" 'Релиз 1.4 "mac"' "$(field "claude-$SA" .title)"
"$DISP" name 0d1d1d "жду билд из CI" >/dev/null
ev claude "{\"session_id\":\"$SA\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TR\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"ещё раз\"}"
ev claude "{\"session_id\":\"$SA\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TR\",\"hook_event_name\":\"Stop\"}"
check "название, заданное через disp name, агент не перетирает" "жду билд из CI" "$(field "claude-$SA" .title)"
SB="ffffffff-6666-4666-8666-0000000d2d2d"
TRB="$T/tr-b.jsonl"
printf '%s\n' '{"type":"user","message":"x"}' >"$TRB"
ev claude "{\"session_id\":\"$SB\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TRB\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Обнови зависимости\"}"
check "названия в транскрипте ещё нет: показываем первый промпт" "Обнови зависимости" "$(field "claude-$SB" .title)"
printf '%s\n' '{"type":"ai-title","aiTitle":"Обновление зависимостей","sessionId":"x"}' >>"$TRB"
ev claude "{\"session_id\":\"$SB\",\"cwd\":\"$T/nbx-142\",\"transcript_path\":\"$TRB\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Bash\"}"
check "название появилось посреди хода: подхватывается на событии инструмента" "Обновление зависимостей" "$(field "claude-$SB" .title)"
mkdir -p "$T/fakezj"
cat >"$T/fakezj/zellij" <<'EOF'
#!/usr/bin/env bash
printf 'PANE_ID  TYPE  TITLE\nterminal_7  terminal  ⠙ Починить импорт цен | site-31\nterminal_8  terminal  rock@host:~/projects\n'
EOF
chmod +x "$T/fakezj/zellij"
SC="019a0000-0000-7000-8000-0000000d3d3d"
ev codex "{\"session_id\":\"$SC\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"глянь, что там\"}" ZELLIJ=0 ZELLIJ_SESSION_NAME=main ZELLIJ_PANE_ID=7 DISP_PANE_TITLE=1 PATH="$T/fakezj:$PATH"
check "Codex: название из заголовка панели zellij, без спиннера и имени проекта" "Починить импорт цен" "$(field "codex-$SC" .title)"
SD="019a0000-0000-7000-8000-0000000d4d4d"
ev codex "{\"session_id\":\"$SD\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"глянь, что там\"}" ZELLIJ=0 ZELLIJ_SESSION_NAME=main ZELLIJ_PANE_ID=8 DISP_PANE_TITLE=1 PATH="$T/fakezj:$PATH"
check "заголовок оболочки (user@host:путь) названием не считается" "глянь, что там" "$(field "codex-$SD" .title)"

echo "== 13. Настройки, права, config, gc, doctor"
printf '# комментарий\nDISP_NOTIFY_MIN_SEC=5\nEVIL=$(touch %s/pwned)\nDISP_STALE_MIN="1"\n' "$T" >"$DISP_CONFIG"
export DISP_NOW=7000
S8="eeeeeeee-5555-4555-8555-0000000b8b8b"
ev claude "{\"session_id\":\"$S8\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"порог из файла\"}"
export DISP_NOW=7010
ev claude "{\"session_id\":\"$S8\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"Stop\"}"
settle
has "порог уведомления читается из файла настроек" "закончил ход (10с)" "$(tail -n 1 "$NOTES")"
check "файл настроек не исполняется как shell" "нет" "$([ -e "$T/pwned" ] && echo да || echo нет)"
check "файлы состояния закрыты от чужих (600)" "600" "$(stat -c %a "$DISP_STATE_DIR/sessions/claude-$S8.json")"
for a in claude codex gemini; do
  "$DISP" config "$a" | jq -e '.hooks | length > 0' >/dev/null 2>&1
  check "config $a: валидный JSON" "0" "$?"
done
has "config: абсолютный путь к скрипту" "$(readlink -f "$DISP") hook codex" "$("$DISP" config codex)"
CS="$T/agent-conf/settings.json"
mkdir -p "$T/agent-conf"
printf '{"model":"x","hooks":{"Stop":[{"hooks":[{"type":"command","command":"/usr/bin/true"}]}]}}\n' >"$CS"
DISP_CLAUDE_SETTINGS="$CS" "$DISP" install claude >/dev/null
check "install: чужой хук и остальные ключи сохранены, наш добавлен" "2/x/12" "$(jq -r '"\(.hooks.Stop | length)/\(.model)/\(.hooks | length)"' "$CS")"
check "install: копия прежнего файла" "1" "$(jq '.hooks.Stop | length' "$CS.bak-disp")"
has "install: повторный запуск сообщает, что менять нечего" "изменений нет" "$(DISP_CLAUDE_SETTINGS="$CS" "$DISP" install claude)"
check "install: повторный запуск ничего не дублирует" "2" "$(jq '.hooks.Stop | length' "$CS")"
DISP_CODEX_HOOKS="$T/agent-conf/new/hooks.json" "$DISP" install codex >/dev/null
check "install: если файла нет, он создаётся" "9" "$(jq '.hooks | length' "$T/agent-conf/new/hooks.json")"
printf 'не json' >"$CS"
DISP_CLAUDE_SETTINGS="$CS" "$DISP" install claude >/dev/null 2>&1
check "install: битый файл настроек не тронут, код 1" "1/не json" "$?/$(cat "$CS")"
export DISP_NOW=$((7010 + 8 * 86400))
"$DISP" gc 7 >/dev/null
check "gc удалил принятые старше 7 дней" "0" "$("$DISP" ls --json | jq '[.[] | select(.accepted)] | length')"
has "gc не трогает непринятые" "порог из файла" "$("$DISP" ls -a)"
has "doctor видит события агентов" "claude" "$("$DISP" doctor)"
echo "     hook.log:"; sed 's/^/     | /' "$DISP_STATE_DIR/hook.log" 2>/dev/null
check "в hook.log нет ошибок самого скрипта" "0" "$(grep -Ec 'unbound variable|syntax error|command not found|compile error' "$DISP_STATE_DIR/hook.log" 2>/dev/null | head -n 1)"

echo "== 14. Молчащая сессия: что с процессом агента"
# Хук Claude Code получает CLAUDE_PID и CLAUDE_CODE_SESSION_ID и запоминает процесс агента,
# если номер сессии совпал. Процесс агента здесь изображает обычный sleep.
export DISP_NOW=20000
SP="cccccccc-3333-4333-8333-0000000057c4"
sleep 300 &
AGENT=$!
ev claude "{\"session_id\":\"$SP\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"UserPromptSubmit\",\"prompt\":\"Долгая проверка\"}" "${Z[@]}" CLAUDE_CODE_SESSION_ID="$SP" CLAUDE_PID="$AGENT"
check "хук запомнил процесс агента своей сессии" "$AGENT" "$(field "claude-$SP" .process.pid)"
ev claude "{\"session_id\":\"$SP\",\"cwd\":\"$T/site-31\",\"hook_event_name\":\"PostToolUse\",\"tool_name\":\"Read\"}" "${Z[@]}" CLAUDE_CODE_SESSION_ID="other" CLAUDE_PID="1"
check "хук из чужой сессии процесс не подменяет" "$AGENT" "$(field "claude-$SP" .process.pid)"
export DISP_NOW=$((20000 + 31 * 60))
has "молчит, агент жив, команд нет = завис" "? завис" "$("$DISP" ls | grep 0057c4)"
check "завис в --json" "stuck" "$("$DISP" ls --json | jq -r '.[] | select(.sid == "0057c4") | .view')"
kill "$AGENT"
wait "$AGENT" 2>/dev/null
row="$("$DISP" ls | grep 0057c4)"
has "молчит, процесса агента нет = ПРЕРВАНА" "x ПРЕРВАНА" "$row"
has "пометка о закрытом агенте" "агент закрыт" "$row"
"$DISP" ok 0057c4 >/dev/null
check "строку закрытого агента убирают без -f" "true" "$(field "claude-$SP" .accepted)"

echo
echo "итого: ok=$pass fail=$fail"
rm -rf "$T"
[ "$fail" -eq 0 ]
