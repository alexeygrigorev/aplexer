# aplexer

`aplexer` is a Linux-first, **daemonless** persistent PTY session runtime. Every session is owned by its own independent worker process and cgroup, and exactly one worker owns its PTY — there is no shared multiplexer daemon whose crash or OOM kill takes down unrelated sessions with it. That's the failure mode aplexer exists to remove: with tmux, a single shared server dying under memory pressure can wipe out every session on the box. Aplexer's core invariant is that **one session may fail, OOM, or be killed without causing unrelated sessions to disappear.** The short `a` binary is the user-facing CLI; `aplexer` is the per-session worker executable.

## Goals

- **Standalone tmux / `tmuxctl` replacement.** Aplexer fills the same role as tmux (persistent, reattachable terminal sessions that survive your client disconnecting) but deliberately uses a different model — workspaces + tags + engines + profiles instead of flat global session names — described in [spec.md](spec.md) sections 1–2.
- **Backing runtime for two sibling client projects** under `~/git`:
  - [`../pocketshell`](../pocketshell) — a voice-first, tmux-native, agent-aware Android SSH client. It's meant to move off owning tmux session lifecycle / `tmuxctl` wrappers / agent engine discovery itself and instead become an aplexer client (see [spec.md](spec.md) section 22).
  - [`../pocketshell-desktop`](../pocketshell-desktop) — a terminal-first, agent-aware SSH client built as a **VS Code fork** (like Cursor or Windsurf) — explicitly *not* a standalone Electron app. It's the desktop companion to PocketShell Android and integrates with tmux today; aplexer is intended to be its session runtime as well.

## Build

```bash
cargo build --release --bins
install -m 0755 target/release/a target/release/aplexer ~/.local/bin/
```

Rust 1.78 or newer is recommended. Runtime state defaults to `$XDG_RUNTIME_DIR/aplexer`; durable records and bounded output history default to `$XDG_STATE_HOME/aplexer` (or `~/.local/state/aplexer`). Override these with `APLEXER_RUNTIME_DIR`, `APLEXER_STATE_DIR`, and `APLEXER_CONFIG`.

## First session

```bash
a start --workspace "$PWD" --tag shell -- /bin/bash -l
a list
a attach --workspace "$PWD" --tag shell
# Ctrl-] detaches without terminating the workload.
```

The canonical identity printed by `start` is a UUID. Commands accept a full UUID, an unambiguous prefix, or `--workspace PATH --tag TAG`. The full CLI surface is `start`, `list`/`snapshot`, `attach`, `send`, `capture`, `status`, `kill`, `rename`, `engines`, `profiles`, and `doctor`.

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
command = ["codex"]

[engines.claude]
command = ["claude"]

[engines.gemini]
command = ["gemini"]

[engines.grok]
command = ["grok"]
```

Override or add one in your config file the same way, e.g. to suppress codex's update check:

```toml
[engines.codex]
command = ["codex", "-c", "check_for_update_on_startup=false"]
```

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
