#!/usr/bin/env bash
# Сквозная проверка HopeBackup на одной машине (нужны root, PostgreSQL 14+, OpenSSH, jq, curl):
#   узел площадки → сервер управления → агент → копия PostgreSQL через SSH-туннель → архив WAL →
#   вторая полная копия (дедупликация) → восстановление на момент времени на «чистый сервер» по SSH
#   (через обратный туннель сервера) → сверка данных.
#
#   BIN=./hopebackup-core/target/release/hopebackup PGBIN=/usr/lib/postgresql/16/bin tests/e2e.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
PGBIN=${PGBIN:-/usr/lib/postgresql/16/bin}
W=${W:-/tmp/hb-e2e}
NODE_PORT=18443; SRV_PORT=17543; SSH_PORT=2222; PGPORT=5432; RPORT=5433
RESTORE_DIR=/var/lib/postgresql/hb-restore
PVE_DIR=/var/lib/postgresql/hb-restore-pve; VMW_DIR=/var/lib/postgresql/hb-restore-vmw; YC_DIR=/var/lib/postgresql/hb-restore-yc

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }

cleanup() {
  [ -f "$W/agent.pid" ] && kill "$(cat "$W/agent.pid")" 2>/dev/null || true
  [ -f "$W/server.pid" ] && kill "$(cat "$W/server.pid")" 2>/dev/null || true
  [ -f "$W/node.pid" ] && kill "$(cat "$W/node.pid")" 2>/dev/null || true
  [ -f "$W/sshd.pid" ] && kill "$(cat "$W/sshd.pid")" 2>/dev/null || true
  for f in pve vmw yc ec2; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done
  for d in $RESTORE_DIR $PVE_DIR $VMW_DIR $YC_DIR; do su postgres -c "$PGBIN/pg_ctl -D $d stop -m fast" >/dev/null 2>&1 || true; done
  sed -i '/hopebackup-/d' /root/.ssh/authorized_keys 2>/dev/null || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT

# Вывод в закрытую читателем трубу («hopebackup … | head»): тихий выход с кодом 0, а не паника «Broken pipe».
python3 - "$BIN" <<'EOPY' || fail "вывод в закрытую трубу: паника вместо тихого выхода"
import subprocess, sys
p = subprocess.Popen([sys.argv[1], "inventory"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
p.stdout.close()
err = p.stderr.read().decode()
sys.exit(0 if p.wait() == 0 and "panicked" not in err else 1)
EOPY

say "Подготовка стенда в $W"
cleanup; sleep 1
rm -rf "$W" "$RESTORE_DIR" "$PVE_DIR" "$VMW_DIR" "$YC_DIR" /var/lib/hopebackup/restore
mkdir -p "$W"; chmod 755 "$W"

# SSH: ключ для туннеля к площадке и для цели восстановления (root@localhost).
ssh-keygen -q -t ed25519 -N '' -f "$W/id_ed25519"
mkdir -p /root/.ssh /run/sshd; chmod 700 /root/.ssh
grep -q "hb-e2e" /root/.ssh/authorized_keys 2>/dev/null || { printf '%s hb-e2e\n' "$(cut -d' ' -f1,2 "$W/id_ed25519.pub")" >> /root/.ssh/authorized_keys; }
sed -i '/hb-e2e/d' /root/.ssh/authorized_keys; printf '%s hb-e2e\n' "$(cut -d' ' -f1,2 "$W/id_ed25519.pub")" >> /root/.ssh/authorized_keys
/usr/sbin/sshd -p $SSH_PORT -o PidFile="$W/sshd.pid" -o PermitRootLogin=prohibit-password
KEY=$(cat "$W/id_ed25519")

# PostgreSQL: исходный кластер.
if ! su postgres -c "$PGBIN/pg_ctl -D /var/lib/postgresql/16/main status" >/dev/null 2>&1; then
  pg_ctlcluster 16 main start
fi
# Как на новом сервере: архив WAL выключен (настройки прошлых прогонов сбрасываются) — его включат «Настроить
# архив WAL» и «Перезапустить PostgreSQL» из панели.
su postgres -c "psql -p $PGPORT -qc 'alter system reset archive_mode' -c 'alter system reset archive_command'"
pg_ctlcluster 16 main restart
su postgres -c "psql -p $PGPORT -qc 'drop database if exists shop'"
su postgres -c "psql -p $PGPORT -qc 'create database shop'"
su postgres -c "psql -p $PGPORT -d shop -qc 'create table orders(id serial primary key, created timestamptz default now(), customer int, amount numeric(12,2), note text)'"
su postgres -c "psql -p $PGPORT -d shop -qc \"insert into orders(customer, amount, note) select (random()*100000)::int, (random()*10000)::numeric(12,2), md5(random()::text) || ' ' || md5(g::text) from generate_series(1, 300000) g\""
# Как на работающей базе: страницы уже «заморожены», биты видимости проставлены.
su postgres -c "psql -p $PGPORT -d shop -qc 'vacuum (freeze, analyze)'"
su postgres -c "psql -p $PGPORT -qc 'checkpoint'"

say "Узел хранения (площадка)"
"$BIN" node init --data "$W/node" --listen 127.0.0.1:$NODE_PORT > "$W/node-init.txt"
cat "$W/node-init.txt"
NODE_FP=$(grep 'Отпечаток' "$W/node-init.txt" | awk '{print $NF}')
NODE_ADMIN=$(grep 'администратора' "$W/node-init.txt" | awk '{print $NF}')
NODE_APPEND=$(grep 'append' "$W/node-init.txt" | awk '{print $NF}')
HOPEBACKUP_LOG=hopebackup=info "$BIN" node run --data "$W/node" > "$W/node.log" 2>&1 & echo $! > "$W/node.pid"

say "Сервер управления"
HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
PW=$(cat "$W/srv/server/initial_admin_password.txt")
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$PW\"}" | jq -r .token)
[ "$TOKEN" != "null" ] || fail "вход в панель"
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() {
  local id=$1 st
  for i in $(seq 1 600); do
    st=$(api GET /jobs/$id | jq -r .status)
    [ "$st" = ok ] && return 0
    [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail -20; fail "задание $id: $(api GET /jobs/$id | jq -r .error)"; }
    sleep 1
  done
  fail "задание $id не завершилось"
}

say "Коннектор SSH-туннель и площадка"
CONN=$(api POST /connectors -d "$(jq -n --arg k "$KEY" --argjson p $SSH_PORT --argjson rp $NODE_PORT '{name:"ЦОД-2 · SSH-туннель", kind:"ssh_tunnel", params:{host:"127.0.0.1", port:$p, user:"root", private_key:$k, remote_host:"127.0.0.1", remote_port:$rp}}')" | jq -r .id)
PROBE=$(api POST /sites/probe -d "{\"url\":\"https://127.0.0.1:$NODE_PORT\"}" | jq -r .fingerprint)
[ "$PROBE" = "$NODE_FP" ] || fail "отпечаток узла: $PROBE ≠ $NODE_FP"
SITE=$(api POST /sites -d "$(jq -n --arg fp "$NODE_FP" --arg t "$NODE_APPEND" --arg pt "$NODE_ADMIN" --arg c "$CONN" --argjson np $NODE_PORT '{name:"ЦОД-2", url:("https://127.0.0.1:"+($np|tostring)), fingerprint:$fp, repo:"main", token:$t, password:"очень-секретный-пароль", prune_token:$pt, connector_ids:[$c], compression:3}')" | jq -r .id)
[ "$SITE" != null ] || fail "площадка"
wait_job "$(api POST /sites/$SITE/init | jq -r .job_id)"
api POST /sites/$SITE/test | jq -c '{ok, via, repo_exists}'
# Место на диске узла площадки — в карточке после проверки (через туннель, по токену).
api GET /sites | jq -e --arg id "$SITE" '.[] | select(.id==$id) | .node_disk.total > 0 and .node_disk.used >= 0' >/dev/null || fail "место на диске узла не показано: $(api GET /sites | jq -c --arg id "$SITE" '.[] | select(.id==$id) | .node_disk')"
[ "$(curl -sk -o /dev/null -w '%{http_code}' https://127.0.0.1:$NODE_PORT/v1/disk)" = 401 ] || fail "место на диске узла без токена должно быть закрыто"

say "Агент"
ENR=$(api POST /agents/enroll-token -d '{"note":"e2e"}')
mkdir -p "$W/agent"; chmod 755 "$W/agent"
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$(echo "$ENR" | jq -r .token)" --fingerprint "$(echo "$ENR" | jq -r .server_fingerprint)" --name db-host
# Каталог данных агента закрыт, как после установки из панели: «Настроить архив WAL» должна открыть проход
# пользователю СУБД к спулу, иначе archive_command не сможет класть сегменты.
chmod 700 "$W/agent"
HOPEBACKUP_DATA="$W/agent" HOPEBACKUP_LOG=hopebackup=info HOPEBACKUP_LOOK_DELAY=1 "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
AGENT=$(api GET /agents | jq -r '.[0].id')

say "Пакеты для коннекторов: проверка на сервере, установка заданием на сервере и на агенте"
api GET "/connectors/requirements?kind=l2tp_ipsec" | jq -e '(.requirements | length) == 4 and (.missing | type) == "array" and (.host | length) > 0' >/dev/null || fail "проверка пакетов: $(api GET '/connectors/requirements?kind=l2tp_ipsec')"
api GET "/connectors/requirements?kind=openvpn" | jq -e '.missing == []' >/dev/null || fail "openvpn должен быть установлен на стенде"
J=$(api POST /connectors/install -d '{"kind":"openvpn"}' | jq -r '.jobs[0]'); wait_job "$J"
api GET /jobs/$J | jq -r '.log[].m' | grep -q "все нужные пакеты уже установлены" || fail "установка на сервере: $(api GET /jobs/$J | jq -c '.log')"
for i in $(seq 1 60); do api GET /agents | jq -e --arg a "$AGENT" '.[] | select(.id == $a) | .caps | index("install-pkgs")' >/dev/null && break; sleep 1; done
J=$(api POST /connectors/install -d "$(jq -n --arg a "$AGENT" '{kind:"openvpn", agents:[$a]}')" | jq -r '.jobs[0]')
[ "$J" != null ] || fail "установка на агенте не принята: $(api POST /connectors/install -d "$(jq -n --arg a "$AGENT" '{kind:"openvpn", agents:[$a]}')")"
wait_job "$J"
api GET /jobs/$J | jq -e '.agent_id != "server" and .status == "ok"' >/dev/null || fail "установка на агенте: $(api GET /jobs/$J | jq -c '{agent_id, status, error}')"
code=$(curl -sk -o /dev/null -w '%{http_code}' -X POST $API/connectors/install -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"kind":"direct"}')
[ "$code" = 400 ] || fail "установка пакетов для «direct» должна отклоняться (код $code)"
api POST /users -d '{"name":"pkg-oper","password":"pkg-oper-pass-1","role":"operator"}' >/dev/null
OT=$(curl -sk $API/auth/login -H 'content-type: application/json' -d '{"user":"pkg-oper","password":"pkg-oper-pass-1"}' | jq -r .token)
code=$(curl -sk -o /dev/null -w '%{http_code}' -X POST $API/connectors/install -H "authorization: Bearer $OT" -H 'content-type: application/json' -d '{"kind":"openvpn"}')
[ "$code" = 403 ] || fail "оператор не должен ставить пакеты (код $code)"

say "Агент сам находит незащищённое: PostgreSQL на сервере — предложение «Защитить»"
for i in $(seq 1 90); do api GET /suggestions | jq -e --arg a "$AGENT" '.[] | select(.agent_id == $a and .catalog_id == "postgresql")' >/dev/null && break; sleep 1; done
api GET /suggestions | jq -c '.[] | {agent_name, id, title}'
SID=$(api GET /suggestions | jq -r --arg a "$AGENT" '.[] | select(.agent_id == $a and .catalog_id == "postgresql") | .id')
[ -n "$SID" ] || fail "агент не сообщил о работающем PostgreSQL"
SUG=$(api POST /suggestions/apply -d "$(jq -n --arg a "$AGENT" --arg i "$SID" --arg s "$SITE" '{agent_id:$a, id:$i, site_id:$s}')" | jq -r .id)
api GET /instances/$SUG | jq -e '.catalog_id == "postgresql" and .schedule.enabled == true and (.config.bin_dir | length) > 0' >/dev/null || fail "экземпляр по предложению: $(api GET /instances/$SUG | jq -c '{catalog_id, config, schedule}')"
api GET /suggestions | jq -e --arg a "$AGENT" '[.[] | select(.agent_id == $a and .catalog_id == "postgresql")] | length == 0' >/dev/null || fail "защищённое всё ещё предлагается"
api DELETE /instances/$SUG >/dev/null
api GET /suggestions | jq -e --arg a "$AGENT" --arg i "$SID" '[.[] | select(.agent_id == $a and .id == $i)] | length == 1' >/dev/null || fail "после удаления экземпляра предложение не вернулось"
api POST /suggestions/dismiss -d "$(jq -n --arg a "$AGENT" --arg i "$SID" '{agent_id:$a, id:$i}')" >/dev/null
api GET /suggestions | jq -e --arg a "$AGENT" --arg i "$SID" '[.[] | select(.agent_id == $a and .id == $i)] | length == 0' >/dev/null || fail "«Не предлагать» не сработало"

say "Экземпляр PostgreSQL: обнаружение и настройка архива WAL"
INST=$(api POST /instances -d "$(jq -n --arg a "$AGENT" --arg s "$SITE" --argjson p $PGPORT '{name:"db-prod-01", catalog_id:"postgresql", agent_id:$a, site_id:$s, config:{port:$p, databases:["shop"]}, schedule:{enabled:false, full:"0 1 * * *"}, retention:{keep_last:7}, rpo_target_min:15}')" | jq -r .id)
J=$(api POST /instances/$INST/discover | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -c '.result | {bin_dir, version, flavor, archive_ok}'
J=$(api POST /instances/$INST/action -d '{"name":"setup-wal"}' | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -c .result
api GET /jobs/$J | jq -e '(.result.restart_command | length) > 0' >/dev/null || fail "нет команды перезапуска PostgreSQL в итоге настройки WAL"
J=$(api POST /instances/$INST/discover | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -e '.result.pending_restart == true and (.result.restart_command | length) > 0' >/dev/null || fail "обнаружение не показало, что нужен перезапуск: $(api GET /jobs/$J | jq -c '.result | {pending_restart, restart_command}')"
say "Перезапуск PostgreSQL из панели (действие restart)"
api GET /agents | jq -e '.[0].caps | index("pg-restart")' >/dev/null || fail "агент не сообщил возможность pg-restart: $(api GET /agents | jq -c '.[0].caps')"
J=$(api POST /instances/$INST/action -d '{"name":"restart"}' | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -c .result
api GET /jobs/$J | jq -e '.result.archive_ok == true and .result.archive_mode == "on" and .result.pending_restart == false' >/dev/null || fail "после перезапуска архив WAL не работает"

say "Ошибка входа peer объясняется; заданный пароль можно удалить"
BAD=$(api POST /instances -d "$(jq -n --arg a "$AGENT" --arg s "$SITE" --argjson p $PGPORT '{name:"peer-test", catalog_id:"postgresql", agent_id:$a, site_id:$s, config:{port:$p, user:"zabbix", os_user:"root", password:"secret1"}, schedule:{enabled:false, full:"0 1 * * *"}, retention:{keep_last:1}, rpo_target_min:15}')" | jq -r .id)
J=$(api POST /instances/$BAD/backup -d '{"kind":"full"}' | jq -r .job_id)
for i in $(seq 1 120); do st=$(api GET /jobs/$J | jq -r .status); [ "$st" = failed ] || [ "$st" = ok ] && break; sleep 1; done
ERR=$(api GET /jobs/$J | jq -r .error)
echo "$ERR" | grep -q "Peer authentication failed" && echo "$ERR" | grep -q "пользователь ОС — «root», роль СУБД — «zabbix»" && echo "$ERR" | grep -q "вкладка «Параметры»" \
  || fail "нет объяснения ошибки peer: $ERR"
# Ошибка на «Обзоре» и её скрытие (задание остаётся, помечено ack).
api GET /status | jq -e --arg j "$J" '[.recent_failures[].job_id] | index($j)' >/dev/null || fail "ошибки нет на обзоре"
api POST /jobs/ack -d "{\"ids\":[\"$J\"]}" | jq -e '.hidden == 1' >/dev/null || fail "ошибка не скрыта"
api GET /status | jq -e --arg j "$J" '[.recent_failures[].job_id] | index($j) | not' >/dev/null || fail "скрытая ошибка осталась на обзоре"
api GET /jobs/$J | jq -e '.status == "failed" and .ack.user == "Administrator"' >/dev/null || fail "задание после скрытия"
api GET /instances/$BAD | jq -e '.config.password == "********"' >/dev/null || fail "пароль не замаскирован"
api PUT /instances/$BAD -d '{"name":"peer-test","config":{"port":'$PGPORT',"user":"zabbix","os_user":"root","password":null}}' >/dev/null
api GET /instances/$BAD | jq -e '.config.password == ""' >/dev/null || fail "пароль не удалён: $(api GET /instances/$BAD | jq -c .config)"
api DELETE /instances/$BAD >/dev/null

say "Сокет не там, где указано: агент находит работающий кластер сам (как у встроенной PostgreSQL GitLab)"
AUTO=$(api POST /instances -d "$(jq -n --arg a "$AGENT" --arg s "$SITE" --argjson p $PGPORT '{name:"auto-sock", catalog_id:"postgresql", agent_id:$a, site_id:$s, config:{port:$p, host:"/nonexistent-hb-sock"}, schedule:{enabled:false, full:"0 1 * * *"}, retention:{keep_last:1}, rpo_target_min:15}')" | jq -r .id)
J=$(api POST /instances/$AUTO/discover | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -e '(.result.connect_error == null) and (.result.autodetected | test("подключаюсь к работающему PostgreSQL: сокет /var/run/postgresql"))' >/dev/null \
  || fail "кластер не найден автоматически: $(api GET /jobs/$J | jq -c '.result | {connect_error, autodetected}')"
api GET /jobs/$J | jq -r '.log[].m' | grep -q "внимание: сокета /nonexistent-hb-sock:$PGPORT нет" || fail "нет предупреждения об автоопределении в журнале"
api DELETE /instances/$AUTO >/dev/null

say "Полная копия №1 — низкоуровневая (pg_backup_start/stop, как для встроенной PostgreSQL GitLab)"
setcfg() { api PUT /instances/$INST -d "$(api GET /instances/$INST | jq --arg m "$1" '{name, config: (.config + {backup_method: $m})}')" >/dev/null; }
setcfg lowlevel
J=$(api POST /instances/$INST/backup -d '{"kind":"full"}' | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -r '.log[].m' | tail -4
api GET /jobs/$J | jq -r '.log[].m' | grep -q "низкоуровневая копия (pg_backup_start/stop)" || fail "копия №1 сделана не низкоуровневым способом"
api GET /jobs/$J | jq -e '.result.snapshot.extra.wal_end | test("^[0-9A-F]{24}$")' >/dev/null || fail "нет конца WAL у низкоуровневой копии: $(api GET /jobs/$J | jq -c .result.snapshot.extra)"
setcfg auto
S1=$(api GET /jobs/$J | jq -r .result.stats.uploaded)

say "Изменения, отметка времени T, ещё изменения"
su postgres -c "psql -p $PGPORT -d shop -qc \"insert into orders(customer, amount, note) select (random()*100000)::int, random()*100, 'после копии ' || md5(random()::text) from generate_series(1, 5000) g\""
sleep 2
T=$(su postgres -c "psql -p $PGPORT -d shop -Atc \"select now()\"")
COUNT_T=$(su postgres -c "psql -p $PGPORT -d shop -Atc 'select count(*) from orders'")
sleep 2
su postgres -c "psql -p $PGPORT -d shop -qc \"insert into orders(customer, amount, note) select (random()*100000)::int, random()*100, 'лишнее ' || md5(random()::text) from generate_series(1, 7000) g\""
su postgres -c "psql -p $PGPORT -d shop -qc 'select pg_switch_wal()'" >/dev/null
echo "T = $T, строк на момент T: $COUNT_T, сейчас: $(su postgres -c "psql -p $PGPORT -d shop -Atc 'select count(*) from orders'")"

say "Полная копия №2 (дедупликация: по каналу должно уйти мало)"
J=$(api POST /instances/$INST/backup -d '{"kind":"full"}' | jq -r .job_id); wait_job "$J"
api GET /jobs/$J | jq -r '.log[].m' | tail -2
S2=$(api GET /jobs/$J | jq -r .result.stats.uploaded)
R2=$(api GET /jobs/$J | jq -r .result.stats.bytes_read)
N2=$(api GET /jobs/$J | jq -r .result.stats.new_bytes)
echo "по каналу: копия №1 — $S1 байт, копия №2 — $S2 байт; новых данных во второй копии $N2 из $R2"
[ "$S2" -lt "$((S1 / 3))" ] || fail "дедупликация не сработала"

say "Отправка WAL на площадку"
for i in $(seq 1 30); do
  Q=$(api GET /instances/$INST | jq -r '.runtime.wal.queued // 99')
  [ "$Q" = 0 ] && break; sleep 1
done
api GET /instances/$INST | jq -c '.runtime | {status, rpo_seconds, wal}'
POINTS=$(api GET /instances/$INST/points)
echo "$POINTS" | jq -c '{snapshots: [.snapshots[] | {id, kind, size, uploaded}], wal, pitr}'
SNAP=$(echo "$POINTS" | jq -r '[.snapshots[] | select(.kind=="full")] | last | .id')

say "Цель восстановления: «чистый сервер» root@127.0.0.1 по SSH"
TGT=$(api POST /targets -d "$(jq -n --arg k "$KEY" --argjson p $SSH_PORT '{name:"dr-host", kind:"ssh", host:"127.0.0.1", port:$p, user:"root", private_key:$k}')" | jq -r .id)
api POST /targets/$TGT/test | jq -c .
# Как на машине с установленным агентом: каталог данных закрыт (0700) — restore_command от postgres туда не пройдёт.
mkdir -p /var/lib/hopebackup && chmod 700 /var/lib/hopebackup

say "Восстановление копии №1 на момент T (данные — через сервер, обратный туннель)"
J=$(api POST /restore -d "$(jq -n --arg i "$INST" --arg s "$SNAP" --arg t "$TGT" --arg tt "$T" --arg d "$RESTORE_DIR" --argjson p $RPORT '{instance_id:$i, snapshot_id:$s, target_id:$t, via:"server", params:{pgdata:$d, os_user:"postgres", target_time:$tt, port:$p, start:true}}')" | jq -r .job_id)
wait_job "$J"
api GET /jobs/$J | jq -r '.log[].m' | tail -8
GOT=$(su postgres -c "psql -h /var/run/postgresql -p $RPORT -d shop -Atc 'select count(*) from orders'")
echo "строк в восстановленной базе: $GOT (ожидалось $COUNT_T)"
[ "$GOT" = "$COUNT_T" ] || fail "данные на момент T не совпали"
REC=$(su postgres -c "psql -h /var/run/postgresql -p $RPORT -Atc 'select pg_is_in_recovery()'")
[ "$REC" = f ] || fail "восстановленный сервер не вышел из режима восстановления"

if [ "${VM_MOCKS:-1}" = 1 ]; then
  say "Восстановление на новую ВМ: имитаторы Proxmox VE и vCenter (HTTPS, закреплённый отпечаток)"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/hv.key" -out "$W/hv.crt" -days 2 -subj /CN=hypervisor 2>/dev/null
  HV_FP=$(openssl x509 -in "$W/hv.crt" -noout -fingerprint -sha256 | cut -d= -f2)
  VM_IP=$(hostname -I | awk '{print $1}')
  python3 tests/mocks/hypervisors.py proxmox 18006 "$W/hv.crt" "$W/hv.key" "$W/pve-calls.jsonl" "$VM_IP" & echo $! > "$W/pve.pid"
  python3 tests/mocks/hypervisors.py vmware 18007 "$W/hv.crt" "$W/hv.key" "$W/vmw-calls.jsonl" "$VM_IP" & echo $! > "$W/vmw.pid"
  sleep 1
  restore_vm() { # $1 имя цели, $2 тип, $3 параметры (JSON), $4 порт, $5 каталог
    local T J GOT
    T=$(api POST /targets -d "$(jq -n --arg n "$1" --arg k "$2" --argjson p "$3" '{name:$n, kind:$k, params:$p}')" | jq -r .id)
    [ "$T" != null ] || fail "цель $2 не создана"
    api POST /targets/$T/test | jq -r '.output // .error'
    J=$(api POST /restore -d "$(jq -n --arg i "$INST" --arg s "$SNAP" --arg t "$T" --arg tt "$T_TIME" --arg d "$5" --argjson p $4 '{instance_id:$i, snapshot_id:$s, target_id:$t, params:{pgdata:$d, os_user:"postgres", target_time:$tt, port:$p, start:true, vm:{memory_mb:2048}}}')" | jq -r .job_id)
    wait_job "$J"
    api GET /jobs/$J | jq -r '.log[].m' | grep -E "ВМ|клонирование|Proxmox|vCenter|включение|запуск ВМ|цели восстановления" | head -12
    GOT=$(su postgres -c "psql -h /var/run/postgresql -p $4 -d shop -Atc 'select count(*) from orders'")
    echo "$2: строк в восстановленной базе $GOT (ожидалось $COUNT_T)"
    [ "$GOT" = "$COUNT_T" ] || fail "$2: данные на момент T не совпали"
    api GET /jobs/$J | jq -e '.result.vm.target_id' >/dev/null || fail "$2: ВМ не добавлена в цели"
  }
  T_TIME=$T
  restore_vm "PVE-кластер" proxmox "$(jq -n --arg fp "$HV_FP" '{url:"https://127.0.0.1:18006", fingerprint:$fp, token_id:"root@pam!hb", token_secret:"11111111-2222-3333-4444-555555555555", node:"pve", template:9000, bridge:"vmbr1", disk_gb:20, ssh_user:"root", ssh_port:2222, boot_timeout:120}')" 5434 "$PVE_DIR"
  grep -q '"path": "/api2/json/nodes/pve/qemu/9000/clone"' "$W/pve-calls.jsonl" && grep -q '"event": "cloud-init"' "$W/pve-calls.jsonl" || fail "Proxmox: клон и cloud-init не выполнены"
  restore_vm "vCenter" vmware "$(jq -n --arg fp "$HV_FP" '{url:"https://127.0.0.1:18007", fingerprint:$fp, username:"hb@vsphere.local", password:"vc-pass", source:"golden-ubuntu", folder:"Restored", datastore:"ds-ssd", ssh_user:"root", ssh_port:2222, boot_timeout:120}')" 5435 "$VMW_DIR"
  grep -q '"path": "/api/vcenter/vm/vm-11/guest/customization"' "$W/vmw-calls.jsonl" && grep -q '"event": "cloud-init"' "$W/vmw-calls.jsonl" || fail "vCenter: настройка cloud-init не выполнена"

  say "Восстановление на новую ВМ: имитатор Yandex Cloud (сервисный аккаунт, JWT PS256)"
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$W/yc.key" 2>/dev/null
  openssl rsa -in "$W/yc.key" -pubout -out "$W/yc.pub" 2>/dev/null
  SA_KEY=$(jq -n --rawfile k "$W/yc.key" '{id:"ajeKEY0hopebackup", service_account_id:"ajeSA0hopebackup", created_at:"2026-10-24T00:00:00Z", key_algorithm:"RSA_2048", public_key:"", private_key:("PLEASE DO NOT REMOVE THIS LINE! Yandex.Cloud SA Key ID <ajeKEY0hopebackup>\n" + $k)}')
  python3 tests/mocks/yandex.py 18010 "$W/yc.pub" "$W/yc-calls.jsonl" "$VM_IP" & echo $! > "$W/yc.pid"
  sleep 1
  restore_vm "Yandex Cloud" yandex_cloud "$(jq -n --arg k "$SA_KEY" '{auth:"service_account_key", sa_key:$k, folder_id:"b1ghopebackup", zone:"ru-central1-a", subnet_id:"e9bhopebackup", public_ip:true, endpoint:"http://127.0.0.1:18010", ssh_user:"root", ssh_port:2222, boot_timeout:120}')" 5436 "$YC_DIR"
  grep -q '"event": "iam-token"' "$W/yc-calls.jsonl" && grep -q '"event": "cloud-init"' "$W/yc-calls.jsonl" || fail "Yandex Cloud: токен или cloud-init"
  grep -q jwt-rejected "$W/yc-calls.jsonl" && fail "Yandex Cloud: JWT отвергнут"

  if command -v moto_server >/dev/null; then
    say "AWS EC2 (moto): создание инстанса, ожидание SSH, удаление при сбое"
    moto_server -H 127.0.0.1 -p 15002 > "$W/moto-ec2.log" 2>&1 & echo $! > "$W/ec2.pid"
    for i in $(seq 1 50); do curl -s http://127.0.0.1:15002/ >/dev/null && break; sleep 0.2; done
    read SUBNET AMI < <(python3 -c "
import boto3; e=boto3.client('ec2', endpoint_url='http://127.0.0.1:15002', region_name='eu-central-1', aws_access_key_id='a', aws_secret_access_key='b')
v=e.create_vpc(CidrBlock='10.50.0.0/16')['Vpc']['VpcId']; s=e.create_subnet(VpcId=v, CidrBlock='10.50.1.0/24')['Subnet']['SubnetId']
print(s, e.describe_images()['Images'][0]['ImageId'])")
    AT=$(api POST /targets -d "$(jq -n --arg s "$SUBNET" --arg a "$AMI" '{name:"AWS eu-central-1", kind:"aws", params:{region:"eu-central-1", access_key:"AKIAHOPEBACKUP", secret_key:"secret", subnet_id:$s, ami:$a, instance_type:"t3.micro", disk_gb:30, endpoint:"http://127.0.0.1:15002", boot_timeout:15}}')" | jq -r .id)
    api POST /targets/$AT/test | jq -r '.output // .error'
    J=$(api POST /restore -d "$(jq -n --arg i "$INST" --arg s "$SNAP" --arg t "$AT" '{instance_id:$i, snapshot_id:$s, target_id:$t, params:{pgdata:"/var/lib/postgresql/16/restore", os_user:"postgres", start:true}}')" | jq -r .job_id)
    for i in $(seq 1 120); do st=$(api GET /jobs/$J | jq -r .status); [ "$st" = failed ] || [ "$st" = ok ] && break; sleep 1; done
    api GET /jobs/$J | jq -r '.log[].m' | grep -E "инстанс|ВМ|SSH" | head -8
    [ "$st" = failed ] || fail "AWS: ожидался сбой на SSH (адрес инстанса moto недоступен)"
    STATE=$(python3 -c "
import boto3; e=boto3.client('ec2', endpoint_url='http://127.0.0.1:15002', region_name='eu-central-1', aws_access_key_id='a', aws_secret_access_key='b')
r=e.describe_instances()['Reservations']; i=r[0]['Instances'][0]; print(i['State']['Name'], i['InstanceType'], [t['Value'] for t in i.get('Tags',[]) if t['Key']=='hopebackup'], i['BlockDeviceMappings'][0]['DeviceName'] if i.get('BlockDeviceMappings') else '-')")
    echo "инстанс после сбоя: $STATE"
    echo "$STATE" | grep -qE "^(terminated|shutting-down) t3.micro \['restore'\]" || fail "AWS: инстанс не удалён после сбоя"
  fi
fi

say "Площадка: статистика и аудит"
api POST /sites/$SITE/test | jq -c .stats
api GET /audit | jq -r '.[] | "\(.time) \(.user) \(.action) \(.detail)"' | head -12
# Журнал аудита: страницы, поиск по всему журналу, выгрузка CSV и JSON (сама выгрузка — в журнале).
P1=$(api GET "/audit/page?limit=2"); echo "$P1" | jq -e '(.items | length) == 2 and .total > 2' >/dev/null || fail "страница журнала: $P1"
P2=$(api GET "/audit/page?limit=2&offset=2"); [ "$(echo "$P1" | jq -c '.items[0]')" != "$(echo "$P2" | jq -c '.items[0]')" ] || fail "вторая страница совпала с первой"
api GET "/audit/page?q=login&limit=500" | jq -e '.total >= 1 and all(.items[]; (.action + .detail + .user) | test("login"; "i"))' >/dev/null || fail "поиск по журналу"
curl -sk "$API/audit/export?format=csv" -H "authorization: Bearer $TOKEN" -o "$W/audit.csv"
head -c 3 "$W/audit.csv" | od -An -tx1 | grep -q 'ef bb bf' && sed -n 1p "$W/audit.csv" | grep -q 'Время;Пользователь;Действие;Подробности' || fail "CSV журнала: $(head -2 "$W/audit.csv")"
curl -sk "$API/audit/export?format=json&q=login" -H "authorization: Bearer $TOKEN" | jq -e 'length >= 1' >/dev/null || fail "JSON журнала"
api GET "/audit/page?q=audit_export" | jq -e '.total >= 2' >/dev/null || fail "выгрузка журнала не записана в журнал"
echo "ok: журнал аудита — страницы, поиск, выгрузка CSV и JSON"

say "Образ машины: выполняет сам сервер управления; оценка места; скачивание — только администратор"
MI=$(api POST /instances -d "$(jq -n --arg s "$SITE" '{name:"Образ · тест", catalog_id:"machine", agent_id:"server", site_id:$s, config:{}}')" | jq -r .id)
[ -n "$MI" ] && [ "$MI" != null ] || fail "экземпляр «Образ машины по SSH» на сервере управления не создан"
code=$(curl -sk -o "$W/srv-agent.txt" -w '%{http_code}' -X POST $API/instances -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$(jq -n --arg s "$SITE" '{name:"pg на сервере", catalog_id:"postgresql", agent_id:"server", site_id:$s, config:{}}')")
[ "$code" = 400 ] || fail "продукт с агентом нельзя назначать серверу управления (код $code)"
api GET "/instances" | jq -e --arg id "$MI" '.[] | select(.id==$id) | .agent_id == "server"' >/dev/null || fail "исполнитель «server» не сохранён"
api GET "/image/estimate?instance_id=$MI" | jq -e '.snapshot == null' >/dev/null || fail "оценка места без копий"
[ "$(curl -sk -o /dev/null -w '%{http_code}' "$API/image/estimate?instance_id=$MI" -H "authorization: Bearer $OT")" = 200 ] || fail "оценка места должна быть доступна для просмотра"
[ "$(curl -sk -o /dev/null -w '%{http_code}' -X POST $API/image/download -H "authorization: Bearer $OT" -H 'content-type: application/json' -d "{\"instance_id\":\"$MI\",\"snapshot_id\":\"x\",\"disk\":\"sda\"}")" = 403 ] || fail "скачивание образа — только администратор"
[ "$(curl -sk -o /dev/null -w '%{http_code}' $API/image/dl/неверный-токен)" != 200 ] || fail "скачивание по неверной ссылке"
api DELETE "/instances/$MI" >/dev/null
echo "ok: образ машины — сервер управления как исполнитель, оценка места, права на скачивание"

say "Репликация экземпляра: зашифрованные копии PostgreSQL на вторую площадку (закладка «Репликация»)"
SITE2=$(api POST /sites -d "$(jq -n --arg fp "$NODE_FP" --arg t "$NODE_APPEND" --arg pt "$NODE_ADMIN" --argjson np $NODE_PORT '{name:"ЦОД-3", url:("https://127.0.0.1:"+($np|tostring)), fingerprint:$fp, repo:"mirror", token:$t, password:"очень-секретный-пароль", prune_token:$pt, connector_ids:[], compression:3}')" | jq -r .id)
[ -n "$SITE2" ] && [ "$SITE2" != null ] || fail "вторая площадка"
code=$(curl -sk -o /dev/null -w '%{http_code}' -X PUT $API/instances/$INST/replication -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$(jq -n --arg s "$SITE" '{targets:[{site_id:$s, enabled:true}]}')")
[ "$code" = 400 ] || fail "репликация на площадку самого экземпляра должна быть запрещена (код $code)"
api PUT /instances/$INST/replication -d "$(jq -n --arg s "$SITE2" '{targets:[{site_id:$s, enabled:true, when:"after_job", keep:2}]}')" >/dev/null
RJ=$(api POST /instances/$INST/replicate -d "$(jq -n --arg s "$SITE2" '{site_id:$s}')" | jq -r .job_id); wait_job "$RJ"
api GET /jobs/$RJ | jq -e '.result.snapshots >= 1 and .result.created_repo == true' >/dev/null || fail "копии экземпляра не реплицированы: $(api GET /jobs/$RJ | jq -c .result)"
[ -f "$W/node/repos/mirror/config.json" ] || ls "$W/node/repos/mirror" >/dev/null || fail "на второй площадке нет репозитория"
# После новой копии экземпляра — репликация сама («сразу после выполнения задания»), передаются только новые копии.
J=$(api POST /instances/$INST/backup -d '{"kind":"full"}' | jq -r .job_id); wait_job "$J"
for i in $(seq 1 60); do AJ=$(api GET "/jobs?instance_id=$INST&limit=20" | jq -r --arg rj "$RJ" '[.[] | select(.kind=="inst_repl" and .id != $rj)][0].id // empty'); [ -n "$AJ" ] && break; sleep 1; done
[ -n "$AJ" ] || fail "после копии репликация не запустилась"
wait_job "$AJ"
api GET /jobs/$AJ | jq -e '.result.snapshots == 1' >/dev/null || fail "после копии должна уйти одна новая копия: $(api GET /jobs/$AJ | jq -c .result)"
api GET /instances/$INST/replication | jq -e --arg s "$SITE2" '.last[$s].status == "ok"' >/dev/null || fail "состояние репликации на закладке"
api PUT /instances/$INST/replication -d '{"targets":[]}' >/dev/null
echo "ok: репликация экземпляра — первая $(api GET /jobs/$RJ | jq -r '.result.snapshots') копий, после новой копии — 1"

say "Репликация площадки целиком: продукт можно исключить (закладка «Репликация»)"
SITE3=$(api POST /sites -d "$(jq -n --arg fp "$NODE_FP" --arg t "$NODE_APPEND" --arg pt "$NODE_ADMIN" --argjson np $NODE_PORT '{name:"ЦОД-4", url:("https://127.0.0.1:"+($np|tostring)), fingerprint:$fp, repo:"mirror2", token:$t, password:"очень-секретный-пароль", prune_token:$pt, connector_ids:[], compression:3}')" | jq -r .id)
[ -n "$SITE3" ] && [ "$SITE3" != null ] || fail "площадка ЦОД-4"
api PUT /sites/$SITE -d "$(api GET /sites | jq --arg id "$SITE" --arg r "$SITE3" '.[] | select(.id==$id) | .replicas=[$r]')" >/dev/null
api GET /sites | jq -e --arg id "$SITE" --arg r "$SITE3" '.[] | select(.id==$id) | .replicas == [$r]' >/dev/null || fail "принимающая площадка не сохранена"
# Образ машины по умолчанию исключён, остальные продукты — нет.
api GET /instances/$INST/replication | jq -e '.site_repl_excluded == false' >/dev/null || fail "PostgreSQL по умолчанию должен передаваться при репликации площадки"
MI2=$(api POST /instances -d "$(jq -n --arg s "$SITE" '{name:"Образ · исключение", catalog_id:"machine", agent_id:"server", site_id:$s, config:{}}')" | jq -r .id)
api GET /instances/$MI2/replication | jq -e '.site_repl_excluded == true' >/dev/null || fail "образ машины по умолчанию должен быть исключён"
api DELETE "/instances/$MI2" >/dev/null
api PUT /instances/$INST/replication -d '{"targets":[], "site_repl": false}' | jq -e '.site_repl_excluded == true' >/dev/null || fail "исключение из репликации площадки не сохранено"
snaps_of() { local n=0 f; for f in "$W/node/repos/$1/snapshots/"*.json; do [ -f "$f" ] && [ "$(jq -r .instance "$f")" = "$2" ] && n=$((n+1)); done; echo $n; }
SJ=$(api POST /sites/$SITE/replicate | jq -r .job_id); wait_job "$SJ"
[ "$(snaps_of mirror2 "$INST")" = 0 ] || fail "исключённый продукт попал на принимающую площадку"
api GET /jobs/$SJ | jq -r '.log[].m' | grep -q "исключённых снапшотов не передано: [1-9]" || fail "в журнале нет числа исключённых снапшотов: $(api GET /jobs/$SJ | jq -r '.log[].m' | tail -3)"
NSRC=$(snaps_of "$(api GET /sites | jq -r --arg id "$SITE" '.[] | select(.id==$id) | .repo')" "$INST")
api PUT /instances/$INST/replication -d '{"targets":[], "site_repl": true}' | jq -e '.site_repl_excluded == false' >/dev/null || fail "включение в репликацию площадки не сохранено"
SJ=$(api POST /sites/$SITE/replicate | jq -r .job_id); wait_job "$SJ"
[ "$(snaps_of mirror2 "$INST")" = "$NSRC" ] || fail "после включения на принимающей площадке копий $(snaps_of mirror2 "$INST") из $NSRC"
api GET /audit | jq -e '[.[] | select(.action=="replication_set" and (.detail|tostring|test("при репликации площадки")))] | length >= 2' >/dev/null || fail "аудит исключения из репликации площадки"
api PUT /sites/$SITE -d "$(api GET /sites | jq --arg id "$SITE" '.[] | select(.id==$id) | .replicas=[]')" >/dev/null
echo "ok: репликация площадки — исключённый продукт не передаётся ($NSRC копий появились после включения)"

say "Образы qcow2: агент нельзя; готовый qcow2 — на удалённую площадку только изменениями"
code=$(curl -sk -o /dev/null -w '%{http_code}' -X POST $API/instances -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d "$(jq -n --arg s "$SITE" --arg a "$AGENT" '{name:"qcow2 на агенте", catalog_id:"qcow2images", agent_id:$a, site_id:$s, config:{}}')")
[ "$code" = 400 ] || fail "«Образы qcow2» на агенте должны быть запрещены (код $code)"
if command -v qemu-img >/dev/null 2>&1; then
  IMGS="$W/imgs"; S1=20261101T010000Z; S2=20261102T010000Z
  mkdir -p "$IMGS/$S1" "$IMGS/$S2"
  truncate -s 256M "$W/disk1.raw"
  for b in 2 7 30 31 60; do dd if=/dev/urandom of="$W/disk1.raw" bs=4M seek=$b count=1 conv=notrunc status=none; done
  cp --sparse=always "$W/disk1.raw" "$W/disk2.raw"
  for b in 7 45; do dd if=/dev/urandom of="$W/disk2.raw" bs=4M seek=$b count=1 conv=notrunc status=none; done
  qemu-img convert -f raw -O qcow2 -o preallocation=metadata "$W/disk1.raw" "$IMGS/$S1/sda.qcow2"
  # Источник обязателен; площадка «Образов qcow2» берётся от источника (в запросе — пусто).
  code=$(curl -sk -o /dev/null -w '%{http_code}' -X POST $API/instances -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '{"name":"без источника","catalog_id":"qcow2images","agent_id":"server","site_id":"","config":{}}')
  [ "$code" = 400 ] || fail "«Образы qcow2» без источника должны быть запрещены (код $code)"
  SRC=$(api POST /instances -d "$(jq -n --arg s "$SITE" '{name:"Образ · источник", catalog_id:"machine", agent_id:"server", site_id:$s, schedule:{enabled:false, full:"0 1 * * *"}, config:{}}')" | jq -r .id)
  QI=$(api POST /instances -d "$(jq -n --arg src "$SRC" --arg d "$IMGS" '{name:"Образы · тест", catalog_id:"qcow2images", agent_id:"server", site_id:"", schedule:{enabled:false, full:"0 1 * * *"}, config:{source:$src, dir:$d}}')" | jq -r .id)
  # Куда отправлять готовые qcow2 — закладка «Репликация» (по умолчанию никуда).
  api GET /instances/$QI/replication | jq -e '.targets == null or (.targets | length) == 0' >/dev/null || fail "по умолчанию репликация должна быть выключена"
  api PUT /instances/$QI/replication -d "$(jq -n --arg s "$SITE" '{targets:[{site_id:$s, enabled:true, when:"after_job", keep:1}]}')" | jq -e '.targets[0].enabled' >/dev/null || fail "площадка для qcow2 не сохранена"
  [ -n "$QI" ] && [ "$QI" != null ] || fail "экземпляр «Образы qcow2» не создан"
  api GET /instances | jq -e --arg id "$QI" --arg s "$SITE" '.[] | select(.id==$id) | .site_id == $s' >/dev/null || fail "площадка «Образов qcow2» не взята от источника"
  code=$(curl -sk -o /dev/null -w '%{http_code}' -X DELETE $API/instances/$SRC -H "authorization: Bearer $TOKEN")
  [ "$code" = 400 ] || fail "источник удалён, хотя из него собирают образы (код $code)"
  api GET "/image/requirements?instance_id=$SRC&dir=$IMGS" | jq -e '.basis == "none"' >/dev/null || fail "оценка места без описи и копий"
  # Первый набор: на площадке пусто — передаётся всё содержимое.
  rm -rf "$IMGS/$S2"
  J1=$(api POST /instances/$QI/ship | jq -r .job_id); wait_job "$J1"
  R1="$W/node/images/main/$QI/$S1/sda.qcow2"
  cmp "$IMGS/$S1/sda.qcow2" "$R1" || fail "qcow2 на площадке отличается от собранного"
  # Второй набор: изменены 2 блока диска — по каналу уходят только они, старый набор на площадке удаляется (хранить 1).
  mkdir -p "$IMGS/$S2"; qemu-img convert -f raw -O qcow2 -o preallocation=metadata "$W/disk2.raw" "$IMGS/$S2/sda.qcow2"
  J2=$(api POST /instances/$QI/ship | jq -r .job_id); wait_job "$J2"
  cmp "$IMGS/$S2/sda.qcow2" "$W/node/images/main/$QI/$S2/sda.qcow2" || fail "второй набор на площадке отличается"
  api GET /jobs/$J2 | jq -e '.result | .base == "'$S1'" and .changed > 0 and .changed <= 8 and .chunks >= 64' >/dev/null || fail "второй набор передан не изменениями: $(api GET /jobs/$J2 | jq -c '.result')"
  [ ! -d "$W/node/images/main/$QI/$S1" ] || fail "старый набор на площадке не удалён (хранить 1)"
  qemu-img check "$W/node/images/main/$QI/$S2/sda.qcow2" >/dev/null || fail "qcow2 на площадке не проходит qemu-img check"
  # Повтор без нового набора — ничего не передаётся.
  J3=$(api GET /jobs/$(api POST /instances/$QI/ship | jq -r .job_id) | jq -r .id); wait_job "$J3"
  api GET /jobs/$J3 | jq -e '.result.already == true' >/dev/null || fail "повторная отправка того же набора"
  # Карточка площадки: где лежат образы.
  api GET /sites | jq -e --arg id "$SITE" --arg s "$S2" '.[] | select(.id==$id) | .node_disk.images.sets | any(.set == $s and .complete)' >/dev/null || fail "образы не показаны на карточке площадки"
  api DELETE "/instances/$QI" >/dev/null; api DELETE "/instances/$SRC" >/dev/null
  echo "ok: qcow2 на площадке совпадает побайтно; второй набор — $(api GET /jobs/$J2 | jq -r '.result | "\(.changed) из \(.chunks) участков"')"
else
  echo "qemu-img нет — проверка отправки qcow2 пропущена"
fi

printf '\n\033[1;32mСКВОЗНАЯ ПРОВЕРКА ПРОЙДЕНА\033[0m\n'
