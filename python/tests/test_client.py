import fcntl
import json
import os
import signal
import tempfile
import time
from pathlib import Path

import pytest

from aplexer.client import AplexerError, Client


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
            fresh=False,
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

    with pytest.raises(AplexerError, match=rf"{variable} must be an absolute path"):
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


@pytest.mark.parametrize(
    ("native_method", "invoke"),
    [
        ("engines", lambda client: client.engines()),
        ("profiles", lambda client: client.profiles()),
        ("launch_spec", lambda client: client.launch_spec()),
        ("snapshot", lambda client: client.snapshot()),
        ("snapshot", lambda client: client.list()),
        ("status", lambda client: client.status("missing")),
        ("capture", lambda client: client.capture("missing")),
        ("start", lambda client: client.start()),
        ("send", lambda client: client.send("missing", b"data")),
        ("kill", lambda client: client.kill("missing")),
        ("forget", lambda client: client.forget("missing")),
    ],
)
def test_public_methods_translate_native_runtime_errors(
    monkeypatch, tmp_path, native_method, invoke
):
    class Native:
        def __getattr__(self, name):
            def fail(*args):
                raise RuntimeError(f"{name} native failure")

            return fail

    monkeypatch.setattr("aplexer.client._native", lambda: Native())
    client = Client(
        state_dir=tmp_path / "state",
        runtime_dir=tmp_path / "run",
        config=tmp_path / "config.toml",
    )

    with pytest.raises(AplexerError, match="native failure") as caught:
        invoke(client)

    assert isinstance(caught.value.__cause__, RuntimeError)
    assert native_method in str(caught.value)


@pytest.mark.parametrize(
    "invoke",
    [
        lambda client: client.status("definitely-missing-session"),
        lambda client: client.capture("definitely-missing-session"),
        lambda client: client.send("definitely-missing-session", b"data"),
        lambda client: client.kill("definitely-missing-session"),
        lambda client: client.forget("definitely-missing-session", force=True),
    ],
)
def test_native_missing_session_errors_use_aplexer_error(tmp_path, invoke):
    client = Client(
        state_dir=tmp_path / "state",
        runtime_dir=tmp_path / "run",
        config=tmp_path / "config.toml",
    )

    with pytest.raises(AplexerError, match="no matching session") as caught:
        invoke(client)

    assert isinstance(caught.value.__cause__, RuntimeError)


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

        # A long-lived workload, deliberately. A session now removes its own
        # record as soon as its workload exits, so a `sleep 0.2` here made the
        # isolation assertions below a race against that cleanup -- on a loaded
        # machine `first.list()` came back empty and the test failed for a
        # reason that has nothing to do with isolation. Kill explicitly instead,
        # and wait for the records to go, which pins the same "no files left
        # behind" property the short sleep was reaching for.
        command = ["/bin/sleep", "30"]
        first_session = first.start(workspace=root, tag="first", command=command)
        second_session = second.start(workspace=root, tag="second", command=command)

        assert {session.id for session in first.list()} == {first_session.id}
        assert {session.id for session in second.list()} == {second_session.id}

        first.kill(first_session.id, signal=9, grace_ms=0)
        second.kill(second_session.id, signal=9, grace_ms=0)
        _wait_until_gone(first, first_session.id)
        _wait_until_gone(second, second_session.id)
        assert first.list() == []
        assert second.list() == []


