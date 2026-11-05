#!/usr/bin/env python3
"""Имитация sqlcmd + SQL Server для проверки плагина mssql: состояние баз в JSON ($MSSQL_STATE),
BACKUP DATABASE/LOG пишут файлы .bak (JSON), RESTORE проверяет порядок цепочки (полная → разностная →
журналы без разрывов), STOPAT, NORECOVERY/RECOVERY, MOVE. Пароль — только из SQLCMDPASSWORD.

  fake_sqlcmd.py --mutate <база> <строка>   — изменение данных (запись в журнал транзакций)
"""
import json, os, re, sys, time, datetime

ST = os.environ.get("MSSQL_STATE", "/tmp/mssql_state.json")
PW = os.environ.get("MSSQL_EXPECT_PASSWORD", "")


def load():
    with open(ST) as f:
        return json.load(f)


def save(s):
    with open(ST + ".tmp", "w") as f:
        json.dump(s, f)
    os.replace(ST + ".tmp", ST)


def die(msg, code=1):
    sys.stderr.write("Msg 3013, Level 16, State 1: " + msg + "\n")
    sys.exit(code)


if len(sys.argv) > 1 and sys.argv[1] == "--mutate":
    s = load()
    db = s["dbs"][sys.argv[2]]
    s["lsn"] += 1
    db["rows"].append(sys.argv[3])
    db["log"].append({"lsn": s["lsn"], "t": time.time(), "row": sys.argv[3]})
    save(s)
    sys.exit(0)

args = sys.argv[1:]
q = None
i = 0
while i < len(args):
    if args[i] == "-Q":
        q = args[i + 1]; i += 2; continue
    i += 1
if PW and os.environ.get("SQLCMDPASSWORD") != PW:
    print("Sqlcmd: Error: Login failed for user 'sa'."); sys.exit(1)
if "-Q" in args and q is None:
    sys.exit(1)
if q is None:
    print("usage"); sys.exit(0)
q = q.replace("SET NOCOUNT ON; ", "").strip()
s = load()
log = s.setdefault("calls", [])
log.append(q)


def unq(n):
    return n[1:-1].replace("]]", "]")


def s_lit(x):
    m = re.match(r"N'((?:[^']|'')*)'", x)
    return m.group(1).replace("''", "'")


if "SERVERPROPERTY" in q:
    props = re.findall(r"SERVERPROPERTY\('(\w+)'\)", q)
    vals = {"ProductVersion": "16.0.4135.4", "Edition": "Developer Edition (64-bit)", "InstanceDefaultDataPath": "/var/opt/mssql/data/", "InstanceDefaultLogPath": "/var/opt/mssql/data/"}
    print("|".join(vals[p] for p in props))
elif "FROM sys.databases" in q:
    for n, d in s["dbs"].items():
        print(f"{n}|{d['recovery']}|{d['state']}")
elif q.startswith("BACKUP DATABASE") or q.startswith("BACKUP LOG"):
    m = re.match(r"BACKUP (DATABASE|LOG) (\[(?:[^\]]|\]\])+\]) TO DISK = (N'(?:[^']|'')*')", q)
    what, db, path = m.group(1), unq(m.group(2)), s_lit(m.group(3))
    d = s["dbs"][db]
    if not os.path.isdir(os.path.dirname(path)):
        die(f"Cannot open backup device '{path}'. Operating system error 2")
    s["lsn"] += 1
    lsn = s["lsn"]
    if what == "LOG":
        if d["recovery"] == "SIMPLE":
            die("The statement BACKUP LOG is not allowed while the recovery model is SIMPLE.")
        if d.get("last_full") is None:
            die("BACKUP LOG cannot be performed because there is no current database backup.")
        frm = d.get("last_log", d["last_full"])
        ent = [e for e in d["log"] if e["lsn"] > frm]
        content = {"type": "log", "db": db, "first": frm, "last": lsn, "entries": ent}
        d["last_log"] = lsn
        btype = "L"
    elif "DIFFERENTIAL" in q:
        if d.get("last_full") is None:
            die("Cannot perform a differential backup, no full backup.")
        content = {"type": "diff", "db": db, "base": d["last_full"], "lsn": lsn, "rows": list(d["rows"])}
        btype = "I"
    else:
        content = {"type": "full", "db": db, "lsn": lsn, "rows": list(d["rows"]), "files": d["files"]}
        if "COPY_ONLY" not in q:
            d["last_full"] = lsn
            d.pop("last_log", None) if d["recovery"] == "SIMPLE" else None
            if "last_log" not in d:
                d["last_log"] = lsn
        btype = "D"
    with open(path, "w") as f:
        json.dump(content, f)
        f.write("\n" + "PAGE" * 5000)  # объём, чтобы было что передавать
    s.setdefault("backupset", []).append({"db": db, "first": lsn - 1, "last": lsn, "dbl": d.get("last_full", 0), "type": btype})
    save(s)
    print(f"Processed 1000 pages for database '{db}'.")
    print(f"BACKUP {what} successfully processed 1000 pages in 0.010 seconds.")
