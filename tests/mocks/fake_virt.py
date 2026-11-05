#!/usr/bin/env python3
"""Имитация API платформ виртуализации для проверки плагина «Образ ВМ целиком».

  fake_virt.py <ovirt|openstack|nutanix|xen|vsphere|spacevm|vmmanager|opennebula> <порт> <сертификат> <ключ>

Состояние — $VIRT_STATE (JSON): {"vms": {id: {...}}, "disks": {id: {"file": путь, ...}}, "events": [...]}.
Диски — настоящие файлы: выгрузка отдаёт их содержимое, загрузка пишет новые файлы в $VIRT_DIR.
"""
import base64, html, http.server, json, os, re, ssl, sys, threading, urllib.parse, uuid

plat, port, cert, key = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
ST = os.environ["VIRT_STATE"]
DIR = os.environ.get("VIRT_DIR", "/tmp")
lock = threading.Lock()
BASE = f"https://127.0.0.1:{port}"


def load():
    return json.load(open(ST))


def save(s):
    json.dump(s, open(ST + ".tmp", "w")); os.replace(ST + ".tmp", ST)


def ev(s, e):
    s.setdefault("events", []).append(e)


def newfile():
    return os.path.join(DIR, "up-" + uuid.uuid4().hex[:8] + ".img")


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a): pass

    def body(self):
        # Тело читается один раз (до разбора): непрочитанное тело сломало бы следующий запрос соединения.
        if not hasattr(self, "_body"):
            n = int(self.headers.get("content-length") or 0)
            self._body = self.rfile.read(n) if n else b""
        return self._body

    def send(self, code, obj=None, raw=None, ctype="application/json", headers=None):
        b = raw if raw is not None else (json.dumps(obj).encode() if obj is not None else b"")
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(b)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(b)

    def sendfile(self, path):
        data = open(path, "rb").read()
        self.send(200, raw=data, ctype="application/octet-stream")

    def recvfile(self, path):
        data = self.body()
        open(path, "wb").write(data)
        return len(data)

    def do_GET(self): self.route("GET")
    def do_POST(self): self.route("POST")
    def do_PUT(self): self.route("PUT")
    def do_DELETE(self): self.route("DELETE")
    def do_PATCH(self): self.route("PATCH")
    def do_OPTIONS(self): self.route("OPTIONS")

    def route(self, m):
        if hasattr(self, "_body"):
            del self._body
        self.body()
        with lock:
            try:
                getattr(self, plat)(m, urllib.parse.urlparse(self.path))
            except Exception as e:  # noqa
                import traceback; traceback.print_exc()
                self.send(500, {"error": str(e)})

    # ------------------------------------------------------------ oVirt
    def ovirt(self, m, u):
        # imageio: доступ по билету в адресе передачи, без учётки Engine.
        if not u.path.startswith("/images/") and self.headers.get("Authorization") != "Basic " + base64.b64encode(b"admin@internal:Virt-Pass-1").decode():
            return self.send(401, {"detail": "auth"})
        s = load(); p = u.path.replace("/ovirt-engine/api", "")
        q = urllib.parse.parse_qs(u.query)
        if p.startswith("/images/"):
            t = s["transfers"][p.split("/")[2]]
            d = s["disks"][t["disk"]]
            if m == "GET":
                assert t["direction"] == "download"; return self.sendfile(d["file"])
            assert t["phase"] == "transferring", "запись в незавершённую или закрытую передачу"
            if m == "OPTIONS":
                return self.send(200, {"features": ["extents", "zero", "flush"], "max_readers": 8, "max_writers": 8})
            d["file"] = d.get("file") or newfile()
            if not os.path.exists(d["file"]):
                open(d["file"], "wb").truncate(int(d["provisioned_size"]))
            if m == "PATCH":
                b = json.loads(self.body())
                if b["op"] == "zero":
                    with open(d["file"], "r+b") as f:
                        f.seek(b["offset"]); left = b["size"]
                        while left:
                            k = min(left, 1 << 22); f.write(b"\0" * k); left -= k
                    t["zeroed"] = t.get("zeroed", 0) + b["size"]
                t.setdefault("ops", []).append(b["op"]); save(s); return self.send(200, {})
            cr = self.headers.get("content-range")
            if cr:
                a = int(re.match(r"bytes (\d+)-", cr).group(1))
                with open(d["file"], "r+b") as f:
                    f.seek(a); f.write(self.body())
                t["puts"] = t.get("puts", 0) + 1; save(s); return self.send(200, {})
            n = self.recvfile(d["file"]); save(s); return self.send(200, {"written": n})
        mm = re.fullmatch(r"/vms", p)
        if mm and m == "GET":
            vms = [dict(id=k, name=v["name"], status=v["status"]) for k, v in s["vms"].items()]
            if "search" in q:
                n = q["search"][0].split("=", 1)[1]; vms = [v for v in vms if v["name"] == n]
            return self.send(200, {"vm": vms})
        if mm and m == "POST":
            b = json.loads(self.body()); vid = "vm-" + uuid.uuid4().hex[:6]
            s["vms"][vid] = {"name": b["name"], "status": "down", "cluster": b["cluster"], "memory": b.get("memory"), "disks": [], "nics": [], "cpu_req": b.get("cpu"), "bios": b.get("bios")}
            ev(s, f"create vm {b['name']}"); save(s); return self.send(201, {"id": vid, "name": b["name"]})
        mm = re.fullmatch(r"/vms/([^/]+)", p)
        if mm:
            v = s["vms"][mm.group(1)]
            if m == "PUT":
                v["name"] = json.loads(self.body())["name"]; save(s); return self.send(200, {})
            return self.send(200, dict(v, id=mm.group(1), cpu={"topology": {"cores": 2}}, os={"type": "other_linux"}))
        mm = re.fullmatch(r"/vms/([^/]+)/diskattachments", p)
        if mm and m == "GET":
            v = s["vms"][mm.group(1)]
            return self.send(200, {"disk_attachment": [{"disk": dict(s["disks"][d], id=d), "interface": "virtio_scsi", "bootable": i == 0} for i, d in enumerate(v["disks"])]})
        if mm and m == "POST":
            b = json.loads(self.body()); s["vms"][mm.group(1)]["disks"].append(b["disk"]["id"]); save(s); return self.send(201, {})
        mm = re.fullmatch(r"/vms/([^/]+)/nics", p)
        if mm and m == "GET":
            return self.send(200, {"nic": s["vms"][mm.group(1)].get("nics", [])})
        if mm and m == "POST":
            n = json.loads(self.body()); n["id"] = "nic-" + uuid.uuid4().hex[:6]; n.setdefault("linked", True)
            s["vms"][mm.group(1)].setdefault("nics", []).append(n); save(s); return self.send(201, n)
        mm = re.fullmatch(r"/vms/([^/]+)/nics/([^/]+)", p)
        if mm and m == "PUT":
            for n in s["vms"][mm.group(1)]["nics"]:
                if n["id"] == mm.group(2): n.update(json.loads(self.body()))
            ev(s, f"nic linked={json.loads(self.body()).get('linked')}"); save(s); return self.send(200, {})
        # Снимки: копии файлов дисков; возврат — копирование обратно.
        mm = re.fullmatch(r"/vms/([^/]+)/snapshots", p)
        if mm and m == "POST":
            v = s["vms"][mm.group(1)]; sid = "sn-" + uuid.uuid4().hex[:6]
            files = {}
            for d in v["disks"]:
                f = s["disks"][d].get("file")
                if f:
                    open(f + "." + sid, "wb").write(open(f, "rb").read()); files[d] = f
            v.setdefault("snapshots", {})[sid] = {"description": json.loads(self.body())["description"], "files": files}
            ev(s, "snapshot " + v["snapshots"][sid]["description"]); save(s); return self.send(201, {"id": sid})
        mm = re.fullmatch(r"/vms/([^/]+)/snapshots/([^/]+)(/restore)?", p)
        if mm:
            v = s["vms"][mm.group(1)]; sn = v.get("snapshots", {}).get(mm.group(2))
            if sn is None:
                return self.send(404, {"detail": "no snapshot"})
            if mm.group(3):
                assert v["status"] == "down", "возврат к снимку работающей ВМ"
                for d, f in sn["files"].items():
                    open(f, "wb").write(open(f + "." + mm.group(2), "rb").read())
                ev(s, "snapshot restore"); save(s); return self.send(200, {})
            if m == "DELETE":
                for f in sn["files"].values():
                    os.remove(f + "." + mm.group(2))
                del v["snapshots"][mm.group(2)]; ev(s, "snapshot delete"); save(s); return self.send(200, {})
            return self.send(200, {"id": mm.group(2), "snapshot_status": "ok", "description": sn["description"]})
        mm = re.fullmatch(r"/vms/([^/]+)/(start|stop)", p)
        if mm:
            s["vms"][mm.group(1)]["status"] = "up" if mm.group(2) == "start" else "down"; save(s); return self.send(200, {})
        mm = re.fullmatch(r"/vms/([^/]+)/backups", p)
        if mm:
            bid = "bk-" + uuid.uuid4().hex[:6]; s.setdefault("backups", {})[bid] = {"phase": "starting", "vm": mm.group(1)}
            ev(s, "backup start"); save(s); return self.send(201, {"id": bid, "phase": "starting"})
        mm = re.fullmatch(r"/vms/([^/]+)/backups/([^/]+)(/finalize)?", p)
        if mm:
            b = s["backups"][mm.group(2)]
            if mm.group(3):
                b["phase"] = "finalizing"; ev(s, "backup finalize")
            elif b["phase"] == "starting":
                b["phase"] = "ready"
            save(s); return self.send(200, {"id": mm.group(2), "phase": b["phase"]})
        if p == "/imagetransfers" and m == "POST":
            b = json.loads(self.body()); tid = "tr-" + uuid.uuid4().hex[:6]
            if b["direction"] == "download":
                assert s["backups"][b["backup"]["id"]]["phase"] == "ready", "передача до готовности копии"
            s.setdefault("transfers", {})[tid] = {"disk": b["disk"]["id"], "direction": b["direction"], "phase": "initializing"}
            save(s); return self.send(201, {"id": tid, "phase": "initializing"})
        mm = re.fullmatch(r"/imagetransfers/([^/]+)(/finalize|/cancel)?", p)
        if mm:
            t = s["transfers"].get(mm.group(1))
            if t is None:
                return self.send(404, {"detail": "gone"})
            if mm.group(2) == "/cancel":
                t["phase"] = "cancelled_user"; ev(s, f"transfer {t['direction']} cancelled")
            elif mm.group(2):
                t["phase"] = "finished_success"; ev(s, f"transfer {t['direction']} done")
            elif t["phase"] == "initializing":
                t["phase"] = "transferring"
            save(s); return self.send(200, {"id": mm.group(1), "phase": t["phase"], "transfer_url": f"{BASE}/images/{mm.group(1)}"})
        if p == "/disks" and m == "POST":
            b = json.loads(self.body()); did = "dk-" + uuid.uuid4().hex[:6]
            s["disks"][did] = {"name": b["name"], "provisioned_size": b["provisioned_size"], "format": b["format"], "sparse": b.get("sparse"), "status": "locked", "storage_domains": b["storage_domains"]}
            save(s); return self.send(201, {"id": did})
        mm = re.fullmatch(r"/disks/([^/]+)", p)
        if mm:
            d = s["disks"][mm.group(1)]
            d["status"] = "ok"; save(s); return self.send(200, dict(d, id=mm.group(1)))
        if p == "/storagedomains":
            n = q.get("search", ["name=x"])[0].split("=", 1)[1]
            return self.send(200, {"storage_domain": [{"id": "sd-" + n, "name": n, "storage": {"type": "iscsi" if n.startswith("san") else "nfs"}}]})
        if p == "/vnicprofiles":
            return self.send(200, {"vnic_profile": [{"id": "vp-1", "name": "ovirtmgmt"}, {"id": "vp-2", "name": "dmz"}]})
        return self.send(404, {"detail": f"no {m} {p}"})

    # ------------------------------------------------------------ OpenStack
    def openstack(self, m, u):
        s = load(); p = u.path
        if p == "/v3/auth/tokens":
            b = json.loads(self.body())
            if b["auth"]["identity"]["password"]["user"]["password"] != "Virt-Pass-1":
                return self.send(401, {"error": "auth"})
            cat = [{"type": t, "endpoints": [{"interface": "public", "region": "RegionOne", "url": f"{BASE}/{n}"}]} for t, n in [("compute", "compute"), ("image", "image"), ("volumev3", "volume")]]
            return self.send(201, {"token": {"catalog": cat}}, headers={"X-Subject-Token": "tok-1"})
        if self.headers.get("X-Auth-Token") != "tok-1":
            return self.send(401, {"error": "token"})
        if p.startswith("/compute/servers/detail"):
            return self.send(200, {"servers": [dict(v, id=k) for k, v in s["vms"].items()]})
        mm = re.fullmatch(r"/compute/servers/([^/]+)", p)
        if mm and m == "GET":
            v = s["vms"][mm.group(1)]
            if v["status"] == "BUILD":
                v["status"] = "ACTIVE"; save(s)
            return self.send(200, {"server": dict(v, id=mm.group(1))})
        mm = re.fullmatch(r"/compute/servers/([^/]+)/action", p)
        if mm:
            b = json.loads(self.body()); v = s["vms"][mm.group(1)]
            if "createImage" in b:
                iid = "img-" + uuid.uuid4().hex[:6]
                s["images"][iid] = {"name": b["createImage"]["name"], "status": "queued", "disk_format": "qcow2", "file": s["disks"][v["root_disk"]]["file"]}
                ev(s, "createImage"); save(s); return self.send(200, {"image_id": iid})
            return self.send(202, {})
        if p == "/compute/servers" and m == "POST":
            b = json.loads(self.body())["server"]; sid = "srv-" + uuid.uuid4().hex[:6]
            s["vms"][sid] = {"name": b["name"], "status": "BUILD", "flavor": {"id": b["flavorRef"]}, "request": b}
            ev(s, f"create server {b['name']}"); save(s); return self.send(202, {"server": {"id": sid}})
        if p.startswith("/compute/flavors/detail"):
            return self.send(200, {"flavors": [{"id": "f1", "name": "m1.small"}, {"id": "f2", "name": "m1.large"}]})
        if p == "/image/v2/images" and m == "POST":
            b = json.loads(self.body()); iid = "img-" + uuid.uuid4().hex[:6]
            s["images"][iid] = {"name": b["name"], "status": "queued", "disk_format": b["disk_format"]}; save(s); return self.send(201, {"id": iid})
        mm = re.fullmatch(r"/image/v2/images/([^/]+)(/file)?", p)
        if mm:
            i = s["images"].get(mm.group(1))
            if m == "DELETE":
                s["images"].pop(mm.group(1), None); ev(s, "delete image"); save(s); return self.send(204)
            if mm.group(2) and m == "PUT":
                i["file"] = newfile(); self.recvfile(i["file"]); i["status"] = "active"; save(s); return self.send(204)
            if mm.group(2):
                return self.sendfile(i["file"])
            if i["status"] == "queued" and i.get("file"):
                i["status"] = "active"; save(s)
            return self.send(200, dict(i, id=mm.group(1)))
        mm = re.fullmatch(r"/volume/volumes/([^/]+)/action", p)
        if mm:
            b = json.loads(self.body()); vol = s["volumes"][mm.group(1)]
            assert b["os-volume_upload_image"]["force"] is True
            iid = "img-" + uuid.uuid4().hex[:6]
            s["images"][iid] = {"name": b["os-volume_upload_image"]["image_name"], "status": "queued", "disk_format": "raw", "file": vol["file"]}
            save(s); return self.send(202, {"os-volume_upload_image": {"image_id": iid}})
        if p == "/volume/volumes" and m == "POST":
            b = json.loads(self.body())["volume"]; vid = "vol-" + uuid.uuid4().hex[:6]
            s["volumes"][vid] = {"name": b["name"], "size": b["size"], "status": "creating", "file": s["images"][b["imageRef"]]["file"]}; save(s); return self.send(202, {"volume": {"id": vid}})
        mm = re.fullmatch(r"/volume/volumes/([^/]+)", p)
        if mm:
            v = s["volumes"][mm.group(1)]
            if v["status"] == "creating":
                v["status"] = "available"; save(s)
            return self.send(200, {"volume": dict(v, id=mm.group(1))})
        return self.send(404, {"error": f"no {m} {p}"})

    # ------------------------------------------------------------ Nutanix
    def nutanix(self, m, u):
        if self.headers.get("Authorization") != "Basic " + base64.b64encode(b"admin:Virt-Pass-1").decode():
            return self.send(401, {})
        s = load(); p = u.path.replace("/api/nutanix/v3", "")
        if p == "/vms/list":
            return self.send(200, {"entities": [{"metadata": {"uuid": k}, "status": {"name": v["spec"]["name"], "resources": {"power_state": "ON"}}} for k, v in s["vms"].items()]})
        mm = re.fullmatch(r"/vms/([^/]+)", p)
        if mm:
            return self.send(200, dict(s["vms"][mm.group(1)], metadata={"uuid": mm.group(1)}))
        if p == "/vms" and m == "POST":
            b = json.loads(self.body()); vid = "vm-" + uuid.uuid4().hex[:6]
            s["vms"][vid] = {"spec": b["spec"]}; ev(s, "create vm"); save(s)
            return self.send(202, {"metadata": {"uuid": vid}, "status": {"execution_context": {"task_uuid": "t-ok"}}})
        if p == "/images" and m == "POST":
            b = json.loads(self.body()); iid = "im-" + uuid.uuid4().hex[:6]
            src = b["spec"]["resources"].get("data_source_reference")
            s["images"][iid] = {"name": b["spec"]["name"], "file": s["disks"][src["uuid"]]["file"] if src else None}
            ev(s, "image " + ("from vm_disk" if src else "empty")); save(s)
            return self.send(202, {"metadata": {"uuid": iid}, "status": {"execution_context": {"task_uuid": "t-ok"}}})
        mm = re.fullmatch(r"/images/([^/]+)(/file)?", p)
        if mm:
            i = s["images"].get(mm.group(1))
            if m == "DELETE":
                s["images"].pop(mm.group(1)); ev(s, "delete image"); save(s); return self.send(202, {})
            if m == "PUT":
                i["file"] = newfile(); self.recvfile(i["file"]); save(s); return self.send(200, {})
            return self.sendfile(i["file"])
        if p.startswith("/tasks/"):
            return self.send(200, {"status": "SUCCEEDED"})
        return self.send(404, {})

    # ------------------------------------------------------------ XAPI
    def xen(self, m, u):
        s = load()
        qs = urllib.parse.parse_qs(u.query)
        if u.path == "/export":
            assert qs["session_id"][0] == "OpaqueRef:sess" and qs["use_compression"][0] == "false"
            snap = [k for k, v in s["snaps"].items() if v["uuid"] == qs["uuid"][0]][0]
            return self.sendfile(s["vms"][s["snaps"][snap]["of"]]["file"])
        if u.path == "/import":
            f = newfile(); self.recvfile(f)
            ref = "OpaqueRef:new-" + uuid.uuid4().hex[:4]
            s["vms"][ref] = {"name_label": "imported", "uuid": uuid.uuid4().hex, "power_state": "Halted", "file": f, "sr": qs["sr_id"][0]}
            s["tasks"][qs["task_id"][0]] = {"status": "success", "result": f"<value><array><data><value>{ref}</value></data></array></value>"}
            ev(s, "import"); save(s); return self.send(200, raw=b"")
        b = self.body().decode()
        meth = re.search(r"<methodName>(.*?)</methodName>", b).group(1)
        args = re.findall(r"<param><value>(?:<string>)?(.*?)(?:</string>)?</value></param>", b, re.S)
        ok = lambda v: self.send(200, raw=("<?xml version='1.0'?><methodResponse><params><param><value><struct><member><name>Status</name><value>Success</value></member><member><name>Value</name>" + v + "</member></struct></value></param></params></methodResponse>").encode(), ctype="text/xml")
        st = lambda x: f"<value>{x}</value>"
        if meth == "session.login_with_password":
            if args[1] != "Virt-Pass-1":
                return self.send(200, raw=b"<methodResponse><params><param><value><struct><member><name>Status</name><value>Failure</value></member><member><name>ErrorDescription</name><value><array><data><value>SESSION_AUTHENTICATION_FAILED</value></data></array></value></member></struct></value></param></params></methodResponse>", ctype="text/xml")
            return ok(st("OpaqueRef:sess"))
        if args and args[0] != "OpaqueRef:sess":
            return self.send(500, raw=b"no session")
        a = args[1:]
        if meth == "VM.get_all_records":
            mem = "".join(f"<member><name>{k}</name><value><struct><member><name>uuid</name><value>{v['uuid']}</value></member><member><name>name_label</name><value>{v['name_label']}</value></member><member><name>power_state</name><value>{v['power_state']}</value></member><member><name>is_a_template</name><value><boolean>0</boolean></value></member></struct></value></member>" for k, v in s["vms"].items())
            return ok(f"<value><struct>{mem}</struct></value>")
        if meth == "VM.get_record":
            v = s["vms"][a[0]]; return ok(f"<value><struct><member><name>name_label</name><value>{v['name_label']}</value></member></struct></value>")
        if meth in ("VM.snapshot", "VM.snapshot_with_quiesce"):
            if meth.endswith("quiesce") and not s.get("quiesce_ok"):
                return self.send(200, raw=b"<methodResponse><params><param><value><struct><member><name>Status</name><value>Failure</value></member><member><name>ErrorDescription</name><value><array><data><value>VM_SNAPSHOT_WITH_QUIESCE_NOT_SUPPORTED</value></data></array></value></member></struct></value></param></params></methodResponse>", ctype="text/xml")
            ref = "OpaqueRef:snap-" + uuid.uuid4().hex[:4]
            s["snaps"][ref] = {"of": a[0], "uuid": uuid.uuid4().hex, "vdis": ["OpaqueRef:vdi-" + ref[-4:]]}
            ev(s, "snapshot"); save(s); return ok(st(ref))
        if meth == "VM.get_uuid":
            return ok(st(s["snaps"][a[0]]["uuid"] if a[0] in s["snaps"] else s["vms"][a[0]]["uuid"]))
        if meth == "VM.get_VBDs":
            return ok("<value><array><data>" + "".join(f"<value>VBD:{x}</value>" for x in s["snaps"][a[0]]["vdis"]) + "</data></array></value>")
        if meth == "VBD.get_type":
            return ok(st("Disk"))
        if meth == "VBD.get_VDI":
            return ok(st(a[0][4:]))
        if meth == "VM.destroy":
            s["snaps"].pop(a[0]); ev(s, "destroy snapshot"); save(s); return ok("<value/>")
        if meth == "VDI.destroy":
            ev(s, "destroy vdi"); save(s); return ok("<value/>")
        if meth == "pool.get_all":
            return ok("<value><array><data><value>OpaqueRef:pool</value></data></array></value>")
        if meth == "pool.get_default_SR":
            return ok(st("OpaqueRef:sr-default"))
        if meth == "SR.get_by_name_label":
            return ok("<value><array><data><value>OpaqueRef:sr-" + a[0] + "</value></data></array></value>")
        if meth == "task.create":
            t = "OpaqueRef:task-" + uuid.uuid4().hex[:4]; s["tasks"][t] = {"status": "pending"}; save(s); return ok(st(t))
        if meth == "task.get_status":
            return ok(st(s["tasks"][a[0]]["status"]))
        if meth == "task.get_result":
            return ok(st(s["tasks"][a[0]]["result"].replace("<", "&lt;").replace(">", "&gt;")))
        if meth == "task.destroy":
            return ok("<value/>")
        if meth == "VM.set_name_label":
            s["vms"][a[0]]["name_label"] = a[1]; save(s); return ok("<value/>")
        if meth == "VM.start":
            s["vms"][a[0]]["power_state"] = "Running"; save(s); return ok("<value/>")
        return self.send(500, raw=("unknown " + meth).encode())

    # ------------------------------------------------------------ vSphere (SOAP)
    def vsphere(self, m, u):
        s = load()
        # Гостевые операции: файлы гостевой ОС — в состоянии.
        if u.path.startswith("/guest/"):
            path = urllib.parse.unquote(u.path.split("/", 3)[3])
            if m == "PUT":
                s.setdefault("guest_files", {})[path] = self.body().decode(); ev(s, "guest upload " + path); save(s); return self.send(200, raw=b"")
            return self.send(200, raw=s.get("guest_files", {}).get(path, "").encode(), ctype="application/octet-stream")
        if u.path.startswith("/nfc/"):
            lease = s["leases"][u.path.split("/")[2]]
            if m == "GET":
                return self.sendfile(s["disks"][u.path.split("/")[3]]["file"])
            f = newfile(); self.recvfile(f); lease.setdefault("uploaded", []).append(f); ev(s, "nfc upload"); save(s); return self.send(200, raw=b"")
        b = self.body().decode()
        meth = re.search(r"<soapenv:Body><(\w+)", b).group(1)
        this = (re.search(r'<_this type="\w+">(.*?)</_this>', b) or [None, None])[1]
        def ok(inner):
            x = f'<?xml version="1.0"?><soapenv:Envelope xmlns:soapenv="http://schemas.xmlsoap.org/soap/envelope/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><soapenv:Body><{meth}Response xmlns="urn:vim25">{inner}</{meth}Response></soapenv:Body></soapenv:Envelope>'
            return self.send(200, raw=x.encode(), ctype="text/xml", headers={"Set-Cookie": 'vmware_soap_session="sess1"; Path=/'})
        def fault(msg):
            return self.send(500, raw=f'<soapenv:Envelope xmlns:soapenv="x"><soapenv:Body><soapenv:Fault><faultstring>{msg}</faultstring></soapenv:Fault></soapenv:Body></soapenv:Envelope>'.encode(), ctype="text/xml")
        if meth == "RetrieveServiceContent":
            return ok("<returnval><rootFolder>group-d1</rootFolder><propertyCollector>propertyCollector</propertyCollector><viewManager>ViewManager</viewManager><sessionManager>SessionManager</sessionManager><ovfManager>OvfManager</ovfManager><guestOperationsManager>guestOperationsManager</guestOperationsManager><about><fullName>VMware vCenter Server 8.0.2 (имитация)</fullName><version>8.0.2</version><apiVersion>8.0.2.0</apiVersion></about></returnval>")
        if meth == "Login":
            if "<password>Virt-Pass-1</password>" not in b:
                return fault("Cannot complete login due to an incorrect user name or password.")
            return ok("<returnval><key>s1</key></returnval>")
        if "sess1" not in (self.headers.get("Cookie") or ""):
            return fault("The session is not authenticated.")
        if meth == "CreateContainerView":
            t = re.search(r"<type>(\w+)</type>", b).group(1); return ok(f"<returnval>view-{t}</returnval>")
        if meth == "DestroyView":
            return ok("")
        if meth == "RetrievePropertiesEx":
            obj = re.search(r'<obj type="(\w+)">(.*?)</obj>', b)
            paths = re.findall(r"<pathSet>(.*?)</pathSet>", b)
            def objset(kind, mo, props):
                ps = "".join(f"<propSet><name>{k}</name><val>{v}</val></propSet>" for k, v in props.items() if k in paths)
                return f'<objects><obj type="{kind}">{mo}</obj>{ps}</objects>'
            if obj.group(1) == "ContainerView":
                kind = obj.group(2).split("-", 1)[1]
                items = {"VirtualMachine": {k: {"name": v["name"], "runtime.powerState": v["state"], "config.template": "false"} for k, v in s["vms"].items()},
                         "HostSystem": {"host-1": {"name": "esxi01.corp", "parent": '<val type="ClusterComputeResource">domain-c1</val>'}},
                         "ResourcePool": {"resgroup-1": {"name": "Resources"}}, "Datastore": {"datastore-1": {"name": "ds1"}, "datastore-2": {"name": "ds2"}},
                         "Datacenter": {"datacenter-1": {"vmFolder": "group-v1"}}, "Network": {"network-1": {"name": "VM Network"}, "network-2": {"name": "DMZ"}}}[kind]
                out = ""
                for mo, props in items.items():
                    ps = "".join((f"<propSet><name>{k}</name>{v}</propSet>" if v.startswith("<val") else f"<propSet><name>{k}</name><val>{v}</val></propSet>") for k, v in props.items() if k in paths)
                    out += f'<objects><obj type="{kind}">{mo}</obj>{ps}</objects>'
                return ok(f"<returnval>{out}</returnval>")
            kind, mo = obj.group(1), obj.group(2)
            if kind == "Task":
                t = s["tasks"][mo]
                res = f"<result>{t['result']}</result>" if t.get("result") else ""
                info = "<state>" + t["state"] + "</state>" + res + "<error><localizedMessage>" + t.get("err", "") + "</localizedMessage></error>"
                return ok("<returnval>" + objset("Task", mo, {"info": info}) + "</returnval>")
            if kind == "VirtualMachine":
                v = s["vms"][mo]
                nics = "".join(f'<VirtualDevice xsi:type="VirtualVmxnet3"><key>{4000+i}</key><deviceInfo><label>Network adapter {i+1}</label></deviceInfo><macAddress>{mac}</macAddress></VirtualDevice>' for i, mac in enumerate(v.get("macs", [])))
                return ok(f'<returnval>{objset(kind, mo, {"config.changeTrackingEnabled": "true" if v.get("cbt") else "false", "runtime.powerState": v["state"], "name": v["name"], "config.hardware.numCPU": str(v.get("cpus", 2)), "config.hardware.memoryMB": str(v.get("memory_mb", 2048)), "config.firmware": v.get("firmware", "bios"), "guest.hostName": v.get("hostname", ""), "config.guestFullName": "Ubuntu Linux (64-bit)", "config.hardware.device": nics})}</returnval>')
            if kind == "GuestOperationsManager":
                return ok(f'<returnval>{objset(kind, mo, {"fileManager": "guestOperationsFileManager", "processManager": "guestOperationsProcessManager"})}</returnval>')
            if kind == "VirtualMachineSnapshot":
                vm = s["snaps"][mo]["vm"]
                devs = "".join(f'<VirtualDevice xsi:type="VirtualDisk"><key>{2000+i}</key><capacityInBytes>{s["disks"][d].get("capacity") or os.path.getsize(s["disks"][d]["file"])}</capacityInBytes><backing><changeId>52 cid/{s["vms"][vm]["cid"]}</changeId></backing></VirtualDevice>' for i, d in enumerate(s["vms"][vm]["disks"]))
                return ok(f'<returnval>{objset(kind, mo, {"config.hardware.device": devs})}</returnval>')
            if kind == "HttpNfcLease":
                l = s["leases"][mo]
                if l["kind"] == "export":
                    urls = "".join(f'<deviceUrl><key>/vm/{2000+i}</key><importKey>/vm/{2000+i}</importKey><url>https://*:{port}/nfc/{mo}/{d}</url><targetId>disk-{i}</targetId><disk>true</disk></deviceUrl>' for i, d in enumerate(l["disks"]))
                else:
                    urls = "".join(f'<deviceUrl><key>k{i}</key><importKey>/vm/disk{i}</importKey><url>https://*:{port}/nfc/{mo}/upload{i}</url><disk>true</disk></deviceUrl>' for i in range(l["n"]))
                info = "<entity>" + l.get("entity", "") + "</entity>" + urls + "<totalDiskCapacityInKB>1024</totalDiskCapacityInKB>"
                return ok("<returnval>" + objset(kind, mo, {"state": "ready", "info": info}) + "</returnval>")
            if kind == "ClusterComputeResource":
                return ok(f'<returnval>{objset(kind, mo, {"resourcePool": "resgroup-1"})}</returnval>')
        if meth == "ReconfigVM_Task":
            s["vms"][this]["cbt"] = True; t = "task-" + uuid.uuid4().hex[:4]; s["tasks"][t] = {"state": "success"}; ev(s, "enable cbt"); save(s); return ok(f"<returnval>{t}</returnval>")
        if meth == "CreateSnapshot_Task":
            t = "task-" + uuid.uuid4().hex[:4]
            q = "<quiesce>true</quiesce>" in b
            if q and not s.get("quiesce_ok"):
                s["tasks"][t] = {"state": "error", "err": "An error occurred while quiescing the virtual machine."}
            else:
                sn = "snapshot-" + uuid.uuid4().hex[:4]; s["snaps"][sn] = {"vm": this}; s["tasks"][t] = {"state": "success", "result": sn}
                s["vms"][this]["cid"] += 1; ev(s, "snapshot" + (" quiesce" if q else ""))
            save(s); return ok(f"<returnval>{t}</returnval>")
        if meth == "QueryChangedDiskAreas":
            cid = re.search(r"<changeId>(.*?)</changeId>", b).group(1)
            ln = 4096 if cid != "*" else 65536
            return ok(f"<returnval><startOffset>0</startOffset><length>{1 << 40}</length><changedArea><start>0</start><length>{ln}</length></changedArea></returnval>")
        if meth == "ExportSnapshot":
            l = "lease-" + uuid.uuid4().hex[:4]; s["leases"][l] = {"kind": "export", "disks": s["vms"][s["snaps"][this]["vm"]]["disks"]}; save(s); return ok(f"<returnval>{l}</returnval>")
        if meth in ("HttpNfcLeaseProgress",):
            return ok("")
        if meth in ("HttpNfcLeaseComplete", "HttpNfcLeaseAbort"):
            l = s["leases"][this]; l["done"] = meth; ev(s, meth)
            if l["kind"] == "import" and meth.endswith("Complete"):
                s["vms"][l["entity"]] = {"name": l["name"], "state": "poweredOff", "disks": [], "cid": 0, "uploaded": l.get("uploaded", []), "datastore": l["ds"], "netmap": l["netmap"]}
            save(s); return ok("")
        if meth == "CreateDescriptor":
            files = re.findall(r"<path>(.*?)</path>", b)
            ovf = "<Envelope><References>" + "".join(f'<File ovf:href="{f}"/>' for f in files) + '</References><NetworkSection><Network ovf:name="VM Network"/></NetworkSection></Envelope>'
            return ok(f"<returnval><ovfDescriptor>{ovf.replace('<', '&lt;').replace('>', '&gt;')}</ovfDescriptor></returnval>")
        if meth == "RemoveSnapshot_Task":
            s["snaps"].pop(this); t = "task-" + uuid.uuid4().hex[:4]; s["tasks"][t] = {"state": "success"}; ev(s, "remove snapshot"); save(s); return ok(f"<returnval>{t}</returnval>")
        if meth == "CreateImportSpec":
            name = re.search(r"<entityName>(.*?)</entityName>", b).group(1)
            ovf = re.search(r"<ovfDescriptor>(.*?)</ovfDescriptor>", b, re.S).group(1)
            n = ovf.count("href=")
            ds = re.search(r'<datastore type="Datastore">(.*?)</datastore>', b).group(1)
            nm = re.findall(r"<networkMapping><name>(.*?)</name><network type=\"Network\">(.*?)</network>", b)
            items = "".join(f"<fileItem><deviceId>/vm/disk{i}</deviceId><path>disk-{i}.vmdk</path></fileItem>" for i in range(n))
            return ok(f'<returnval><importSpec xsi:type="VirtualMachineImportSpec"><configSpec><name>{name}</name><datastore>{ds}</datastore><net>{json.dumps(nm)}</net></configSpec></importSpec>{items}</returnval>')
        if meth == "ImportVApp":
            name = re.search(r"<configSpec><name>(.*?)</name>", b).group(1)
            ds = re.search(r"<configSpec><name>.*?</name><datastore>(.*?)</datastore>", b).group(1)
            nm = json.loads(re.search(r"<net>(.*?)</net>", b).group(1).replace("&quot;", '"'))
            l = "lease-" + uuid.uuid4().hex[:4]
            s["leases"][l] = {"kind": "import", "n": s.get("import_disks", 2), "name": name, "entity": "vm-" + uuid.uuid4().hex[:4], "ds": ds, "netmap": nm}
            ev(s, "import vapp"); save(s); return ok(f"<returnval>{l}</returnval>")
        if meth == "ShutdownGuest":
            s["vms"][this]["state"] = "poweredOff"; ev(s, "shutdown guest"); save(s); return ok("")
        if meth == "PowerOffVM_Task":
            s["vms"][this]["state"] = "poweredOff"; t = "task-" + uuid.uuid4().hex[:4]; s["tasks"][t] = {"state": "success"}; ev(s, "power off"); save(s); return ok(f"<returnval>{t}</returnval>")
        if meth in ("InitiateFileTransferToGuest", "InitiateFileTransferFromGuest", "StartProgramInGuest", "ListProcessesInGuest", "DeleteFileInGuest"):
            user = re.search(r"<username>(.*?)</username>", b).group(1); pw = re.search(r"<password>(.*?)</password>", b).group(1)
            if (user, pw) != ("root", "Guest-Pass-1"):
                return fault("Failed to authenticate with the guest operating system using the supplied credentials.")
            if meth == "InitiateFileTransferToGuest":
                path = re.search(r"<guestFilePath>(.*?)</guestFilePath>", b).group(1)
                return ok(f"<returnval>https://*:{port}/guest/up/{urllib.parse.quote(path, safe='')}</returnval>")
            if meth == "InitiateFileTransferFromGuest":
                path = re.search(r"<guestFilePath>(.*?)</guestFilePath>", b).group(1)
                return ok(f"<returnval><size>1</size><url>https://*:{port}/guest/down/{urllib.parse.quote(path, safe='')}</url></returnval>")
            if meth == "StartProgramInGuest":
                args = html.unescape(re.search(r"<arguments>(.*?)</arguments>", b, re.S).group(1))
                ev(s, "guest run " + args)
                # «Гостевая ОС»: сценарий выполнен — вывод в файл, указанный после «>».
                out = re.search(r"> (/tmp/hb-prep-\w+\.out)", args).group(1)
                sp = re.search(r"sh (/tmp/hb-prep-\w+\.sh)", args).group(1)
                ok_script = "HB-PREP-DONE" in s.get("guest_files", {}).get(sp, "")
                s.setdefault("guest_files", {})[out] = "initramfs: dracut, добавлены virtio\nсеть: имена 1 интерфейсов закреплены за MAC\nfstab: уже по UUID/LABEL/LVM\nqemu-guest-agent: установлен (запустится на KVM)\n" + ("HB-PREP-DONE\n" if ok_script else "")
                save(s); return ok("<returnval>4242</returnval>")
            if meth == "ListProcessesInGuest":
                return ok("<returnval><name>sh</name><pid>4242</pid><exitCode>0</exitCode><endTime>2026-10-26T10:00:00Z</endTime></returnval>")
            return ok("")
        if meth == "PowerOnVM_Task":
            s["vms"][this]["state"] = "poweredOn"; t = "task-" + uuid.uuid4().hex[:4]; s["tasks"][t] = {"state": "success"}; save(s); return ok(f"<returnval>{t}</returnval>")
        return fault("unknown method " + meth)

    # ------------------------------------------------------------ SpaceVM / VMmanager / OpenNebula
    def spacevm(self, m, u):
        s = load(); p = u.path
        if p == "/auth/":
            return self.send(200, {"token": "jwt-1"})
        if self.headers.get("Authorization") != "jwt jwt-1":
            return self.send(401, {})
        if p == "/api/domains/":
            return self.send(200, {"count": len(s["vms"]), "next": None, "results": [dict(v, id=k) for k, v in s["vms"].items()]})
        mm = re.fullmatch(r"/api/domains/([^/]+)/", p)
        if mm:
            return self.send(200, dict(s["vms"][mm.group(1)], id=mm.group(1)))
        mm = re.fullmatch(r"/api/nodes/([^/]+)/", p)
        if mm:
            return self.send(200, {"id": mm.group(1), "management_ip": "127.0.0.1"})
        return self.send(404, {})

    def vmmanager(self, m, u):
        s = load(); p = u.path
        if p == "/auth/v4/public/token":
            return self.send(200, {"token": "t-1"})
        if self.headers.get("x-xsrf-token") != "t-1":
            return self.send(401, {})
        if p == "/vm/v3/host":
            return self.send(200, {"list": [dict(v, id=int(k)) for k, v in s["vms"].items()], "size": len(s["vms"])})
        mm = re.fullmatch(r"/vm/v3/host/(\d+)", p)
        if mm:
            return self.send(200, dict(s["vms"][mm.group(1)], id=int(mm.group(1))))
        mm = re.fullmatch(r"/vm/v3/node/(\d+)", p)
        if mm:
            return self.send(200, {"id": int(mm.group(1)), "ip": {"ip": "127.0.0.1"}})
        return self.send(404, {})

    def opennebula(self, m, u):
        s = load(); b = self.body().decode()
        meth = re.search(r"<methodName>(.*?)</methodName>", b).group(1)
        args = re.findall(r"<param><value>(?:<(?:string|int|boolean)>)?(.*?)(?:</(?:string|int|boolean)>)?</value></param>", b, re.S)
        def ok(v, typ="string"):
            v = str(v).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;") if typ == "string" else v
            return self.send(200, raw=f"<?xml version='1.0'?><methodResponse><params><param><value><array><data><value><boolean>1</boolean></value><value><{typ}>{v}</{typ}></value><value><int>0</int></value></data></array></value></param></params></methodResponse>".encode(), ctype="text/xml")
        if args[0] != "oneadmin:Virt-Pass-1":
            return self.send(200, raw=b"<methodResponse><params><param><value><array><data><value><boolean>0</boolean></value><value><string>[one.vm.info] User couldn't be authenticated</string></value></data></array></value></param></params></methodResponse>", ctype="text/xml")
        unesc = lambda x: x.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", '"').replace("&amp;", "&")
        if meth in ("one.vmpool.infoextended", "one.vmpool.info"):
            return ok("<VM_POOL>" + "".join(f"<VM><ID>{k}</ID><NAME>{v['name']}</NAME><STATE>3</STATE><HISTORY_RECORDS><HISTORY><HOSTNAME>localhost</HOSTNAME></HISTORY></HISTORY_RECORDS></VM>" for k, v in s["vms"].items()) + "</VM_POOL>")
        if meth == "one.vm.info":
            v = s["vms"][args[1]]
            return ok(f"<VM><ID>{args[1]}</ID><NAME>{v['name']}</NAME><TEMPLATE><CPU>1</CPU><VCPU>2</VCPU><MEMORY>512</MEMORY><NIC><NETWORK_ID>3</NETWORK_ID></NIC><OS><ARCH>x86_64</ARCH></OS></TEMPLATE><HISTORY_RECORDS><HISTORY><HOSTNAME>localhost</HOSTNAME></HISTORY></HISTORY_RECORDS></VM>")
        if meth == "one.image.allocate":
            t = unesc(args[1]); path = re.search(r'PATH="(.*?)"', t).group(1)
            iid = len(s["images"]) + 100
            s["images"][str(iid)] = {"template": t, "size": os.path.getsize(path), "fmt": open(path, "rb").read(4).hex()}
            ev(s, "image allocate"); save(s); return ok(iid, "int")
        if meth == "one.image.info":
            return ok(f"<IMAGE><ID>{args[1]}</ID><STATE>1</STATE></IMAGE>")
        if meth == "one.template.allocate":
            tid = len(s["templates"]) + 10; s["templates"][str(tid)] = unesc(args[1]); save(s); return ok(tid, "int")
        if meth == "one.template.instantiate":
            vid = 500 + len(s["created"]); s["created"].append({"template": args[1], "name": unesc(args[2]), "hold": args[3]}); save(s); return ok(vid, "int")
        return self.send(500, raw=b"unknown")


srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
if plat != "opennebula":
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
