#!/usr/bin/env bash
# Проверка устойчивости к обрывам посреди копии (нужны jq, curl):
#   площадка — отдельный узел хранения, сервер управления и агент — на этой машине;
#   1) сервер управления «падает» (kill -9) посреди копии: агент докопирует, сохранит итог на диск и
#      отправит его, когда сервер поднимется — задание засчитано, снапшот на площадке;
#   2) площадка недоступна 10 с посреди копии: агент ждёт её (предел стенда — 30 с) и докопирует без ошибки;
#   3) площадка недоступна дольше предела: копия падает, через «интервал» (стенд: минута = 1 с) ставится
#      повтор, он проходит; при исчерпании повторов — «повторов больше не будет»;
#   4) итог задания потерян (сервер лежал, итог агента удалён): перед повтором сервер находит снапшот
#      задания на площадке и засчитывает его;
#   5) агент «падает» посреди копии: задание прервано, повтор снимает копию;
#   6) догон расписания: запуск пришёлся на время, когда сервер был выключен, — копия после старта.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/resilience.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-res}
NODE_PORT=18543; SRV_PORT=18043

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; for f in server agent node; do echo "--- $f.log"; tail -15 "$W/$f.log" 2>/dev/null; done; exit 1; }
cleanup() { for f in server agent node; do [ -f "$W/$f.pid" ] && kill -9 "$(cat "$W/$f.pid")" 2>/dev/null || true; done; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/src"

# Пределы стенда (только при HOPEBACKUP_TESTING=1): минута политики повторов = 1 с, площадку ждать 30 с.
export HOPEBACKUP_TESTING=1 HOPEBACKUP_TEST_MINUTE_SECS=1 HOPEBACKUP_TEST_RETRY_DELAY=1 HOPEBACKUP_TEST_RETRY_EVERY=1 HOPEBACKUP_TEST_CATCHUP_EVERY=2 HOPEBACKUP_TEST_NET_PATIENCE=30 HOPEBACKUP_TEST_NOTIFY_DELAY=3600

data() { rm -rf "$W/src"/*; for i in $(seq 1 "${1:-6}"); do head -c 64M /dev/urandom > "$W/src/f$i.bin"; done; }

say "Узел хранения, сервер, агент"
"$BIN" node init --data "$W/node" --listen 127.0.0.1:$NODE_PORT > "$W/node-init.txt"
NODE_FP=$(grep 'Отпечаток' "$W/node-init.txt" | awk '{print $NF}')
NODE_ADMIN=$(grep 'администратора' "$W/node-init.txt" | awk '{print $NF}')
NODE_APPEND=$(grep 'append' "$W/node-init.txt" | awk '{print $NF}')
node_up() { "$BIN" node run --data "$W/node" >> "$W/node.log" 2>&1 & echo $! > "$W/node.pid"; for i in $(seq 1 50); do curl -sk https://127.0.0.1:$NODE_PORT/v1/health >/dev/null && break; sleep 0.1; done; }
node_down() { kill -9 "$(cat "$W/node.pid")"; rm -f "$W/node.pid"; }
node_up
server_up() {
  HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT >> "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
  for i in $(seq 1 100); do curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null 2>&1 && break; sleep 0.1; done
}
server_down() { kill -9 "$(cat "$W/server.pid")"; rm -f "$W/server.pid"; }
server_up
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat $W/srv/server/initial_admin_password.txt)\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
st() { api GET /jobs/$1 | jq -r .status; }
wait_end() { for i in $(seq 1 300); do s=$(st "$1"); [ "$s" = ok ] || [ "$s" = failed ] && { echo "$s"; return; }; sleep 0.5; done; echo timeout; }
wait_running() { for i in $(seq 1 200); do [ "$(st "$1")" = running ] && [ "$(api GET /jobs/$1 | jq '.log | length')" -gt 1 ] && return 0; sleep 0.1; done; fail "задание $1 не началось"; }
SITE=$(api POST /sites -d "$(jq -n --arg fp "$NODE_FP" --arg t "$NODE_APPEND" --arg pt "$NODE_ADMIN" --argjson np $NODE_PORT '{name:"Площадка", url:("https://127.0.0.1:"+($np|tostring)), fingerprint:$fp, repo:"main", token:$t, password:"repo-password-1", prune_token:$pt, compression:1}')" | jq -r .id)
[ "$(wait_end "$(api POST /sites/$SITE/init | jq -r .job_id)")" = ok ] || fail "репозиторий"
ENR=$(api POST /agents/enroll-token -d '{}')
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$(echo "$ENR" | jq -r .token)" --fingerprint "$(echo "$ENR" | jq -r .server_fingerprint)" --name res-host >/dev/null
agent_up() { HOPEBACKUP_DATA="$W/agent" "$BIN" agent run >> "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"; }
agent_down() { kill -9 "$(cat "$W/agent.pid")"; rm -f "$W/agent.pid"; }
agent_up
for i in $(seq 1 50); do [ "$(api GET /agents | jq -r '.[0].online')" = true ] && break; sleep 0.2; done
AGENT=$(api GET /agents | jq -r '.[0].id')
INST=$(api POST /instances -d "$(jq -n --arg a "$AGENT" --arg s "$SITE" --arg p "$W/src" '{name:"files-res", catalog_id:"files", agent_id:$a, site_id:$s, config:{paths:[$p], snapshot:"off"}, schedule:{enabled:false, full:"0 1 * * *"}, retention:{keep_last:30}, retry_mode:"custom", retry:{enabled:true, attempts:2, interval_min:3}}')" | jq -r .id)
[ "$INST" != null ] || fail "экземпляр"
api GET /instances/$INST | jq -e '.retry == {"enabled":true,"attempts":2,"interval_min":3}' >/dev/null || fail "своя политика повторов не сохранилась"
snaps_of() { api GET "/instances/$INST/points" | jq -r '.snapshots[] | "\(.id) \(.extra.job // "")"'; }

say "1) Сервер управления падает посреди копии"
data 8
J=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_running "$J"
server_down; echo "  сервер остановлен (kill -9), задание $J выполняется у агента"
for i in $(seq 1 120); do ls "$W/agent/outbox/$J.json" >/dev/null 2>&1 && break; sleep 0.5; done
[ -f "$W/agent/outbox/$J.json" ] || fail "агент не сохранил итог на диск"
[ "$(stat -c %a "$W/agent/outbox/$J.json")" = 600 ] || fail "файл итога должен быть 0600"
echo "  агент докопировал и сохранил итог: $(jq -c '{ok, snapshot: .result.snapshot.id}' "$W/agent/outbox/$J.json")"
server_up
for i in $(seq 1 120); do [ "$(st "$J")" = ok ] && break; sleep 0.5; done
[ "$(st "$J")" = ok ] || fail "после подъёма сервера задание не засчитано ($(st "$J"))"
for i in $(seq 1 20); do [ -f "$W/agent/outbox/$J.json" ] || break; sleep 0.5; done
[ ! -f "$W/agent/outbox/$J.json" ] || fail "итог не удалён после доставки"
SNAP=$(api GET /jobs/$J | jq -r .result.snapshot.id)
snaps_of | grep -q "^$SNAP $J$" || fail "снапшот задания с меткой задания не найден на площадке"
api GET /instances/$INST | jq -e --arg s "$SNAP" '.runtime.last_backup.snapshot_id == $s' >/dev/null || fail "последняя копия экземпляра не учтена"
echo "ok: итог доставлен после подъёма сервера, снапшот $SNAP на площадке"

say "2) Площадка недоступна 8 с посреди копии — агент ждёт и докопирует"
data 8
J=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_running "$J"
node_down; sleep 8; node_up
[ "$(wait_end "$J")" = ok ] || fail "копия не пережила короткий обрыв: $(api GET /jobs/$J | jq -r .error)"
grep -q "жду площадку" "$W/agent.log" || fail "агент не ждал площадку"
echo "ok: копия пережила обрыв связи с площадкой"

say "3) Площадка недоступна дольше предела — копия падает, повтор проходит"
data 6
J=$(api POST /instances/$INST/backup -d '{"retry":true}' | jq -r .job_id); wait_running "$J"
node_down
[ "$(wait_end "$J")" = failed ] || fail "копия должна упасть"
node_up
api GET /jobs/$J | jq -r .error | grep -qv "Broken pipe" || fail "ошибка копии — следствие («Broken pipe»), а не причина"
api GET /jobs/$J | jq -e '.origin == "manual_retry"' >/dev/null || fail "происхождение задания"
for i in $(seq 1 30); do [ "$(api GET /jobs/$J | jq -r '.retry_at // empty')" != "" ] && break; sleep 0.5; done
api GET /jobs/$J | jq -r '.log[].m' | grep -q "повтор в" || fail "повтор не назначен"
for i in $(seq 1 60); do R=$(api GET /jobs/$J | jq -r '.retry_job // empty'); [ -n "$R" ] && break; sleep 0.5; done
[ -n "$R" ] && [ "$R" != skipped ] || fail "повтор не поставлен"
[ "$(wait_end "$R")" = ok ] || fail "повтор не прошёл"
api GET /jobs/$R | jq -e '.origin == "retry" and .attempt == 2' >/dev/null || fail "повтор: попытка 2"
echo "ok: копия упала ($J), повтор $R прошёл"
# Повторы исчерпаны: площадка недоступна всё время.
data 2
node_down
J=$(api POST /instances/$INST/backup -d '{"retry":true}' | jq -r .job_id)
for i in $(seq 1 240); do
  last=$(api GET /jobs | jq -r --arg i "$INST" '[.[] | select(.instance_id==$i and .kind=="backup")] | sort_by(.created) | last')
  echo "$last" | jq -e '.status=="failed" and .attempt==3 and .retry_decided==true' >/dev/null 2>&1 && break; sleep 1
done
echo "$last" | jq -e '.attempt==3' >/dev/null || fail "ожидалось 3 попытки (1 + 2 повтора)"
api GET /jobs/$(echo "$last" | jq -r .id) | jq -r '.log[].m' | grep -q "повторов больше не будет" || fail "нет отметки об исчерпании повторов"
api GET /jobs/$J | jq -r .error | grep -q "недоступна" || fail "в ошибке задания должна быть причина (площадка недоступна)"
node_up
echo "ok: 3 попытки, затем «повторов больше не будет»"

say "4) Итог задания потерян — сервер находит снапшот задания на площадке"
data 6
J=$(api POST /instances/$INST/backup -d '{"retry":true}' | jq -r .job_id); wait_running "$J"
server_down
for i in $(seq 1 120); do ls "$W/agent/outbox/$J.json" >/dev/null 2>&1 && break; sleep 0.5; done
[ -f "$W/agent/outbox/$J.json" ] || fail "итог не сохранён"
agent_down; rm -f "$W/agent/outbox/$J.json"; echo "  итог задания удалён у агента (имитация сбоя агента сразу после снапшота)"
server_up; agent_up
for i in $(seq 1 120); do [ "$(st "$J")" = ok ] && break; sleep 0.5; done
[ "$(st "$J")" = ok ] || fail "задание не засчитано по снапшоту на площадке ($(st "$J"), $(api GET /jobs/$J | jq -r .error))"
api GET /jobs/$J | jq -e '.result.reconciled == true and .retry_job == "skipped"' >/dev/null || fail "задание должно быть засчитано сверкой, без повтора"
api GET /jobs/$J | jq -r '.log[].m' | grep "найден снапшот" | sed 's/^/  /'
echo "ok: снапшот задания найден на площадке, повтор не понадобился"

say "5) Агент падает посреди копии — повтор снимает копию"
data 8
J=$(api POST /instances/$INST/backup -d '{"retry":true}' | jq -r .job_id); wait_running "$J"
agent_down; sleep 1; agent_up
for i in $(seq 1 60); do [ "$(st "$J")" = failed ] && break; sleep 0.5; done
api GET /jobs/$J | jq -e '.interrupted == true' >/dev/null || fail "задание должно быть прерванным ($(api GET /jobs/$J | jq -c '{status, error}'))"
for i in $(seq 1 60); do R=$(api GET /jobs/$J | jq -r '.retry_job // empty'); [ -n "$R" ] && break; sleep 0.5; done
[ -n "$R" ] && [ "$R" != skipped ] || fail "повтор после падения агента не поставлен"
[ "$(wait_end "$R")" = ok ] || fail "повтор после падения агента не прошёл"
echo "ok: прерванное задание $J, повтор $R прошёл"

say "6) Догон расписания: запуск пришёлся на время, когда сервер был выключен"
server_down
# Экземпляр «создан» три часа назад, расписание — каждый час; заданий у него нет (новый экземпляр).
INST2=files-catchup
jq --arg a "$AGENT" --arg s "$SITE" --arg p "$W/src" --arg t "$(date -u -d '-3 hours' +%Y-%m-%dT%H:%M:%SZ)" \
  '.instances += [{id:"files-catchup", name:"files-catchup", catalog_id:"files", plugin:"files", agent_id:$a, site_id:$s, config:{paths:[$p], snapshot:"off"}, schedule:{enabled:true, full:"0 * * * *", dump:null}, retention:{keep_last:3}, rpo_target_min:60, created:$t}]' \
  "$W/srv/server/state.json" > "$W/state.new" && mv "$W/state.new" "$W/srv/server/state.json" && chmod 600 "$W/srv/server/state.json"
server_up
for i in $(seq 1 60); do C=$(api GET /jobs | jq -r '[.[] | select(.instance_id=="files-catchup" and .origin=="catchup")][0].id // empty'); [ -n "$C" ] && break; sleep 0.5; done
[ -n "$C" ] || fail "догоняющая копия не запущена"
api GET /jobs/$C | jq -r '.log[].m' | grep "пропущен" | sed 's/^/  /'
[ "$(wait_end "$C")" = ok ] || fail "догоняющая копия не прошла"
sleep 5
[ "$(api GET /jobs | jq '[.[] | select(.instance_id=="files-catchup" and .origin=="catchup")] | length')" = 1 ] || fail "догон должен быть один раз"
echo "ok: пропущенный запуск догнан один раз"

say "Задания"
api GET /jobs | jq -r --arg i "$INST" '.[] | select(.instance_id==$i or .instance_id=="files-catchup") | "  \(.status)\t\(.origin // "")\tпопытка \(.attempt)\t\(if .interrupted then "прервано " else "" end)\(.error // "" | .[0:70])"' | head -20
printf '\n\033[1;32mПРОВЕРКА УСТОЙЧИВОСТИ К ОБРЫВАМ ПРОЙДЕНА\033[0m\n'
