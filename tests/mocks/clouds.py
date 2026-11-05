#!/usr/bin/env python3
"""Имитаторы облачных дисков для tests/storage.sh.

  clouds.py yadisk PORT TOKEN   — REST API Яндекс Диска (cloud-api.yandex.net/v1/disk) + OAuth (device code)
  clouds.py gdrive PORT         — Drive API v3 (+ upload) + OAuth (device code, refresh token)

Данные — в памяти. Каждый 40-й запрос отвечает 429 (проверка повторов клиента).
"""
import json, re, sys, time, uuid, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs, quote, unquote

MODE, PORT = sys.argv[1], int(sys.argv[2])
TOKENS = set(sys.argv[3:4])
LOCK = threading.Lock()
COUNT = [0]
POLLS = {}

def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%S+00:00", time.gmtime())

# ---------------------------------------------------------------- Яндекс Диск
YA_FILES = {}   # путь -> (данные, время)
YA_DIRS = {"app:", "disk:"}
UPLOADS = {}

def ya_norm(p):
    p = p.rstrip("/")
    if p.endswith(":"):
        return p
    return p

def ya_parent(p):
    if ":/" not in p:
        return None
    pre, rest = p.split(":/", 1)
    if "/" not in rest:
        return pre + ":"
    return pre + ":/" + rest.rsplit("/", 1)[0]

def ya_name(p):
    return p.rsplit("/", 1)[-1]

# ---------------------------------------------------------------- Google Drive
G_FILES = {}    # id -> dict
SESS = {}

def g_new(meta, data=b""):
    fid = uuid.uuid4().hex[:20]
    G_FILES[fid] = {"id": fid, "name": meta.get("name", ""), "mimeType": meta.get("mimeType", "application/octet-stream"),
                    "parents": meta.get("parents", ["root"]), "data": data, "modifiedTime": now_iso()}
    return fid

