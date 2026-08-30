from pathlib import Path

from aplexer.models import Session

def test_session_model():
    session = Session.from_dict({
        "id": "00000000-0000-0000-0000-000000000000", "workspace": "/tmp/w", "tag": "x",
        "engine": "shell", "profile": None, "command": ["/bin/sh"], "cwd": "/tmp/w",
        "phase": "running", "socket_path": "/tmp/s", "history_path": "/tmp/h",
    })
    assert session.tag == "x"
    assert session.command == ("/bin/sh",)


def test_session_containment_proof_fields_and_legacy_exit():
    base = {
        "id": "00000000-0000-0000-0000-000000000000",
        "workspace": "/tmp/w",
        "tag": "x",
        "engine": "shell",
        "profile": None,
        "command": ["/bin/sh"],
        "cwd": "/tmp/w",
        "phase": "running",
        "socket_path": "/tmp/s",
        "history_path": "/tmp/h",
    }
    current = Session.from_dict(
        {
            **base,
            "containment_cgroup": "/sys/fs/cgroup/aplexer-workload-test.scope",
            "containment_empty": True,
        }
    )
    assert current.containment_cgroup == Path(
        "/sys/fs/cgroup/aplexer-workload-test.scope"
    )
    assert current.containment_empty

    legacy = Session.from_dict(
        {
            **base,
            "phase": "exited",
            "exit": {
                "code": 0,
                "signal": None,
                "oom_killed": False,
                "exited_at_ms": 1,
            },
        }
    )
    assert legacy.containment_empty

    explicit_failure = Session.from_dict(
        {
            **base,
            "phase": "failed",
            "containment_empty": False,
            "exit": {
                "code": None,
                "signal": None,
                "oom_killed": False,
                "exited_at_ms": 2,
            },
        }
    )
    assert not explicit_failure.containment_empty
