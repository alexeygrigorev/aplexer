# Aplexer Specification

Status: initial architecture / v1 specification  
Project: **Aplexer**  
CLI executables: `aplexer`, short alias `a`  
Target platform: **Linux only**  
Primary implementation language: **Rust**  
CLI distribution: **`aplexer`** on PyPI; Python import package: **`aplexer`**,
provided by **`aplexer-client`**

## 1. Summary

Aplexer is an **agent-native persistent terminal multiplexer and runtime for Linux**.

Its primary use case is running multiple long-lived coding agents such as Claude Code, Codex, OpenCode, and Grok across many workspaces, while making them easy to discover, attach to, inspect, and control.

Aplexer is not intended to be a tmux-compatible replacement. It deliberately uses a different model centered on:

- workspaces rather than globally named sessions,
- agent engines and profiles as first-class concepts,
- one independent PTY/session worker per running session,
- one independent workload cgroup per session,
- no shared process whose death destroys multiple sessions,
- fast machine-readable lookup for clients such as PocketShell,
- a Rust runtime with a thin Python client package.

The motivating failure mode is a shared tmux server dying under memory pressure and taking unrelated sessions with it. Aplexer removes that shared failure domain by construction.

The core invariant is:

> **One session may fail, OOM, or be killed without causing unrelated sessions to disappear.**

A second important invariant is:

> **Agent identity, profile, workspace, and tag are authoritative runtime metadata, not values inferred later from terminal names, process trees, or tmux user options.**

---

## 2. Product model

Aplexer models a running terminal session with four user-facing dimensions:

```text
workspace + tag + engine + profile
```

Example:

```text
workspace: /home/alexey/git/pocketshell
tag:       review
engine:    codex
profile:   zai
```

A user may have several sessions in one workspace:

```text
~/git/pocketshell
├── main
│   └── claude / default
├── review
│   └── codex / zai
├── issue-2294
│   └── codex / go
└── shell
    └── bash
```

The meanings are intentionally separate:

- **workspace** — where the work belongs.
- **tag** — why this specific running session exists, or how the user distinguishes it from sibling sessions.
- **engine** — which agent/runtime is being launched, e.g. `claude`, `codex`, `opencode`, `grok`.
- **profile** — which configuration/account/provider variant of an engine is being used, e.g. `default`, `zai`, `go`.

A tag is not an agent type.

A profile is not a session name.

An engine is not a workspace.

This separation should remain visible in the internal schema.

---

## 3. Identity

### 3.1 Immutable internal ID

Every session receives an immutable internal ID at creation time.

Recommended representation: UUIDv7.

Example:

```text
019d4d1f-9f52-7f21-94ce-7cc175f4ab8d
```

The immutable ID is used for:

- runtime socket names,
- systemd/cgroup identities,
- metadata files,
- PTY worker identity,
- event-stream identity,
- references from clients,
- durable history.

User-facing metadata such as workspace and tag may change without changing the ID.

### 3.2 Human addressing

The primary human selector is:

```text
workspace + tag
```

The pair should be unique among live sessions.

Examples:

```text
/home/alexey/git/pocketshell + review
/home/alexey/git/pocketshell + main
/home/alexey/git/tmuxctl     + oom
```

From inside a workspace, the workspace may be implicit:

```bash
cd ~/git/pocketshell
a review
a main
```

An explicit selector syntax can be:

```text
<workspace>:<tag>
```

Examples:

```bash
a attach .:review
a attach ~/git/pocketshell:review
```

Exact parsing rules may be adjusted during CLI implementation, but the public model should remain workspace + tag.

### 3.3 Rename semantics

Changing a tag updates only metadata.

It must not rename:

- sockets,
- cgroups,
- systemd units,
- PTY ownership,
- process identity.

This avoids the class of lifecycle bugs caused by using mutable names as operating-system identities.

---

## 4. Session types

Aplexer is agent-native but not agent-only.

A session may be one of:

```text
agent
shell
process
```

### 4.1 Agent session

Example:

```text
kind:     agent
engine:   codex
profile:  zai
```

### 4.2 Shell session

Example:

```text
kind:     shell
command:  ["/bin/bash", "-l"]
```

### 4.3 Arbitrary process session

Example:

```text
kind:     process
command:  ["python", "train.py"]
```

Most higher-level UX should optimize for agent sessions while preserving the ability to run arbitrary PTY-backed processes.

---

## 5. Architecture

### 5.1 Principle: no shared PTY owner

Aplexer must not have one server process that owns all PTYs.

The prohibited architecture is:

```text
                shared daemon
                    │
        ┌───────────┼───────────┐
        ▼           ▼           ▼
     session A   session B   session C
```

where losing the shared daemon destroys all sessions.

