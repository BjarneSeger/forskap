#!/usr/bin/env python3
"""Unlock a running gnome-keyring-daemon the way pam_gnome_keyring does at
login: hand it the password over its control socket (GKD_CONTROL_OP_UNLOCK).

    pam-unlock.py $XDG_RUNTIME_DIR/keyring/control <password>

Only for the scratch keyring of the verify skill: it refuses a control socket
outside /tmp.
"""
import socket
import struct
import sys

control, password = sys.argv[1], sys.argv[2].encode()
if not control.startswith("/tmp/"):
    sys.exit(f"refusing: {control} is no scratch keyring")
OP_UNLOCK = 1
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(control)
s.sendall(b"\0")  # the credentials byte; the daemon reads SO_PEERCRED
body = struct.pack(">I", len(password)) + password
s.sendall(struct.pack(">II", 8 + len(body), OP_UNLOCK) + body)
reply = b""
while len(reply) < 8:
    chunk = s.recv(8 - len(reply))
    if not chunk:
        break
    reply += chunk
result = struct.unpack(">II", reply)[1] if len(reply) == 8 else -1
print(f"keyring control: result {result} (0 is unlocked)")
sys.exit(0 if result == 0 else 1)
