import json
import os
import tempfile
import time
from pathlib import Path

import pytest

from aplexer.client import Client


def test_client_calls_native_not_subprocess(monkeypatch):
    calls = []

    class Native:
        @staticmethod
        def engines(state_dir=None, runtime_dir=None, config=None):
            calls.append(("engines", state_dir, runtime_dir, config))
            return '[{"name":"codex","command":["codex"],"available":true,"env_unset":[]}]'

        @staticmethod
        def profiles(state_dir=None, runtime_dir=None, config=None):
            calls.append(("profiles", state_dir, runtime_dir, config))
            return '{"zlaude":{"engine":"claude","env":{"CLAUDE_CONFIG_DIR":"/home/me/.zlaude"}}}'

        @staticmethod
        def launch_spec(
            engine,
            profile,
            cwd,
            no_skip,
            state_dir=None,
            runtime_dir=None,
            config=None,
        ):
            calls.append(
                (
                    "launch_spec",
                    engine,
                    profile,
                    cwd,
                    no_skip,
                    state_dir,
                    runtime_dir,
                    config,
                )
            )
            return '{"engine":"codex","argv":["codex"],"env_set":{},"env_unset":[]}'

        @staticmethod
        def snapshot(running=False, state_dir=None, runtime_dir=None, config=None):
            calls.append(("snapshot", running, state_dir, runtime_dir, config))
            return "[]"

    monkeypatch.setattr("aplexer.client._native", lambda: Native)
    client = Client()
    assert client.engines()[0]["name"] == "codex"
    assert "zlaude" in client.profiles()
    spec = client.launch_spec(engine="codex", cwd="/ws")
    assert spec["argv"] == ["codex"]
    assert client.snapshot() == []
    assert ("engines", None, None, None) in calls
    assert ("profiles", None, None, None) in calls


def test_client_paths_are_instance_local_including_start(monkeypatch, tmp_path):
    calls = []

    class Native:
        @staticmethod
        def snapshot(running, state_dir, runtime_dir, config):
            calls.append(("snapshot", state_dir, runtime_dir, config))
            return "[]"

        @staticmethod
        def start(
            workspace,
            tag,
            engine,
            profile,
            cwd,
            env,
            command,
            memory,
            pids,
            no_skip_permissions,
            python,
            startup_timeout_ms,
            state_dir,
            runtime_dir,
            config,
        ):
            calls.append(("start", state_dir, runtime_dir, config))
            return json.dumps(
                {
                    "id": "00000000-0000-0000-0000-000000000001",
                    "workspace": workspace,
                    "tag": tag,
                    "engine": engine or "shell",
                    "profile": profile,
                    "command": command,
                    "cwd": cwd or workspace,
                    "phase": "running",
                    "socket_path": f"{runtime_dir}/control.sock",
                    "history_path": f"{state_dir}/history.bin",
                }
            )

    monkeypatch.setattr("aplexer.client._native", lambda: Native)
    ambient = {
        "APLEXER_STATE_DIR": "ambient-state",
        "APLEXER_RUNTIME_DIR": "ambient-runtime",
        "APLEXER_CONFIG": "ambient-config",
    }
    for key, value in ambient.items():
        monkeypatch.setenv(key, value)

    first_paths = tuple(str(tmp_path / "first" / name) for name in ("state", "run", "config"))
    second_paths = tuple(str(tmp_path / "second" / name) for name in ("state", "run", "config"))
    first = Client(state_dir=first_paths[0], runtime_dir=first_paths[1], config=first_paths[2])
    second = Client(
        state_dir=second_paths[0], runtime_dir=second_paths[1], config=second_paths[2]
    )

    first.snapshot()
    second.snapshot()
    first.start(workspace=tmp_path, tag="first", command=["/bin/true"])
    second.start(workspace=tmp_path, tag="second", command=["/bin/true"])

    assert calls == [
        ("snapshot", *first_paths),
        ("snapshot", *second_paths),
        ("start", *first_paths),
        ("start", *second_paths),
    ]
    assert {key: os.environ[key] for key in ambient} == ambient