Instead:

```text
                    CLI / PocketShell
                           │
              ┌────────────┼────────────┐
              │            │            │
              ▼            ▼            ▼
         Unix socket   Unix socket   Unix socket
              │            │            │
        session worker session worker session worker
              │            │            │
             PTY          PTY          PTY
              │            │            │
          workload A   workload B   workload C
```

Every session worker is an independent process.

Each worker owns exactly one persistent PTY in v1.

### 5.2 Session worker responsibilities

The per-session Rust worker owns:

- PTY master,
- Unix domain socket,
- child/workload lifecycle observation,
- terminal attach/detach,
- terminal resize,
- input forwarding,
- output forwarding,
- bounded output history,
- activity timestamps,
- client count,
- state reporting,
- event emission,
- OOM/exit status reporting.

The session worker must remain small and predictable.

It should not perform expensive agent workloads itself.

### 5.3 Workload separation

The session worker must not live inside the workload's memory-limited cgroup.

Conceptually:

```text
session worker
    │
    │ owns PTY
    │
    └── workload cgroup
         └── agent / shell
              └── descendants
```

This distinction is fundamental.

If a Codex session launches tests that exceed its memory limit, the workload may be killed while the Aplexer session worker remains available.

Other sessions are unaffected.

### 5.4 Failure properties

The architecture must satisfy all of the following:

```text
kill session B workload  -> A and C continue
OOM session B workload   -> A and C continue
kill session B worker    -> A and C continue
kill optional control    -> A, B, C continue
SSH client disconnects   -> session continues
terminal app exits       -> session continues
PocketShell disconnects  -> session continues
```

If a session's own worker dies, losing that session's PTY is acceptable in v1, provided unrelated sessions remain unaffected.

A future stronger design may split the worker into a tiny PTY keeper and a replaceable protocol/control worker, but that is not a v1 requirement.

---

## 6. PTY runtime

A session worker roughly performs:

```text
open PTY
fork/clone child
child:
    setsid()
    acquire controlling tty
    wire slave PTY to stdin/stdout/stderr
    chdir(workspace)
    enter workload cgroup
    exec command

parent:
    keep PTY master
    expose Unix socket
    proxy bytes
    track resize/client/activity/state
```

Linux-specific APIs are allowed and encouraged.

Candidate primitives:

- `openpty` / `posix_openpt`,
- `setsid`,
- `TIOCSCTTY`,
- `TIOCSWINSZ`,
- `epoll`,
- `pidfd_open`,
- `signalfd` or conventional signal handling,
- Unix domain sockets,
- cgroup v2.

The implementation should prefer boring, explicit systems code over a large async framework unless benchmarking shows a clear benefit.

Tokio is not required for v1.

---

## 7. Resource isolation and OOM behavior

### 7.1 Per-session workload cgroup

Each session workload receives an independent cgroup/systemd scope.

Example:

```text
aplexer-workload-<session-id>.scope
```

Possible controls:

```text
MemoryHigh=
MemoryMax=
MemorySwapMax=
```

Memory configuration may come from:

1. explicit session creation options,
2. workspace configuration,
3. user defaults.

### 7.2 Worker/control cgroup

Aplexer session workers must live outside the workload cgroup.

They should also live outside an aggregate capped workload slice.

### 7.3 Aggregate workload boundary

Per-session memory caps alone do not prevent many sessions from collectively exhausting host memory.

Aplexer should support an aggregate workload slice:

```text
user
├── aplexer-control.slice
│   ├── session-worker-A
│   ├── session-worker-B
│   └── optional-control-process
│
└── aplexer-workloads.slice
    ├── workload-A.scope
    ├── workload-B.scope
    └── workload-C.scope
```

The aggregate workload slice may itself have a safe host-level memory ceiling.

This keeps the control plane outside the workload blast radius.

### 7.4 OOM semantics

A workload OOM should be reported explicitly.

Example state:

```text
state: exited
reason: oom
```

or:

```text
state: oom
```

The exact enum may be chosen during implementation.

The worker should survive long enough to expose useful diagnostics and optionally launch a replacement shell or allow an explicit restart.

The runtime should prefer killing the workload as a group rather than leaving an agent in an unknown partially killed state.

---

## 8. Agent engines

Aplexer owns agent engine definitions.

This functionality should be extracted from PocketShell rather than duplicated.

An engine definition contains:

```text
id
family
label
launch argv
permission-bypass argv
environment set rules
environment unset rules
profile support
profile environment variable
profile discovery rules
availability detection
workspace preparation
process detection hints
optional usage-provider metadata
```

Built-in initial engines:

```text
claude
codex
opencode
grok
```

### 8.1 Engine IDs

Engine IDs are stable machine identifiers.

