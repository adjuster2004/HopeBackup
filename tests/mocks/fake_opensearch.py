#!/usr/bin/env python3
"""Имитация Snapshot API OpenSearch: HTTPS, Basic, индексы в памяти (и в $OS_STATE для проверок),
репозиторий fs — настоящие файлы в каталоге (location должен быть внутри $OS_PATH_REPO).

  fake_opensearch.py <порт> <сертификат> <ключ>
"""
import base64, http.server, json, os, re, shutil, ssl, sys, urllib.parse

port, cert, key = int(sys.argv[1]), sys.argv[2], sys.argv[3]
ST = os.environ.get("OS_STATE", "/tmp/os_state.json")
PATH_REPO = os.environ.get("OS_PATH_REPO", "/var/backups")
AUTH = "Basic " + base64.b64encode(os.environ.get("OS_AUTH", "admin:admin").encode()).decode()


def load():
    return json.load(open(ST))


def save(s):
    json.dump(s, open(ST, "w"))


class H(http.server.BaseHTTPRequestHandler):
    def reply(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def err(self, code, reason):
        self.reply(code, {"error": {"root_cause": [{"reason": reason}], "reason": reason}, "status": code})

    def handle_any(self, method):
        if self.headers.get("Authorization") != AUTH:
            return self.err(401, "missing authentication credentials")
        u = urllib.parse.urlparse(self.path)
        p = u.path
        n = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}") if n else {}
        s = load()
        if p == "/" and method == "GET":
            return self.reply(200, {"cluster_name": "hb-test", "version": {"distribution": "opensearch", "number": "2.17.0"}})
        if p.startswith("/_cluster/health"):
            return self.reply(200, {"status": "green", "number_of_nodes": 1})
        m = re.match(r"^/_snapshot/([^/]+)$", p)
        if m and method == "PUT":
            loc = body["settings"]["location"]
            if not os.path.abspath(loc).startswith(os.path.abspath(PATH_REPO)):
                return self.err(500, f"[{m.group(1)}] location [{loc}] doesn't match any of the locations specified by path.repo")
            s["repos"][m.group(1)] = body; save(s)
            return self.reply(200, {"acknowledged": True})
        if m and method == "DELETE":
            s["repos"].pop(m.group(1), None); save(s)
            return self.reply(200, {"acknowledged": True})
        m = re.match(r"^/_snapshot/([^/]+)/_all$", p)
        if m:
            loc = s["repos"][m.group(1)]["settings"]["location"]
            snaps = sorted(f[5:-4] for f in os.listdir(loc) if f.startswith("snap-"))
            return self.reply(200, {"snapshots": [{"snapshot": x} for x in snaps]})
        m = re.match(r"^/_snapshot/([^/]+)/([^/]+)/_restore$", p)
        if m and method == "POST":
            loc = s["repos"][m.group(1)]["settings"]["location"]
            snap = json.load(open(os.path.join(loc, f"snap-{m.group(2)}.dat")))
            want = body["indices"].split(",")
            for i in want:
                docs = json.load(open(os.path.join(loc, "indices", snap["ids"][i], "data.json")))
                name = re.sub(body["rename_pattern"], body["rename_replacement"].replace("$1", r"\1"), i) if "rename_pattern" in body else i
                if name in s["indices"] and not s["indices"][name].get("closed"):
                    return self.err(500, f"cannot restore index [{name}] because an open index with same name already exists in the cluster")
                s["indices"][name] = {"docs": docs}
            save(s)
            return self.reply(200, {"snapshot": {"snapshot": m.group(2), "indices": want}})
        m = re.match(r"^/_snapshot/([^/]+)/([^/]+)$", p)
        if m and method == "PUT":
            repo, name = m.groups()
            loc = s["repos"][repo]["settings"]["location"]
            pats = body.get("indices", "*").split(",")
            sel = [i for i in s["indices"] if any(re.fullmatch(x.replace("*", ".*"), i) for x in pats if not x.startswith("-")) and not i.startswith(".")]
            ids = {}
            for i in sel:
                iid = "idx-" + str(abs(hash(i + json.dumps(s["indices"][i]["docs"]))))[:10]
                d = os.path.join(loc, "indices", iid)
                os.makedirs(d, exist_ok=True)
                json.dump(s["indices"][i]["docs"], open(os.path.join(d, "data.json"), "w"))
                ids[i] = iid
            json.dump({"ids": ids}, open(os.path.join(loc, f"snap-{name}.dat"), "w"))
            open(os.path.join(loc, "index-0"), "w").write(json.dumps({"snapshots": sorted(f for f in os.listdir(loc) if f.startswith("snap-"))}))
            return self.reply(200, {"snapshot": {"snapshot": name, "state": "SUCCESS", "indices": sel, "shards": {"total": len(sel), "successful": len(sel)}, "failures": []}})
        if m and method == "GET":
            loc = s["repos"][m.group(1)]["settings"]["location"]
            snap = json.load(open(os.path.join(loc, f"snap-{m.group(2)}.dat")))
            return self.reply(200, {"snapshots": [{"snapshot": m.group(2), "indices": list(snap["ids"])}]})
        if m and method == "DELETE":
            loc = s["repos"][m.group(1)]["settings"]["location"]
            os.remove(os.path.join(loc, f"snap-{m.group(2)}.dat"))
            return self.reply(200, {"acknowledged": True})
        m = re.match(r"^/([^/_][^/]*)/_settings$", p)
        if m:
            return self.reply(200, {}) if m.group(1) in s["indices"] else self.err(404, "no such index")
        m = re.match(r"^/([^/_][^/]*)/_close$", p)
        if m and method == "POST":
            for i in m.group(1).split(","):
                s["indices"][i]["closed"] = True
            save(s)
            return self.reply(200, {"acknowledged": True})
        return self.err(400, f"no handler for {method} {p}")

    def do_GET(self): self.handle_any("GET")
    def do_PUT(self): self.handle_any("PUT")
    def do_POST(self): self.handle_any("POST")
    def do_DELETE(self): self.handle_any("DELETE")
    def log_message(self, *x): pass


srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
