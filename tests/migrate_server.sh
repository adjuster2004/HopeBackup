#!/usr/bin/env bash
# «Переезд» через сервер управления: проект переезда, синхронизации, автоматическое применение на цели,
# тестовый запуск без сети, переключение (последняя синхронизация → применение → выключение источника →
# запуск ВМ), откат и повторное переключение, завершение.
# Источник и цель — один агент на этой машине: диски источника — loop с ext4 под записью; цель — имитация
# узла Proxmox (tests/mocks/pve_mock.sh) с настоящими томами. Выключение источника пропускается
# (HOPEBACKUP_NO_POWEROFF), остальное — как на стенде.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/migrate_server.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
. "$HERE/mocks/pve_mock.sh"
W=${W:-/tmp/hb-migsrv}
SRV_PORT=17611
SITE_DIR=/mnt/hb-migsrv-site
cleanup() {
  [ -n "${WR:-}" ] && kill "$WR" 2>/dev/null || true
  for p in agent server; do [ -f "$W/$p.pid" ] && kill "$(cat "$W/$p.pid")" 2>/dev/null || true; done
  mountpoint -q "$W/mnt" 2>/dev/null && { fsfreeze -u "$W/mnt" 2>/dev/null || true; umount -l "$W/mnt" || true; }
  mountpoint -q "$W/chk" 2>/dev/null && umount -l "$W/chk" || true
  for l in $(losetup -a | grep "$W/" | cut -d: -f1); do losetup -d "$l" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; sleep 0.5; rm -rf "$W" "$SITE_DIR"; mkdir -p "$W/mnt" "$W/chk"

say "Сервер, площадка «диск сервера», агент (он же источник и цель)"
HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat "$W/srv/server/initial_admin_password.txt")\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
SITE=$(api POST /sites -d "{\"name\":\"Диск\",\"kind\":\"disk\",\"disk_path\":\"$SITE_DIR\",\"public_url\":\"https://127.0.0.1:$SRV_PORT\",\"password\":\"site-password-1\"}" | jq -r .id)
[ "$SITE" != null ] || fail "площадка"
J=$(api POST /sites/$SITE/init | jq -r .job_id)
for i in $(seq 1 60); do [ "$(api GET /jobs/$J | jq -r .status)" = ok ] && break; sleep 0.5; done
pve_mock "$W/pve"
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$(api GET /status | jq -r .fingerprint)" --name web01 >/dev/null
PATH="$M/bin:$PATH" HOPEBACKUP_NO_POWEROFF=1 HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
AGENT=$(api GET /agents | jq -r '.[0].id')
wait_job() {
  local id=$1 st
  for i in $(seq 1 300); do
    st=$(api GET /jobs/$id | jq -r .status)
    [ "$st" = ok ] && return 0
    [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail -15; fail "задание $id: $(api GET /jobs/$id | jq -r .error)"; }
    sleep 0.5
  done
  fail "задание $id не завершилось"
}
# Ждать состояния переезда: wait_mig <выражение jq> <что ждём>
wait_mig() {
  for i in $(seq 1 240); do
    api GET /migrations/$MIG | jq -e "$1" >/dev/null && return 0
    sleep 0.5
  done
  api GET /migrations/$MIG | jq '{phase, busy, last_sync, applied: .state.applied, events: [.events[:6][] | .what]}'
  fail "не дождались: $2"
}

say "Источник: системный диск (ext4, запись идёт) и диск данных"
truncate -s 200M "$W/d1.img"; truncate -s 32M "$W/d2.img"
L1=$(losetup -f --show "$W/d1.img"); L2=$(losetup -f --show "$W/d2.img")
mkfs.ext4 -q "$L1"; mount "$L1" "$W/mnt"
head -c 30M /dev/urandom > "$W/mnt/base.bin"; mkdir "$W/mnt/etc"; echo web01 > "$W/mnt/etc/hostname"; sync
dd if=/dev/urandom of="$L2" bs=1M count=5 seek=3 conv=notrunc oflag=direct status=none
( i=0; while true; do echo "запись $i" >> "$W/mnt/app.log"; i=$((i+1)); sleep 0.05; done ) & WR=$!

say "Проект переезда: Proxmox, хранилище local-lvm, сеть vmbr1"
R=$(api POST /migrations -d "$(jq -n --arg a "$AGENT" --arg s "$SITE" --arg l1 "$L1" --arg l2 "$L2" '{name:"web01", source_agent:$a, site_id:$s, source:{disks:[$l1,$l2]}, target_agent:$a, platform:"proxmox", params:{storage:"local-lvm", networks:{"*":"vmbr1"}, start_delay:1}, auto_apply:true, shutdown_source:true}')")
MIG=$(echo "$R" | jq -r .id); [ "$MIG" != null ] || fail "переезд не создан: $R"
api POST /migrations -d "{\"name\":\"x\",\"source_agent\":\"$AGENT\",\"site_id\":\"$SITE\",\"target_agent\":\"$AGENT\",\"platform\":\"proxmox\",\"params\":{}}" | jq -r .error | grep -q "хранилище" || fail "без хранилища переезд создаваться не должен"
api DELETE /instances/$MIG | jq -r .error | grep -q "источник переезда" || fail "экземпляр переезда удаляется в обход"
api POST /migrations/$MIG/cutover | jq -r .error | grep -q "сначала примените" || fail "переключение до первого применения"
echo "ok: проект создан, проверки параметров и порядка шагов"

