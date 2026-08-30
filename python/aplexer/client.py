from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Iterable

from .models import ForgetResult, Session


class AplexerError(RuntimeError):
    pass


def _native():
    try:
        from aplexer import _native as native
    except ImportError as exc:
        raise AplexerError(
            "aplexer native bindings are not installed; build with "
            "`maturin develop --features python`"
        ) from exc
    return native


class Client:
    """In-process client: calls the Rust library through PyO3, never a subprocess."""

    def __init__(
        self,
        *,
        state_dir: str | os.PathLike[str] | None = None,
        runtime_dir: str | os.PathLike[str] | None = None,
        config: str | os.PathLike[str] | None = None,
    ) -> None:
        # Resolve explicit overrides once. A long-lived client must not silently
        # switch registries if its process changes working directory later.
        self.state_dir = self._absolute_override(state_dir)
        self.runtime_dir = self._absolute_override(runtime_dir)
        self.config = self._absolute_override(config)

    @staticmethod
    def _absolute_override(
        path: str | os.PathLike[str] | None,
    ) -> str | None:
        return None if path is None else os.path.abspath(os.fsdecode(path))

    def _path_args(self) -> tuple[str | None, str | None, str | None]:
        return self.state_dir, self.runtime_dir, self.config

    def engines(self) -> list[dict[str, Any]]:
        payload = json.loads(_native().engines(*self._path_args()))
        if not isinstance(payload, list):
            raise AplexerError("engines payload was not a list")
        return payload

    def profiles(self) -> dict[str, Any]:
        payload = json.loads(_native().profiles(*self._path_args()))
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
        payload = json.loads(
            _native().launch_spec(
                engine,
                profile,
                None if cwd is None else os.fspath(cwd),
                no_skip_permissions,
                *self._path_args(),
            )
        )
        if not isinstance(payload, dict):
            raise AplexerError("launch-spec payload was not an object")
        return payload

    def snapshot(self, *, running: bool = False) -> list[dict[str, Any]]:
        payload = json.loads(_native().snapshot(running, *self._path_args()))
        if not isinstance(payload, list):
            raise AplexerError("snapshot payload was not a list")
        return payload

    def list(self) -> list[Session]:
        return [Session.from_dict(row) for row in self.snapshot()]

    def status(self, selector: str) -> Session:
        """Return live status, or persisted status with reachability details."""
        payload = json.loads(_native().status(selector, *self._path_args()))
        if not isinstance(payload, dict):
            raise AplexerError("status payload was not an object")
        return Session.from_dict(payload)

    def capture(self, selector: str, *, max_bytes: int | None = None) -> bytes:
        """Return raw captured PTY history bytes without decoding or transcoding."""
        payload = _native().capture(selector, max_bytes, *self._path_args())
        if not isinstance(payload, bytes):
            raise AplexerError("capture payload was not bytes")
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
        no_skip_permissions: bool = False,
    ) -> Session:
        raw = json.loads(
            _native().start(
                os.fspath(workspace),
                tag,
                engine,
                profile,
                None if cwd is None else os.fspath(cwd),
                env,
                list(command),
                memory,
                pids,
                no_skip_permissions,
                sys.executable,
                10_000,
                *self._path_args(),
            )
        )
        return Session.from_dict(raw)

    def send(self, selector: str, data: bytes) -> int:
        """Send raw bytes to a session PTY and return the acknowledged count."""
        if not isinstance(data, bytes):
            raise TypeError("data must be bytes")
        sent = _native().send(selector, data, *self._path_args())
        if isinstance(sent, bool) or not isinstance(sent, int):
            raise AplexerError("send result was not an integer byte count")
        if sent != len(data):
            raise AplexerError(
                f"send byte count mismatch: supplied {len(data)}, acknowledged {sent}"
            )
        return sent

    def kill(self, selector: str, *, signal: int = 15, grace_ms: int = 2_000) -> None:
        """Signal a live session's complete workload containment domain."""
        _native().kill(selector, signal, grace_ms, *self._path_args())

    def forget(self, selector: str, *, force: bool = False) -> ForgetResult:
        """Delete a dead session's records without signalling any process."""
        payload = json.loads(_native().forget(selector, force, *self._path_args()))
        if not isinstance(payload, dict):
            raise AplexerError("forget payload was not an object")
        return ForgetResult.from_dict(payload)
