#!/usr/bin/env python3
"""Sends a single QMP command to a running VM's QMP socket and prints the
reply. Used for network fault injection (set_link) since that has no bash-
friendly equivalent without a QMP client.

Usage: qmp.py <socket-path> <command-json>
Example: qmp.py work/run1/n4.qmp '{"execute":"set_link","arguments":{"name":"net0","up":false}}'
"""
import json
import socket
import sys


def recv_json(sock):
    buf = b""
    while True:
        chunk = sock.recv(4096)
        if not chunk:
            break
        buf += chunk
        try:
            return json.loads(buf)
        except json.JSONDecodeError:
            continue
    return json.loads(buf)


def main():
    if len(sys.argv) != 3:
        print("usage: qmp.py <socket-path> <command-json>", file=sys.stderr)
        sys.exit(2)
    sock_path, command = sys.argv[1], sys.argv[2]

    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(sock_path)
    recv_json(s)  # greeting
    s.sendall(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
    recv_json(s)  # capabilities ack
    s.sendall(command.encode() + b"\n")
    reply = recv_json(s)
    print(json.dumps(reply))
    s.close()


if __name__ == "__main__":
    main()
