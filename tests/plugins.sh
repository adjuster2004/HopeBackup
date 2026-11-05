#!/usr/bin/env bash
# Проверка плагинов S3, Vault и GitLab через `hopebackup repo` (без сервера):
#   S3 — эмулятор moto: сотни мелких объектов с метаданными, «папка», объект 70 МБ (поток и
#        многочастная загрузка); вторая копия (дедупликация); восстановление в другой бакет и сверка;
#   Vault — имитатор API: снапшот raft туда и обратно, сверка SHA-256;
#   GitLab — подставные gitlab-backup / gitlab-ctl / gitlab-rake: проверка вызовов и переменных
#        (COMPRESS_CMD=cat для 16.7+, DECOMPRESS_CMD, GITLAB_ASSUME_YES), секреты и конфигурация.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/plugins.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-plugins}
HERE=$(cd "$(dirname "$0")" && pwd)
S3PORT=15101; VPORT=18201

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() { for f in moto vault; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done; [ -f "$W/manifest.bak" ] || rm -f /opt/gitlab/version-manifest.txt; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"
export HOPEBACKUP_PASSWORD=plugins-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init

# ------------------------------------------------------------------ S3
say "S3: эмулятор и бакет с данными"
moto_server -H 127.0.0.1 -p $S3PORT > "$W/moto.log" 2>&1 & echo $! > "$W/moto.pid"
for i in $(seq 1 50); do curl -s http://127.0.0.1:$S3PORT/ >/dev/null && break; sleep 0.2; done
PYS3="import boto3; s3=boto3.client('s3', endpoint_url='http://127.0.0.1:$S3PORT', aws_access_key_id='k', aws_secret_access_key='s', region_name='us-east-1')"
python3 - <<EOF
$PYS3
import os, random
s3.create_bucket(Bucket="app-data")
r = random.Random(7)
for i in range(300):
    s3.put_object(Bucket="app-data", Key=f"docs/{i:04d}.json", Body=os.urandom(r.randint(100, 40000)), ContentType="application/json", Metadata={"owner": f"user{i%5}", "src": "hb-test"})
s3.put_object(Bucket="app-data", Key="images/", Body=b"")
s3.put_object(Bucket="app-data", Key="images/logo.png", Body=os.urandom(150000), ContentType="image/png")
s3.put_object(Bucket="app-data", Key="dumps/big.bin", Body=os.urandom(70 << 20), ContentType="application/octet-stream", Metadata={"kind": "dump"})
print("объектов:", s3.list_objects_v2(Bucket="app-data")["KeyCount"])
EOF
CFG=$(jq -n --arg e "http://127.0.0.1:$S3PORT" '{endpoint:$e, region:"us-east-1", access_key:"k", secret_key:"s", buckets:["app-data"], path_style:true}')
R backup --plugin s3 --instance s3-app --config "$CFG" | tee "$W/s3-1.txt"
python3 -c "$PYS3
import os; s3.put_object(Bucket='app-data', Key='docs/0001.json', Body=os.urandom(5000), ContentType='application/json'); s3.put_object(Bucket='app-data', Key='docs/new.txt', Body=b'new object', ContentType='text/plain')"
R backup --plugin s3 --instance s3-app --config "$CFG" | tee "$W/s3-2.txt"
echo "вторая копия: $(grep -o 'новых [^,]*' "$W/s3-2.txt")"
SNAP=$(R list --instance s3-app | tail -1 | awk '{print $1}')
R show "$SNAP"
say "S3: восстановление в бакет app-restored и сверка"
R restore --plugin s3 --instance s3-app --snapshot "$SNAP" --params "$(echo "$CFG" | jq '. + {target_bucket:"app-restored", create_bucket:true}')"
python3 - <<EOF
$PYS3
import hashlib
def objs(b):
    out = {}
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=b):
        for o in page.get("Contents", []):
            g = s3.get_object(Bucket=b, Key=o["Key"])
            out[o["Key"]] = (hashlib.md5(g["Body"].read()).hexdigest(), g["ContentType"], g.get("Metadata", {}))
    return out
a, b = objs("app-data"), objs("app-restored")
assert a.keys() == b.keys(), (len(a), len(b), set(a) ^ set(b))
bad = [k for k in a if a[k] != b[k]]
assert not bad, [(k, a[k], b[k]) for k in bad[:3]]
print(f"ok: {len(a)} объектов совпали (содержимое, тип, метаданные), включая dumps/big.bin 70 МБ и «папку» images/")
EOF

# ------------------------------------------------------------------ Vault
say "Vault: снапшот raft и восстановление (имитатор API)"
python3 "$HERE/mocks/vault.py" $VPORT vault-token "$W/vault.jsonl" & echo $! > "$W/vault.pid"
for i in $(seq 1 50); do curl -s "http://127.0.0.1:$VPORT/v1/sys/health" >/dev/null && break; sleep 0.2; done
VCFG=$(jq -n --arg a "http://127.0.0.1:$VPORT" '{addr:$a, token:"vault-token", config_paths:[]}')
R backup --plugin vault --instance vault-prod --config "$VCFG"
VS=$(R list --instance vault-prod | tail -1 | awk '{print $1}')
R restore --plugin vault --instance vault-prod --snapshot "$VS" --params "$VCFG"
grep -q '"event": "restore", "bytes": 6291456, "match": true' "$W/vault.jsonl" || { cat "$W/vault.jsonl"; fail "Vault: восстановленный снапшот не совпал"; }
echo "ok: снапшот 6 МиБ восстановлен, SHA-256 совпал"
OUT=$(R backup --plugin vault --instance vault-prod --config "$(echo "$VCFG" | jq '.token="wrong"')" 2>&1 || true)
echo "$OUT" | grep -q "403" || fail "Vault: неверный токен — нет понятной ошибки: $OUT"; echo "ok: неверный токен — $(echo "$OUT" | tail -1 | cut -c1-90)"

