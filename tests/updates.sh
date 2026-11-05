#!/usr/bin/env bash
# «Обновления»: сервер управления и агент обновляются из панели — архивом выпуска (закрытый контур) и онлайн
# по манифесту; неудачная новая версия — автоматический откат; агент скачивает свою сборку с сервера.
# Служб systemd на стенде нет: перезапуск выполняет tests-скрипт (HOPEBACKUP_UPDATER_RESTART), установщик
# запускается отдельным сеансом (setsid) — как под OpenRC.
#
#   BIN=…/0.8.1/hopebackup NEXT=…/0.8.99/hopebackup tests/updates.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
NEXT=$(readlink -f "${NEXT:-./hopebackup-core/target-next/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-upd}
PORT=17633; WWW=17634
SITE_DIR=/mnt/hb-upd-site
cleanup() {
  for p in agent server www; do [ -f "$W/$p.pid" ] && kill "$(cat "$W/$p.pid")" 2>/dev/null || true; done
  pkill -f "$W/srv/server/updates/hopebackup-applier" 2>/dev/null || true
  pkill -f "$W/agent/updates/hopebackup-applier" 2>/dev/null || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W" "$SITE_DIR"; mkdir -p "$W/opt/dist" "$W/aopt" "$W/www"
CUR=$("$BIN" --version | awk '{print $2}'); NEW=$("$NEXT" --version | awk '{print $2}')
[ "$CUR" != "$NEW" ] || fail "нужны две разные версии (BIN и NEXT)"
say "Версии: установлена $CUR, новая $NEW"

# Архив выпуска: dist/ со сборкой и контрольной суммой, VERSION.
mkpkg() { # mkpkg <программа> <версия>
  local d="$W/pkg/hopebackup-$2"; rm -rf "$W/pkg"; mkdir -p "$d/dist"
  gzip -c "$1" > "$d/dist/hopebackup-x86_64-gnu.gz"
  (cd "$d/dist" && sha256sum hopebackup-x86_64-gnu.gz > hopebackup-x86_64-gnu.gz.sha256)
  echo "$2" > "$d/VERSION"
  tar -czf "$W/hopebackup-$2-linux.tar.gz" -C "$W/pkg" "hopebackup-$2"
}
mkpkg "$NEXT" "$NEW"; mkpkg "$BIN" "$CUR"
cp "$BIN" "$W/opt/hopebackup"; cp "$BIN" "$W/aopt/hopebackup"
gzip -c "$BIN" > "$W/opt/dist/hopebackup-x86_64-gnu.gz"; (cd "$W/opt/dist" && sha256sum hopebackup-x86_64-gnu.gz > hopebackup-x86_64-gnu.gz.sha256)

# «Служба»: перезапуск по команде установщика. Файл fail-<роль> с номером версии — эта версия не запустится.
cat > "$W/restart.sh" <<EOF
#!/bin/sh
role=\$1; W=$W
if [ "\$role" = server ]; then OPT=\$W/opt; else OPT=\$W/aopt; fi
P=\$W/\$role.pid
if [ -f \$P ]; then kill \$(cat \$P) 2>/dev/null; i=0; while kill -0 \$(cat \$P) 2>/dev/null && [ \$i -lt 50 ]; do sleep 0.2; i=\$((i+1)); done; fi
V=\$(\$OPT/hopebackup --version | awk '{print \$2}')
BROKEN=0; [ -f \$W/fail-\$role ] && [ "\$(cat \$W/fail-\$role)" = "\$V" ] && BROKEN=1
export HOPEBACKUP_UPDATER_FORCE=1 HOPEBACKUP_UPDATER_WAIT=25 HOPEBACKUP_UPDATER_RESTART="sh \$W/restart.sh \$role" HOPEBACKUP_SERVICE=hopebackup-\$role
if [ "\$role" = server ]; then
  if [ \$BROKEN = 1 ]; then A="server --listen не-адрес"; else A="server --listen 127.0.0.1:$PORT"; fi
  HOPEBACKUP_DATA=\$W/srv nohup \$OPT/hopebackup \$A >> \$W/server.log 2>&1 &
else
  if [ \$BROKEN = 1 ]; then A="agent нет-такой-команды"; else A="agent run"; fi
  HOPEBACKUP_DATA=\$W/agent nohup \$OPT/hopebackup \$A >> \$W/agent.log 2>&1 &
fi
echo \$! > \$P
echo "\$(date +%T) \$role: запущена \$V\$([ \$BROKEN = 1 ] && echo ' (сломанная)')" >> \$W/restarts.log
EOF

say "Сервер управления $CUR, площадка, агент $CUR"
sh "$W/restart.sh" server
API=https://127.0.0.1:$PORT/api
for i in $(seq 1 50); do curl -sk $API/health >/dev/null && [ -f "$W/srv/server/initial_admin_password.txt" ] && break; sleep 0.2; done
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat "$W/srv/server/initial_admin_password.txt")\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
ver() { curl -sk -D - -o /dev/null $API/health 2>/dev/null | tr -d '\r' | sed -n 's/^x-hopebackup-version: //Ip'; }
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$PORT --token "$ET" --fingerprint "$(api GET /status | jq -r .fingerprint)" --name app01 >/dev/null
sh "$W/restart.sh" agent
for i in $(seq 1 40); do [ "$(api GET /updates | jq -r '.agents[0].version')" = "$CUR" ] && break; sleep 0.5; done
api GET /updates | jq -e ".current_version == \"$CUR\" and .agents[0].build == \"x86_64-gnu\" and .agents[0].outdated == false and .updater_installed" >/dev/null || { api GET /updates | jq .; fail "состояние до обновления"; }
echo "ok: панель видит версию сервера и агента, сборку агента"

say "Пакет из архива: проверка, отказ для не-программы, удаление"
echo "не программа" | gzip > "$W/junk.gz"
api POST "/updates/upload?filename=junk.gz" -H 'content-type: application/octet-stream' --data-binary @"$W/junk.gz" | jq -r .error | grep -q "не является программой" || fail "мусор принят как пакет"
R=$(api POST "/updates/upload?filename=hopebackup-$NEW-linux.tar.gz" -H 'content-type: application/octet-stream' --data-binary @"$W/hopebackup-$NEW-linux.tar.gz")
echo "$R" | jq -e ".version == \"$NEW\" and (.kit | index(\"x86_64-gnu\")) != null" >/dev/null || fail "архив выпуска: $R"
api POST "/updates/upload?filename=x.tar.gz&version=9.9.9" -H 'content-type: application/octet-stream' --data-binary @"$W/hopebackup-$NEW-linux.tar.gz" | jq -r .error | grep -q "другая версия" || fail "проверка указанной версии"
api GET /updates | jq -e ".offline_package.version == \"$NEW\" and .target_version == \"$NEW\"" >/dev/null || fail "пакет не готов к установке"
api POST /updates/discard >/dev/null
api GET /updates | jq -e '.offline_package == null and .target_version == ""' >/dev/null || fail "пакет не удалён"
echo "ok: архив выпуска проверен (сборки, суммы, версия), мусор отклонён, пакет удаляется"

say "Загрузка частями: обрыв, продолжение, повтор части, контрольная сумма"
ARC="$W/hopebackup-$NEW-linux.tar.gz"; SZ=$(stat -c %s "$ARC"); SHA=$(sha256sum "$ARC" | awk '{print $1}')
split -b 4194304 -d -a 3 "$ARC" "$W/part."
S1=$(api POST /updates/upload/start -d "{\"filename\":\"hopebackup-$NEW-linux.tar.gz\",\"size\":$SZ,\"sha256\":\"$SHA\"}")
UID_=$(echo "$S1" | jq -r .id); [ "$(echo "$S1" | jq -r .received)" = 0 ] || fail "начало загрузки: $S1"
# Первая часть, затем «обрыв»: продолжение по тому же файлу возвращает ту же загрузку и принятый объём.
api PUT "/updates/upload/$UID_?offset=0" -H 'content-type: application/octet-stream' --data-binary @"$W/part.000" | jq -e '.received == 4194304' >/dev/null || fail "первая часть"
S2=$(api POST /updates/upload/start -d "{\"filename\":\"hopebackup-$NEW-linux.tar.gz\",\"size\":$SZ,\"sha256\":\"$SHA\",\"resume\":\"$UID_\"}")
echo "$S2" | jq -e ".id == \"$UID_\" and .received == 4194304 and .resumed" >/dev/null || fail "продолжение загрузки: $S2"
# Повтор уже принятой части (ответ потерялся) ничего не дописывает.
api PUT "/updates/upload/$UID_?offset=0" -H 'content-type: application/octet-stream' --data-binary @"$W/part.000" | jq -e '.received == 4194304' >/dev/null || fail "повтор части не должен дописывать"
# Завершить раньше времени нельзя.
api POST "/updates/upload/$UID_/finish" | jq -r .error | grep -q "продолжите загрузку" || fail "незаконченная загрузка принята"
OFF=4194304
for f in $(ls "$W"/part.* | tail -n +2); do OFF=$(api PUT "/updates/upload/$UID_?offset=$OFF" -H 'content-type: application/octet-stream' --data-binary @"$f" | jq -r .received); done
[ "$OFF" = "$SZ" ] || fail "передано $OFF из $SZ"
R=$(api POST "/updates/upload/$UID_/finish"); echo "$R" | jq -e ".version == \"$NEW\"" >/dev/null || fail "сборка из частей: $R"
api POST /updates/discard >/dev/null
# Неверная контрольная сумма — отказ после сборки, файл удаляется.
S3=$(api POST /updates/upload/start -d "{\"filename\":\"x.tar.gz\",\"size\":$(stat -c %s "$W/part.000"),\"sha256\":\"$(printf '0%.0s' $(seq 64))\"}" | jq -r .id)
api PUT "/updates/upload/$S3?offset=0" -H 'content-type: application/octet-stream' --data-binary @"$W/part.000" >/dev/null
api POST "/updates/upload/$S3/finish" | jq -r .error | grep -q "контрольной сумме" || fail "неверная контрольная сумма принята"
rm -f "$W"/part.*
echo "ok: загрузка частями — продолжение после обрыва, повтор части безопасен, сумма сверяется"

say "Онлайн-источник: манифест без контрольной суммы отклоняется; с ней — доступно обновление"
cp "$W/hopebackup-$NEW-linux.tar.gz" "$W/www/"
echo "{\"stable\":{\"version\":\"$NEW\",\"url\":\"hopebackup-$NEW-linux.tar.gz\",\"notes\":\"проверка\"}}" > "$W/www/manifest.json"
(cd "$W/www" && exec python3 -m http.server $WWW --bind 127.0.0.1 >/dev/null 2>&1) & echo $! > "$W/www.pid"; sleep 0.8
api PUT /updates/config -d "{\"channel\":\"stable\",\"source_url\":\"http://127.0.0.1:$WWW/manifest.json\",\"auto_check\":true}" >/dev/null
api POST /updates/check | jq -e '.kind == "error" and (.note | contains("sha256"))' >/dev/null || fail "манифест без sha256 принят"
SHA=$(sha256sum "$W/www/hopebackup-$NEW-linux.tar.gz" | cut -d' ' -f1)
echo "{\"stable\":{\"version\":\"$NEW\",\"url\":\"hopebackup-$NEW-linux.tar.gz\",\"sha256\":\"$SHA\",\"notes\":\"проверка обновления\"}}" > "$W/www/manifest.json"
api POST /updates/check | jq -e ".update_available and .latest_version == \"$NEW\"" >/dev/null || fail "обновление не найдено"
echo "ok: доступна $NEW"

say "Установка онлайн: загрузка, проверка, замена программы и комплекта, перезапуск"
api POST /updates/apply | jq -e .ok >/dev/null || fail "установка не начата"
for i in $(seq 1 120); do [ "$(ver)" = "$NEW" ] && break; sleep 0.5; done
[ "$(ver)" = "$NEW" ] || { tail -20 "$W/srv/server/updates/apply.log" 2>/dev/null; fail "сервер не перешёл на $NEW"; }
for i in $(seq 1 40); do api GET /updates | jq -e '.job.finished' >/dev/null 2>&1 && break; sleep 0.5; done
api GET /updates | jq -e ".job.phase == \"done\" and .job.failed == false and .current_version == \"$NEW\"" >/dev/null || { api GET /updates | jq .job; fail "итог установки"; }
cmp "$W/opt/hopebackup.prev" "$BIN" || fail "прежняя программа не сохранена"
gunzip -c "$W/opt/dist/hopebackup-x86_64-gnu.gz" | cmp - "$NEXT" || fail "комплект сборок не обновлён"
[ -d "$W/opt/dist.prev" ] || fail "прежний комплект не сохранён"
for i in $(seq 1 30); do api GET "/audit?limit=50" | jq -r '.[].detail' | grep -q "обновление $CUR → $NEW установлено" && break; sleep 0.5; done
api GET "/audit?limit=50" | jq -r '.[].detail' | grep -q "обновление $CUR → $NEW установлено" || fail "нет итога в журнале аудита"
[ ! -f "$W/srv/server/updates/package.bin" ] || fail "пакет не убран после установки"
echo "ok: сервер на $NEW, прежняя программа и комплект сохранены, итог в аудите"

say "Агент: первая попытка — новая версия не выходит на связь → откат на агенте"
for i in $(seq 1 40); do api GET /updates | jq -e '.agents[0].outdated' >/dev/null && break; sleep 0.5; done
echo "$NEW" > "$W/fail-agent"
J=$(api POST /updates/agents -d '{"ids":[]}' | jq -r '.jobs[0].job_id'); [ "$J" != null ] || fail "обновление агента не начато"
for i in $(seq 1 60); do st=$(api GET /jobs/$J | jq -r .status); [ "$st" = ok ] || [ "$st" = failed ] && break; sleep 0.5; done
[ "$(api GET /jobs/$J | jq -r .status)" = ok ] || { api GET /jobs/$J | jq -r '.log[].m'; fail "задание обновления агента"; }
for i in $(seq 1 100); do api GET /updates | jq -e '.agents[0].update.finished and .agents[0].online' >/dev/null 2>&1 && break; sleep 0.5; done
api GET /updates | jq -e ".agents[0].update.phase == \"rolled_back\" and .agents[0].version == \"$CUR\"" >/dev/null || { api GET /updates | jq '.agents[0]'; cat "$W/restarts.log"; fail "откат агента"; }
echo "ok: агент вернулся на $CUR и снова на связи"

say "Агент: повтор — обновление до $NEW"
rm -f "$W/fail-agent"
J=$(api POST /updates/agents -d '{"ids":[]}' | jq -r '.jobs[0].job_id')
for i in $(seq 1 120); do api GET /updates | jq -e ".agents[0].version == \"$NEW\" and .agents[0].update.phase == \"done\"" >/dev/null 2>&1 && break; sleep 0.5; done
api GET /updates | jq -e ".agents[0].version == \"$NEW\" and .agents[0].outdated == false and .agents[0].update.phase == \"done\"" >/dev/null || { api GET /updates | jq '.agents[0]'; fail "агент не обновился"; }
cmp "$W/aopt/hopebackup" "$NEXT" && cmp "$W/aopt/hopebackup.prev" "$BIN" || fail "программа агента"
echo "ok: агент на $NEW (сборка с сервера, SHA-256 сверена)"

say "Сервер: новая версия не запускается → автоматический откат"
echo "$CUR" > "$W/fail-server"
R=$(api POST "/updates/upload?filename=hopebackup-$CUR-linux.tar.gz" -H 'content-type: application/octet-stream' --data-binary @"$W/hopebackup-$CUR-linux.tar.gz")
echo "$R" | jq -r .note | grep -q "более старая" || fail "понижение версии не отмечено"
api POST /updates/apply | jq -e .ok >/dev/null
for i in $(seq 1 160); do api GET /updates 2>/dev/null | jq -e '.job.finished' >/dev/null 2>&1 && break; sleep 0.5; done
api GET /updates | jq -e ".job.phase == \"rolled_back\" and .current_version == \"$NEW\"" >/dev/null || { api GET /updates | jq .job; cat "$W/restarts.log"; fail "откат сервера"; }
[ "$(ver)" = "$NEW" ] || fail "после отката сервер не на $NEW"
gunzip -c "$W/opt/dist/hopebackup-x86_64-gnu.gz" | cmp - "$NEXT" || fail "комплект после отката"
for i in $(seq 1 30); do api GET "/audit?limit=50" | jq -r '.[].detail' | grep -q "не установлено" && break; sleep 0.5; done
api GET "/audit?limit=50" | jq -r '.[].detail' | grep -q "не установлено" || fail "нет неудачи в журнале аудита"
grep -q "сломанная" "$W/restarts.log" || fail "сломанная версия не запускалась"
echo "ok: откат — сервер снова на $NEW, комплект на месте, причина в аудите"

printf '\n\033[1;32mПРОВЕРКА ОБНОВЛЕНИЙ ПРОЙДЕНА\033[0m\n'
