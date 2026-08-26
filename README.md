# aplexer

`aplexer` is an **agent multiplexer** — an agent-first alternative to tmux for running coding agents (Claude, Codex, Gemini, Grok) and shells side by side. tmux is a generic terminal grid that doesn't know what's running in a pane; aplexer is built around the idea that an AI agent is a first-class thing to run, address, isolate, and talk to, not an afterthought bolted onto a pane. The short `a` binary is the user-facing CLI; `aplexer` is the per-session worker executable.

## What's different from tmux

- **Sessions are `workspace + tag + engine + profile`, not flat pane names.** `~/git/pocketshell:review` running `codex/zai` is a real, addressable identity (spec.md §§1–3) — `a list` groups sessions by workspace and shows engine/profile for each, instead of a flat list of pane titles you have to keep straight yourself.
- **Aplexer owns how each agent actually launches**, not just how it's displayed. Engine definitions (spec.md §8) capture the real launch command, permission-bypass flags, and profile config-dir wiring per engine — `a - clz` starts Claude routed through its Z.AI profile (the right `CLAUDE_CONFIG_DIR` set automatically) with one two-letter shortcut, instead of a hand-assembled command line you have to get right every time. See [Engines, profiles, and shortcuts](#engines-profiles-and-shortcuts).
- **Every session is independently resource-isolated**, because agent workloads — a runaway test loop, an agent that spawns its own build — are exactly the kind of thing that eats memory unpredictably. A `--memory`-capped session gets OOM-killed on its own by the kernel's real cgroup OOM killer, without taking any other session down. See [Resource isolation](#resource-isolation) and [Daemonless vs. tmux](#daemonless-vs-tmux) below.
- **Agents can find out where they are and talk to each other.** `a whoami` lets an agent (or a hook, or a script) running inside a session ask "which session am I, what engine/profile, what workspace" — there's no tmux equivalent because a tmux pane has no agent identity to ask about. `a message` gives sibling agents in the same workspace a durable inbox plus direct-to-pane delivery for real handoffs ("backend's done, see api.md"). See [Inter-agent messaging](#inter-agent-messaging).

