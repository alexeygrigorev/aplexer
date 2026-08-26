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

## Validation

```bash
./scripts/validate.sh
```

## Full design doc

See [spec.md](spec.md) (mirrored at [docs/SPEC.md](docs/SPEC.md)) for the complete architecture: identity model, session types, control protocol, machine API, event stream, and the PocketShell integration plan.