Examples:

```text
claude
codex
opencode
grok
```

Display labels are presentation only.

### 8.2 Launch policies

Agent-specific launch behavior belongs in Aplexer.

Examples include:

- Codex update-check suppression.
- Codex permission-bypass arguments.
- Claude permission-bypass arguments.
- Claude workspace-trust preparation.
- Grok approval arguments.
- provider-key environment stripping.
- profile-specific environment setup.

PocketShell should not reconstruct agent launch commands.

### 8.3 Declarative engine configuration

Aplexer should support declarative built-ins plus user additions/overrides.

Example shape:

```toml
[[engines]]
id = "codex"
family = "codex"
label = "Codex"

[engines.launch]
argv = ["codex", "-c", "check_for_update_on_startup=false"]
skip_permissions_argv = ["--dangerously-bypass-approvals-and-sandbox"]
profile_env = "CODEX_HOME"
```

The exact config format may be TOML, YAML, or another simple format. Prefer one format across Aplexer configuration.

TOML is preferred unless migration from existing PocketShell config makes YAML materially simpler.

---

## 9. Profiles

Profiles are engine-specific configuration variants.

Examples:

```text
codex/default
codex/zai
codex/go
claude/default
claude/zai
```

A profile has a stable ID and separate display label.

Example:

```text
id:         zai
engine:     codex
label:      Z.AI
config_dir: /home/alexey/.zodex
```

A running session records:

```text
engine:  codex
profile: zai
```

not:

```text
agent: "Codex (Z.AI)"
```

### 9.1 Profile responsibilities

Profiles may define:

- config directory,
- environment variables,
- label,
- whether they are default,
- agent-specific optional metadata.

Profiles must never expose secrets through listing APIs.

### 9.2 Auto-discovery

Aplexer should absorb PocketShell's existing profile discovery concepts.

For engines that use config directories, discovery may use:

- default directory,
- sibling directory naming hints,
- marker files,
- explicit configuration.

Examples:

```text
CODEX_HOME
CLAUDE_CONFIG_DIR
GROK_HOME
```

### 9.3 Explicit profile configuration

A user config file may augment or override discovered profiles.

Example:

```toml
[[profiles]]
id = "zai"
engine = "codex"
label = "Z.AI"
config_dir = "~/.zodex"

[[profiles]]
id = "go"
engine = "codex"
label = "Go"
config_dir = "~/.godex"
```

Display names must not be used as durable profile identity.

---

## 10. Workspace

Workspace is a first-class field, not an inferred property.

Creation explicitly records a canonical workspace path.

Example:

```text
/home/alexey/git/pocketshell
```

The initial command starts in that directory.

Aplexer should normalize workspace paths so equivalent spellings resolve consistently.

At minimum:

- resolve `~`,
- make the path absolute,
- eliminate trivial `.` / `..`,
- consider realpath/symlink policy explicitly.

Whether symlinks remain distinct workspaces or collapse to their real path must be specified before v1 stabilization.

Recommended default: canonicalize with `realpath` at creation time, while retaining the originally requested path as optional presentation metadata if useful.

---

## 11. Tag

Tag is a user-facing discriminator within a workspace.

Examples:

```text
main
review
tests
issue-2294
refactor-auth
2
3
```

The pair:

```text
(workspace, tag)
```

must be unique among live sessions.

Tags should not encode engine/profile identity.

A UI may choose to display:

```text
Codex · Z.AI
Claude
Claude 2
```

while the runtime still stores:

```text
tag=2
engine=claude
profile=default
```

Presentation is a client concern.

---

## 12. Session metadata

A v1 session record should contain at least:

```json
{
  "id": "019d4d1f-9f52-7f21-94ce-7cc175f4ab8d",
  "workspace": "/home/alexey/git/pocketshell",
  "tag": "review",
  "kind": "agent",
  "engine": "codex",
  "profile": "zai",
  "command": ["codex", "-c", "check_for_update_on_startup=false"],
  "created_at": 1787738302,
  "activity_at": 1787738821,
  "state": "running",
  "worker_pid": 12345,
  "workload_pid": 12352
}
```

Additional useful fields:

```text
attached_clients
foreground_pid
foreground_exe
observed_engine
containment_cgroup
containment_cgroup_identity
containment_empty
exit_code
exit_signal
exit_reason
oom_count
memory_current
memory_peak
memory_max
socket_path
```

Machine-visible schemas must be versioned.

---

## 13. Declared vs observed engine

A session may be created as an agent explicitly:

```text
declared_engine = codex
```

A generic shell may later manually launch an agent.

Aplexer may inspect the PTY foreground process group and `/proc` to infer:

```text
observed_engine = claude
```

These concepts should remain separate.

Example:

