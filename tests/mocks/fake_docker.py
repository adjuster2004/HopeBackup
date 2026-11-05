#!/usr/bin/env python3
"""Имитация docker CLI над состоянием $DOCKER_STATE (тома с каталогами на диске, контейнеры, проекты
compose, образы); журнал действий — в state["events"]. Плюс exec в harbor-db (pg_dumpall/psql/pg_isready)."""
import json, os, sys

ST = os.environ.get("DOCKER_STATE", "/tmp/docker_state.json")
a = sys.argv[1:]
s = json.load(open(ST))
ev = s.setdefault("events", [])


def save():
    json.dump(s, open(ST, "w"))


def opt(flag):
    for i, x in enumerate(a):
        if x == flag and i + 1 < len(a):
            return a[i + 1]
        if x.startswith(flag + "="):
            return x.split("=", 1)[1]
    return None


def opts(flag):
    out = []
    for i, x in enumerate(a):
        if x == flag and i + 1 < len(a):
            out.append(a[i + 1])
    return out


def fmt(c, name, f):
    return f.replace("{{.Names}}", name).replace("{{.State}}", c["State"]).replace("{{.Image}}", c.get("Image", ""))


cmd = a[0] if a else ""
if cmd == "version":
    print("27.3.1")
elif cmd == "volume":
    sub = a[1]
    if sub == "ls":
        for n in s["volumes"]:
            print(n)
    elif sub == "inspect":
        n = a[2]
        if n not in s["volumes"]:
            sys.stderr.write(f"Error: No such volume: {n}\n"); sys.exit(1)
        v = s["volumes"][n]
        print(json.dumps([{"Name": n, "Driver": v.get("Driver", "local"), "Mountpoint": v["Mountpoint"], "Labels": v.get("Labels", {}), "Options": v.get("Options", {})}]))
    elif sub == "create":
        n = a[-1]
        labels = {}
        for i, x in enumerate(a):
            if x == "--label":
                k, _, val = a[i + 1].partition("="); labels[k] = val
        mp = os.path.join(s["root"], n, "_data")
        os.makedirs(mp, exist_ok=True)
        s["volumes"][n] = {"Driver": opt("--driver") or "local", "Mountpoint": mp, "Labels": labels}
        ev.append(f"create {n}"); save(); print(n)
elif cmd == "ps":
    # Как настоящий docker: без -a видны работающие и перезапускающиеся (restarting); фильтры — все сразу.
    f = opt("--format") or "{{.Names}}"
    for n, c in s["containers"].items():
        if "-a" not in a and "-aq" not in a and c["State"] not in ("running", "restarting"):
            continue
        ok = True
        for flt in opts("--filter"):
            if flt.startswith("volume=") and flt[7:] not in c.get("Volumes", []):
                ok = False
            if flt.startswith("label=com.docker.compose.project=") and c.get("Project") != flt.split("=", 2)[2]:
                ok = False
            if flt.startswith("status=") and c["State"] != flt[7:]:
                ok = False
        if ok:
            print(n if "-aq" in a else fmt(c, n, f))
elif cmd in ("pause", "unpause", "stop", "start"):
    # Как docker CLI: каждый контейнер по очереди, ошибки — в stderr, код 1 в конце.
    rc = 0
    for n in a[1:]:
        c = s["containers"][n]
        if cmd == "pause" and c.get("PauseFails"):
            c["State"] = c.pop("PauseFails")   # успел уйти в перезапуск между docker ps и docker pause
        if cmd == "pause" and c["State"] == "restarting":
            sys.stderr.write(f"Error response from daemon: Container {n}0123 is restarting, wait until the container is running\n"); rc = 1
            continue
        c["State"] = {"pause": "paused", "unpause": "running", "stop": "exited", "start": "running"}[cmd]
        ev.append(f"{cmd} {n}")
    save()
    sys.exit(rc)
elif cmd == "inspect":
    if a[1] == "--type" or a[1] == "-f":
        n = a[-1]
        if n not in s["containers"]:
            sys.exit(1)
        if "-f" in a:
            f = opt("-f")
            c = s["containers"][n]
            if "State.Status" in f:
                print(c["State"])
            elif "Running" in f:
                print("true" if c["State"] == "running" else "false")
            elif "Mounts" in f:
                print(json.dumps(c.get("Mounts", [])))
            elif "Env" in f:
                print(json.dumps(c.get("Env", [])))
        sys.exit(0)
    print(json.dumps([{"Name": "/" + n, "Config": {"Image": s["containers"][n].get("Image", "")}} for n in a[1:] if n in s["containers"]]))
elif cmd == "compose":
    if a[1] == "ls":
        print(json.dumps([{"Name": n, "Status": "running(1)", "ConfigFiles": p["ConfigFiles"]} for n, p in s["projects"].items()]))
    else:
        ev.append("compose " + " ".join(a[1:]))
        if "up" in a:
            p = opt("-p")
            for n, c in s["containers"].items():
                if p and c.get("Project") == p:
                    c["State"] = "running"
                if "postgresql" in a and n == "harbor-db":
                    c["State"] = "running"
                elif "up" in a and "postgresql" not in a and not p:
                    c["State"] = "running"
        if "stop" in a:
            for c in s["containers"].values():
                c["State"] = "exited"
        save()
elif cmd == "image":
    for i in s.get("images", []):
        print(i)
elif cmd == "save":
    sys.stdout.write(json.dumps({"image": a[1], "layers": "L" * 1000}))
elif cmd == "load":
    d = json.loads(sys.stdin.read())
    s.setdefault("loaded", []).append(d["image"]); save()
    print("Loaded image: " + d["image"])
elif cmd == "exec":
    args = [x for x in a[1:]]
    while args and args[0].startswith("-"):
        if args[0] in ("-u", "-w", "-e"):
            args = args[2:]
        else:
            args = args[1:]
    ct, prog = args[0], args[1:]
    if ct not in s["containers"] or s["containers"][ct]["State"] != "running":
        sys.stderr.write(f"Error response from daemon: container {ct} is not running\n"); sys.exit(1)
    if prog[0] == "pg_dumpall":
        sys.stdout.write("-- PostgreSQL database cluster dump\n" + "\n".join(f"INSERT {r};" for r in s["containers"][ct].get("db", [])) + "\n")
    elif prog[0] == "psql":
        data = sys.stdin.read()
        s["containers"][ct]["db"] = [l[7:-1] for l in data.splitlines() if l.startswith("INSERT ")]
        ev.append("psql load"); save()
    elif prog[0] == "pg_isready":
        print("accepting connections")
    else:
        os.execvp(prog[0], prog)
else:
    sys.stderr.write("fake docker: " + " ".join(a) + "\n"); sys.exit(2)
