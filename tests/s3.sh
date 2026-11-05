#!/usr/bin/env bash
# Проверка площадки S3 (как Yandex Object Storage / AWS S3) на эмуляторе moto (pip install "moto[server]"):
#   командная строка: репозиторий в бакете с Object Lock, две копии (дедупликация), восстановление и сверка,
#   WORM-атрибуты объектов, очистка; затем площадка S3 в сервере управления и копия агентом.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/s3.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-s3}
S3PORT=15001; SRV_PORT=17843

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() { for f in moto server agent; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"

say "Эмулятор S3 (moto) и бакеты"
moto_server -H 127.0.0.1 -p $S3PORT > "$W/moto.log" 2>&1 & echo $! > "$W/moto.pid"
for i in $(seq 1 50); do curl -s http://127.0.0.1:$S3PORT/ >/dev/null && break; sleep 0.2; done
python3 - <<EOF
import boto3
s3 = boto3.client("s3", endpoint_url="http://127.0.0.1:$S3PORT", aws_access_key_id="agent", aws_secret_access_key="agent-secret", region_name="us-east-1")
s3.create_bucket(Bucket="hb-worm", ObjectLockEnabledForBucket=True)
s3.create_bucket(Bucket="hb-site")
print("бакеты:", [b["Name"] for b in s3.list_buckets()["Buckets"]])
EOF

export HOPEBACKUP_S3_ENDPOINT=http://127.0.0.1:$S3PORT HOPEBACKUP_S3_REGION=us-east-1 HOPEBACKUP_S3_ACCESS_KEY=agent HOPEBACKUP_S3_SECRET_KEY=agent-secret HOPEBACKUP_S3_PATH_STYLE=true
export HOPEBACKUP_PASSWORD=s3-repo-password HOPEBACKUP_DATA="$W/cli"
R() { "$BIN" repo --url s3://hb-worm/hopebackup --s3-lock-mode GOVERNANCE --s3-lock-days 1 "$@"; }

say "Репозиторий в бакете с Object Lock"
R init
mkdir -p "$W/src"; for i in $(seq 1 40); do head -c 300000 /dev/urandom > "$W/src/f$i.bin"; done; echo "конфиг v1" > "$W/src/app.conf"
R backup --instance files-a "$W/src" | tee "$W/b1.txt"
echo "конфиг v2" > "$W/src/app.conf"; head -c 300000 /dev/urandom > "$W/src/f41.bin"
R backup --instance files-a "$W/src" | tee "$W/b2.txt"
NEW2=$(grep -o 'новых [^,]*' "$W/b2.txt" | head -1); echo "вторая копия: $NEW2"
R list
SNAP=$(R list | tail -1 | awk '{print $1}')
R show "$SNAP"

say "Восстановление из S3 и сверка"
R restore --instance files-a --snapshot "$SNAP" --dest "$W/out"
diff -r "$W/src" "$W/out$W/src" && echo "ok: файлы совпали"

say "WORM: у объектов срок хранения Object Lock"
python3 - <<EOF
import boto3
s3 = boto3.client("s3", endpoint_url="http://127.0.0.1:$S3PORT", aws_access_key_id="agent", aws_secret_access_key="agent-secret", region_name="us-east-1")
keys = [o["Key"] for o in s3.list_objects_v2(Bucket="hb-worm", Prefix="hopebackup/main/packs/")["Contents"]]
h = s3.head_object(Bucket="hb-worm", Key=keys[0])
assert h.get("ObjectLockMode") == "GOVERNANCE", h
print("pack", keys[0].split("/")[-1][:12], "— ObjectLockMode", h["ObjectLockMode"], "до", h["ObjectLockRetainUntilDate"])
EOF

say "Блокировки записи сняты после копий (ключ агентов без удаления — пометка released)"
python3 - <<EOF
import boto3, json
s3 = boto3.client("s3", endpoint_url="http://127.0.0.1:$S3PORT", aws_access_key_id="agent", aws_secret_access_key="agent-secret", region_name="us-east-1")
locks = [o["Key"] for o in s3.list_objects_v2(Bucket="hb-worm", Prefix="hopebackup/main/locks/").get("Contents", [])]
for k in locks:
    assert json.loads(s3.get_object(Bucket="hb-worm", Key=k)["Body"].read()).get("released"), k
print("блокировок:", len(locks), "— все сняты")
EOF

say "Очистка: оставить одну копию (pack-файлы под свежей блокировкой записи не трогаются)"
R prune --keep-last 1 | tee "$W/prune.txt"
grep -q "снапшотов 1" "$W/prune.txt" || fail "очистка не удалила старый снапшот"
[ "$(R list | wc -l)" -eq 1 ] || fail "после очистки должен остаться один снапшот"
R restore --instance files-a --snapshot "$SNAP" --dest "$W/out2" && diff -r "$W/src" "$W/out2$W/src" && echo "ok: оставшаяся копия восстанавливается"

say "Площадка S3 в сервере управления, копия агентом"
unset HOPEBACKUP_DATA
HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat $W/srv/server/initial_admin_password.txt)\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() { local id=$1 st; for i in $(seq 1 300); do st=$(api GET /jobs/$id | jq -r .status); [ "$st" = ok ] && return 0; [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail; fail "задание: $(api GET /jobs/$id | jq -r .error)"; }; sleep 1; done; fail "задание не завершилось"; }
SITE=$(api POST /sites -d "{\"name\":\"S3 (эмулятор)\",\"kind\":\"s3\",\"repo\":\"main\",\"password\":\"site-password-1\",\"s3\":{\"provider\":\"s3\",\"endpoint\":\"http://127.0.0.1:$S3PORT\",\"region\":\"us-east-1\",\"bucket\":\"hb-site\",\"prefix\":\"dc1\",\"access_key\":\"agent\",\"secret_key\":\"agent-secret\",\"admin_access_key\":\"admin\",\"admin_secret_key\":\"admin-secret\",\"path_style\":true}}" | jq -r .id)
[ "$SITE" != null ] || fail "площадка S3 не создана"
api GET /sites | jq -c '.[] | {name, kind, url, s3: .s3 | {bucket, secret_key, admin_secret_key}}'
J=$(api POST /sites/$SITE/init | jq -r .job_id); wait_job "$J"
SRV_FP=$(api GET /status | jq -r .fingerprint)
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$SRV_FP" --name s3-host >/dev/null
HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
sleep 2
AGENT=$(api GET /agents | jq -r '.[0].id')
INST=$(api POST /instances -d "{\"name\":\"files-s3\",\"catalog_id\":\"files\",\"agent_id\":\"$AGENT\",\"site_id\":\"$SITE\",\"config\":{\"paths\":[\"$W/src\"]},\"retention\":{\"keep_last\":1}}" | jq -r .id)
for n in 1 2; do J=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_job "$J"; api GET /jobs/$J | jq -r '.log[].m' | grep готово; done
sleep 3
api POST /sites/$SITE/test | jq -c '{repo_exists, stats}'
N=$(api GET /instances/$INST/points | jq '.snapshots | length')
echo "точек восстановления: $N (хранить 1 — очистка ключом администратора после копии)"
[ "$N" -eq 1 ] || fail "очистка по политике хранения не сработала"
api GET /audit | jq -r '.[] | select(.action=="prune") | .detail' | head -2

printf '\n\033[1;32mПРОВЕРКА ПЛОЩАДКИ S3 ПРОЙДЕНА\033[0m\n'