```text
kind:            shell
declared_engine: null
observed_engine: claude
```

Do not silently overwrite declared metadata based on heuristic process detection.

For sessions created through Aplexer's agent API, declared engine/profile is authoritative.

---

## 14. Runtime storage

Recommended layout:

```text
${XDG_STATE_HOME:-~/.local/state}/aplexer/
    sessions/
        <uuid>.json
    history/
        <uuid>.log

${XDG_RUNTIME_DIR}/aplexer/
    sessions/
        <uuid>.sock
```

Configuration:

```text
${XDG_CONFIG_HOME:-~/.config}/aplexer/
    config.toml
    engines.toml
    profiles.toml
```

Exact file splitting may change, but XDG locations should remain stable.

### 14.1 Metadata persistence

Metadata writes should use:

- unique temporary file,
- fsync file,
- atomic rename,
- fsync directory where practical,
- restrictive permissions.

The dataset is small.

Do not introduce SQLite until benchmarks or concurrency requirements justify it.

---

## 15. Control process

Aplexer may eventually include a central control/index/event process for speed.

It must never own session PTYs.

Safe architecture:

```text
                 optional aplexer control
                    registry / index
                    profiles / events
                           │
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
        worker A        worker B        worker C
           │               │               │
          PTY             PTY             PTY
```

If the control process dies:

```text
all session workers continue
all PTYs continue
all workloads continue
```

The control process may restart and reconstruct state by scanning durable metadata and runtime sockets.

### 15.1 V1 position

A global control daemon is not required for the first PTY milestone.

Start with direct metadata scanning and per-session sockets.

Add a control process only when profiling demonstrates a meaningful benefit for:

- repeated list queries,
- event fanout,
- PocketShell refresh latency,
- engine/profile discovery caching.

---

## 16. CLI

Both commands must be installed:

```text
aplexer
a
```

`a` is an exact short alias for the main CLI.

### 16.1 Creation

Examples:

```bash
aplexer start . --tag main --engine claude
a start . -t review -e codex -p zai
a start ~/git/pocketshell -t issue-2294 -e codex -p go
a start . -t shell -- bash -l
```

Possible convenience:

```bash
cd ~/git/pocketshell
a start main -e claude
```

Exact positional shortcuts can be refined after the explicit command form is stable.

### 16.2 Listing

Human:

```bash
a list
a list --workspace .
a list --engine codex
a list --profile zai
a list --state running
```

Machine-readable:

```bash
a list --json
a snapshot --json
```

Example human output:

```text
WORKSPACE      TAG         ENGINE   PROFILE   STATE     ACTIVITY
pocketshell    main        claude   default   running   2m
pocketshell    review      codex    zai       waiting   7m
pocketshell    issue-2294  codex    go        running   11m
tmuxctl        oom         claude   default   running   23m
```

### 16.3 Attach

```bash
a attach .:main
a attach ~/git/pocketshell:review
```

Possible convenience from current workspace:

```bash
a main
a review
```

### 16.4 Other operations

V1 target:

```bash
a list
a start
a attach
a kill
a rename
a send
a capture
a status
a engines
a profiles
a doctor
```

Later:

```bash
a watch
a restart
a logs
a usage
```

### 16.5 Send

Examples:

```bash
a send .:review "please continue"
```

`send` writes bytes into the session's PTY.

The API must support arbitrary bytes without shell interpolation.

### 16.6 Capture

Example:

```bash
a capture .:review
a capture .:review --bytes 500
a capture .:review --screen
a capture .:review --screen --plain
```

Raw capture returns a tail of the session's byte-preserving PTY history.
`--screen` instead returns the worker's rendered current-screen snapshot;
`--screen --plain` returns its plain-text contents.

JSON capture is lossless for arbitrary PTY bytes. Its payload shape is
`{"id":"...","bytes":N,"encoding":"base64","data":"..."}`. A redundant
`utf8` field is present only when the bytes are valid UTF-8; consumers must
decode `data` according to `encoding` for the byte-authoritative result.

---

## 17. Terminal history

The worker maintains and persists a byte-exact PTY history ring. Per-session
`history_bytes` is configurable from `0` through `16777216` bytes (16 MiB);
larger values are rejected before worker startup. Raw capture and post-mortem
capture are bounded by the same 16 MiB protocol-frame ceiling. Persisted
records from older versions may report a larger configured capacity and remain
available to list, status, capture, and forget operations, but a new worker
will not allocate a ring above the current ceiling.

The worker checkpoints dirty history every 500 ms. Each ordinary checkpoint
appends only the new bytes to both the raw compatibility view (`history.bin`)
and the active v2 data bank, syncs the data, then atomically publishes a small
checksummed commit record. Two data banks and two alternating commit slots keep
the prior valid generation recoverable if an append or metadata publication is
torn. A bank is compacted from the exact in-memory tail only after its payload
would exceed twice `history_bytes`, so full-ring writes are amortized by new
output rather than multiplied by the flush count.