def test_client_resolves_relative_paths_once_before_chdir(monkeypatch, tmp_path):
    calls = []

    class Native:
        @staticmethod
        def snapshot(running, state_dir, runtime_dir, config):
            calls.append((state_dir, runtime_dir, config))
            return "[]"

    monkeypatch.setattr("aplexer.client._native", lambda: Native)
    original = tmp_path / "original"
    elsewhere = tmp_path / "elsewhere"
    original.mkdir()
    elsewhere.mkdir()
    monkeypatch.chdir(original)

    client = Client(
        state_dir="state",
        runtime_dir="runtime",
        config="config.toml",
    )
    expected = tuple(
        str(original / name) for name in ("state", "runtime", "config.toml")
    )
    assert client._path_args() == expected

    client.snapshot()
    monkeypatch.chdir(elsewhere)
    client.snapshot()
    assert calls == [expected, expected]


@pytest.mark.parametrize(
    ("variable", "value"),
    [
        ("XDG_RUNTIME_DIR", "relative-runtime"),
        ("XDG_STATE_HOME", "relative-state"),
        ("XDG_CONFIG_HOME", "relative-config"),
    ],
)
def test_native_client_rejects_relative_xdg_paths(
    monkeypatch, tmp_path, variable, value
):
    monkeypatch.delenv("APLEXER_RUNTIME_DIR", raising=False)
    monkeypatch.delenv("APLEXER_STATE_DIR", raising=False)
    monkeypatch.delenv("APLEXER_CONFIG", raising=False)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path / "runtime"))
    monkeypatch.setenv("XDG_STATE_HOME", str(tmp_path / "state"))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "config"))
    monkeypatch.setenv(variable, value)

    with pytest.raises(RuntimeError, match=rf"{variable} must be an absolute path"):
        Client().snapshot()


def test_operational_methods_use_native_boundary_and_preserve_bytes(monkeypatch, tmp_path):
    calls = []
    payload = b"\x00\xffA\r\n\x1b[31m"
    record = {
        "id": "00000000-0000-0000-0000-000000000001",
        "workspace": "/ws",
        "tag": "raw",
        "engine": "shell",
        "profile": None,
        "command": ["/bin/cat"],
        "cwd": "/ws",
        "phase": "running",
        "socket_path": "/run/control.sock",
        "history_path": "/state/history.bin",
        "worker_alive": True,
        "worker_reachable": True,
    }

    class Native:
        @staticmethod
        def status(selector, state_dir, runtime_dir, config):
            calls.append(("status", selector, state_dir, runtime_dir, config))
            return json.dumps(record)

        @staticmethod
        def capture(selector, max_bytes, state_dir, runtime_dir, config):
            calls.append(
                ("capture", selector, max_bytes, state_dir, runtime_dir, config)
            )
            return payload

        @staticmethod
        def send(selector, data, state_dir, runtime_dir, config):
            calls.append(("send", selector, data, state_dir, runtime_dir, config))
            return len(data)

        @staticmethod
        def kill(selector, signal, grace_ms, state_dir, runtime_dir, config):
            calls.append(
                (
                    "kill",
                    selector,
                    signal,
                    grace_ms,
                    state_dir,
                    runtime_dir,
                    config,
                )
            )

        @staticmethod
        def forget(selector, force, state_dir, runtime_dir, config):
            calls.append(("forget", selector, force, state_dir, runtime_dir, config))
            return json.dumps(
                {
                    "id": selector,
                    "forgotten": True,
                    "signalled": False,
                    "containment_proven_empty": True,
                    "workload_may_survive": False,
                }
            )

    monkeypatch.setattr("aplexer.client._native", lambda: Native)
    paths = tuple(str(tmp_path / name) for name in ("state", "run", "config"))
    client = Client(state_dir=paths[0], runtime_dir=paths[1], config=paths[2])
    selector = record["id"]

    assert client.status(selector).raw["worker_reachable"] is True
    assert client.capture(selector, max_bytes=123) is payload
    assert client.send(selector, payload) == len(payload)
    assert client.kill(selector, signal=9, grace_ms=0) is None
    forgotten = client.forget(selector, force=True)
    assert forgotten.forgotten
    assert forgotten.containment_proven_empty
    assert calls == [
        ("status", selector, *paths),
        ("capture", selector, 123, *paths),
        ("send", selector, payload, *paths),
        ("kill", selector, 9, 0, *paths),
        ("forget", selector, True, *paths),
    ]

    with pytest.raises(TypeError, match="data must be bytes"):
        client.send(selector, "not bytes")


