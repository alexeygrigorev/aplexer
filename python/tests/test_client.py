from pathlib import Path

from aplexer.client import AplexerError, Client, resolve_cli
from aplexer.models import Session


def test_resolve_cli_does_not_search_path(tmp_path, monkeypatch):
    fake = tmp_path / "bin" / "a"
    fake.parent.mkdir()
    fake.write_text("#!/bin/sh\n")
    fake.chmod(0o755)
    monkeypatch.setattr("aplexer.client.sys.executable", str(tmp_path / "bin" / "python"))
    monkeypatch.setenv("PATH", str(tmp_path / "other"))
    (tmp_path / "other").mkdir()
    path_hit = tmp_path / "other" / "a"
    path_hit.write_text("#!/bin/sh\n")
    path_hit.chmod(0o755)
    assert resolve_cli() == fake


def test_resolve_cli_errors_when_missing(tmp_path, monkeypatch):
    monkeypatch.setattr("aplexer.client.sys.executable", str(tmp_path / "python"))
    try:
        resolve_cli()
    except AplexerError as exc:
        assert "next to this Python interpreter" in str(exc)
    else:
        raise AssertionError("expected AplexerError")


def test_snapshot_reads_records_not_cli(tmp_path):
    session_dir = tmp_path / "sessions" / "abc"
    session_dir.mkdir(parents=True)
    (session_dir / "session.json").write_text(
        '{"id":"00000000-0000-0000-0000-000000000000","workspace":"/ws","tag":"t",'
        '"engine":"codex","profile":"zodex","command":["codex"],"cwd":"/ws",'
        '"phase":"running","socket_path":"/s","history_path":"/h",'
        '"created_at_ms":1,"worker_pid":null}',
        encoding="utf-8",
    )
    client = Client(cli=tmp_path / "missing-cli", state_dir=tmp_path, runtime_dir=tmp_path)
    rows = client.snapshot()
    assert len(rows) == 1
    assert rows[0]["tag"] == "t"
    assert rows[0]["worker_alive"] is False


def test_start_invokes_explicit_cli(tmp_path):
    script = tmp_path / "a"
    payload = tmp_path / "session.json"
    payload.write_text(
        '{"id":"00000000-0000-0000-0000-000000000001","workspace":"/ws","tag":"t",'
        '"engine":"shell","profile":null,"command":["sh"],"cwd":"/ws",'
        '"phase":"running","socket_path":"/s","history_path":"/h"}',
        encoding="utf-8",
    )
    script.write_text(f"#!/bin/sh\ncat {payload}\n", encoding="utf-8")
    script.chmod(0o755)
    client = Client(cli=script, state_dir=tmp_path, runtime_dir=tmp_path)
    session = client.start(workspace="/ws", tag="t", engine="shell")
    assert isinstance(session, Session)
    assert session.tag == "t"
    assert session.engine == "shell"
