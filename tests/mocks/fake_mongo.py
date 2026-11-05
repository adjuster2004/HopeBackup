#!/usr/bin/env python3
"""Имитация mongodump / mongorestore / mongosh (по имени ссылки) над состоянием в $MONGO_STATE:
{"rs": true, "dbs": {"shop": {"orders": [...]}}}. Пароль — только из файла --config (YAML),
архив — свой формат строк JSON с заголовком (oplog=1 при --oplog)."""
import json, os, re, sys

ST = os.environ.get("MONGO_STATE", "/tmp/mongo_state.json")
PW = os.environ.get("MONGO_EXPECT_PASSWORD", "")
prog = os.path.basename(sys.argv[0])
args = sys.argv[1:]


def load():
    return json.load(open(ST))


def save(s):
    json.dump(s, open(ST, "w"))


def opt(name):
    for a in args:
        if a.startswith(name + "="):
            return a.split("=", 1)[1]
    return None


def multi(name):
    return [a.split("=", 1)[1] for a in args if a.startswith(name + "=")]


if "--version" in args:
    print(f"{prog} version: 100.10.0"); sys.exit(0)

if prog == "mongosh":
    s = load()
    f = [a for a in args if a.endswith(".js")]
    js = open(f[0]).read() if f else ""
    lits = [json.loads(x) for x in re.findall(r'"(?:[^"\\]|\\.)*"', js)]
    if PW and PW not in lits:
        print("MongoServerError: Authentication failed."); sys.exit(1)
    print("RS=" + ("rs0" if s.get("rs") else ""))
    sys.exit(0)

cfg = open(opt("--config")).read() if opt("--config") else ""
m = re.search(r'password: "((?:[^"\\]|\\.)*)"', cfg)
pw = m.group(1).replace('\\"', '"').replace("\\\\", "\\") if m else ""
if PW and pw != PW:
    sys.stderr.write("Failed: can't create session: connection() error: auth error: sasl conversation error: unable to authenticate\n"); sys.exit(1)
if 'uri: "' not in cfg:
    sys.stderr.write("Failed: no uri\n"); sys.exit(1)

s = load()
if prog == "mongodump":
    if "--archive" not in args:
        sys.exit(2)
    oplog = "--oplog" in args
    if oplog and not s.get("rs"):
        sys.stderr.write("Failed: error getting oplog start: oplog mode is only supported on replica sets\n"); sys.exit(1)
    db = opt("--db")
    out = sys.stdout
    out.write(json.dumps({"archive": "mock", "oplog": oplog}) + "\n")
    for name, colls in s["dbs"].items():
        if db and name != db:
            continue
        for c, docs in colls.items():
            for d in docs:
                out.write(json.dumps({"ns": f"{name}.{c}", "doc": d}) + "\n")
    sys.stderr.write("done dumping\n")
elif prog == "mongorestore":
    lines = sys.stdin.read().splitlines()
    head = json.loads(lines[0])
    if "--oplogReplay" in args and not head["oplog"]:
        sys.stderr.write("Failed: no oplog file to replay; make sure you run mongodump with --oplog\n"); sys.exit(1)
    inc = multi("--nsInclude")
    nfrom, nto = opt("--nsFrom"), opt("--nsTo")
    got = {}
    for l in lines[1:]:
        r = json.loads(l)
        ns = r["ns"]
        if inc and not any(ns.startswith(x.rstrip("*")) for x in inc):
            continue
        if nfrom and ns.startswith(nfrom.rstrip("*")):
            ns = nto.rstrip("*") + ns[len(nfrom.rstrip("*")):]
        got.setdefault(ns, []).append(r["doc"])
    for ns, docs in got.items():
        db, c = ns.split(".", 1)
        colls = s["dbs"].setdefault(db, {})
        if "--drop" in args or c not in colls:
            colls[c] = []
        colls[c].extend(docs)
    s["last_restore"] = {"args": args, "oplog_replay": "--oplogReplay" in args}
    save(s)
    sys.stderr.write(f"{sum(len(v) for v in got.values())} document(s) restored successfully. 0 document(s) failed to restore.\n")
