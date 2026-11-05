#!/usr/bin/env python3
"""Имитация kubectl над $KUBE_STATE: {"resources": {"namespaced": [...], "cluster": [...]},
"objects": {"<ресурс>": [объекты]}, "applied": [...]}. apply -f - складывает объекты в applied
(и в objects — как сделал бы сервер), проверяя, что служебные поля очищены."""
import json, os, sys

ST = os.environ.get("KUBE_STATE", "/tmp/kube_state.json")
a = [x for x in sys.argv[1:] if not x.startswith("--context=")]
if not os.environ.get("KUBECONFIG") or not os.path.exists(os.environ["KUBECONFIG"]):
    sys.stderr.write("error: no kubeconfig\n"); sys.exit(1)
s = json.load(open(ST))


def save():
    json.dump(s, open(ST, "w"))


KIND2RES = {"Namespace": "namespaces", "Deployment": "deployments.apps", "Service": "services", "ConfigMap": "configmaps", "Secret": "secrets",
            "PersistentVolume": "persistentvolumes", "PersistentVolumeClaim": "persistentvolumeclaims", "ServiceAccount": "serviceaccounts",
            "ClusterRole": "clusterroles.rbac.authorization.k8s.io", "CustomResourceDefinition": "customresourcedefinitions.apiextensions.k8s.io", "Widget": "widgets.example.com"}

if a[:1] == ["version"]:
    print(json.dumps({"clientVersion": {"gitVersion": "v1.31.0"}, "serverVersion": {"gitVersion": "v1.31.0"}}))
elif a[:1] == ["api-resources"]:
    ns = "--namespaced=true" in a
    for r in s["resources"]["namespaced" if ns else "cluster"]:
        print(r)
elif a[:1] == ["get"]:
    r = a[1]
    print(json.dumps({"apiVersion": "v1", "kind": "List", "items": s["objects"].get(r, [])}))
elif a[:1] == ["wait"]:
    print("condition met")
elif a[:1] == ["apply"]:
    body = json.loads(sys.stdin.read())
    dry = "--dry-run=server" in a
    for o in body["items"]:
        md = o["metadata"]
        for k in ("uid", "resourceVersion", "creationTimestamp", "managedFields"):
            if k in md:
                sys.stderr.write(f"error: {o['kind']}/{md['name']}: поле {k} не очищено\n"); sys.exit(1)
        if "status" in o:
            sys.stderr.write(f"error: {o['kind']}/{md['name']}: status не очищен\n"); sys.exit(1)
        ns = md.get("namespace")
        if ns and ns != "default" and not any(x["metadata"]["name"] == ns for x in s["objects"].get("namespaces", [])) and not any(x["kind"] == "Namespace" and x["metadata"]["name"] == ns for x in s["applied"]):
            sys.stderr.write(f'Error from server (NotFound): namespaces "{ns}" not found\n'); sys.exit(1)
        if not dry:
            s["applied"].append(o)
            res = KIND2RES.get(o["kind"], o["kind"].lower() + "s")
            lst = s["objects"].setdefault(res, [])
            lst[:] = [x for x in lst if not (x["metadata"]["name"] == md["name"] and x["metadata"].get("namespace") == ns)]
            lst.append(o)
        print(f"{o['kind'].lower()}/{md['name']} {'configured (server dry run)' if dry else 'configured'}")
    save()
else:
    sys.stderr.write("fake kubectl: " + " ".join(a) + "\n"); sys.exit(2)