V2 recovery validates fixed size bounds, session/store identity, generation
and slot bindings, and SHA-256 checksums before selecting the newest valid commit; a
trailing uncommitted suffix is ignored and a corrupt newest commit falls back
to the prior one. Once v2 metadata exists, readers never fall back to stale raw
history. Legacy raw-only files are seek-read from their bounded tail and
migrated when a worker opens them. The raw compatibility view is itself bounded
to twice `history_bytes` during a live session and is compacted to the exact
tail on clean exit. All history artifacts are opened without following links
or blocking on special files.

The worker also maintains terminal screen state for current-screen attach and
capture, including alternate-screen behavior. This screen model is separate
from the authoritative raw history bytes and is not a copy-mode interface.

---

## 18. Machine API

Aplexer must treat machine consumers as first-class clients.

Human tables are never the API contract.

At minimum:

```bash
a snapshot --json
```

returns a bare JSON array of session records in stable newest-first order
(`created_at_ms` descending). There is no enclosing object and no global
snapshot generation. The array is a registry scan of independently updated
session records, not an atomic point-in-time view across all workers.

Representative element (additional persisted fields may also be present):

```json
[
  {
    "schema_version": 1,
    "id": "7f3e8a82-4438-4fd5-bbb8-e3b0c66e7716",
    "workspace": "/home/alexey/git/pocketshell",
    "tag": "main",
    "engine": "claude",
    "command": ["claude"],
    "cwd": "/home/alexey/git/pocketshell",
    "history_bytes": 4194304,
    "created_at_ms": 1787738302000,
    "updated_at_ms": 1787738821000,
    "phase": "running",
    "worker_pid": 12345,
    "workload_pid": 12352,
    "socket_path": "/run/user/1000/aplexer/sessions/7f3e8a82-4438-4fd5-bbb8-e3b0c66e7716/control.sock",
    "history_path": "/home/alexey/.local/state/aplexer/sessions/7f3e8a82-4438-4fd5-bbb8-e3b0c66e7716/history.bin",
    "worker_alive": true
  }
]
```

The schema must support deterministic reverse lookups by:

- workspace,
- tag,
- engine,
- profile,
- state,
- ID.

---

## 19. Event stream

Aplexer should eventually expose:

```bash
a watch --jsonl
```

This emits host-originated events.

Examples:

```json
{"type":"session.created","id":"019d..."}
{"type":"session.activity","id":"019d...","at":1787739912}
{"type":"agent.state","id":"019d...","state":"waiting"}
{"type":"session.oom","id":"019d..."}
{"type":"session.exited","id":"019d...","reason":"signal"}
{"type":"session.deleted","id":"019d..."}
```

The event schema should be designed early even if event streaming lands after v1 attach/list.

Event streams carry their own sequence/generation where practical so clients
can detect gaps and fall back to a full snapshot. That stream-local cursor is
not a global generation attached to `a snapshot --json`.

---

## 20. Agent state

Aplexer should eventually represent agent state beyond process liveness.

Potential states:

```text
starting
running
waiting
idle
exited
oom
error
unknown
```

Agent-specific adapters may derive state from:

- foreground process,
- output patterns,
- agent logs,
- native agent state sources where available.

This should be extensible and must not be required for initial PTY persistence.

The core state contract should distinguish:

```text
session worker alive
workload alive
agent semantic state
```

These are different facts.

---

## 21. Python package

Aplexer will publish a Python package named:

```text
aplexer
```

This package is primarily for PocketShell and other programmatic consumers.

The Python implementation must be a thin typed client.

It must not duplicate:

- engine registry logic,
- profile discovery,
- workspace preparation,
- cgroup logic,
- PTY lifecycle.

Rust remains authoritative.

Example:

```python
from aplexer import Client

client = Client()

sessions = client.list()
profiles = client.profiles()

session = client.start(
    workspace="/home/alexey/git/pocketshell",
    tag="review",
    engine="codex",
    profile="zai",
)

status = client.status(session.id)
client.send(session.id, b"printf 'hello\\n'\n")
raw_output = client.capture(session.id, max_bytes=4096)
client.kill(session.id, signal=15, grace_ms=2000)
# After status reports that the worker is gone:
client.forget(session.id, force=True)
```

`send()` and `capture()` preserve arbitrary bytes without text decoding.
`forget()` is force-gated and removes only a dead session's records; it does
not signal a process.

### 21.1 Native transport