None of this gives up what tmux is actually good at operationally — persistent, reattachable sessions that survive a disconnected client (see [First session](#first-session)) — aplexer just refuses to also be a shared failure domain while doing it, which is enough of an architectural difference to get its own section: **one session may fail, OOM, or be killed without causing unrelated sessions to disappear.** See [Daemonless vs. tmux](#daemonless-vs-tmux).

## Goals

- **Standalone tmux / `tmuxctl` replacement**, usable on its own for plain shells too — see [What's different from tmux](#whats-different-from-tmux) above and [spec.md](spec.md) sections 1–2 for the full model.
- **Backing runtime for two sibling client projects**, also cloned under `~/git`:
  - [pocketshell](https://github.com/alexeygrigorev/pocketshell) — a voice-first, tmux-native, agent-aware Android SSH client. It's meant to move off owning tmux session lifecycle / `tmuxctl` wrappers / agent engine discovery itself and instead become an aplexer client (see [spec.md](spec.md) section 22).
  - [pocketshell-electron](https://github.com/alexeygrigorev/pocketshell-electron) — a desktop **Electron** port of PocketShell (Vue 3 + Vite + `ssh2` + `xterm.js`), the keyboard-first desktop companion to PocketShell Android. It integrates with tmux today; aplexer is intended to be its session runtime as well. (Note: an earlier, unrelated desktop project, `pocketshell-desktop`, a VS Code fork, is now archived — this is the active one.)

## Build

```bash
cargo build --release --bins
install -m 0755 target/release/a target/release/aplexer ~/.local/bin/
```

Rust 1.78 or newer is recommended. Runtime state defaults to `$XDG_RUNTIME_DIR/aplexer`; durable records and bounded output history default to `$XDG_STATE_HOME/aplexer` (or `~/.local/state/aplexer`). Override these with `APLEXER_RUNTIME_DIR`, `APLEXER_STATE_DIR`, and `APLEXER_CONFIG`.

## Shell completions

`a completions <shell>` prints a completion script for `a`'s subcommands and flags to stdout (via [`clap_complete`](https://docs.rs/clap_complete)), covering `bash`, `zsh`, `fish`, `elvish`, and `powershell`. Install it once and open a new shell:

```bash
# bash (per-user; requires the bash-completion package to auto-load it)
mkdir -p ~/.local/share/bash-completion/completions
a completions bash > ~/.local/share/bash-completion/completions/a

# zsh
mkdir -p ~/.zfunc
a completions zsh > ~/.zfunc/_a
# then, before `compinit` in ~/.zshrc:
#   fpath+=(~/.zfunc)

# fish (auto-loaded, no extra config needed)
a completions fish > ~/.config/fish/completions/a.fish
```

## First session

```bash
a start --workspace "$PWD" --tag shell -- /bin/bash -l
a list
a attach --workspace "$PWD" --tag shell
# Ctrl-] detaches without terminating the workload.
```

The canonical identity printed by `start` is a UUID. Commands accept a full UUID, an unambiguous prefix, or `--workspace PATH --tag TAG`. The full CLI surface is `start`, `list`/`snapshot`, `attach`, `send`, `capture`, `status`, `kill`, `rename`, `engines`, `profiles`, `watch`, and `doctor`.

```bash
a send --workspace "$PWD" --tag shell --enter 'printf "hello\\n"'
a capture --workspace "$PWD" --tag shell
a status --workspace "$PWD" --tag shell
a kill --workspace "$PWD" --tag shell --signal TERM --grace-ms 2000
```

Output capture is byte-preserving. `send --stdin` and the Python API also transport bytes directly rather than asking a shell to reinterpret them.

## Engines, profiles, and shortcuts

Configuration is one TOML file, `~/.config/aplexer/config.toml` (override with `APLEXER_CONFIG`). It declares three related things together, in the same place, so it's clear how they relate:

- **`[engines.<id>]`** — how to launch an agent/shell/command: argv plus any env. `codex` is a plain engine entry — `a - codex` just runs `codex` with no profile.
- **`[profiles.<id>]`** — an engine variant, usually a different config directory via that engine's env var (`CLAUDE_CONFIG_DIR` for claude, `CODEX_HOME` for codex) — an alternate account or provider.
- **`[shortcuts.<id>]`** — a short mnemonic for `a - <id>` that resolves to an (engine, profile) pair. `coz` is a shortcut meaning "codex with the Z.AI profile" (`config_dir` `~/.zodex`) — a fast path onto exactly what `a start --engine codex --profile zodex` already does, distinct from the plain `codex` engine above.

All three layer identically: aplexer ships built-in/auto-discovered defaults for each map, then your config file's `[engines.*]` / `[profiles.*]` / `[shortcuts.*]` tables extend it, and your file wins on any id collision.

### Engines

Built-ins (before any config file is read):

```toml
[engines.shell]
command = ["$SHELL", "-l"]   # resolved from $SHELL at load time, "/bin/sh" as a last resort

[engines.codex]
command = ["codex", "-c", "check_for_update_on_startup=false"]

[engines.claude]
command = ["claude"]

[engines.gemini]
command = ["gemini"]

[engines.grok]
command = ["grok"]
```

Override or add an engine in your config file the same way. Codex's builtin already suppresses the startup update-check modal (the same flag PocketShell's host CLI has used since #703).

### Profiles

Only claude and codex currently support profiles. Zero-config auto-discovery (ported from PocketShell's `tools/pocketshell/src/pocketshell/profiles.py`, spec.md 9.2/23) scans the top level of `$HOME` for `~/.<name>` directories that: aren't the engine's own default dir (`~/.claude` / `~/.codex`), have a name containing a hint for that engine (claude: `claude`/`laude`; codex: `codex`/`odex` — catching swaps like `zlaude`), and carry a real marker file (claude: `.claude.json` or `settings.json`; codex: `config.toml` or `auth.json`). A match becomes a profile named after its own directory stem — never a humanized display name, since `[profiles.*]` is one flat namespace shared by every engine and two engines' same-sounding profiles (e.g. both called "zai") would otherwise clobber each other. On a machine with a Z.AI-routed codex account at `~/.zodex`, discovery derives the equivalent of:

```toml
# what discovery derives automatically from ~/.zodex -- shown for reference,
# you don't need to write this yourself unless you want to override it
[profiles.zodex]
engine = "codex"

[profiles.zodex.env]
CODEX_HOME = "/home/alexey/.zodex"
```

Add your own, or override a discovered one, in your config file the same way:

```toml
[profiles.review]
engine = "shell"
args = []
history_bytes = 8388608

[profiles.review.env]
MODE = "review"

[profiles.review.limits]
memory_bytes = 2147483648
pids = 256
```

Launch a profile explicitly with `a start --engine codex --profile zodex`, or from inside a workspace `a start --profile zodex` (the profile's own `engine` field fills in `--engine`).

### Shortcuts

Built-in defaults, resolving `a - <id>` to an (engine, profile) pair:

```toml
[shortcuts.cl]
engine = "claude"

[shortcuts.co]
engine = "codex"

[shortcuts.g]
engine = "grok"

[shortcuts.clz]
engine = "claude"
profile = "zlaude"

[shortcuts.coz]
engine = "codex"
profile = "zodex"

[shortcuts.cog]
engine = "codex"
profile = "godex"
```

So `a - codex` (a real engine id) runs plain codex tagged `codex`, while `a - coz` (a shortcut id) runs codex with the Z.AI profile tagged `coz` — same engine, different profile and tag, so the two sessions never collide. `a - coz review` uses the same engine+profile but tags the session `review` instead of `coz`. Word matching checks real engine ids first (so a real engine name always means exactly what it says), then shortcut ids, then falls back to running the words as a literal command — see the doc comment on `cmd_quick_launch` in `src/bin/a.rs` for the full precedence rationale.

Add your own the same way:

```toml
[shortcuts.rev]
engine = "codex"
profile = "review"   # the [profiles.review] example above
```

Inspect effective discovery/resolution any time with `a engines`, `a profiles`, and `a doctor`.

## Resource isolation

When any limit is requested, the worker creates a per-session cgroup-v2 leaf and moves the workload into it before releasing the child from a pre-exec launch gate. If the current cgroup is not delegated to the user, launch fails rather than silently running without the requested limit. `kill` serializes termination and uses the cgroup for descendant-wide signaling and escalation.

## Durable lifecycle

Session records use versioned JSON and atomic `fsync` + rename replacement. PTY history is kept within the configured byte bound and remains available after workload exit. A worker keeps its socket alive after exit so reattach, status, and capture do not race record finalization.

## Watching events

```bash
a watch --jsonl
a watch --jsonl --workspace "$PWD"
a watch --jsonl --all   # also include shell (non-agent) sessions
```

`a watch` is a client-side poller, not a server-push mechanism — it scans the same durable session records `a list` reads, on a timer, and emits one JSON line per detected change: session creation/exit/deletion, OOM kills, and a coarse `agent.state` (`running`/`waiting`) signal derived from PTY-output recency. By default it only watches agent sessions (`engine != "shell"`), matching how it's meant to be used. The event envelope is heru's `UnifiedEvent` schema; see spec.md section 19 for the event stream's design intent and [docs/pocketshell-integration-plan.md](docs/pocketshell-integration-plan.md) Part 2 for the full field-by-field mapping rationale. `src/watch.rs` has the implementation and its own reasoning for the poll interval, activity threshold, and startup-replay behavior.

**`a watch` never looks at what an agent is actually saying or doing** — it only sees host-level lifecycle (created/exited/oom/a running-vs-waiting heuristic). `a transcript` (below) is the complementary capability: parsing an agent's real conversation — messages, tool calls, tool results, usage — into the same `UnifiedEvent` envelope. Use `a watch` to know a session changed state; use `a transcript` to know what the agent actually said or did.

## Conversation events (`a transcript`)

PocketShell's conversation pane needs structured events from a live `a start` session, not a second headless invocation of the agent. `a transcript` locates the native JSONL the engine CLI already writes (`~/.claude/projects/<encoded-cwd>/<session>.jsonl`, `~/.codex/sessions/<Y>/<M>/<D>/<session>.jsonl`, `$GROK_HOME/sessions/<urlencoded-cwd>/<id>/updates.jsonl`), parses it, and emits heru `UnifiedEvent` JSONL (or a compact human rendering without `--json`).

How the log is captured and kept: aplexer does **not** copy conversation bytes into its own state. The engine's append-only JSONL is the source of truth (PTY `history.bin` is a separate, raw terminal capture). The first successful locate writes a bind sidecar `<state>/sessions/<id>/transcript.json` so later pages and `--follow` hit the same file even if another session shares the cwd. If the bound file disappears, the next call re-runs the heuristic.

```bash
a transcript --tag review --last 5 --json            # last 5 parsed events
a transcript --tag review --kind message --last 3    # last 3 turns only
a transcript --tag review --before 12 --last 20 --json   # older page
a transcript --tag review --after 31 --follow --json     # live tail
a transcript --json --last 5                         # inside a session: uses $APLEXER_SESSION_ID (`a whoami`)
```

`--last` / `--before` / `--after` are the PocketShell pagination cursors (sequence is stable across calls as long as the native file is append-only). `--follow` is `tail -f` of parsed events. `--max-line-bytes N` replaces a huge native line with PocketShell's `@@PS_LINE_TRUNCATED@@` marker so one oversized tool result cannot balloon an SSH read.

Claude, Codex, and Grok native logs in this pass; `opencode`/`gemini`/`shell` are out of scope. Location is a cwd+mtime heuristic until the bind sidecar exists — see `src/agent_events.rs`.

## Python client

```bash
python3 -m pip install ./python
```

```python
from aplexer import Client

client = Client()
session = client.start(workspace=".", tag="demo", command=["/bin/bash", "-l"])
client.send(session.id, b"printf 'hello\\n'\n")
print(client.capture(session.id).decode(errors="replace"))
```

The Python package is intentionally thin: Rust remains authoritative for profile resolution, launch policy, PTY ownership, metadata durability, and cgroups.

## Inter-agent messaging

Sessions that share a workspace can hand off work, broadcast, or interrupt each other over `a message`, a durable per-workspace mailbox that needs no shared process (one JSON file per message under the state directory, atomic-write-and-rename, same discipline as session metadata):

```bash
a message send --to review "backend done, see api.md"
a message send --all "rebasing main in 5 minutes, hold your pushes"
a message send --pane --to review "stop: the API contract changed"   # injects into review's live PTY
a message inbox --new --json   # unread messages for the calling session
a message ack --all
a message log --json           # the whole workspace conversation, in order
```

See [docs/inter-agent-messaging-design.md](docs/inter-agent-messaging-design.md) for the full design (addressing, delivery semantics, retention, envelope schema) and [`.claude/skills/aplexer-messaging`](.claude/skills/aplexer-messaging/SKILL.md) for the agent-facing usage guide. V1 is pull-based only: there's no event-stream push or deferred `--when-waiting` pane delivery yet.

## Daemonless vs. tmux

"Daemonless" here has a specific, narrow meaning: there is no single server process that owns every session's PTY. tmux's architecture is one `tmux(1)` server holding every pane, window, and session for that server in its own memory; kill that process (OOM, a crash, `kill -9`, a bug tripped by one unrelated pane) and every session it hosts dies with it. That's the literal failure mode this project exists to remove (spec.md section 1). Aplexer instead gives each session its own worker process, with one Unix socket, one PTY, and one optional cgroup each (spec.md section 5.1 diagrams the prohibited shared-daemon shape against the actual per-worker one). A session's worker can die without touching any other session.

This isn't just a design claim — it's covered by destructive integration tests that actually run the scenario rather than simulate it. `tests/oom_isolation.rs::three_sessions_oom_isolation` starts three memory-capped sessions, runs a real memory bomb inside one of them until the kernel OOM-kills it, and asserts the other two stayed responsive throughout. `tests/oom_isolation.rs::worker_kill_isolation` `SIGKILL`s one session's *worker process* directly and asserts the other two are unaffected (and that the killed session's own status correctly flips to "worker not alive" instead of silently reporting stale `running` state forever). Both isolation properties were also where the real bugs showed up while this was being built: `git log` has a commit fixing a cgroup-delegation bug that made every memory-limited session fail closed with `EACCES` (found by actually running the OOM scenario), and a separate commit fixing a worker that never exited after its workload finished — an immortal process, socket, and lock file per session ever started, which directly contradicted the "nothing lingers once a session is over" premise this design depends on. Both were caught by running the thing, not by reading the spec.

The costs are real too, not just theoretical:

- **No single process to attach to for "everything" observability.** With one tmux server you can strace it, profile it, or inspect its memory to see every session at once. With N independent workers there's no such vantage point — `a list` has to scan durable per-session state files (spec.md section 14, 32). Spec.md section 15 allows for an optional control/index process later for faster queries, but it's explicit that this process must stay a rebuildable cache, never a new shared failure domain — if it dies, workers and PTYs keep running. That control process doesn't exist yet.
- **Per-session overhead.** N sessions means N worker processes, N sockets, N lock files, N runtime directories, instead of one server process multiplexing N panes. Spec.md section 30 sets a qualitative target ("dozens of sessions should be normal") but this hasn't been benchmarked at that scale in this repo yet — the overhead is real, whether it matters in practice is untested.
- **No atomic global snapshot.** A tmux server can answer "what's the state of everything, right now" from one process's memory. Aplexer's `a list` is a scan of independently-updating files, so under churn it can observe a slightly stale or racy view rather than one consistent instant. Spec.md section 32 leans into this rather than hiding it: it asks the runtime to distinguish live/stale/exited/broken sessions explicitly instead of pretending a single scan is an authoritative snapshot.
- **No tmux ecosystem.** Fifteen years of panes, splits, copy mode, plugins, and session-restore tooling don't carry over. V1 explicitly excludes all of that (spec.md section 27's non-goals list) — a deliberate scope cut for now, not a permanent ceiling, but a real gap today if that's what you rely on.
- **No tmux compatibility.** The workspace/tag/engine/profile model (see Goals above) is not tmux's flat session-name model, and there's no tmux control-mode-compatible protocol. Muscle memory, scripts, and tooling built against tmux don't transfer.

In short: aplexer trades a single, simple, inspectable server for a swarm of small, individually-boring, individually-replaceable processes, on the bet that one session's failure staying contained is worth more than the convenience of one process to rule them all. See spec.md sections 5, 7, 15, and 29 for the full architecture and test rationale.

## Validation

```bash
./scripts/validate.sh
```

## Full design doc

See [spec.md](spec.md) (mirrored at [docs/SPEC.md](docs/SPEC.md)) for the complete architecture: identity model, session types, control protocol, machine API, event stream, and the PocketShell integration plan.

For using aplexer on a remote host over a slow or flaky link (PocketShell on cellular), see [docs/low-bandwidth-remote-access-design.md](docs/low-bandwidth-remote-access-design.md) — what SSH compression already solves for free, status-bar/replay frugality, and reconnect/resume semantics (planning doc, not yet implemented).

For switching between sessions without detaching (`Ctrl-b n`/`p`/`1-9`/`l` inside `a attach`, reusing the same numbering `a list` prints), see [docs/fast-session-switching-design.md](docs/fast-session-switching-design.md) — the in-process switch architecture, keybinding scheme, and failure handling (design doc, not yet implemented).

For scrolling through a session's recent output without blocking input (tmux copy-mode's job, minus the input freeze), see [docs/scrollback-design.md](docs/scrollback-design.md) — why the host terminal's native scrollback is the mechanism, the status-bar hygiene invariants that keep it clean, and why a custom in-band scrollback view is rejected for v1 (design doc; mostly verification work).

For correct, instant reattach — the worker maintaining a live tmux-style terminal-state model (via the `vt100` crate) and repainting the current screen instead of replaying raw byte history — see [docs/terminal-state-design.md](docs/terminal-state-design.md), which supersedes spec.md §17/§27's "no terminal emulator state" non-goal and fixes the reproduced reattach corruption of full-screen agent TUIs (design doc, not yet implemented).
