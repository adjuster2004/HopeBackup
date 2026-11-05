#!/usr/bin/env python3
"""Имитация clickhouse-client над состоянием в $CH_STATE: базы, таблицы (DDL, движок, строки), диск копий.
Пароль — только из --config-file (XML). «Native» — строки JSON."""
import json, os, re, sys, shutil

ST = os.environ.get("CH_STATE", "/tmp/ch_state.json")
PW = os.environ.get("CH_EXPECT_PASSWORD", "")
args = sys.argv[1:]


def opt(name):
    for i, a in enumerate(args):
        if a.startswith(name + "="):
            return a.split("=", 1)[1]
        if a == name and i + 1 < len(args):
            return args[i + 1]
    return None


cfgf = opt("--config-file")
cfg = open(cfgf).read() if cfgf else ""
m = re.search(r"<password>(.*?)</password>", cfg)
if PW and (not m or m.group(1).replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">") != PW):
    sys.stderr.write("Code: 516. DB::Exception: default: Authentication failed: password is incorrect\n"); sys.exit(4)
q = (opt("--query") or "").strip()
s = json.load(open(ST))


def save():
    json.dump(s, open(ST, "w"))


def name(x):
    x = x.strip()
    return x[1:-1].replace("\\`", "`") if x.startswith("`") else x


def lit(x):
    return x.strip()[1:-1].replace("\\'", "'")


if q == "SELECT version()":
    print("24.8.4.13")
elif q.startswith("SELECT name FROM system.databases"):
    mm = re.search(r"WHERE name = ('.*')", q)
    for n in s["dbs"]:
        if not mm or n == lit(mm.group(1)):
            print(n)
    if not mm:
        print("system"); print("INFORMATION_SCHEMA")
elif q.startswith("SELECT path FROM system.disks"):
    d = lit(re.search(r"name = ('.*')", q).group(1))
    if d in s.get("disks", {}):
        print(s["disks"][d])
elif q.startswith("SHOW CREATE DATABASE"):
    n = name(q[len("SHOW CREATE DATABASE"):])
    print(f"CREATE DATABASE {n}\nENGINE = {s['dbs'][n]['engine']}")
elif q.startswith("SELECT name, engine FROM system.tables"):
    db = lit(re.search(r"database = ('[^']*')", q).group(1))
    for t, v in s["dbs"][db]["tables"].items():
        print(f"{t}\t{v['engine']}")
elif q.startswith("SHOW CREATE TABLE"):
    db, t = [name(x) for x in re.match(r"SHOW CREATE TABLE (`[^`]*`)\.(`[^`]*`)", q).groups()]
    print(s["dbs"][db]["tables"][t]["ddl"])
elif q.startswith("SELECT * FROM") and q.endswith("FORMAT Native"):
    db, t = [name(x) for x in re.match(r"SELECT \* FROM (`[^`]*`)\.(`[^`]*`)", q).groups()]
    for r in s["dbs"][db]["tables"][t]["rows"]:
        sys.stdout.write(json.dumps(r) + "\n")
elif q.startswith("INSERT INTO") and q.endswith("FORMAT Native"):
    db, t = [name(x) for x in re.match(r"INSERT INTO (`[^`]*`)\.(`[^`]*`)", q).groups()]
    rows = [json.loads(l) for l in sys.stdin.read().splitlines() if l]
    s["dbs"][db]["tables"][t]["rows"].extend(rows)
    save()
elif q.startswith("CREATE DATABASE"):
    mm = re.match(r"CREATE DATABASE (\S+)\s*(?:ENGINE = (\w+))?", q)
    n = name(mm.group(1))
    if n in s["dbs"]:
        sys.stderr.write(f"Code: 82. DB::Exception: Database {n} already exists.\n"); sys.exit(82)
    s["dbs"][n] = {"engine": mm.group(2) or "Atomic", "tables": {}}
    save()
elif q.startswith("CREATE TABLE") or q.startswith("CREATE VIEW") or q.startswith("CREATE MATERIALIZED VIEW"):
    mm = re.match(r"CREATE (?:TABLE|VIEW|MATERIALIZED VIEW) (\S+?)\.(\S+)", q)
    db, t = name(mm.group(1)), name(mm.group(2).split("\n")[0])
    eng = re.search(r"ENGINE = (\w+)", q)
    eng = eng.group(1) if eng else ("View" if "VIEW" in q.split("\n")[0] else "Memory")
    if db not in s["dbs"]:
        sys.stderr.write(f"Code: 81. DB::Exception: Database {db} does not exist.\n"); sys.exit(81)
    s["dbs"][db]["tables"][t] = {"engine": eng, "ddl": q, "rows": []}
    save()
elif q.startswith("RENAME DATABASE"):
    a, b = [name(x) for x in re.match(r"RENAME DATABASE (\S+) TO (\S+)", q).groups()]
    s["dbs"][b] = s["dbs"].pop(a)
    # DDL таблиц ссылается на базу по имени
    for t in s["dbs"][b]["tables"].values():
        t["ddl"] = t["ddl"].replace(f"{a}.", f"{b}.", 1)
    save()
elif q.startswith("BACKUP"):
    mm = re.match(r"BACKUP (.*) TO Disk\(('[^']*'), ('[^']*')\)", q)
    dbs = [name(x) for x in re.findall(r"DATABASE (`[^`]*`)", mm.group(1))]
    d = os.path.join(s["disks"][lit(mm.group(2))], lit(mm.group(3)))
    os.makedirs(os.path.join(d, "metadata"), exist_ok=True)
    for n in dbs:
        json.dump(s["dbs"][n], open(os.path.join(d, "metadata", n + ".json"), "w"))
    open(os.path.join(d, ".backup"), "w").write("<config><version>1</version></config>")
    print(f"{lit(mm.group(3))}\tBACKUP_CREATED")
elif q.startswith("RESTORE"):
    mm = re.match(r"RESTORE (.*) FROM Disk\(('[^']*'), ('[^']*')\)", q)
    d = os.path.join(s["disks"][lit(mm.group(2))], lit(mm.group(3)))
    if not os.path.exists(os.path.join(d, ".backup")):
        sys.stderr.write(f"Code: 598. DB::Exception: Backup {d} not found.\n"); sys.exit(598)
    for src, dst in re.findall(r"DATABASE (`[^`]*`)(?: AS (`[^`]*`))?", mm.group(1)):
        src = name(src); dst = name(dst) if dst else src
        if dst in s["dbs"]:
            sys.stderr.write(f"Code: 607. DB::Exception: Database {dst} already exists.\n"); sys.exit(607)
        s["dbs"][dst] = json.load(open(os.path.join(d, "metadata", src + ".json")))
    save()
    print("RESTORED")
else:
    sys.stderr.write("Code: 62. DB::Exception: Syntax error (mock): " + q[:100] + "\n"); sys.exit(62)
