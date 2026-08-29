import json
import os
import tempfile
import time
from pathlib import Path

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
