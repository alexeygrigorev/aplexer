from aplexer.client import Client


def test_client_calls_native_not_subprocess(monkeypatch):
    calls = []

    class Native:
        @staticmethod
        def engines():
            calls.append("engines")
            return '[{"name":"codex","command":["codex"],"available":true,"env_unset":[]}]'

        @staticmethod
        def profiles():
            calls.append("profiles")
            return '{"zlaude":{"engine":"claude","env":{"CLAUDE_CONFIG_DIR":"/home/me/.zlaude"}}}'

        @staticmethod
        def launch_spec(engine, profile, cwd, no_skip):
            calls.append(("launch_spec", engine, profile, cwd, no_skip))
            return '{"engine":"codex","argv":["codex"],"env_set":{},"env_unset":[]}'

        @staticmethod
        def snapshot(running=False):
            calls.append(("snapshot", running))
            return "[]"

    monkeypatch.setattr("aplexer.client._native", lambda: Native)
    client = Client()
    assert client.engines()[0]["name"] == "codex"
    assert "zlaude" in client.profiles()
    spec = client.launch_spec(engine="codex", cwd="/ws")
    assert spec["argv"] == ["codex"]
    assert client.snapshot() == []
    assert "engines" in calls
    assert "profiles" in calls