The `aplexer-client` distribution provides the `aplexer` import package as a
PyO3 extension plus a typed Python facade. It calls the Rust library in-process
and does not spawn or scrape the `a` executable. Rust owns configuration,
session resolution/startup, control-protocol framing, PTY byte transport,
lifecycle checks, and durable-state mutation.

The public Python API should be designed so the underlying transport can change without breaking callers.

---

## 22. PocketShell integration

PocketShell should become a client of Aplexer.

The long-term dependency direction is:

```text
PocketShell
    │
    ▼
Python aplexer package / machine protocol
    │
    ▼
Aplexer Rust runtime
```

PocketShell should stop owning:

- tmux session lifecycle,
- `tmuxctl` wrappers,
- agent engine registry,
- agent profile discovery,
- agent launch command construction,
- agent-specific environment preparation,
- tmux user-option agent identity,
- workspace inference from session names,
- agent identity inference required only because tmux lacks metadata.

Instead, PocketShell consumes:

```text
workspaces
sessions
engines
profiles
state
events
terminal attach
```

### 22.1 Folder/workspace UI

Aplexer's model maps naturally to PocketShell's folder workspace UI:

```text
root
└── workspace
    ├── main       Claude / default
    ├── review     Codex / Z.AI
    └── tests      Codex / Go
```

PocketShell should not need session-name prefix conventions to group these.

### 22.2 Fast reverse lookup

Required efficient queries include:

```text
all sessions in workspace X
all Codex sessions
all sessions using profile zai
session for workspace X + tag Y
all waiting agents
all OOM/exited agents
```

The runtime schema should support these directly.

### 22.3 SSH

PocketShell may continue to reach remote Aplexer hosts over SSH.

Initial integration can use SSH exec for snapshots and an SSH PTY/shell for terminal attachment.

Later, a long-lived `a watch --jsonl` SSH channel can replace repeated polling.

---

## 23. Engine/profile extraction from PocketShell

The first migration slice should extract concepts currently implemented in PocketShell:

```text
engine registry
profile discovery
agent launch preparation
agent-specific environment policy
permission bypass flags
workspace trust/update prompt suppression
```

Aplexer becomes authoritative.

PocketShell then consumes the Aplexer engine/profile API even before tmux is fully removed.

This enables incremental migration:

```text
Phase A:
    Aplexer owns engines/profiles
    tmux still hosts terminals

Phase B:
    Aplexer PTY runtime hosts selected sessions
    PocketShell supports both backends

Phase C:
    Aplexer becomes default
    tmux compatibility path removed or retained only as migration tooling
```

The exact migration policy can be simplified if hard cutover is preferable.

---

## 24. Installation and packaging

Target user experience:

```bash
uv tool install aplexer
pipx install aplexer
```

These tool installers expose `a` and `aplexer` from an isolated environment.
Applications that also import the client install the same distribution into
their Python environment:

```bash
python -m pip install aplexer
```

This should install:

```text
aplexer
a
```

and, within that installed environment, the Python module:

```python
import aplexer
```

The PyPI wheel/distribution must include or install the Rust binary.

The published `aplexer` CLI distribution contains the `a` and `aplexer`
executables and declares an exact-version dependency on `aplexer-client`,
which provides the `aplexer` import package. Releases publish the client
wheels before the dependent CLI wheels. Both distributions require Python
3.11 or newer and currently publish wheels only for Linux x86_64 and aarch64.

The CLI uses a wheel containing standalone Rust executables; the client uses
maturin to build the PyO3 extension. Together they provide the simple
`uv tool install aplexer` experience described above.

The bundled Rust executables do not embed Python. PyPI installations use thin
Python console-script launchers to locate and execute those binaries.

---

## 25. Configuration

The user configuration file is `~/.config/aplexer/config.toml` by default
(`APLEXER_CONFIG` overrides it). The schema is versioned and rejects unknown
fields and invalid engine/profile/shortcut references. For example:

```toml
version = 1
default_engine = "shell"

[profiles.review]
engine = "codex"
history_bytes = 4194304

[profiles.review.limits]
memory_bytes = 2147483648
pids = 256
```

There is currently no workspace-local configuration layer.

---

## 26. Security

Aplexer is a local per-user runtime.

Requirements:

- Unix sockets mode `0600`.
- Runtime/state directories mode `0700`.
- Metadata files must not contain secrets.
- Profile listing must expose paths/labels but not auth-file contents.
- Environment variables that may contain secrets must not be emitted in normal listing/snapshot APIs.
- PTY send APIs must pass bytes directly and not reconstruct shell commands.
- JSON/RPC interfaces must use bounded frame sizes and strict parsing.
- All machine-visible strings originating from configuration should be treated as untrusted input at protocol/UI boundaries.

---

## 27. V1 scope

