import socket
import struct
import threading

import pytest

from aplexer.protocol import DATA, MAGIC, recv_frame, send_frame, send_json


def test_binary_frame_round_trip():
    left, right = socket.socketpair()
    try:
        payload = b"a\x00b\xff"
        thread = threading.Thread(target=send_frame, args=(left, DATA, payload))
        thread.start()
        kind, received = recv_frame(right)
        thread.join()
        assert kind == DATA
        assert received == payload
    finally:
        left.close()
        right.close()


def test_recv_frame_rejects_nonzero_reserved_flags():
    left, right = socket.socketpair()
    try:
        left.sendall(struct.pack(">4sB3sI", MAGIC, DATA, b"\0\1\0", 0))
        with pytest.raises(ValueError, match="unsupported frame flags"):
            recv_frame(right)
    finally:
        left.close()
        right.close()


@pytest.mark.parametrize("kind", [0, 4, True])
def test_send_frame_rejects_unknown_kind(kind):
    left, right = socket.socketpair()
    try:
        with pytest.raises(ValueError, match="unknown frame type"):
            send_frame(left, kind, b"")
    finally:
        left.close()
        right.close()


def test_send_json_rejects_non_finite_numbers():
    left, right = socket.socketpair()
    try:
        with pytest.raises(ValueError, match="Out of range float values"):
            send_json(left, {"value": float("nan")})
    finally:
        left.close()
        right.close()
