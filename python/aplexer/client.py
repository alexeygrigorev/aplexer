from __future__ import annotations

import json
import os
import sys
from pathlib import Path
import socket
import subprocess
import uuid
from typing import Any, Iterable

from .models import Session
from .protocol import DATA, MAX_FRAME_BYTES, recv_frame, recv_json, send_frame, send_json


class AplexerError(RuntimeError):
    pass


def _pid_alive(pid: int | None) -> bool:
    if not pid:
        return False
    return Path(f"/proc/{pid}").exists()


def resolve_cli() -> Path:
    """Locate the Rust ``a`` CLI next to this interpreter.

    Same rule as PocketShell's pinned ``quse``: console/runtime binaries
    shipped into a venv land beside ``sys.executable``. Never search PATH.
    """
    exe_dir = Path(sys.executable).parent
    candidates = [exe_dir]
    resolved_dir = Path(sys.executable).resolve().parent
    if resolved_dir != exe_dir:
        candidates.append(resolved_dir)
    for bin_dir in candidates:
        for name in ("a", "aplexer"):
            candidate = bin_dir / name
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return candidate
    raise AplexerError(
        "aplexer CLI is not installed next to this Python interpreter "
        f"({exe_dir / 'a'})"
    )


class Client:
    def __init__(
        self,
        *,
        cli: str | os.PathLike[str] | None = None,
        state_dir: str | os.PathLike[str] | None = None,
        runtime_dir: str | os.PathLike[str] | None = None,
    ) -> None:
        self.cli = os.fspath(cli) if cli is not None else os.fspath(resolve_cli())
        self.state_dir = Path(state_dir) if state_dir else self._default_state_dir()
        self.runtime_dir = Path(runtime_dir) if runtime_dir else self._default_runtime_dir()

    @staticmethod
    def _default_state_dir() -> Path:
        override = os.getenv("APLEXER_STATE_DIR")
        if override:
            return Path(override)
        return Path(os.getenv("XDG_STATE_HOME", Path.home() / ".local" / "state")) / "aplexer"

    @staticmethod
    def _default_runtime_dir() -> Path:
        override = os.getenv("APLEXER_RUNTIME_DIR")
        if override:
            return Path(override)
        xdg = os.getenv("XDG_RUNTIME_DIR")
        return Path(xdg) / "aplexer" if xdg else Path(f"/tmp/aplexer-{os.geteuid()}")

    def _cli_env(self) -> dict[str, str]:
        env = dict(os.environ)
        env["APLEXER_STATE_DIR"] = os.fspath(self.state_dir)
        env["APLEXER_RUNTIME_DIR"] = os.fspath(self.runtime_dir)
        return env

    def _run_json(self, args: list[str], *, timeout: float | None = None) -> Any:
        argv = [self.cli, "--json", *args]
        try:
            completed = subprocess.run(
                argv,
                check=False,
                capture_output=True,
                text=True,
                env=self._cli_env(),
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            raise AplexerError("aplexer CLI timed out") from exc
        except OSError as exc:
            raise AplexerError(str(exc)) from exc
        if completed.returncode:
            raise AplexerError(
                completed.stderr.strip() or f"aplexer CLI exited {completed.returncode}"
            )
        try:
            return json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            raise AplexerError("aplexer CLI returned non-JSON") from exc

    def list(self) -> list[Session]:
        sessions: list[Session] = []
        for record in (self.state_dir / "sessions").glob("*/session.json"):
            try:
                sessions.append(Session.from_dict(json.loads(record.read_text())))
            except (OSError, ValueError, KeyError, TypeError):
                continue
        return sorted(sessions, key=lambda s: int(s.raw.get("created_at_ms", 0)), reverse=True)

    def snapshot(self) -> list[dict[str, Any]]:
        """Session records plus ``worker_alive``, matching ``a snapshot --json``."""
        out: list[dict[str, Any]] = []
        for session in self.list():
            raw = dict(session.raw)
            raw["worker_alive"] = _pid_alive(session.worker_pid)
            out.append(raw)
        return out

    def resolve(
        self,
        selector: str | None = None,
        *,
        workspace: str | os.PathLike[str] | None = None,
        tag: str = "default",
    ) -> Session:
        sessions = self.list()
        if selector is not None:
            lowered = selector.lower()
            matches = [
                s
                for s in sessions
                if s.id == lowered
                or (len(lowered) >= 8 and s.id.startswith(lowered))
                or f"{s.workspace}:{s.tag}" == selector
            ]
        else:
            ws = Path(workspace or ".").resolve()
            matches = [s for s in sessions if s.workspace == ws and s.tag == tag]
        if not matches:
            raise AplexerError("no matching session")
        if len(matches) != 1:
            raise AplexerError("ambiguous session selector")
        return matches[0]

    def engines(self) -> list[dict[str, Any]]:
        payload = self._run_json(["engines"])
        if not isinstance(payload, list):
            raise AplexerError("engines payload was not a list")
        return payload

    def profiles(self) -> dict[str, Any]:
        payload = self._run_json(["profiles"])
        if not isinstance(payload, dict):
            raise AplexerError("profiles payload was not an object")
        return payload

    def launch_spec(
        self,
        *,
        engine: str | None = None,
        profile: str | None = None,
        cwd: str | os.PathLike[str] | None = None,
        no_skip_permissions: bool = False,
    ) -> dict[str, Any]:
        args = ["launch-spec"]
        if engine:
            args += ["--engine", engine]
        if profile:
            args += ["--profile", profile]
        if cwd is not None:
            args += ["--cwd", os.fspath(cwd)]
        if no_skip_permissions:
            args.append("--no-skip-permissions")
        payload = self._run_json(args)
        if not isinstance(payload, dict):
            raise AplexerError("launch-spec payload was not an object")
        return payload

    def start(
        self,
        *,
        workspace: str | os.PathLike[str] = ".",
        tag: str = "default",
        engine: str | None = None,
        profile: str | None = None,
        cwd: str | os.PathLike[str] | None = None,
        command: Iterable[str] = (),
        env: dict[str, str] | None = None,
        memory: str | None = None,
        pids: int | None = None,
        history_bytes: int | None = None,
        no_skip_permissions: bool = False,
    ) -> Session:
        argv = [
            self.cli,
            "--json",
            "start",
            "--workspace",
            os.fspath(workspace),
            "--tag",
            tag,
        ]
        if engine:
            argv += ["--engine", engine]
        if profile:
            argv += ["--profile", profile]
        if cwd is not None:
            argv += ["--cwd", os.fspath(cwd)]
        if memory:
            argv += ["--memory", memory]
        if pids is not None:
            argv += ["--pids", str(pids)]
        if history_bytes is not None:
            argv += ["--history-bytes", str(history_bytes)]
        if no_skip_permissions:
            argv.append("--no-skip-permissions")
        for key, value in (env or {}).items():
            argv += ["--env", f"{key}={value}"]
        command = list(command)
        if command:
            argv += ["--", *command]
        try:
            completed = subprocess.run(
                argv,
                check=False,
                capture_output=True,
                text=True,
                env=self._cli_env(),
            )
        except OSError as exc:
            raise AplexerError(str(exc)) from exc
        if completed.returncode:
            raise AplexerError(
                completed.stderr.strip() or f"aplexer start exited {completed.returncode}"
            )
        return Session.from_dict(json.loads(completed.stdout))

    def attach(self, selector: str) -> None:
        """Replace this process with the aplexer attach client."""
        os.execvpe(self.cli, [self.cli, "attach", selector], self._cli_env())

    def status(self, selector: str) -> Session:
        session = self.resolve(selector)
        return Session.from_dict(self._rpc(session, {"op": "status"}))

    def send(self, selector: str, data: bytes) -> int:
        session = self.resolve(selector)
        total = 0
        for offset in range(0, len(data), MAX_FRAME_BYTES):
            chunk = data[offset : offset + MAX_FRAME_BYTES]
            self._rpc(session, {"op": "send", "bytes": len(chunk)}, chunk)
            total += len(chunk)
        return total

    def capture(self, selector: str, *, max_bytes: int | None = None) -> bytes:
        session = self.resolve(selector)
        with self._connect(session) as sock:
            request_id = str(uuid.uuid4())
            send_json(
                sock,
                {"version": 1, "request_id": request_id, "op": "capture", "max_bytes": max_bytes},
            )
            response = recv_json(sock)
            self._check_response(response, request_id)
            kind, payload = recv_frame(sock)
            if kind != DATA:
                raise AplexerError("worker did not return capture bytes")
            return payload

    def kill(self, selector: str, *, signal: int = 15, grace_ms: int = 2000) -> None:
        session = self.resolve(selector)
        self._rpc(session, {"op": "kill", "signal": signal, "grace_ms": grace_ms})

    def rename(self, selector: str, *, workspace: str | os.PathLike[str], tag: str) -> Session:
        session = self.resolve(selector)
        result = self._rpc(
            session,
            {"op": "rename", "workspace": os.fspath(Path(workspace).resolve()), "tag": tag},
        )
        return Session.from_dict(result)

    def _connect(self, session: Session) -> socket.socket:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(os.fspath(session.socket_path))
        except Exception:
            sock.close()
            raise
        return sock

    def _rpc(
        self, session: Session, operation: dict[str, Any], data: bytes | None = None
    ) -> dict[str, Any]:
        with self._connect(session) as sock:
            request_id = str(uuid.uuid4())
            request = {"version": 1, "request_id": request_id, **operation}
            send_json(sock, request)
            if data is not None:
                send_frame(sock, DATA, data)
            response = recv_json(sock)
            result = self._check_response(response, request_id)
            if not isinstance(result, dict):
                raise AplexerError("worker returned a non-object result")
            return result

    @staticmethod
    def _check_response(response: dict[str, Any], request_id: str) -> Any:
        if response.get("request_id") != request_id:
            raise AplexerError("response request id mismatch")
        if not response.get("ok"):
            raise AplexerError(str(response.get("error") or "request failed"))
        return response.get("result")