V1 should intentionally remain smaller than tmux.

Required:

- Linux-only Rust runtime.
- One independent session worker per session.
- One PTY per session.
- Unix socket attach/detach.
- Workspace + tag addressing.
- Immutable internal ID.
- Agent engine + profile metadata.
- Basic built-in agent launch adapters.
- Per-session workload cgroup.
- OOM isolation.
- `list`.
- `start`.
- `attach`.
- `kill`.
- `rename`.
- `send`.
- `capture`.
- `status`.
- `engines`.
- `profiles`.
- machine-readable JSON snapshot.
- Python client package.
- `aplexer` and `a` executables.

Explicit non-goals for v1:

- tmux command compatibility,
- tmux config compatibility,
- panes,
- windows,
- split layouts,
- plugins,
- copy mode,
- full terminal emulator state,
- cross-host orchestration,
- web UI,
- scheduler,
- agent conversation storage,
- replacing SSH,
- central daemon as a required dependency.

A session in v1 is one persistent PTY.

---

## 28. V1 source layout

Proposed Rust repository shape:

```text
aplexer/
├── Cargo.toml
├── crates/
│   ├── aplexer-cli/
│   ├── aplexer-core/
│   ├── aplexer-worker/
│   ├── aplexer-protocol/
│   ├── aplexer-agents/
│   └── aplexer-linux/
├── python/
│   ├── pyproject.toml
│   └── aplexer/
│       ├── __init__.py
│       ├── client.py
│       ├── models.py
│       └── errors.py
├── tests/
├── spec.md
└── README.md
```

A single Rust crate is also acceptable initially if multiple crates slow development. Split only at meaningful boundaries.

Logical modules should include:

```text
cli
session
metadata
runtime
pty
attach
protocol
process
agents
profiles
resource/systemd
linux/pidfd
linux/cgroup
events
```

---

## 29. Testing philosophy

The test suite must encode the architectural reasons Aplexer exists.

### 29.1 Critical isolation test

```text
create A
create B
create C

run memory bomb in B

assert:
    B hits its memory boundary
    A still executes commands
    C still executes commands
    A remains attachable
    C remains attachable
    A metadata remains intact
    C metadata remains intact
```

### 29.2 Worker death isolation test

```text
create A
create B
create C

SIGKILL worker B

assert:
    A unaffected
    C unaffected
```

### 29.3 Client disconnect test

```text
create session
attach
run long command
disconnect client
reattach
assert command continued
```

### 29.4 SSH-equivalent transport loss test

Kill the attaching client process unexpectedly and verify the worker and workload remain alive.

### 29.5 Metadata lookup test

Create:

```text
workspace X, tag main, codex/default
workspace X, tag review, codex/zai
workspace Y, tag main, claude/default
```

Verify exact reverse lookups by:

```text
workspace
tag
engine
profile
workspace+tag
```

### 29.6 Rename test

Rename a tag and verify:

```text
same internal ID
same worker
same socket
same cgroup
new lookup key
old lookup key absent
```

### 29.7 Profile launch test

Verify engine/profile combinations resolve to correct:

```text
argv
config env variable
config directory
env set/unset policy
workspace preparation
```

---

## 30. Performance goals

The main PocketShell-facing operations should be cheap enough to poll interactively even before `watch` exists.

Target qualitative goals:

```text
list/snapshot:
    milliseconds on tens of sessions

workspace reverse lookup:
    effectively immediate

attach:
    no Python startup in the data path

session worker idle overhead:
    small enough that dozens of sessions are normal
```

Benchmarks should include:

- 10 sessions,
- 50 sessions,
- 200 sessions.

If scanning tiny JSON metadata files becomes measurable, introduce an optional index/control process while preserving direct recovery from durable state.

---

## 31. Observability

`a status` and `a doctor` should expose useful Linux/runtime information.

Examples:

```text
worker PID
workload PID
worker alive
workload alive
socket reachable
cgroup path
memory current
memory peak
memory high
memory max
OOM events
created time
last activity
attached clients
engine
profile
workspace
tag
```

Diagnostics must never require PocketShell to scrape `/proc` or systemd itself for ordinary session metadata.

---

## 32. Recovery

Aplexer should prefer reconstructable state over a fragile central database.

On CLI/control restart:

1. scan durable session metadata,
2. test worker sockets,
3. reconcile worker PIDs with pidfds or `/proc`,
4. reconcile workload state/cgroups,
5. classify sessions as live/stale/exited,
6. repair indexes/caches.

The runtime should distinguish:

```text
authoritatively empty
temporarily unavailable
stale metadata
live session
exited session
broken worker
```

Never treat an enumeration failure as “all sessions are gone”.

---

## 33. Possible later features

Not part of v1, but the architecture should not block:

