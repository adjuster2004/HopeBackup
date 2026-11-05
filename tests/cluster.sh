#!/usr/bin/env bash
# Проверка принимающих узлов (нужны root, jq, curl):
#   площадка A (узел N1) реплицируется на площадку B (узел N2); сервер S1 активный, S2 — принимающий;
#   «потеря» основной площадки (S1 и N1 остановлены) → на S2 «Принять управление» → агент сам
#   переключается на S2 → экземпляр переводится на B → копия и восстановление с B → S1 возвращается
#   и сам становится принимающим (эпоха).
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/cluster.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-cluster}
P1=17961; P2=17962; P3=17963; N1=18461; N2=18462

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() { for f in agent s1 s2 s3 n1 n2; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; sleep 0.5; rm -rf "$W"; mkdir -p "$W"

node() { # $1 имя $2 порт
  "$BIN" node init --data "$W/$1" --listen 127.0.0.1:$2 > "$W/$1-init.txt"
  "$BIN" node run --data "$W/$1" > "$W/$1.log" 2>&1 & echo $! > "$W/$1.pid"
}
server() { # $1 имя $2 порт
  HOPEBACKUP_DATA="$W/$1" "$BIN" server --listen 127.0.0.1:$2 >> "$W/$1.log" 2>&1 & echo $! > "$W/$1.pid"
  for i in $(seq 1 50); do curl -sk https://127.0.0.1:$2/api/health >/dev/null && break; sleep 0.2; done
}
login() { curl -sk https://127.0.0.1:$1/api/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$2\"}" | jq -r .token; }
api() { local port=$1 tok=$2 m=$3 p=$4; shift 4; curl -sk -X "$m" "https://127.0.0.1:$port/api$p" -H "authorization: Bearer $tok" -H 'content-type: application/json' "$@"; }
wait_job() { local port=$1 tok=$2 id=$3 st; for i in $(seq 1 120); do st=$(api $port $tok GET /jobs/$id | jq -r .status); [ "$st" = ok ] && return 0; [ "$st" = failed ] && { api $port $tok GET /jobs/$id | jq -r '.log[].m' | tail; fail "задание $id: $(api $port $tok GET /jobs/$id | jq -r .error)"; }; sleep 1; done; fail "задание $id не завершилось"; }
field() { grep "$2" "$W/$1-init.txt" | awk '{print $NF}'; }

say "Узлы хранения: A (N1) и B (N2)"
node n1 $N1; node n2 $N2
say "Серверы управления S1 (активный) и S2 (будущий принимающий)"
server s1 $P1; server s2 $P2
PW1=$(cat "$W/s1/server/initial_admin_password.txt"); PW2=$(cat "$W/s2/server/initial_admin_password.txt")
T1=$(login $P1 "$PW1"); T2=$(login $P2 "$PW2")

say "Площадка A с принимающей площадкой B"
B=$(api $P1 $T1 POST /sites -d "{\"name\":\"B · резервная\",\"url\":\"https://127.0.0.1:$N2\",\"fingerprint\":\"$(field n2 Отпечаток)\",\"token\":\"$(field n2 append)\",\"prune_token\":\"$(field n2 администратора)\",\"password\":\"cluster-pass-1\"}" | jq -r .id)
A=$(api $P1 $T1 POST /sites -d "{\"name\":\"A · основная\",\"url\":\"https://127.0.0.1:$N1\",\"fingerprint\":\"$(field n1 Отпечаток)\",\"token\":\"$(field n1 append)\",\"prune_token\":\"$(field n1 администратора)\",\"password\":\"cluster-pass-1\",\"replicas\":[\"$B\"],\"replication_interval_min\":1}" | jq -r .id)
J=$(api $P1 $T1 POST /sites/$A/init | jq -r .job_id); wait_job $P1 $T1 "$J"
# У принимающей площадки репозиторий создаёт репликация: «Создать репозиторий» отказывает с объяснением.
R=$(api $P1 $T1 POST /sites/$B/init -w '\n%{http_code}')
[ "$(echo "$R" | tail -1)" = 400 ] && echo "$R" | grep -q "принимающая площадка для «A · основная»" || fail "создание репозитория на принимающей площадке должно быть запрещено: $R"
# Узел на «/» отдаёт страницу для браузера (без токена), API по-прежнему требует токен.
for i in $(seq 1 50); do curl -sk "https://127.0.0.1:$N1/v1/health" >/dev/null && break; sleep 0.2; done
# Страница — целиком в переменную: при pipefail «curl | grep -q» падает, если grep закрыл трубу раньше, чем curl дописал.
PAGE=$(curl -sk "https://127.0.0.1:$N1/" || true)
grep -q "Узел хранения HopeBackup" <<< "$PAGE" || fail "узел: нет страницы на /"
[ "$(curl -sk -o /dev/null -w '%{http_code}' "https://127.0.0.1:$N1/v1/r/main/snapshots")" = 401 ] || fail "узел: API без токена должен отвечать 401"
api $P1 $T1 GET /sites | jq -c '.[] | {name, replicas}'

say "Агент и экземпляр файлов на площадке A"
FP1=$(api $P1 $T1 GET /status | jq -r .fingerprint)
ET=$(api $P1 $T1 POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$P1 --token "$ET" --fingerprint "$FP1" --name app-01 >/dev/null
HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
sleep 2
AG=$(api $P1 $T1 GET /agents | jq -r '.[0].id')
mkdir -p "$W/data"; for i in $(seq 1 20); do head -c 200000 /dev/urandom > "$W/data/f$i.bin"; done; echo "версия 1" > "$W/data/state.txt"
INST=$(api $P1 $T1 POST /instances -d "{\"name\":\"app-config\",\"catalog_id\":\"files\",\"agent_id\":\"$AG\",\"site_id\":\"$A\",\"config\":{\"paths\":[\"$W/data\"]}}" | jq -r .id)

say "S2 подключается принимающим узлом к S1"
CODE=$(api $P1 $T1 POST /cluster/invite -d "{\"self_url\":\"https://127.0.0.1:$P1\",\"name\":\"S1 · ЦОД-1\"}" | jq -r .code)
api $P2 $T2 POST /cluster/join -d "{\"code\":\"$CODE\",\"self_url\":\"https://127.0.0.1:$P2\",\"name\":\"S2 · ЦОД-2\"}" | jq -c .
# Каталог (и пользователи) теперь от S1: вход на S2 — паролем S1.
T2=$(login $P2 "$PW1"); [ "$T2" != null ] || fail "вход на S2 с учётной записью S1"
api $P2 $T2 GET /cluster | jq -c '{role, epoch, active_url, peers: [.peers[].name]}'
[ "$(api $P2 $T2 GET /cluster | jq -r .role)" = standby ] || fail "S2 не принимающий"
[ "$(api $P2 $T2 GET /instances | jq length)" = 1 ] || fail "каталог не передан на S2"
R=$(api $P2 $T2 PUT /sites/$A -d '{"name":"x","url":"https://x:1","fingerprint":"aa"}'); echo "изменение на принимающем: $(echo "$R" | jq -r .error | cut -c1-80)…"

say "S3 подключается с активного по коду узла (доступ только S1 → S3)"
HOPEBACKUP_UPDATER_FORCE=1 HOPEBACKUP_DATA="$W/s3" "$BIN" server --listen 127.0.0.1:$P3 >> "$W/s3.log" 2>&1 & echo $! > "$W/s3.pid"
for i in $(seq 1 50); do curl -sk https://127.0.0.1:$P3/api/health >/dev/null && break; sleep 0.2; done
T3=$(login $P3 "$(cat "$W/s3/server/initial_admin_password.txt")")
OC=$(api $P3 $T3 POST /cluster/offer -d "{\"self_url\":\"https://127.0.0.1:$P3\",\"name\":\"S3 · ЦОД-3\"}" | jq -r .code)
[ -n "$OC" ] && [ "$OC" != null ] || fail "код подключения S3"
# Код приглашения активного узла здесь не подходит — понятная подсказка.
R0=$(api $P1 $T1 POST /cluster/adopt -d "{\"code\":\"$CODE\"}"); echo "$R0" | grep -q "код приглашения активного узла" || fail "adopt: код приглашения должен отклоняться с подсказкой: $R0"
R=$(api $P1 $T1 POST /cluster/adopt -d "{\"code\":\"$OC\",\"self_url\":\"https://127.0.0.1:$P1\",\"name\":\"S1 · ЦОД-1\"}")
echo "$R" | jq -e .ok >/dev/null || fail "подключение S3 по коду: $R"
S3ID=$(echo "$R" | jq -r .id)
# Код одноразовый.
api $P1 $T1 POST /cluster/adopt -d "{\"code\":\"$OC\"}" | grep -q "неверный или истёк" || fail "повторное подключение по тому же коду должно отказывать"
T3=$(login $P3 "$PW1"); [ "$T3" != null ] || fail "вход на S3 с учётной записью S1"
[ "$(api $P3 $T3 GET /cluster | jq -r .role)" = standby ] || fail "S3 не принимающий"
[ "$(api $P3 $T3 GET /instances | jq length)" = 1 ] || fail "каталог не передан на S3"
for i in $(seq 1 40); do V3=$(api $P1 $T1 GET /cluster | jq -r --arg id "$S3ID" '.peers[] | select(.id==$id) | select(.sync.ok==true) | .version // empty'); [ -n "$V3" ] && break; sleep 1; done
[ "$V3" = "$("$BIN" --version | awk '{print $2}')" ] || fail "S1 не видит версию S3 (синхронизация): $(api $P1 $T1 GET /cluster | jq -c '.peers')"
echo "ok: S3 принимающий, синхронизация с S1 идёт, версия S3 — $V3"
# Обновление S3 с S1: пакет собран из программы S1 и принят S3 (подпись верна), но он не новее — отказ.
api $P1 $T1 POST /cluster/peers/$S3ID/update | jq -e .ok >/dev/null || fail "обновление S3 не запустилось"
for i in $(seq 1 60); do U=$(api $P1 $T1 GET /cluster | jq -c --arg id "$S3ID" '.peers[] | select(.id==$id) | .update'); [ "$(echo "$U" | jq -r .running)" = false ] && break; sleep 1; done
echo "обновление S3: $U"
echo "$U" | jq -r .error | grep -q "не новее" || fail "S3 должен принять пакет и отказать только из-за версии: $U"
# Чужая подпись — 401.
[ "$(curl -sk -o /dev/null -w '%{http_code}' -X POST https://127.0.0.1:$P3/api/cluster/peer/update -H "x-hopebackup-ts: $(date +%s)" -H "x-hopebackup-sha256: $(printf x | sha256sum | awk '{print $1}')" -H 'x-hopebackup-sig: 00' --data-binary x)" = 401 ] || fail "обновление без подписи кластера должно отклоняться"
api $P1 $T1 DELETE /cluster/peers/$S3ID >/dev/null; kill "$(cat "$W/s3.pid")"; rm -f "$W/s3.pid"

say "Копия на S1 → репликация A → B"
J=$(api $P1 $T1 POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_job $P1 $T1 "$J"
for i in $(seq 1 60); do RJ=$(api $P1 $T1 GET /jobs | jq -r '[.[] | select(.kind=="replicate")][0].id // empty'); [ -n "$RJ" ] && break; sleep 1; done
[ -n "$RJ" ] || fail "задание репликации не появилось"
wait_job $P1 $T1 "$RJ"; api $P1 $T1 GET /jobs/$RJ | jq -r '.log[].m'
sleep 32   # синхронизация каталога (30 с)
N=$(api $P2 $T2 GET "/instances/$INST/points?site=$B" | jq '.snapshots | length')
echo "на S2 видно точек на площадке B: $N"; [ "$N" -ge 1 ] || fail "точки на B не видны с S2"
api $P2 $T2 GET /agents | jq -r '.[0].name' | grep -q app-01 || fail "агенты не переданы на S2"
grep -q "\"url\": \"https://127.0.0.1:$P2\"" "$W/agent/agent.json" || fail "агент не узнал о S2"

say "Потеря площадки A: S1 и N1 остановлены"
kill "$(cat "$W/s1.pid")" "$(cat "$W/n1.pid")"; rm -f "$W/s1.pid" "$W/n1.pid"
echo "версия 2 — после аварии" > "$W/data/state.txt"
sleep 2

say "На S2: «Принять управление»"
api $P2 $T2 POST /cluster/takeover -d '{}' | jq -c .
[ "$(api $P2 $T2 GET /cluster | jq -r .role)" = active ] || fail "S2 не стал активным"
for i in $(seq 1 90); do grep -q "\"server\": \"https://127.0.0.1:$P2\"" "$W/agent/agent.json" && break; sleep 1; done
grep -q "\"server\": \"https://127.0.0.1:$P2\"" "$W/agent/agent.json" || fail "агент не переключился на S2"
echo "агент переключился на S2: $(grep 'переключение' "$W/agent.log" | tail -1 | cut -c1-140)"

say "Экземпляр — на площадку B, копия и восстановление"
api $P2 $T2 POST /sites/$A/failover -d "{\"to\":\"$B\"}" | jq -c .
J=$(api $P2 $T2 POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_job $P2 $T2 "$J"
SNAP=$(api $P2 $T2 GET "/instances/$INST/points" | jq -r '.snapshots[0].id')
TG=$(api $P2 $T2 POST /targets -d "{\"name\":\"app-01\",\"kind\":\"agent\",\"agent_id\":\"$AG\"}" | jq -r .id)
J=$(api $P2 $T2 POST /restore -d "{\"instance_id\":\"$INST\",\"snapshot_id\":\"$SNAP\",\"target_id\":\"$TG\",\"params\":{\"root\":\"$W/restored\"}}" | jq -r .job_id); wait_job $P2 $T2 "$J"
diff -r "$W/data" "$W/restored$W/data" && echo "ok: восстановлено с площадки B, данные после аварии на месте"

say "Прежний активный S1 возвращается и сам становится принимающим"
server s1 $P1
for i in $(seq 1 45); do [ "$(api $P1 $T1 GET /cluster | jq -r .role)" = standby ] && break; sleep 1; done
api $P1 $T1 GET /cluster | jq -c '{role, epoch, active_url}'
[ "$(api $P1 $T1 GET /cluster | jq -r .role)" = standby ] || fail "S1 не стал принимающим"
sleep 3
grep -q "\"server\": \"https://127.0.0.1:$P2\"" "$W/agent/agent.json" || fail "агент вернулся на устаревший узел"
api $P2 $T2 GET /audit | jq -r '.[] | select(.action|test("cluster|failover")) | "\(.action) \(.detail)"' | head -6

printf '\n\033[1;32mПРОВЕРКА ПРИНИМАЮЩИХ УЗЛОВ ПРОЙДЕНА\033[0m\n'
