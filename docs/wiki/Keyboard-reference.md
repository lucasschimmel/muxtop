# Keyboard reference

**Press `?` inside muxtop for the authoritative list.** The help screen is
generated from the same table the event loop dispatches on
(`crates/muxtop-tui/src/keymap.rs`), so it cannot drift from what the keys
actually do. This page is that table written out, plus the rules that govern
it — which is the part the help screen cannot explain.

---

## Three rules that explain most surprises

**1. Tab-scoped keys only act on their own tab.** Pressing `F9` on the Network
tab does nothing. It does not fall through to killing the process that happened
to be selected on the Processes tab — a monitor that acts on a row you cannot
see is a monitor that will eventually kill the wrong thing.

**2. A tab-scoped binding shadows a global one, on its tab only.** `D` switches
the Kube sub-view to Deployments and the GPU sub-view to Devices; on any other
tab it is unbound. Same key, different tab, different meaning, no ambiguity.

**3. Actions that change the host are disabled over `--remote`.** Renice, kill,
force-kill and the container stop/kill/restart/shell actions are local-only. Remotely
they are rejected rather than silently ignored, so you get a message instead of
wondering why nothing happened.

---

## Application

| Key | Action |
|---|---|
| `?` · `F1` | Help — the generated keymap |
| `Ctrl+P` · `Ctrl+K` | Command palette (fuzzy) |
| `:` | Command mode (typed) |
| `Space` | Pause / resume refresh |
| `r` | Refresh now |
| `Ctrl+L` | Message log |
| `q` · `Ctrl+C` | Quit |

`Space` freezes the view without stopping collection — useful when a row you
want to read keeps moving under a sort. `r` forces a collection cycle instead of
waiting out `--refresh`.

## Tabs and sub-views

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Next / previous tab |
| `Alt+1` … `Alt+6` | General · Processes · Network · Containers · Kubernetes · GPU |
| `]` / `[` | Next / previous sub-view within the active tab |
| `P` / `N` / `D` | Kube sub-view: **P**ods / **N**odes / **D**eployments |
| `D` / `P` | GPU sub-view: **D**evices / **P**rocs |
| `A` | Kubernetes: toggle one namespace ↔ **A**ll namespaces (local mode) |

`]` and `[` are the tab-agnostic way to move between sub-views, which is why the
letter keys can overlap between the Kube and GPU tabs without conflict.

## Moving around a table

| Key | Action |
|---|---|
| `j` / `k` · `↓` / `↑` | Move the row cursor |
| `PgDn` / `PgUp` | Page down / up |
| `Ctrl+D` / `Ctrl+U` | Half page down / up |
| `g` / `G` · `Home` / `End` | First / last row |
| `h` / `l` · `←` / `→` | Scroll columns — for terminals too narrow for the full table |
| `Enter` · `i` | Inspect the selected row |
| `x` | Contextual actions menu for the selected row |
| `y` | Copy the row's identifier to the clipboard |

**`y` works over ssh.** It uses OSC 52, so the copy lands in *your* clipboard
rather than the remote host's, provided your terminal allows it (kitty, iTerm2,
WezTerm, foot and recent tmux do; some require enabling it).

**The inspector (`Enter`) is where the truncated data lives** — the full command
line that the table had to cut, a container's image and ports, a pod's node,
per-GPU clocks, an interface's error counters.

## Sorting and filtering

| Key | Action |
|---|---|
| `/` · `F3` / `F4` | Filter the active tab |
| `s` · `F6` | Cycle the sort column |
| `S` · `I` | Reverse the sort direction |
| `Esc` | One step back |

Filters are **per tab** — a process filter does not hide containers.

`Esc` unwinds one layer at a time, in this order: close an open overlay →
dismiss pending messages → leave the filter input → clear the filter. Pressing
it twice from a filtered table therefore leaves the input and then clears it,
which is usually what you meant.

## Processes tab

| Key | Action |
|---|---|
| `t` · `F5` | Tree view — parent/child hierarchy |
| `F8` · `+` | Raise priority (nice −1) |
| `F7` · `-` | Lower priority (nice +1) |
| `F9` | Kill — SIGTERM |
| `F10` | Force kill — SIGKILL |

Raising priority needs privilege that lowering it does not: on Linux an
unprivileged user can only increase a nice value, never decrease it back. `F8`
failing with a permission error on a process you already niced is the kernel,
not muxtop.

Only SIGTERM and SIGKILL can be sent, ever — the signal is chosen from a fixed
set rather than passed through, so there is no path to sending an arbitrary
signal number to PID 1.

## Containers tab

| Key | Action |
|---|---|
| `F9` | Stop the container (SIGTERM + grace period) |
| `F10` | Kill the container (SIGKILL) |
| `F11` | Restart the container |
| `e` | Open `/bin/sh` with a PTY (Linux/macOS, running containers) |

Stop/kill/restart ask for confirmation first. All container actions are local-only.
Inside the shell, `Ctrl+]` detaches and `exit` returns normally; keys otherwise
go to the shell. See [Containers](Containers#interactive-shell-linuxmacos-local-mode).

---

## Command palette vs command mode

Two ways in, same command set:

- **`Ctrl+P`** (or `Ctrl+K`) opens the fuzzy palette — type fragments, matched
  by [nucleo](https://github.com/helix-editor/nucleo), the matcher from Helix.
  Best when you know roughly what the thing is called.
- **`:`** opens command mode — type the command out. Best when you know exactly
  what you want and it takes an argument.

```
:kill firefox
:sort mem
:filter ngin
:theme mono
:tab gpu
```

Commands that act on the host go through the same confirmation dialog as their
keybindings, and are subject to the same local-only rule.

---

## Mouse

Mouse support is on where a pointer is detected and off where it is not
(`TERM=linux`, `TERM=dumb`). Everything the mouse can do, the keyboard can do —
the mouse is never the only route to a feature.

Mouse capture takes the terminal's own text selection away, which is a bad
trade if you copy from the screen a lot. Turn it off with `--no-mouse`.

---

## Overrides at startup

```sh
muxtop --tree                 # start in tree view
muxtop --sort mem             # start sorted by memory
muxtop --filter firefox       # start with a process filter
muxtop --theme mono           # no hue at all
muxtop --no-color             # same as NO_COLOR=1
muxtop --ascii                # force the ASCII glyph set
muxtop --no-mouse             # never enable mouse reporting
```