- multiple PTYs per workspace/session group,
- split panes,
- terminal screen snapshots,
- copy mode,
- semantic agent waiting/running detection,
- agent usage/quota information,
- session restart after OOM,
- persistent command history,
- richer event subscriptions,
- remote host federation,
- PocketShell-native direct socket protocol,
- per-workspace policies,
- agent templates,
- session snapshots/checkpoints,
- PTY keeper separate from protocol worker.

---

## 34. Open decisions

These should be resolved during the first implementation milestone:

1. PTY library/API choice in Rust.
2. systemd transient scope vs direct delegated cgroup-v2 manipulation.
3. exact socket protocol and framing.
4. UUIDv7 implementation/library.
5. exact workspace canonicalization/symlink semantics.
6. raw output history representation.
7. whether session metadata is one JSON file per session or another simple durable store.
8. exact TOML schema for engines/profiles.
9. how the Python wheel bundles the Rust binary.
10. exact CLI selector grammar around `workspace:tag`.
11. whether an exited/OOM session remains addressable until explicitly removed.
12. whether a workload OOM automatically starts a shell, remains stopped, or requires explicit restart.
13. whether the first control process exists in v1 or only after benchmarking.
14. exact event-stream sequence/generation contract (snapshots deliberately
    have no global generation).

None of these should change the core failure-domain model.

---

## 35. Implementation order

### Milestone 0 — repository skeleton and contracts

- Rust workspace.
- Python package skeleton.
- CLI with `aplexer` + `a`.
- session metadata types.
- engine/profile types.
- JSON schema/types.
- test harness.

### Milestone 1 — persistent PTY

- worker process.
- PTY creation.
- Unix socket.
- start.
- attach.
- detach.
- resize.
- kill.
- list.
- activity timestamps.

Exit criterion:

```text
start shell -> detach -> reattach -> process never stops
```

### Milestone 2 — identity and workspace model

- immutable UUID.
- canonical workspace.
- tag uniqueness.
- rename.
- lookup by workspace/tag.
- JSON snapshot.
- send.
- capture.

Exit criterion:

```text
PocketShell-like client can enumerate and address sessions
without tmux naming conventions.
```

### Milestone 3 — OOM isolation

- workload cgroups.
- systemd/cgroup integration.
- per-session limits.
- aggregate workload slice.
- OOM detection.
- doctor/status memory information.
- destructive integration tests.

Exit criterion:

```text
OOM session B while A/C remain attachable and alive.
```

### Milestone 4 — engines and profiles

- migrate engine registry concepts from PocketShell.
- migrate profile discovery.
- built-in Claude/Codex/OpenCode/Grok.
- profile IDs.
- workspace preparation.
- permission flags.
- provider environment policy.
- `a engines`.
- `a profiles`.

Exit criterion:

```text
Aplexer alone knows how to launch codex/default,
codex/zai, codex/go, claude/default, etc.
```

### Milestone 5 — Python package and PocketShell bridge

- typed Python client.
- stable JSON/RPC protocol.
- PocketShell experimental backend.
- list sessions by workspace.
- list engines/profiles.
- create agent session.
- attach terminal.

Exit criterion:

```text
PocketShell can run a complete workspace+agent workflow
without tmuxctl metadata inference.
```

### Milestone 6 — events

- sequence/generation model.
- `a watch --jsonl`.
- create/delete/rename/activity/OOM events.
- PocketShell event-driven refresh.

Exit criterion:

```text
PocketShell no longer needs fast polling for session churn.
```

---

## 36. Core architectural commitments

The following should be treated as load-bearing decisions unless real implementation evidence forces a change:

1. **Linux only.**
2. **Rust owns the runtime.**
3. **Python is a thin client/package surface.**
4. **`aplexer` is the main CLI; `a` is an alias.**
5. **Workspace + tag is the human session identity.**
6. **Every session has an immutable internal ID.**
7. **Engine and profile are first-class metadata.**
8. **Aplexer, not PocketShell, owns engine/profile launch semantics.**
9. **One independent PTY worker per session in v1.**
10. **No shared process owns multiple session PTYs.**
11. **Workload cgroups are separate from session workers.**
12. **One session OOM must not destroy unrelated sessions.**
13. **A central control process may exist only as a rebuildable optimization.**
14. **Machine-readable schemas are first-class APIs; human tables are not parsed by clients.**
15. **PocketShell should consume authoritative workspace/agent metadata rather than infer it from tmux/process state.**

---

## 37. One-sentence definition

> **Aplexer is a Linux-native, agent-aware terminal multiplexer that runs each persistent session in its own isolated PTY/runtime and exposes first-class workspace, tag, engine, and profile metadata to humans and programmatic clients.**
