#!/usr/bin/env python3
"""Имитаторы API Proxmox VE и VMware vCenter для сквозной проверки (HTTPS, самоподписанный сертификат).

«ВМ» — это сам хост: при включении имитатор применяет cloud-init (ключ SSH из sshkeys / userdata
добавляется в /root/.ssh/authorized_keys), а адрес ВМ — внешний адрес хоста (6-й аргумент). Формат запросов проверяется так же
строго, как у настоящих API: sshkeys Proxmox — в процентной кодировке, спецификация vSphere —
configuration_spec.cloud_config с type=CLOUDINIT, global_DNS_settings и interfaces.

  hypervisors.py proxmox 18006 cert.pem key.pem calls.jsonl 10.0.0.5
  hypervisors.py vmware  18007 cert.pem key.pem calls.jsonl 10.0.0.5
"""
import base64, json, re, ssl, sys, threading, urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KIND, PORT, CERT, KEY, LOG = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
# Адрес «ВМ» — внешний адрес самого хоста (петлевой гостевой агент не сообщает).
VM_IP = sys.argv[6] if len(sys.argv) > 6 else "127.0.0.1"
TOKEN = "PVEAPIToken=root@pam!hb=11111111-2222-3333-4444-555555555555"
VC_USER, VC_PASS = "hb@vsphere.local", "vc-pass"
lock = threading.Lock()
vms = {}
next_id = [105]
tasks = {}


def record(**kw):
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(kw, ensure_ascii=False) + "\n")


