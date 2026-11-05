#!/usr/bin/env bash
# Проверка площадок: диск сервера управления и облачные диски (нужны root, jq, curl, python3,
# pip install --break-system-packages wsgidav cheroot):
#   Nextcloud/WebDAV — настоящий сервер WebDAV (wsgidav) с адресом как у Nextcloud и паролем приложения;
#   Яндекс Диск и Google Диск — имитаторы API (tests/mocks/clouds.py, с ответами 429 для проверки повторов);
#   для каждого облака через `hopebackup repo --cloud`: репозиторий, две копии (дедупликация), список,
#   восстановление со сверкой, очистка и восстановление оставшейся копии;
#   сервер управления: площадка «диск сервера» (/node/<id>) и облачные площадки с входом OAuth по коду
#   устройства — копия агентом, очистка по политике хранения.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/storage.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-storage}
HERE=$(cd "$(dirname "$0")" && pwd)
DAV=15301; YA=15302; GD=15303; SRV_PORT=17643

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
DISKDIR=/mnt/hb-storage-disk
cleanup() { for f in dav ya gd server agent; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done; rm -rf "$DISKDIR"; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/src/sub" "$W/davroot"
export HOPEBACKUP_PASSWORD=storage-repo-password HOPEBACKUP_DATA="$W/data"

say "Данные для копий"
head -c 6000000 /dev/urandom > "$W/src/big.bin"
for i in $(seq 1 40); do head -c $((RANDOM * 3)) /dev/urandom > "$W/src/sub/f$i.dat"; done
echo "конфигурация" > "$W/src/app.conf"

say "Облака: WebDAV (как Nextcloud), имитаторы Яндекс Диска и Google Диска"
cat > "$W/dav.yaml" <<EOF
host: 127.0.0.1
port: $DAV
provider_mapping:
  "/remote.php/dav/files/hb": "$W/davroot"
simple_dc:
  user_mapping:
    "*":
      "hb":
        password: "app-pass-123"
http_authenticator:
  accept_basic: true
  accept_digest: false
  default_to_digest: false
verbose: 0
EOF
wsgidav --config "$W/dav.yaml" > "$W/dav.log" 2>&1 & echo $! > "$W/dav.pid"
python3 "$HERE/mocks/clouds.py" yadisk $YA ya-token-1 > "$W/ya.log" 2>&1 & echo $! > "$W/ya.pid"
python3 "$HERE/mocks/clouds.py" gdrive $GD > "$W/gd.log" 2>&1 & echo $! > "$W/gd.pid"
for p in $DAV $YA $GD; do for i in $(seq 1 50); do curl -s -o /dev/null http://127.0.0.1:$p/ && break; sleep 0.2; done; done

C_DAV="{\"provider\":\"webdav\",\"url\":\"http://127.0.0.1:$DAV/remote.php/dav/files/hb\",\"username\":\"hb\",\"password\":\"app-pass-123\",\"folder\":\"Backups/HopeBackup\"}"
C_YA="{\"provider\":\"yadisk\",\"access_token\":\"ya-token-1\",\"folder\":\"app:/HopeBackup\",\"endpoint\":\"http://127.0.0.1:$YA\"}"
C_GD="{\"provider\":\"gdrive\",\"client_id\":\"cid\",\"client_secret\":\"cs\",\"refresh_token\":\"refresh-gdrive\",\"folder\":\"HopeBackup\",\"endpoint\":\"http://127.0.0.1:$GD\"}"

cloud_cycle() {
  local name=$1 cfg=$2
  say "$name: репозиторий, две копии, восстановление, очистка"
  R() { "$BIN" repo --url cloud:// --cloud "$cfg" "$@"; }
  R init
  R backup --instance files --kind files "$W/src" | tee "$W/$name-b1.txt"
  echo "изменение" >> "$W/src/app.conf"
  R backup --instance files --kind files "$W/src" | tee "$W/$name-b2.txt"
  grep -q "новых" "$W/$name-b2.txt" || fail "$name: нет второй копии"
  [ "$(R list | wc -l)" -eq 2 ] || fail "$name: должно быть 2 снапшота"
  local first last
  first=$(R list | head -1 | awk '{print $1}'); last=$(R list | tail -1 | awk '{print $1}')
  R show "$last" | head -3
  rm -rf "$W/out-$name"; R restore --instance files --snapshot "$last" --dest "$W/out-$name"
  diff -r "$W/src" "$W/out-$name$W/src" || fail "$name: восстановленные файлы не совпали"
  R prune --keep-last 1 | tee "$W/$name-prune.txt"
  [ "$(R list | wc -l)" -eq 1 ] || fail "$name: после очистки должен остаться 1 снапшот"
  rm -rf "$W/out2-$name"; R restore --instance files --snapshot "$last" --dest "$W/out2-$name"
  diff -r "$W/src" "$W/out2-$name$W/src" || fail "$name: после очистки копия не восстанавливается"
  echo "ok: $name"
}
cloud_cycle webdav "$C_DAV"
find "$W/davroot/Backups/HopeBackup/main" -maxdepth 1 | sort | sed "s|$W/davroot|  nextcloud:|"
cloud_cycle yadisk "$C_YA"
cloud_cycle gdrive "$C_GD"

say "Неверный пароль приложения WebDAV — понятная ошибка"
BAD=$(echo "$C_DAV" | sed 's/app-pass-123/wrong/')
"$BIN" repo --url cloud:// --cloud "$BAD" list 2>&1 | tee "$W/bad.txt" | tail -1 || true
grep -q "401" "$W/bad.txt" || fail "нет ошибки 401"

# ------------------------------------------------------------------ сервер управления
say "Сервер управления: площадка «диск сервера» и облачные площадки"
unset HOPEBACKUP_DATA
HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat $W/srv/server/initial_admin_password.txt)\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() { local id=$1 st; for i in $(seq 1 300); do st=$(api GET /jobs/$id | jq -r .status); [ "$st" = ok ] && return 0; [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail; fail "задание: $(api GET /jobs/$id | jq -r .error)"; }; sleep 1; done; fail "задание не завершилось"; }

api POST /sites -d "{\"name\":\"Плохой\",\"kind\":\"disk\",\"disk_path\":\"/etc/hb\",\"public_url\":\"https://127.0.0.1:$SRV_PORT\",\"password\":\"disk-password-1\"}" | jq -e '.error | test("не подходит")' >/dev/null || fail "каталог в /etc должен быть отклонён"
mkdir -p "$DISKDIR"
DISK=$(api POST /sites -d "{\"name\":\"Диск сервера\",\"kind\":\"disk\",\"disk_path\":\"$DISKDIR/hopebackup\",\"public_url\":\"https://127.0.0.1:$SRV_PORT\",\"password\":\"disk-password-1\"}")
echo "$DISK" | jq -c '{id, kind, url, disk}'
DISK=$(echo "$DISK" | jq -r .id); [ "$DISK" != null ] || fail "площадка на диске не создана"
J=$(api POST /sites/$DISK/init | jq -r .job_id); wait_job "$J"
[ -f "$DISKDIR/hopebackup/repos/main/config.json" ] || fail "репозиторий не создан на диске"
code=$(curl -sk -o /dev/null -w '%{http_code}' -X PUT "https://127.0.0.1:$SRV_PORT/node/$DISK/v1/r/main/pack/$(printf 'a%.0s' $(seq 64))" --data-binary @"$W/src/big.bin")
[ "$code" = 401 ] || fail "встроенный узел принял данные без токена (код $code)"
echo "ok: без токена встроенный узел отвечает 401"

say "Google Диск: вход по коду устройства (OAuth)"
FL=$(api POST /sites/oauth/start -d "{\"provider\":\"gdrive\",\"client_id\":\"cid\",\"client_secret\":\"cs\",\"endpoint\":\"http://127.0.0.1:$GD\"}")
echo "$FL" | jq -c '{user_code, verification_url}'
FLOW=$(echo "$FL" | jq -r .flow)
for i in $(seq 1 30); do st=$(api GET /sites/oauth/$FLOW | jq -r .status); [ "$st" = done ] && break; [ "$st" = error ] && fail "вход: $(api GET /sites/oauth/$FLOW | jq -r .error)"; sleep 1; done
[ "$st" = done ] || fail "вход OAuth не завершился"
GSITE=$(api POST /sites -d "{\"name\":\"Google Диск\",\"kind\":\"cloud\",\"password\":\"gdrive-password\",\"repo\":\"srv\",\"cloud\":{\"provider\":\"gdrive\",\"client_id\":\"cid\",\"client_secret\":\"cs\",\"folder\":\"HopeBackup\",\"endpoint\":\"http://127.0.0.1:$GD\",\"oauth_flow\":\"$FLOW\"}}")
echo "$GSITE" | jq -c '{id, url, cloud: .cloud | {account, refresh_token, client_secret, connected}}'
echo "$GSITE" | jq -e '.cloud.refresh_token == "********" and .cloud.account == "hb@test.example"' >/dev/null || fail "площадка Google: нет учётной записи или секрет не замаскирован"
GSITE=$(echo "$GSITE" | jq -r .id)
J=$(api POST /sites/$GSITE/init | jq -r .job_id); wait_job "$J"

say "Яндекс Диск: вход по коду устройства"
FLOW=$(api POST /sites/oauth/start -d "{\"provider\":\"yadisk\",\"client_id\":\"ycid\",\"client_secret\":\"ycs\",\"endpoint\":\"http://127.0.0.1:$YA\"}" | jq -r .flow)
for i in $(seq 1 30); do st=$(api GET /sites/oauth/$FLOW | jq -r .status); [ "$st" = done ] && break; sleep 1; done
YSITE=$(api POST /sites -d "{\"name\":\"Яндекс Диск\",\"kind\":\"cloud\",\"password\":\"yadisk-password\",\"repo\":\"srv\",\"cloud\":{\"provider\":\"yadisk\",\"client_id\":\"ycid\",\"client_secret\":\"ycs\",\"folder\":\"app:/HopeBackup\",\"endpoint\":\"http://127.0.0.1:$YA\",\"oauth_flow\":\"$FLOW\"}}")
echo "$YSITE" | jq -c '{id, url, cloud: .cloud | {account, access_token, token_expires}}'
YSITE=$(echo "$YSITE" | jq -r .id); [ "$YSITE" != null ] || fail "площадка Яндекс Диска не создана"
J=$(api POST /sites/$YSITE/init | jq -r .job_id); wait_job "$J"

say "Агент: копии на диск сервера, Google Диск и Яндекс Диск"
SRV_FP=$(api GET /status | jq -r .fingerprint)
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$SRV_FP" --name st-host >/dev/null
HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
sleep 2
AGENT=$(api GET /agents | jq -r '.[0].id')
for site in $DISK $GSITE $YSITE; do
  INST=$(api POST /instances -d "{\"name\":\"files-$site\",\"catalog_id\":\"files\",\"agent_id\":\"$AGENT\",\"site_id\":\"$site\",\"config\":{\"paths\":[\"$W/src\"],\"exclude\":[\"*.conf\"]},\"retention\":{\"keep_last\":1}}" | jq -r .id)
  for n in 1 2; do J=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_job "$J"; done
  api GET /jobs/$J | jq -r '.log[].m' | grep -E "архив|готово" | sed 's/^/  /'
  # Явно указанный каталог в /tmp не отменяется общим исключением /tmp; *.conf исключён.
  api GET /jobs/$J | jq -r '.log[].m' | grep -q "архив: файлов 41" || fail "в архиве должно быть 41 файл (без *.conf)"
  sleep 3
  N=$(api GET /instances/$INST/points | jq '.snapshots | length')
  echo "  площадка $site: точек восстановления $N (хранить 1)"
  [ "$N" -eq 1 ] || fail "очистка по политике хранения на площадке $site не сработала"
done
api POST /sites/$DISK/test | jq -c '{repo_exists, stats: .stats | {packs, snapshots}}'
api POST /sites/$GSITE/test | jq -c '{repo_exists, cloud}'
api GET /sites | jq -c '.[] | select(.kind=="disk") | .disk'
ls "$DISKDIR/hopebackup/repos/main/packs" | head -3 | sed 's/^/  pack-каталог на диске: /'

printf '\n\033[1;32mПРОВЕРКА ПЛОЩАДОК: ДИСК СЕРВЕРА, NEXTCLOUD/WEBDAV, ЯНДЕКС ДИСК, GOOGLE ДИСК ПРОЙДЕНА\033[0m\n'
