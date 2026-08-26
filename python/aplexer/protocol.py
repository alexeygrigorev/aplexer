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
_HEADER = struct.Struct(">4sB3xI")

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
    if len(payload) > MAX_FRAME_BYTES:
        raise ValueError("frame exceeds protocol limit")
    sock.sendall(_HEADER.pack(MAGIC, kind, len(payload)) + payload)

def recv_frame(sock: socket.socket) -> tuple[int, bytes]:
    magic, kind, length = _HEADER.unpack(recv_exact(sock, _HEADER.size))
    if magic != MAGIC:
        raise ValueError("invalid protocol magic")
    if kind not in (JSON, DATA, END):
        raise ValueError(f"unknown frame type {kind}")
    if length > MAX_FRAME_BYTES:
        raise ValueError("frame exceeds protocol limit")
    return kind, recv_exact(sock, length)

def send_json(sock: socket.socket, value: Any) -> None:
    send_frame(sock, JSON, json.dumps(value, separators=(",", ":")).encode())

def recv_json(sock: socket.socket) -> dict[str, Any]:
    kind, payload = recv_frame(sock)
    if kind != JSON:
        raise ValueError("expected JSON frame")
    value = json.loads(payload)
    if not isinstance(value, dict):
        raise ValueError("expected JSON object")
    return value