# ------------------------------------------------------------------ GitLab
say "GitLab: подставные gitlab-backup / gitlab-ctl / gitlab-rake"
G="$W/gl"; mkdir -p "$G/bin" "$G/backups" "$G/etc/gitlab" "$G/data/repositories" "$G/restored"
[ -f /opt/gitlab/version-manifest.txt ] && cp /opt/gitlab/version-manifest.txt "$W/manifest.bak"
mkdir -p /opt/gitlab; printf 'gitlab-ee 16.11.1\n\nComponent  Installed Version\n' > /opt/gitlab/version-manifest.txt
for i in $(seq 1 30); do head -c 100000 /dev/urandom > "$G/data/repositories/repo$i.bundle"; done
echo "external_url 'https://gitlab.example'" > "$G/etc/gitlab/gitlab.rb"; echo '{"gitlab_rails":{"secret_key_base":"s3cr3t"}}' > "$G/etc/gitlab/gitlab-secrets.json"
cat > "$G/bin/gitlab-backup" <<EOF
#!/bin/sh
echo "gitlab-backup \$* COMPRESS_CMD=\${COMPRESS_CMD:-} DECOMPRESS_CMD=\${DECOMPRESS_CMD:-} GZIP_RSYNCABLE=\${GZIP_RSYNCABLE:-} ASSUME=\${GITLAB_ASSUME_YES:-}" >> "$G/calls.log"
cmd=\$1; shift; for a in "\$@"; do case \$a in BACKUP=*) B=\${a#BACKUP=};; esac; done
if [ "\$cmd" = create ]; then [ "\$COMPRESS_CMD" = cat ] || { echo "COMPRESS_CMD не задан" >&2; exit 2; }
  echo "Dumping database ... done"; tar -C "$G/data" -cf "$G/backups/\${B}_gitlab_backup.tar" . ; echo "Backup \$B is done."
else [ "\$GITLAB_ASSUME_YES" = 1 ] && [ "\$DECOMPRESS_CMD" = cat ] || { echo "нет GITLAB_ASSUME_YES/DECOMPRESS_CMD" >&2; exit 2; }
  tar -C "$G/restored" -xf "$G/backups/\${B}_gitlab_backup.tar"; echo "Restore task is done."; fi
EOF
printf '#!/bin/sh\necho "gitlab-ctl $*" >> "%s/calls.log"\n[ "$1" = status ] && echo "run: puma: (pid 1) 100s" && echo "run: sidekiq: (pid 2) 100s"\nexit 0\n' "$G" > "$G/bin/gitlab-ctl"
printf '#!/bin/sh\necho "gitlab-rake $*" >> "%s/calls.log"\necho "Checking GitLab ... Finished"\n' "$G" > "$G/bin/gitlab-rake"
chmod +x "$G/bin/"*
export PATH="$G/bin:$PATH"
GCFG=$(jq -n --arg b "$G/backups" --arg e "$G/etc/gitlab" '{mode:"omnibus", backup_dir:$b, config_paths:[($e+"/gitlab.rb"), ($e+"/gitlab-secrets.json")]}')
R backup --plugin gitlab --instance gitlab-prod --config "$GCFG"
ls "$G/backups" | grep -q . && fail "локальный архив GitLab не удалён после отправки" || echo "ok: локальный архив удалён"
R backup --plugin gitlab --instance gitlab-prod --config "$GCFG" | tee "$W/gl-2.txt"
echo "вторая копия GitLab: $(grep -o 'новых [^,]*' "$W/gl-2.txt")"
GS=$(R list --instance gitlab-prod | tail -1 | awk '{print $1}')
rm -f "$G/etc/gitlab/gitlab-secrets.json"
R restore --plugin gitlab --instance gitlab-prod --snapshot "$GS" --params "$GCFG"
diff -r "$G/data" "$G/restored" >/dev/null && echo "ok: данные GitLab восстановлены"
grep -q s3cr3t "$G/etc/gitlab/gitlab-secrets.json" && echo "ok: gitlab-secrets.json восстановлен"
for c in "gitlab-ctl reconfigure" "gitlab-ctl stop puma" "gitlab-ctl stop sidekiq" "gitlab-ctl restart" "gitlab-rake gitlab:doctor:secrets" "gitlab-rake gitlab:check SANITIZE=true"; do
  grep -q "^$c" "$G/calls.log" || fail "GitLab: не вызвано «$c»"
done
grep "gitlab-backup restore" "$G/calls.log" | grep -q "force=yes" || fail "GitLab: restore без force=yes"
echo "ok: порядок восстановления GitLab: $(grep -c . "$G/calls.log") вызовов"
printf 'gitlab-ee 17.0.0\n' > /opt/gitlab/version-manifest.txt
OUT=$(R restore --plugin gitlab --instance gitlab-prod --snapshot "$GS" --params "$GCFG" 2>&1 || true)
echo "$OUT" | grep -q "только в ту же версию" || fail "GitLab: другая версия на цели не отвергнута: $OUT"; echo "ok: другая версия GitLab на цели — $(echo "$OUT" | tail -1 | cut -c1-100)"
[ -f "$W/manifest.bak" ] && cp "$W/manifest.bak" /opt/gitlab/version-manifest.txt || rm -f /opt/gitlab/version-manifest.txt

R stats
printf '\n\033[1;32mПРОВЕРКА ПЛАГИНОВ S3, VAULT, GITLAB ПРОЙДЕНА\033[0m\n'
