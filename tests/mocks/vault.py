#!/usr/bin/env python3
"""Имитатор API Vault для проверки плагина: health, конфигурация raft, снапшот и восстановление.

Снапшот — детерминированные 6 МиБ; при snapshot-force имитатор сверяет SHA-256 полученного тела
с отданным и записывает итог в журнал.  vault.py 18200 TOKEN calls.jsonl
"""
import hashlib, json, random, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, TOKEN, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3]
rnd = random.Random(42)
SNAP = bytes(rnd.getrandbits(8) for _ in range(6 << 20))
SNAP_SHA = hashlib.sha256(SNAP).hexdigest()


def record(**kw):
    with open(LOG, "a") as f:
        f.write(json.dumps(kw) + "\n")


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def send(self, code, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authed(self):
        return self.headers.get("X-Vault-Token") == TOKEN

    def do_GET(self):
        if self.path == "/v1/sys/health":
            return self.send(200, {"initialized": True, "sealed": False, "standby": False, "version": "1.15.4", "cluster_name": "vault-cluster-hb"})
        if not self.authed():
            return self.send(403, {"errors": ["permission denied"]})
        if self.path == "/v1/sys/storage/raft/configuration":
            return self.send(200, {"data": {"config": {"servers": [{"node_id": "vault-1", "address": "10.0.0.11:8201", "leader": True, "voter": True}, {"node_id": "vault-2", "address": "10.0.0.12:8201", "leader": False, "voter": True}]}}})
        if self.path == "/v1/sys/storage/raft/snapshot":
            record(event="snapshot", sha=SNAP_SHA)
            return self.send(200, SNAP, "application/gzip")
        self.send(404, {"errors": []})

    def do_POST(self):
        if not self.authed():
            return self.send(403, {"errors": ["permission denied"]})
        if self.path == "/v1/sys/storage/raft/snapshot-force":
            h = hashlib.sha256()
            n = 0
            if self.headers.get("transfer-encoding", "").lower() == "chunked":
                while True:
                    size = int(self.rfile.readline().strip(), 16)
                    if size == 0:
                        self.rfile.readline()
                        break
                    chunk = self.rfile.read(size)
                    h.update(chunk)
                    n += len(chunk)
                    self.rfile.readline()
            else:
                rest = int(self.headers.get("content-length") or 0)
                while rest:
                    chunk = self.rfile.read(min(rest, 1 << 20))
                    h.update(chunk)
                    n += len(chunk)
                    rest -= len(chunk)
            ok = h.hexdigest() == SNAP_SHA
            record(event="restore", bytes=n, match=ok)
            return self.send(204 if ok else 400, b"" if ok else {"errors": ["snapshot checksum mismatch"]})
        self.send(404, {"errors": []})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