def g_match(f, q):
    for m in re.finditer(r"'([^']*)' in parents", q):
        if m.group(1) not in f["parents"]:
            return False
    m = re.search(r"name = '((?:[^'\\]|\\.)*)'", q)
    if m and f["name"] != re.sub(r"\\(.)", r"\1", m.group(1)):
        return False
    m = re.search(r"mimeType = '([^']*)'", q)
    if m and f["mimeType"] != m.group(1):
        return False
    return True


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def send(self, code, body=b"", ctype="application/json", headers=None):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def form(self):
        return {k: v[0] for k, v in parse_qs(self.body().decode()).items()}

    def throttle(self):
        with LOCK:
            COUNT[0] += 1
            n = COUNT[0]
        if n % 40 == 0:
            self.body()
            self.send(429, {"error": "TooManyRequests"}, headers={"Retry-After": "0"})
            return True
        return False

    def ranged(self, data):
        r = self.headers.get("Range")
        if r:
            m = re.match(r"bytes=(\d+)-(\d+)", r)
            s, e = int(m.group(1)), int(m.group(2))
            part = data[s:e + 1]
            return self.send(206, part, "application/octet-stream", {"Content-Range": f"bytes {s}-{e}/{len(data)}"})
        self.send(200, data, "application/octet-stream")

    # ---------------- OAuth (общий для обоих)
    def oauth(self, path):
        f = self.form()
        if path == "/device/code":
            dc = uuid.uuid4().hex
            POLLS[dc] = 0
            return self.send(200, {"device_code": dc, "user_code": "HBTE-ST42", "verification_url": "https://example.test/device",
                                   "verification_uri": "https://example.test/device", "interval": 1, "expires_in": 300})
        if path == "/token":
            gt = f.get("grant_type", "")
            if gt in ("device_code", "urn:ietf:params:oauth:grant-type:device_code"):
                dc = f.get("device_code") or f.get("code")
                if dc not in POLLS:
                    return self.send(400, {"error": "invalid_grant"})
                POLLS[dc] += 1
                if POLLS[dc] < 2:
                    return self.send(400, {"error": "authorization_pending"})
                tok = "acc-" + uuid.uuid4().hex
                TOKENS.add(tok)
                return self.send(200, {"access_token": tok, "refresh_token": "refresh-" + MODE, "expires_in": 3600 if MODE == "gdrive" else 31536000})
            if gt == "refresh_token":
                if f.get("refresh_token") != "refresh-" + MODE:
                    return self.send(400, {"error": "invalid_grant", "error_description": "Token has been expired or revoked."})
                tok = "acc-" + uuid.uuid4().hex
                TOKENS.add(tok)
                return self.send(200, {"access_token": tok, "expires_in": 3600})
        self.send(404, {"error": "not found"})

    def authed(self):
        h = self.headers.get("Authorization", "")
        tok = h.split(" ", 1)[1] if " " in h else ""
        if tok not in TOKENS:
            self.body()
            self.send(401, {"error": "UnauthorizedError", "message": "Не авторизован."})
            return False
        return True

    def handle_any(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        if u.path in ("/device/code", "/token"):
            return self.oauth(u.path)
        if self.throttle():
            return
        if MODE == "yadisk":
            return self.yadisk(u.path, q)
        return self.gdrive(u.path, q)

    do_GET = do_PUT = do_POST = do_DELETE = do_PATCH = do_HEAD = handle_any

    # ---------------- Яндекс Диск
    def yadisk(self, path, q):
        if path.startswith("/upload/"):
            uid = path.split("/")[-1]
            p = UPLOADS.pop(uid, None)
            if p is None:
                return self.send(404, {"error": "no upload"})
            data = self.body()
            with LOCK:
                YA_FILES[p] = (data, now_iso())
            return self.send(201, b"")
        if path.startswith("/dl/"):
            p = unquote(path[4:])
            if p not in YA_FILES:
                return self.send(404, {"error": "DiskNotFoundError"})
            return self.ranged(YA_FILES[p][0])
        if not self.authed():
            return
        if path == "/v1/disk":
            used = sum(len(v[0]) for v in YA_FILES.values())
            return self.send(200, {"user": {"login": "hb-test"}, "total_space": 10 << 30, "used_space": used})
        p = ya_norm(q.get("path", ""))
        if path == "/v1/disk/resources":
            if self.command == "GET":
                if p in YA_DIRS:
                    kids = []
                    for d in sorted(YA_DIRS):
                        if ya_parent(d) == p:
                            kids.append({"name": ya_name(d), "type": "dir", "modified": now_iso()})
                    for f, (data, t) in sorted(YA_FILES.items()):
                        if ya_parent(f) == p:
                            kids.append({"name": ya_name(f), "type": "file", "size": len(data), "modified": t})
                    off, lim = int(q.get("offset", 0)), min(int(q.get("limit", 20)), 300)
                    return self.send(200, {"name": ya_name(p), "type": "dir", "_embedded": {"items": kids[off:off + lim], "total": len(kids), "offset": off, "limit": lim}})
                if p in YA_FILES:
                    return self.send(200, {"name": ya_name(p), "type": "file", "size": len(YA_FILES[p][0])})
                return self.send(404, {"error": "DiskNotFoundError"})
            if self.command == "PUT":
                if p in YA_DIRS:
                    return self.send(409, {"error": "DiskPathPointsToExistentDirectoryError"})
                if ya_parent(p) not in YA_DIRS:
                    return self.send(409, {"error": "DiskPathDoesntExistsError"})
                YA_DIRS.add(p)
                return self.send(201, {"href": "x"})
            if self.command == "DELETE":
                if p in YA_FILES:
                    del YA_FILES[p]
                    return self.send(204, b"")
                if p in YA_DIRS:
                    for f in [f for f in YA_FILES if f.startswith(p + "/")]:
                        del YA_FILES[f]
                    for d in [d for d in YA_DIRS if d == p or d.startswith(p + "/")]:
                        YA_DIRS.discard(d)
                    return self.send(202, {"href": "op"})
                return self.send(404, {"error": "DiskNotFoundError"})
        if path == "/v1/disk/resources/upload":
            if ya_parent(p) not in YA_DIRS:
                return self.send(409, {"error": "DiskPathDoesntExistsError"})
            if p in YA_FILES and q.get("overwrite") != "true":
                return self.send(409, {"error": "DiskResourceAlreadyExistsError"})
            uid = uuid.uuid4().hex
            UPLOADS[uid] = p
            return self.send(200, {"href": f"http://127.0.0.1:{PORT}/upload/{uid}", "method": "PUT"})
        if path == "/v1/disk/resources/download":
            if p not in YA_FILES:
                return self.send(404, {"error": "DiskNotFoundError"})
            return self.send(200, {"href": f"http://127.0.0.1:{PORT}/dl/{quote(p, safe='')}", "method": "GET"})
        self.send(404, {"error": "not found"})

    # ---------------- Google Drive
    def gdrive(self, path, q):
        if path.startswith("/resumable/"):
            sid = path.split("/")[-1]
            meta = SESS.pop(sid, None)
            if meta is None:
                return self.send(404, {"error": "no session"})
            data = self.body()
            with LOCK:
                fid = g_new(meta, data)
            return self.send(200, {"id": fid})
        if not self.authed():
            return
        if path == "/drive/v3/about":
            return self.send(200, {"user": {"emailAddress": "hb@test.example"}, "storageQuota": {"limit": str(15 << 30), "usage": str(sum(len(f["data"]) for f in G_FILES.values()))}})
        if path == "/drive/v3/files" and self.command == "GET":
            qq = q.get("q", "")
            items = [f for f in G_FILES.values() if g_match(f, qq)]
            off = int(q.get("pageToken") or 0)
            size = min(int(q.get("pageSize", 100)), 250)
            page = items[off:off + size]
            out = {"files": [{"id": f["id"], "name": f["name"], "mimeType": f["mimeType"], "size": str(len(f["data"])), "modifiedTime": f["modifiedTime"]} for f in page]}
            if off + size < len(items):
                out["nextPageToken"] = str(off + size)
            return self.send(200, out)
        if path == "/drive/v3/files" and self.command == "POST":
            meta = json.loads(self.body() or b"{}")
            with LOCK:
                fid = g_new(meta)
            return self.send(200, {"id": fid})
        if path == "/upload/drive/v3/files" and self.command == "POST":
            ut = q.get("uploadType")
            if ut == "resumable":
                meta = json.loads(self.body() or b"{}")
                sid = uuid.uuid4().hex
                SESS[sid] = meta
                return self.send(200, b"", headers={"Location": f"http://127.0.0.1:{PORT}/resumable/{sid}"})
            if ut == "multipart":
                ct = self.headers.get("Content-Type", "")
                b = ct.split("boundary=")[1].encode()
                raw = self.body()
                parts = raw.split(b"--" + b)
                meta = json.loads(parts[1].split(b"\r\n\r\n", 1)[1].rstrip(b"\r\n"))
                data = parts[2].split(b"\r\n\r\n", 1)[1]
                if data.endswith(b"\r\n"):
                    data = data[:-2]
                with LOCK:
                    fid = g_new(meta, data)
                return self.send(200, {"id": fid})
        m = re.match(r"^/upload/drive/v3/files/([^/]+)$", path)
        if m and self.command == "PATCH":
            f = G_FILES.get(m.group(1))
            if not f:
                return self.send(404, {"error": {"code": 404}})
            f["data"] = self.body()
            f["modifiedTime"] = now_iso()
            return self.send(200, {"id": f["id"]})
        m = re.match(r"^/drive/v3/files/([^/]+)$", path)
        if m:
            f = G_FILES.get(m.group(1))
            if not f:
                return self.send(404, {"error": {"code": 404, "message": "File not found"}})
            if self.command == "DELETE":
                with LOCK:
                    G_FILES.pop(f["id"], None)
                return self.send(204, b"")
            if q.get("alt") == "media":
                return self.ranged(f["data"])
            return self.send(200, {"id": f["id"], "name": f["name"]})
        self.send(404, {"error": "not found"})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
