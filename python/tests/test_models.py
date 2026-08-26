from aplexer.models import Session

def test_session_model():
    session = Session.from_dict({
        "id": "00000000-0000-0000-0000-000000000000", "workspace": "/tmp/w", "tag": "x",
        "engine": "shell", "profile": None, "command": ["/bin/sh"], "cwd": "/tmp/w",
        "phase": "running", "socket_path": "/tmp/s", "history_path": "/tmp/h",
    })
    assert session.tag == "x"
    assert session.command == ("/bin/sh",)
