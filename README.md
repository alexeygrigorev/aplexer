# aplexer

`aplexer` is a Linux-first, daemonless PTY session runtime. Each session is owned by an independent worker process, so a crash or resource failure in one session cannot take down a shared multiplexer daemon. The short `a` binary is the user-facing CLI; `aplexer` is the worker executable.

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

The canonical identity printed by `start` is a UUID. Commands accept a full UUID, an unambiguous prefix, or `--workspace PATH --tag TAG`.

```bash
a send --workspace "$PWD" --tag shell --enter 'printf "hello\\n"'
a capture --workspace "$PWD" --tag shell
a status --workspace "$PWD" --tag shell
a kill --workspace "$PWD" --tag shell --signal TERM --grace-ms 2000
```

Output capture is byte-preserving. `send --stdin` and the Python API also transport bytes directly rather than asking a shell to reinterpret them.

## Engines and profiles

Configuration is TOML. Built-in engine entries are supplied for the login shell and common agent CLIs; user configuration overrides them.

```toml
version = 1
default_engine = "shell"

[engines.shell]
command = ["/bin/bash", "-l"]

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

Inspect effective discovery with `a engines`, `a profiles`, and `a doctor`.

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

See `VALIDATION.md` for the checks run when this source package was produced.