def apply_key(key, who):
    key = key.strip()
    if not key.startswith("ssh-"):
        raise ValueError("ключ не похож на OpenSSH: " + key[:30])
    with lock:
        p = "/root/.ssh/authorized_keys"
        cur = open(p).read() if __import__("os").path.exists(p) else ""
        if key not in cur:
            open(p, "a").write(key + "\n")
    record(event="cloud-init", who=who, key=key.split()[-1])


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def handle_any(self, m):
        u = urllib.parse.urlparse(self.path)
        raw = self.body()
        record(method=m, path=u.path, query=u.query)
        try:
            (self.proxmox if KIND == "proxmox" else self.vmware)(m, u.path, urllib.parse.parse_qs(u.query), raw)
        except ValueError as e:
            self.reply(400, {"errors": {"param": str(e)}, "error_type": "INVALID_ARGUMENT", "messages": [str(e)]})

    do_GET = lambda s: s.handle_any("GET")
    do_POST = lambda s: s.handle_any("POST")
    do_PUT = lambda s: s.handle_any("PUT")
    do_PATCH = lambda s: s.handle_any("PATCH")
    do_DELETE = lambda s: s.handle_any("DELETE")

    # ------------------------------------------------------------ Proxmox
    def task(self):
        upid = f"UPID:pve:{len(tasks):08X}:00000000:6720A000:qmtask:{next_id[0]}:root@pam!hb:"
        tasks[upid] = 0
        return upid

    def proxmox(self, m, path, q, raw):
        if self.headers.get("authorization") != TOKEN:
            return self.reply(401, {"data": None})
        form = {k: v[0] for k, v in urllib.parse.parse_qs(raw.decode(), keep_blank_values=True).items()}
        p = path[len("/api2/json"):]
        if p == "/version":
            return self.reply(200, {"data": {"version": "8.2.4", "release": "8.2"}})
        if p == "/cluster/nextid":
            return self.reply(200, {"data": str(next_id[0])})
        mt = re.match(r"^/nodes/pve/tasks/([^/]+)/status$", p)
        if mt:
            upid = urllib.parse.unquote(mt.group(1))
            if upid not in tasks:
                return self.reply(404, {"data": None})
            tasks[upid] += 1
            return self.reply(200, {"data": {"status": "running"} if tasks[upid] < 2 else {"status": "stopped", "exitstatus": "OK"}})
        mv = re.match(r"^/nodes/pve/qemu/(\d+)(/.*)?$", p)
        if not mv:
            return self.reply(501, {"data": None})
        vmid, rest = mv.group(1), mv.group(2) or ""
        if vmid == "9000":
            if rest == "/config" and m == "GET":
                return self.reply(200, {"data": {"name": "ubuntu-2404-tpl", "template": 1, "net0": "virtio=BC:24:11:00:00:01,bridge=vmbr0", "ide2": "local-lvm:vm-9000-cloudinit,media=cdrom", "scsi0": "local-lvm:base-9000-disk-0,size=10G"}})
            if rest == "/clone" and m == "POST":
                nid = form.get("newid")
                if not nid or not form.get("name"):
                    raise ValueError("newid и name обязательны")
                vms[nid] = {"name": form["name"], "status": "stopped", "net0": "virtio=BC:24:11:00:00:02,bridge=vmbr0", "full": form.get("full")}
                next_id[0] += 1
                return self.reply(200, {"data": self.task()})
        vm = vms.get(vmid)
        if vm is None:
            return self.reply(500, {"data": None, "errors": {"vmid": f"VM {vmid} does not exist"}})
        if rest == "/config":
            if m == "GET":
                return self.reply(200, {"data": vm})
            if "sshkeys" in form:
                # Как Proxmox: значение — процентная кодировка (пробел — %20, «/» — %2F).
                rawv = re.search(r"(?:^|&)sshkeys=([^&]*)", raw.decode()).group(1)
                once = urllib.parse.unquote(rawv)
                if "%20" not in once:
                    raise ValueError("sshkeys: invalid urlencoded string")
                form["sshkeys"] = urllib.parse.unquote(once)
            if "ipconfig0" in form and not re.match(r"^ip=(dhcp|\d+\.\d+\.\d+\.\d+/\d+(,gw=\d+\.\d+\.\d+\.\d+)?)$", form["ipconfig0"]):
                raise ValueError("ipconfig0: неверный формат")
            vm.update(form)
            return self.reply(200, {"data": None})
        if rest == "/resize" and m == "PUT":
            return self.reply(200, {"data": self.task()})
        if rest == "/status/start":
            apply_key(vm.get("sshkeys", ""), "proxmox:" + vmid)
            vm["status"] = "running"
            return self.reply(200, {"data": self.task()})
        if rest == "/status/stop":
            vm["status"] = "stopped"
            return self.reply(200, {"data": self.task()})
        if rest == "/status/current":
            return self.reply(200, {"data": {"status": vm["status"], "vmid": int(vmid)}})
        if rest == "/agent/network-get-interfaces":
            if vm["status"] != "running":
                return self.reply(500, {"data": None, "errors": {"agent": "VM is not running"}})
            return self.reply(200, {"data": {"result": [
                {"name": "lo", "ip-addresses": [{"ip-address-type": "ipv4", "ip-address": "127.0.0.1"}]},
                {"name": "eth0", "ip-addresses": [{"ip-address-type": "ipv6", "ip-address": "fe80::1"}, {"ip-address-type": "ipv4", "ip-address": VM_IP}]},
            ]}})
        if rest == "" and m == "DELETE":
            vms.pop(vmid, None)
            return self.reply(200, {"data": self.task()})
        return self.reply(501, {"data": None})

    # ------------------------------------------------------------ vCenter
    def vmware(self, m, path, q, raw):
        if path == "/api/session" and m == "POST":
            auth = self.headers.get("authorization", "")
            if auth != "Basic " + base64.b64encode(f"{VC_USER}:{VC_PASS}".encode()).decode():
                return self.reply(401, {"error_type": "UNAUTHENTICATED", "messages": []})
            return self.reply(201, "vmware-session-1")
        if self.headers.get("vmware-api-session-id") != "vmware-session-1":
            return self.reply(401, {"error_type": "UNAUTHENTICATED", "messages": []})
        if path == "/api/session" and m == "DELETE":
            return self.reply(204)
        body = json.loads(raw) if raw else None
        inv = {"folder": ("folder", "group-v4"), "resource-pool": ("resource_pool", "resgroup-8"), "datastore": ("datastore", "datastore-12"), "cluster": ("cluster", "domain-c7"), "host": ("host", "host-9")}
        mi = re.match(r"^/api/vcenter/(folder|resource-pool|datastore|cluster|host)$", path)
        if mi and m == "GET":
            key, ident = inv[mi.group(1)]
            return self.reply(200, [{key: ident, "name": q.get("names", [""])[0]}])
        if path == "/api/vcenter/vm" and m == "GET":
            name = q.get("names", [""])[0]
            return self.reply(200, [{"vm": "vm-10", "name": name, "power_state": "POWERED_OFF"}] if name == "golden-ubuntu" else [])
        if path == "/api/vcenter/vm" and m == "POST" and q.get("action") == ["clone"]:
            if body.get("source") != "vm-10" or not body.get("name"):
                raise ValueError("clone: source и name")
            pl = body.get("placement", {})
            if pl.get("folder") != "group-v4" or pl.get("datastore") != "datastore-12":
                raise ValueError("clone: placement должен содержать идентификаторы, а не имена")
            vid = f"vm-{11 + len(vms)}"
            vms[vid] = {"name": body["name"], "power": "POWERED_OFF", "cust": None}
            return self.reply(200, vid)
        mv = re.match(r"^/api/vcenter/vm/(vm-\d+)(/.*)?$", path)
        if not mv:
            return self.reply(404, {"error_type": "NOT_FOUND", "messages": []})
        vid, rest = mv.group(1), mv.group(2) or ""
        vm = vms.get(vid)
        if vm is None:
            return self.reply(404, {"error_type": "NOT_FOUND", "messages": []})
        if rest == "/hardware/cpu" and m == "PATCH":
            if not isinstance(body.get("count"), int):
                raise ValueError("cpu.count")
            return self.reply(204)
        if rest == "/hardware/memory" and m == "PATCH":
            if not isinstance(body.get("size_MiB"), int):
                raise ValueError("memory.size_MiB")
            return self.reply(204)
        if rest == "/hardware/disk" and m == "GET":
            return self.reply(200, [{"disk": "2000"}])
        if rest == "/hardware/disk/2000":
            return self.reply(200, {"capacity": 10 << 30, "label": "Hard disk 1"})
        if rest == "/guest/customization" and m == "PUT":
            spec = body.get("spec") or {}
            cc = spec.get("configuration_spec", {}).get("cloud_config", {})
            if cc.get("type") != "CLOUDINIT" or "global_DNS_settings" not in spec or "interfaces" not in spec:
                raise ValueError("ожидалась спецификация cloud-init (configuration_spec.cloud_config, global_DNS_settings, interfaces)")
            ud = cc["cloudinit"]["userdata"]
            if not ud.startswith("#cloud-config") or "instance-id:" not in cc["cloudinit"]["metadata"]:
                raise ValueError("userdata/metadata")
            vm["cust"] = ud
            return self.reply(204)
        if rest == "/power" and m == "POST":
            if q.get("action") == ["start"]:
                if vm["cust"]:
                    k = re.search(r"ssh_authorized_keys:\n\s+- (.+)\n", vm["cust"]).group(1)
                    apply_key(k, "vmware:" + vid)
                vm["power"] = "POWERED_ON"
            else:
                vm["power"] = "POWERED_OFF"
            return self.reply(204)
        if rest == "/guest/identity":
            if vm["power"] != "POWERED_ON":
                return self.reply(503, {"error_type": "SERVICE_UNAVAILABLE", "messages": []})
            return self.reply(200, {"ip_address": VM_IP, "host_name": vm["name"], "family": "LINUX"})
        if rest == "" and m == "DELETE":
            vms.pop(vid, None)
            return self.reply(204)
        return self.reply(404, {"error_type": "NOT_FOUND", "messages": []})


srv = ThreadingHTTPServer(("127.0.0.1", PORT), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(CERT, KEY)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