def _wait_until_gone(client, selector, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if all(item.id != selector for item in client.list()):
            return
        time.sleep(0.02)
    raise AssertionError(f"session {selector} did not disappear from the list")


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
            f"stty raw -echo; printf APX_READY; dd bs=1 count={len(payload)} 2>/dev/null; sleep 1",
        ]
        session = client.start(workspace=root, tag="bytes", command=command)

        with pytest.raises(AplexerError, match="force=True"):
            client.forget(session.id)
        with pytest.raises(AplexerError, match="live worker"):
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
        deadline = time.monotonic() + 5
        captured = b""
        while time.monotonic() < deadline:
            captured = client.capture(session.id)
            if captured.endswith(marker + payload):
                break
            time.sleep(0.02)
        else:
            raise AssertionError("captured bytes did not include the payload")
        assert captured.endswith(marker + payload)
        _wait_until_gone(client, session.id)
        with pytest.raises(AplexerError, match="no matching session"):
            client.forget(session.id, force=True)


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
        with pytest.raises(AplexerError, match="signal out of range"):
            client.kill(session.id, signal=0)
        with pytest.raises(AplexerError, match="kill grace exceeds maximum"):
            client.kill(session.id, grace_ms=30_001)
        assert client.kill(session.id, signal=15, grace_ms=200) is None
        _wait_until_gone(client, session.id)


def _pid_alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def test_native_forget_matches_cli_contract(capfd):
    """The Python binding is `a forget`: same gate, warning, and fence.

    `a forget` and `client.forget` used to be two independent destructive
    bodies (issue #11) and only the CLI one was covered. This pins the
    previously-untested path against the CLI's contract, so a mutation in the
    shared `api::forget_session` reddens both suites.
    """
    with tempfile.TemporaryDirectory(prefix="apx-py-forget-") as directory:
        root = Path(directory)
        state = root / "state"
        runtime = root / "run"
        client = Client(
            state_dir=state,
            runtime_dir=runtime,
            config=root / "config.toml",
        )

        with pytest.raises(AplexerError, match="no matching session"):
            client.forget("definitely-missing-session", force=True)

        session = client.start(
            workspace=root,
            tag="forget-cli",
            command=["/bin/sleep", "30"],
        )
        worker_pid = session.worker_pid
        assert worker_pid is not None
        try:
            with pytest.raises(AplexerError, match="force"):
                client.forget(session.id)
            with pytest.raises(AplexerError, match="live worker"):
                client.forget(session.id, force=True)

            # A worker killed without warning leaves the record behind: there
            # is no one left to remove it, which is exactly the wreckage
            # `forget` exists for.
            os.kill(worker_pid, signal.SIGKILL)
            deadline = time.monotonic() + 5
            while _pid_alive(worker_pid) and time.monotonic() < deadline:
                time.sleep(0.02)
            assert not _pid_alive(worker_pid), "worker did not die"

            record_path = state / "sessions" / session.id / "session.json"
            assert record_path.exists(), "SIGKILL removed the diagnostic record"

            # Rewind the record into the pre-PID startup window, the only
            # shape where `worker_alive()` is false but a worker may still be
            # coming up. That is what the worker-lock fence guards.
            record = json.loads(record_path.read_text())
            record["phase"] = "starting"
            record["worker_pid"] = None
            record["workload_pid"] = None
            record["exit"] = None
            record["error"] = None
            record["containment_empty"] = False
            record_path.write_text(json.dumps(record))

            runtime_session = runtime / "sessions" / session.id
            runtime_session.mkdir(parents=True, exist_ok=True)
            lock_path = runtime_session / "worker.lock"
            lock_fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
            try:
                fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with pytest.raises(AplexerError, match="still has a worker holding"):
                    client.forget(session.id, force=True)
            finally:
                fcntl.flock(lock_fd, fcntl.LOCK_UN)
                os.close(lock_fd)
                os.remove(lock_path)

            capfd.readouterr()
            forgotten = client.forget(session.id, force=True)
            err = capfd.readouterr().err
            assert forgotten.forgotten
            assert not forgotten.signalled
            assert not forgotten.containment_proven_empty
            assert forgotten.workload_may_survive
            # The CLI's warning, on the CLI's stream. Once the record is gone
            # this line is the only trace that workload processes may survive.
            assert "workload processes may survive" in err
            assert all(item.id != session.id for item in client.list())
        finally:
            if _pid_alive(worker_pid):
                os.kill(worker_pid, signal.SIGKILL)
