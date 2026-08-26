import socket
import threading
from aplexer.protocol import DATA, recv_frame, send_frame

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
        left.close(); right.close()