say "Синхронизация → автоматическое применение (создание ВМ)"
wait_job "$(api POST /migrations/$MIG/sync | jq -r .job_id)"
wait_mig '.last_sync.id != null and .state.applied == .last_sync.id and .busy == null' "первое применение"
grep -q "qm create 120" "$M/calls" || fail "ВМ на цели не создана"
api GET /migrations/$MIG | jq -r '.events[:3][] | "  " + .what'
echo "ok: ВМ 120 создана, синхронизация применена"

say "Тестовый запуск без сети и его окончание"
wait_job "$(api POST /migrations/$MIG/test-start | jq -r .job_id)"
wait_mig '.phase == "testing" and .busy == null' "тестовый запуск"
grep -q "link_down=1" "$M/net0-120" && [ -f "$M/running-120" ] || fail "ВМ теста: без сети и запущена"
api POST /migrations/$MIG/apply | jq -r .error | grep -q "тестовый запуск" || fail "применение во время теста"
# Синхронизация во время теста: применение откладывается.
wait_job "$(api POST /migrations/$MIG/sync | jq -r .job_id)"
api GET /migrations/$MIG | jq -e '.pending_apply == true' >/dev/null || fail "применение не отложено"
V1=$(cat "$M/vols/vm-120-disk-1"); dd if=/dev/urandom of="$V1" bs=1M count=2 seek=20 conv=notrunc status=none   # «гость» пишет
wait_job "$(api POST /migrations/$MIG/test-stop | jq -r .job_id)"
wait_mig '.phase == "syncing" and .busy == null and .state.applied == .last_sync.id and (.state.dirty // false) == false and .pending_apply == false' "окончание теста и отложенное применение"
api GET /migrations/$MIG | jq -r '.events[:3][] | "  " + .what'
echo "ok: тест без сети; отложенное применение выполнено после теста (тома переписаны: снимков нет)"

say "Переключение: последняя синхронизация → применение → выключение источника → запуск ВМ"
kill "$WR"; WR=
wait_job "$(api POST /migrations/$MIG/cutover | jq -r .job_id)"
wait_mig '.phase == "running" and .busy == null' "переключение"
api GET /migrations/$MIG | jq -r '.events[:5][] | "  " + .what'
[ -f "$M/running-120" ] && ! grep -q link_down "$M/net0-120" || fail "ВМ после переключения: запущена и в сети"
# Итог последней синхронизации доходит до сервера отдельным сообщением агента — ждём его до 10 с.
for i in $(seq 1 20); do api GET /migrations/$MIG | jq -e '.last_sync.cutover == true and .state.applied == .last_sync.id' >/dev/null && break; sleep 0.5; done
api GET /migrations/$MIG | jq -e '.last_sync.cutover == true and .state.applied == .last_sync.id' >/dev/null || fail "применена не последняя синхронизация: $(api GET /migrations/$MIG | jq -c '{last: .last_sync, applied: .state.applied}')"
api GET /instances/$MIG | jq -e '.schedule.enabled == false' >/dev/null || fail "расписание синхронизаций не выключено"
grep -q "пропущено: HOPEBACKUP_NO_POWEROFF" "$W/agent.log" || api GET /jobs | jq -r '.[].title' | grep -q "выключение источника" || fail "источник не выключался"
sum() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 md5sum); }
sum "$W/mnt" > "$W/src.md5"
V0=$(cat "$M/vols/vm-120-disk-0")
e2fsck -fn "$V0" >/dev/null 2>&1 || fail "ФС системного тома с ошибками"
mount -o ro,noload "$V0" "$W/chk"; sum "$W/chk" > "$W/t.md5"; umount "$W/chk"
diff "$W/src.md5" "$W/t.md5" >/dev/null || fail "файлы на цели отличаются от источника"
cmp -n $((32<<20)) "$L2" "$V1" || fail "диск данных на цели не равен источнику (следы теста?)"
api DELETE /migrations/$MIG | jq -r .error | grep -q "работает после переключения" || fail "удаление работающего переезда"
echo "ok: ВМ на цели работает, данные — как на источнике в момент переключения"

say "Откат и повторное переключение, завершение"
wait_job "$(api POST /migrations/$MIG/rollback | jq -r .job_id)"
wait_mig '.phase == "syncing" and .busy == null and .state.dirty == true' "откат"
[ ! -f "$M/running-120" ] || fail "ВМ после отката работает"
api GET /instances/$MIG | jq -e '.schedule.enabled == false' >/dev/null || fail "расписание без cron включено"
echo "после отката" > "$W/mnt/etc/note"; sync
wait_job "$(api POST /migrations/$MIG/cutover | jq -r .job_id)"
wait_mig '.phase == "running" and .busy == null' "повторное переключение"
V0=$(cat "$M/vols/vm-120-disk-0")
mount -o ro,noload "$V0" "$W/chk"; grep -q "после отката" "$W/chk/etc/note" || fail "изменения после отката не дошли"; umount "$W/chk"
wait_job "$(api POST /migrations/$MIG/finish | jq -r .job_id)"
wait_mig '.phase == "done" and .busy == null' "завершение"
api DELETE /migrations/$MIG | jq -e .ok >/dev/null || fail "удаление завершённого переезда"
api GET /instances | jq -e "map(select(.id == \"$MIG\")) | length == 0" >/dev/null || fail "экземпляр источника не удалён с переездом"
echo "ok: откат вернул проект к синхронизациям, повторное переключение донесло изменения, переезд завершён"

printf '\n\033[1;32mПРОВЕРКА ПЕРЕЕЗДА ЧЕРЕЗ СЕРВЕР ПРОЙДЕНА\033[0m\n'