def test_native_clients_isolate_worker_start_and_snapshot():
    # Keep the root short enough for Linux's 108-byte Unix-socket path limit.
    with tempfile.TemporaryDirectory(prefix="apx-py-") as directory:
        root = Path(directory)
        first_root = root / "first"
        second_root = root / "second"
        first = Client(
            state_dir=first_root / "state",
            runtime_dir=first_root / "run",
            config=first_root / "config.toml",
        )
        second = Client(
            state_dir=second_root / "state",
            runtime_dir=second_root / "run",
            config=second_root / "config.toml",
        )

        command = ["/bin/sleep", "0.2"]
        first_session = first.start(workspace=root, tag="first", command=command)
        second_session = second.start(workspace=root, tag="second", command=command)

        assert {session.id for session in first.list()} == {first_session.id}
        assert {session.id for session in second.list()} == {second_session.id}

        # Let both short-lived workers release their files before cleanup.
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            sessions = first.list() + second.list()
            if all(session.phase in {"exited", "failed"} for session in sessions):
                break
            time.sleep(0.02)
        else:
            raise AssertionError("short-lived isolation-test workers did not exit")
        time.sleep(0.1)


def _wait_for_terminal_status(client, selector, timeout=5):
    deadline = time.monotonic() + timeout
    latest = None
    while time.monotonic() < deadline:
        latest = client.status(selector)
        if latest.phase in {"exited", "failed"} and not latest.raw["worker_alive"]:
            return latest
        time.sleep(0.02)
    raise AssertionError(f"session did not become terminal: {latest!r}")


def test_native_operations_round_trip_arbitrary_bytes_and_forget():
    # Keep the root short enough for Linux's 108-byte Unix-socket path limit.
    with tempfile.TemporaryDirectory(prefix="apx-py-op-") as directory:
        root = Path(directory)
        client = Client(
            state_dir=root / "state",
            runtime_dir=root / "run",
            config=root / "config.toml",
        )
        payload = b"\x00\xffA\r\n\x1b[31mZ\x1b[0m"
        marker = b"APX_READY"
        command = [
            "/bin/sh",
            "-c",
            f"stty raw -echo; printf APX_READY; dd bs=1 count={len(payload)} 2>/dev/null",
        ]
        session = client.start(workspace=root, tag="bytes", command=command)

        with pytest.raises(RuntimeError, match="force=True"):
            client.forget(session.id)
        with pytest.raises(RuntimeError, match="live worker"):
            client.forget(session.id, force=True)

        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if client.capture(session.id).endswith(marker):
                break
            time.sleep(0.02)
        else:
            raise AssertionError("byte transport workload did not become ready")

        assert client.status(session.id).raw["worker_reachable"] is True
        assert client.send(session.id, payload) == len(payload)
        terminal = _wait_for_terminal_status(client, session.id)
        assert terminal.phase == "exited"
        assert client.capture(session.id).endswith(marker + payload)
        forgotten = client.forget(session.id, force=True)
        assert forgotten.forgotten
        assert not forgotten.signalled
        assert forgotten.containment_proven_empty
        assert not forgotten.workload_may_survive
        assert all(item.id != session.id for item in client.list())


def test_native_kill_stops_live_session():
    with tempfile.TemporaryDirectory(prefix="apx-py-kill-") as directory:
        root = Path(directory)
        client = Client(
            state_dir=root / "state",
            runtime_dir=root / "run",
            config=root / "config.toml",
        )
        session = client.start(
            workspace=root,
            tag="kill",
            command=["/bin/sleep", "10"],
        )
        with pytest.raises(RuntimeError, match="signal out of range"):
            client.kill(session.id, signal=0)
        with pytest.raises(RuntimeError, match="kill grace exceeds maximum"):
            client.kill(session.id, grace_ms=30_001)
        assert client.kill(session.id, signal=15, grace_ms=200) is None
        _wait_for_terminal_status(client, session.id)
        assert client.forget(session.id, force=True).forgotten
