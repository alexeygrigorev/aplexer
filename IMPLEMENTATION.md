# Implementation map

This repository implements the v1 `aplexer` design as a Linux-first, daemonless process multiplexer.

## Runtime invariants

- Every session has an independent worker process and exactly one worker owns its PTY.
- Session discovery is durable and does not depend on a central daemon.
- Session records are versioned JSON and are replaced atomically after a file flush; the containing directory is flushed after rename.
- A workspace and tag form a unique human-readable selector; a UUID remains the canonical identity.
- Control sockets are per-session Unix-domain sockets with bounded, versioned framing.
- Workload input is transported as bytes, not shell-escaped text.
- PTY output is retained in a bounded history and can be captured after the workload exits.
- The worker remains available after workload exit so status, capture, and reattach remain deterministic.
- Memory limits fail closed when a delegated cgroup-v2 subtree cannot be created.
- Kill operations are serialized and prefer cgroup-wide termination, avoiding signals to stale process groups.

## User surfaces

The Rust command-line surface includes `start`, `list`/`snapshot`, `attach`, `send`, `capture`, `status`, `kill`, `rename`, `engines`, `profiles`, and `doctor`. A thin Python package mirrors the declarative client operations without taking ownership of launch policy, profile resolution, cgroup setup, or PTY lifecycle.

## Validation

Run:

```bash
./scripts/validate.sh
```

For a distributable source archive, run:

```bash
./scripts/package.sh
```

`VALIDATION.md` records the checks that were possible in the environment where this package was produced. In particular, an unavailable Rust toolchain is recorded as an unexecuted check rather than presented as a passing build.
