from __future__ import annotations
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

@dataclass(frozen=True)
class ExitInfo:
    code: int | None
    signal: int | None
    oom_killed: bool
    exited_at_ms: int

@dataclass(frozen=True)
class Session:
    id: str
    workspace: Path
    tag: str
    engine: str
    profile: str | None
    command: tuple[str, ...]
    cwd: Path
    phase: str
    socket_path: Path
    history_path: Path
    worker_pid: int | None = None
    workload_pid: int | None = None
    containment_cgroup: Path | None = None
    containment_empty: bool = False
    exit: ExitInfo | None = None
    error: str | None = None
    raw: dict[str, Any] = field(default_factory=dict, repr=False, compare=False)

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> "Session":
        exit_value = value.get("exit")
        exit_info = ExitInfo(**exit_value) if exit_value else None
        return cls(
            id=str(value["id"]), workspace=Path(value["workspace"]), tag=str(value["tag"]),
            engine=str(value["engine"]), profile=value.get("profile"),
            command=tuple(str(v) for v in value.get("command", [])), cwd=Path(value["cwd"]),
            phase=str(value["phase"]), socket_path=Path(value["socket_path"]),
            history_path=Path(value["history_path"]), worker_pid=value.get("worker_pid"),
            workload_pid=value.get("workload_pid"),
            containment_cgroup=(
                Path(value["containment_cgroup"])
                if value.get("containment_cgroup")
                else None
            ),
            containment_empty=bool(value.get("containment_empty", bool(exit_info))),
            exit=exit_info, error=value.get("error"), raw=value,
        )
