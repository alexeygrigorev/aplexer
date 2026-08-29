from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Iterable

from .models import Session


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
        self.state_dir = None if state_dir is None else os.fsdecode(state_dir)
        self.runtime_dir = None if runtime_dir is None else os.fsdecode(runtime_dir)
        self.config = None if config is None else os.fsdecode(config)

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
