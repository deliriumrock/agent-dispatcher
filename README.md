# agent-dispatcher

**English** · [Русский](README.ru.md)

`disp` is a session dispatcher for AI coding agents: Claude Code, Codex CLI and
Gemini CLI. One table covers every open session: which one is working, which one
is waiting for you, and which one has finished and waits for you to review the
result.

- The agent calls you itself: a desktop or Telegram notification when it hits a
  permission prompt or finishes a long turn.
- Nothing leaves the list until you remove it explicitly (`disp ok`, `x` in the
  panel).
- A live panel for zellij: where each session runs, a jump to it, answering the
  agent “yes”, a timeline. A silent session is checked against its processes: a
  long command is running, the agent is stuck, or it is gone.
- Agents in containers are visible too, with no hooks and no changes to the
  container.

## Install

You need Linux or macOS and Rust 1.90 or newer. Jumping to an agent and answering
“yes” work through zellij. The process check for silent sessions reads `/proc`
and exists on Linux only; on macOS such a session is simply shown as “quiet”.

```sh
make install          # ~/.local/bin/disp
disp install claude   # hooks into the agent settings; same for codex and gemini
disp doctor           # which agents already send events
```

The graphical shell is a separate package and takes a few minutes to build:

```sh
make install-gui      # ~/.local/bin/disp-gui
disp gui --window     # as a separate window; without the flag — inside a terminal pane via term-everything
```

The tray icon works on Linux and macOS:

```sh
make install-tray     # ~/.local/bin/disp-tray
disp watch            # the terminal panel, and the tray icon along with it
disp tray             # only the icon; runs in the background, apart from the terminal
disp tray --autostart # show it at login (--no-autostart to stop)
```

`disp watch` and `disp gui` show the icon themselves if it is not there yet. There
is one icon for everything, and it stays when the panel is closed. It is not shown
with `DISP_TRAY=0` in the settings, in an SSH session or without a desktop. Errors
of an icon started this way go to `tray.log` in the state directory.

The panel decides the icon size: GNOME draws every AppIndicator icon in a 16×16
square, and the icon tile fills it completely. Bigger is only possible for all
icons at once: `gsettings set org.gnome.shell.extensions.appindicator icon-size 20`
(to undo: `gsettings reset … icon-size`).

The icon is a tile (a square with rounded corners) with a session count, coloured
by precedence: yellow — that many are waiting for you; blue — that many are
working; green — nothing is working, that many are done; grey — that many are
idle. An empty grey frame means there are no open sessions. The menu follows the
table order: “Waiting for you”, “Working”, “Idle”, “Done”. Each session has
“Answer “yes””, “Go to the pane” and “Remove from the list” buttons; a double
click on the icon opens the panel. On Linux the icon talks to the shell over D-Bus
(StatusNotifierItem), no GTK needed; in GNOME the AppIndicator extension shows such
icons. On macOS the icon lives in the menu bar, with no Dock icon.

Agents in containers (VS Code dev containers, Docker, Podman) are visible without
hooks and without configuring the containers: the icon, `disp watch`, `disp gui`
and `disp` look into them every few seconds. Claude Code shows “working”,
“waiting for permission” and “done”; Codex shows “working” and “done”, because
Codex does not log permission requests. Notifications about such sessions come
from whichever process noticed the change first — usually the icon. On Linux the
containers are read through `/proc` when the agent runs under your user (dev
containers do that); otherwise, and on macOS, through `docker exec` or
`podman exec`. The WHERE column shows `container <id>`, as in `docker ps`. Turn
it off with `DISP_CONTAINERS=0`.

Settings are `DISP_*` variables in the environment or in `~/.config/disp/env`:
Telegram (`DISP_TG_TOKEN`, `DISP_TG_CHAT`), the minimum turn length worth a
notification (`DISP_NOTIFY_MIN_SEC`, 120 s), the silence threshold
(`DISP_STALE_MIN`, 30 min), zellij pane labels (`DISP_PANE_LABEL=1`), watching
containers (`DISP_CONTAINERS`). The full list is in the specification.

The interface speaks English, Russian or Ukrainian: `DISP_LANG=en`, `ru` or `uk`.
Without it the system language is used (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`,
`LANG`), and if that is none of the three — English. The table, panel, window,
icon, help and notifications, Telegram included, all use it. `disp doctor` shows
which language is chosen.

## Every day

```sh
disp watch       # live panel and tray icon; ? lists all keys
disp             # table of open sessions
disp show <id>   # prompt, last reply, subtasks
disp ok <id>     # result reviewed — remove it
disp yes <id>    # answer a waiting agent “yes”
disp log         # what happened while you were away
```

## Documents

These documents are in Russian:

- `dispatcher-v0.md` — the specification: how it works, statuses, installation,
  limitations. The appendices hold the first bash version and the acceptance tests.
- `ROADMAP.md` — what comes next: a `disp serve` service, Telegram buttons, a phone.
- `disp` at the root — that first bash version (0.1.0), kept for comparison.

`CONTRIBUTING.md` (in English) describes the Git workflow.

## Development

```sh
make test   # unit tests and the test.sh acceptance suite
make bump   # build number +1 (0.8.005 → 0.8.006) before a build that goes into use
```

State lives in `~/.local/state/disp/`. For experiments point `DISP_STATE_DIR` and
`DISP_CONFIG` at a temporary directory, or the records end up in your real list,
and set `DISP_TRAY=0`, or `disp watch` adds another icon to the tray.
`DISP_CONTAINERS=0` keeps agents from the containers on this machine out of the
test list.