elif "msdb.dbo.backupset" in q:
    db = s_lit(re.search(r"database_name = (N'(?:[^']|'')*')", q).group(1))
    b = [x for x in s.get("backupset", []) if x["db"] == db][-1]
    print(f"{b['first']}|{b['last']}|{b['dbl']}")
elif "SET SINGLE_USER" in q or "SET MULTI_USER" in q:
    pass
elif q.startswith("RESTORE FILELISTONLY"):
    path = s_lit(re.search(r"DISK = (N'(?:[^']|'')*')", q).group(1))
    c = json.loads(open(path).readline())
    for lg, ph, t in c["files"]:
        print(f"{lg}|{ph}|{t}|PRIMARY|8388608")
elif q.startswith("RESTORE DATABASE") or q.startswith("RESTORE LOG"):
    m = re.match(r"RESTORE (DATABASE|LOG) (\[(?:[^\]]|\]\])+\])(?: FROM DISK = (N'(?:[^']|'')*'))? WITH (.*)$", q, re.S)
    what, db, opts = m.group(1), unq(m.group(2)), m.group(4)
    rec = "NORECOVERY" not in opts
    if m.group(3) is None:  # RESTORE DATABASE x WITH RECOVERY
        d = s["dbs"][db]
        d["state"] = "ONLINE"
        save(s); print("RESTORE DATABASE successfully processed 0 pages."); sys.exit(0)
    c = json.loads(open(s_lit(m.group(3))).readline())
    if c["type"] == "full":
        exists = db in s["dbs"] and s["dbs"][db]["state"] == "ONLINE"
        if exists and "REPLACE" not in opts:
            die("The tail of the log for the database has not been backed up.")
        files = [list(x) for x in c["files"]]
        for lg, to in re.findall(r"MOVE (N'(?:[^']|'')*') TO (N'(?:[^']|'')*')", opts):
            for f in files:
                if f[0] == s_lit(lg):
                    f[1] = s_lit(to)
        phys = [f[1] for f in files]
        for n, o in s["dbs"].items():
            if n != db and any(p in phys for p in [f[1] for f in o["files"]]):
                die(f"The file '{phys[0]}' cannot be overwritten. It is being used by database '{n}'.")
        old = s["dbs"].get(db, {})
        s["dbs"][db] = {"recovery": old.get("recovery", "FULL"), "state": "ONLINE" if rec else "RESTORING", "rows": list(c["rows"]), "log": old.get("log", []),
                        "files": files, "restored_full": c["lsn"], "pos": c["lsn"], "restored_from": [c["type"]]}
    else:
        d = s["dbs"].get(db)
        if not d or d["state"] != "RESTORING":
            die(f"The database '{db}' is not in a restoring state (NORECOVERY).")
        if c["type"] == "diff":
            if c["base"] != d["restored_full"]:
                die("This differential backup cannot be restored because the database has not been restored to the correct earlier state.")
            d["rows"] = list(c["rows"]); d["pos"] = c["lsn"]
        else:
            if c["first"] > d["pos"]:
                die("The log in this backup set begins at LSN %d, which is too recent to apply to the database." % c["first"])
            stop = None
            mm = re.search(r"STOPAT = (N'(?:[^']|'')*')", opts)
            if mm:
                stop = datetime.datetime.strptime(s_lit(mm.group(1)), "%Y-%m-%dT%H:%M:%S").timestamp()
            stopped = False
            for e in c["entries"]:
                if e["lsn"] <= d["pos"]:
                    continue
                if stop is not None and e["t"] > stop:
                    stopped = True
                    break
                d["rows"].append(e["row"]); d["pos"] = e["lsn"]
            if not stopped:
                d["pos"] = max(d["pos"], c["last"])
        d["restored_from"].append(c["type"])
        if rec:
            d["state"] = "ONLINE"
    save(s)
    print(f"RESTORE {what} successfully processed 1000 pages in 0.020 seconds.")
else:
    print("Msg 102: Incorrect syntax: " + q[:80]); sys.exit(1)
