#!/usr/bin/env python3
"""Имитатор API Yandex Cloud (IAM, Compute, Operation, VPC) для сквозной проверки.

IAM проверяет JWT сервисного аккаунта по-настоящему: подпись PS256 открытым ключом, kid, iss, aud,
срок. Compute создаёт «ВМ» — сам хост: ключ из cloud-init (metadata.user-data) добавляется в
/root/.ssh/authorized_keys, адрес ВМ — внешний адрес хоста.

  yandex.py 18010 sa.pub calls.jsonl 10.0.0.5
"""
import base64, json, re, sys, threading, time, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding

PORT, PUB, LOG, VM_IP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
KID, SA = "ajeKEY0hopebackup", "ajeSA0hopebackup"
pub = serialization.load_pem_public_key(open(PUB, "rb").read())
lock = threading.Lock()
tokens, instances, ops = set(), {}, {}


def record(**kw):
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(kw, ensure_ascii=False) + "\n")


def b64d(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def err(self, code, msg):
        self.reply(code, {"code": code, "message": msg})

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(n)) if n else None

    def authed(self):
        a = self.headers.get("authorization", "")
        return a.startswith("Bearer ") and a[7:] in tokens

    def op(self, meta, response=None):
        oid = "op-" + uuid.uuid4().hex[:12]
        ops[oid] = {"id": oid, "done": False, "metadata": meta, "polls": 0, "response": response}
        return {"id": oid, "done": False, "metadata": meta}

    def do_POST(self):
        path = self.path.split("?")[0]
        b = self.body()
        record(method="POST", path=path)
        if path == "/iam/v1/tokens":
            jwt = (b or {}).get("jwt", "")
            try:
                h, p, s = jwt.split(".")
                hdr, pl = json.loads(b64d(h)), json.loads(b64d(p))
                pub.verify(b64d(s), f"{h}.{p}".encode(), padding.PSS(mgf=padding.MGF1(hashes.SHA256()), salt_length=32), hashes.SHA256())
                assert hdr["alg"] == "PS256" and hdr["kid"] == KID, "kid/alg"
                assert pl["iss"] == SA and pl["aud"] == "https://iam.api.cloud.yandex.net/iam/v1/tokens", "iss/aud"
                assert pl["iat"] <= time.time() + 60 < pl["exp"] + 60, "срок"
            except Exception as e:
                record(event="jwt-rejected", error=str(e))
                return self.err(401, f"invalid JWT: {e}")
            t = "t1." + uuid.uuid4().hex
            tokens.add(t)
            record(event="iam-token")
            return self.reply(200, {"iamToken": t, "expiresAt": "2099-01-01T00:00:00Z"})
        if not self.authed():
            return self.err(401, "The token is invalid")
        if path == "/compute/v1/instances":
            for k in ("folderId", "zoneId", "platformId", "resourcesSpec", "bootDiskSpec", "networkInterfaceSpecs"):
                if k not in b:
                    return self.err(400, f"{k} is required")
            if not re.match(r"^[a-z]([-a-z0-9]{0,61}[a-z0-9])?$", b.get("name", "")):
                return self.err(400, "Illegal argument name")
            rs, ds = b["resourcesSpec"], b["bootDiskSpec"]["diskSpec"]
            if int(rs["memory"]) % (1 << 20) or int(ds["size"]) < (10 << 30):
                return self.err(400, "memory/disk size")
            ud = b["metadata"]["user-data"]
            key = re.search(r"ssh_authorized_keys:\n\s+- (.+)\n", ud).group(1).strip()
            with lock:
                cur = open("/root/.ssh/authorized_keys").read()
                if key not in cur:
                    open("/root/.ssh/authorized_keys", "a").write(key + "\n")
            iid = "fhm" + uuid.uuid4().hex[:17]
            nat = "oneToOneNatSpec" in b["networkInterfaceSpecs"][0].get("primaryV4AddressSpec", {})
            instances[iid] = {"id": iid, "name": b["name"], "status": "RUNNING", "networkInterfaces": [{"index": "0", "primaryV4Address": {"address": VM_IP, **({"oneToOneNat": {"address": VM_IP, "ipVersion": "IPV4"}} if nat else {})}}]}
            record(event="cloud-init", instance=iid)
            return self.reply(200, self.op({"instanceId": iid}, {"id": iid}))
        return self.err(404, "not found")

    def do_GET(self):
        path = self.path.split("?")[0]
        record(method="GET", path=path)
        if not self.authed():
            return self.err(401, "The token is invalid")
        m = re.match(r"^/operations/(op-\w+)$", path)
        if m:
            o = ops.get(m.group(1))
            if not o:
                return self.err(404, "operation not found")
            o["polls"] += 1
            done = o["polls"] >= 2
            r = {"id": o["id"], "done": done, "metadata": o["metadata"]}
            if done and o["response"]:
                r["response"] = o["response"]
            return self.reply(200, r)
        if path == "/compute/v1/images:latestByFamily":
            return self.reply(200, {"id": "fd8hopebackupimg", "family": "ubuntu-2404-lts", "minDiskSize": str(10 << 30)})
        if path == "/compute/v1/instances":
            return self.reply(200, {"instances": list(instances.values())[:1]})
        m = re.match(r"^/compute/v1/instances/(\w+)$", path)
        if m:
            i = instances.get(m.group(1))
            return self.reply(200, i) if i else self.err(404, "instance not found")
        m = re.match(r"^/vpc/v1/subnets/([\w-]+)$", path)
        if m:
            return self.reply(200, {"id": m.group(1), "zoneId": "ru-central1-a", "v4CidrBlocks": ["10.128.0.0/24"]})
        return self.err(404, "not found")

    def do_DELETE(self):
        path = self.path.split("?")[0]
        record(method="DELETE", path=path)
        if not self.authed():
            return self.err(401, "The token is invalid")
        m = re.match(r"^/compute/v1/instances/(\w+)$", path)
        if m and instances.pop(m.group(1), None) is not None:
            return self.reply(200, self.op({"instanceId": m.group(1)}))
        return self.err(404, "instance not found")


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
