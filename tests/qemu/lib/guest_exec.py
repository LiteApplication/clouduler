#!/usr/bin/env python3
"""Runs one shell command inside a guest, over the exec listener a scenario's
probe script starts (see scenarios/container-network-probe.sh), and prints its
combined output. The listener is reached through the guest's forwarded
`expose_port`, the only way in from the host that is not the api.

usage: guest_exec.py <host> <port> <command>
Exit status is 0 when the transport worked, whatever the command returned
(the command's own status is printed as a final `rc=N` line).
"""
import socket
import sys


def main():
    host, port, cmd = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    s = socket.create_connection((host, port), timeout=10)
    s.settimeout(40)
    s.sendall(("exec 2>&1\n" + cmd + "\necho rc=$?\n").encode())
    s.shutdown(socket.SHUT_WR)
    out = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        pass
    sys.stdout.write(out.decode(errors="replace"))


if __name__ == "__main__":
    main()
