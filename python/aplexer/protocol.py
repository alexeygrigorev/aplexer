from __future__ import annotations
import json
import socket
import struct
from typing import Any

MAGIC = b"APX1"
JSON = 1
DATA = 2
END = 3
MAX_FRAME_BYTES = 16 * 1024 * 1024
_FRAME_KINDS = frozenset((JSON, DATA, END))
_HEADER = struct.Struct(">4sB3sI")

def recv_exact(sock: socket.socket, count: int) -> bytes:
    chunks: list[bytes] = []
    remaining = count
    while remaining:
        chunk = sock.recv(remaining)
        if not chunk:
            raise EOFError("socket closed while reading a frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)

def send_frame(sock: socket.socket, kind: int, payload: bytes) -> None:
    if isinstance(kind, bool) or not isinstance(kind, int) or kind not in _FRAME_KINDS:
        raise ValueError(f"unknown frame type {kind}")
    if len(payload) > MAX_FRAME_BYTES:
        raise ValueError("frame exceeds protocol limit")
    sock.sendall(_HEADER.pack(MAGIC, kind, b"\0\0\0", len(payload)) + payload)

def recv_frame(sock: socket.socket) -> tuple[int, bytes]:
    magic, kind, flags, length = _HEADER.unpack(recv_exact(sock, _HEADER.size))
    if magic != MAGIC:
        raise ValueError("invalid protocol magic")
    if flags != b"\0\0\0":
        raise ValueError("unsupported frame flags")
    if kind not in _FRAME_KINDS:
        raise ValueError(f"unknown frame type {kind}")
    if length > MAX_FRAME_BYTES:
        raise ValueError("frame exceeds protocol limit")
    return kind, recv_exact(sock, length)

def send_json(sock: socket.socket, value: Any) -> None:
    payload = json.dumps(value, allow_nan=False, separators=(",", ":")).encode()
    send_frame(sock, JSON, payload)

def recv_json(sock: socket.socket) -> dict[str, Any]:
    kind, payload = recv_frame(sock)
    if kind != JSON:
        raise ValueError("expected JSON frame")
    value = json.loads(payload)
    if not isinstance(value, dict):
        raise ValueError("expected JSON object")
    return value
