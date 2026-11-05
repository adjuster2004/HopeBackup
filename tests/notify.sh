#!/usr/bin/env bash
# Проверка почты, восстановления доступа и уведомлений (нужны jq, curl, openssl, python3):
#   заглушка SMTP (tests/mocks/smtp_sink.py) на 127.0.0.1:2525 принимает письма в каталог;
#   сервер управления: настройки почты, тестовое письмо, проверочное уведомление;
#   «Забыли пароль?»: письмо со ссылкой на адрес пользователя → проверка ссылки → новый пароль → вход;
#   ответ одинаковый для несуществующих имён; без адреса — ссылка в файле 0600 на сервере; ссылка
#   одноразовая; старые сеансы закрываются; ограничение частоты; уведомление администраторам;
#   уведомления о событиях: подбор пароля, агент не на связи и снова на связи, мало места, ошибка
#   копирования, успешное копирование;
#   аварийный токен вендора (если задан BIN_TESTKEY — сборка с тестовым ключом tests/mocks/vendor_test_*.pem):
#   чужой сервер, чужой ключ, истёкший срок, повтор — отклоняются; верный токен задаёт пароль.
#
#   BIN=./hopebackup-core/target/release/hopebackup [BIN_TESTKEY=…] tests/notify.sh
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-notify}
SRV_PORT=17843; SMTP_PORT=2525

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() { for f in smtp server agent server2; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done; }
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W" /mnt/hb-notify-bad /mnt/hb-notify-site; mkdir -p "$W/mail" "$W/src"; echo data > "$W/src/a.txt"

python3 "$HERE/mocks/smtp_sink.py" $SMTP_PORT "$W/mail" > "$W/smtp.log" 2>&1 & echo $! > "$W/smtp.pid"
mails() { python3 "$HERE/mocks/read_mails.py" "$W/mail"; }
# Ждать письмо: тема содержит $1 (и получатель $2, если задан); печатает JSON письма.
wait_mail() {
  for i in $(seq 1 60); do
    m=$(mails | jq -c --arg s "$1" --arg t "${2:-}" 'select((.subject|contains($s)) and (.to|contains($t)))' | tail -1)
    [ -n "$m" ] && { echo "$m"; return 0; }
    sleep 0.5
  done
  return 1
}
nmail() { mails | jq -c --arg s "$1" 'select(.subject|contains($s))' | wc -l; }

say "Сервер управления (короткие пределы монитора — только для стенда)"
start_server() {
  HOPEBACKUP_TESTING=1 HOPEBACKUP_TEST_NOTIFY_DELAY=1 HOPEBACKUP_TEST_NOTIFY_EVERY=1 HOPEBACKUP_TEST_OFFLINE_SECS=4 HOPEBACKUP_TEST_SPACE_PCT=101 \
    HOPEBACKUP_DATA="$W/srv" "$1" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
  for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
}
start_server "$BIN"
API=https://127.0.0.1:$SRV_PORT/api
login() { curl -sk $API/auth/login -H 'content-type: application/json' -d "$(jq -n --arg u "$1" --arg p "$2" '{user:$u, password:$p}')"; }
ADMIN_PW=$(cat $W/srv/server/initial_admin_password.txt)
TOKEN=$(login Administrator "$ADMIN_PW" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
pub() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H 'content-type: application/json' "$@"; }
wait_job() { local id=$1 st; for i in $(seq 1 120); do st=$(api GET /jobs/$id | jq -r .status); [ "$st" = ok ] || [ "$st" = failed ] && { echo "$st"; return 0; }; sleep 1; done; fail "задание не завершилось"; }

say "Почта: настройки, проверка, тестовое письмо"
api GET /mail | jq -e '.configured == false and (.catalog|length) >= 10 and .notify.enabled == false' >/dev/null || fail "начальное состояние раздела почты"
api PUT /mail/smtp -d '{"host":"127.0.0.1:25","security":"none","from":"hb@test.local"}' | jq -r .error | grep -q "без порта" || fail "адрес сервера с портом должен отклоняться"
api PUT /mail/smtp -d "{\"host\":\"127.0.0.1\",\"port\":$SMTP_PORT,\"security\":\"none\",\"from\":\"hb@test.local\",\"from_name\":\"Ковчег\",\"username\":\"\",\"password\":\"smtp-secret\"}" | jq -e .ok >/dev/null || fail "настройки почты не сохранены"
api GET /mail | jq -e '.configured and .mail.has_password and (.mail.password == null)' >/dev/null || fail "пароль SMTP не должен возвращаться"
api POST /mail/test -d '{"to":"admin@test.local"}' | jq -e .ok >/dev/null || fail "тестовое письмо"
wait_mail "Проверка почты" admin@test.local | jq -c '{to, subject}' || fail "тестовое письмо не пришло"
api POST /mail/test -d "{\"to\":\"admin@test.local\",\"smtp\":{\"host\":\"127.0.0.1\",\"port\":1,\"security\":\"none\",\"from\":\"hb@test.local\"}}" | jq -r .error | tee "$W/smtp_err.txt"
grep -q "сервер почты" "$W/smtp_err.txt" || fail "ошибка отправки должна быть понятной"
# Оператор не видит раздел почты.
api POST /users -d '{"name":"oper","password":"oper-pass-1","role":"operator"}' >/dev/null
OT=$(login oper oper-pass-1 | jq -r .token)
curl -sk -o /dev/null -w '%{http_code}' $API/mail -H "authorization: Bearer $OT" | grep -q 403 || fail "раздел почты доступен не администратору"

say "Уведомления: получатели, события, проверочное письмо"
api PUT /mail/notify -d '{"enabled":true,"recipients":["bad-address"],"events":[]}' | jq -r .error | grep -q "неверный адрес" || fail "неверный получатель принят"
EVENTS=$(api GET /mail | jq -c '[.catalog[].id]')
api PUT /mail/notify -d "{\"enabled\":true,\"recipients\":[\"admin@test.local, duty@test.local\"],\"events\":$EVENTS}" | jq -e .ok >/dev/null || fail "настройки уведомлений"
api GET /mail | jq -e '.notify.recipients == ["admin@test.local","duty@test.local"]' >/dev/null || fail "получатели не разобраны"
api POST /mail/test-notify | jq -e '.ok' >/dev/null || fail "проверочное уведомление"
wait_mail "Проверка уведомлений" duty@test.local | jq -c '{to, subject}' || fail "проверочное уведомление не пришло"
api GET /mail | jq -e 'any(.log[]; .event == "test" and .ok)' >/dev/null || fail "журнал отправки"

say "Забыли пароль? Ссылка по почте"
api POST /users -d '{"name":"ivan","password":"ivan-old-pass","role":"restore","email":"bad"}' | jq -r .error | grep -q "почты" || fail "неверный адрес почты пользователя принят"
api POST /users -d '{"name":"ivan","password":"ivan-old-pass","role":"restore","email":"Ivan@Test.local"}' | jq -e .ok >/dev/null || fail "пользователь не создан"
api GET /users | jq -e '.[] | select(.name=="ivan") | .email == "Ivan@Test.local"' >/dev/null || fail "почта пользователя"
IT=$(login ivan ivan-old-pass | jq -r .token); [ "$IT" != null ] || fail "вход ivan"
R1=$(pub POST /auth/recover-request -d '{"identifier":"ivan@test.local"}'); R2=$(pub POST /auth/recover-request -d '{"identifier":"nobody"}')
[ "$R1" = "$R2" ] && echo "$R1" | jq -e .ok >/dev/null || fail "ответ должен быть одинаковым для любых имён ($R1 / $R2)"
M=$(wait_mail "Сброс пароля" Ivan@Test.local) || fail "письмо со ссылкой не пришло"
LINK=$(echo "$M" | jq -r .body | grep -o 'https://[^ ]*reset-password?token=[A-Za-z0-9_-]*' | head -1)
echo "  ссылка: ${LINK%%token=*}token=…"
RT=${LINK##*token=}
pub GET "/auth/recover-check?token=$RT" | jq -e '.user == "ivan"' >/dev/null || fail "ссылка не проверяется"
pub GET "/auth/recover-check?token=wrong" | jq -r .error | grep -q "недействительна" || fail "чужой токен принят"
pub POST /auth/recover-reset -d "{\"token\":\"$RT\",\"password\":\"short\"}" | jq -r .error | grep -q "8 символов" || fail "короткий пароль принят"
pub POST /auth/recover-reset -d "{\"token\":\"$RT\",\"password\":\"ivan-new-pass\"}" | jq -e '.user == "ivan"' >/dev/null || fail "сброс по ссылке"
pub POST /auth/recover-reset -d "{\"token\":\"$RT\",\"password\":\"ivan-new-pass2\"}" | jq -r .error | grep -q "недействительна" || fail "ссылка должна быть одноразовой"
login ivan ivan-old-pass | jq -e '.token == null' >/dev/null || fail "старый пароль ещё действует"
login ivan ivan-new-pass | jq -e '.token' >/dev/null || fail "вход с новым паролем"
curl -sk $API/auth/me -H "authorization: Bearer $IT" | jq -r .error | grep -q "войдите заново" || fail "старый сеанс не закрыт"
wait_mail "сменён по ссылке сброса" admin@test.local | jq -c '{subject}' || fail "уведомление администраторам о сбросе"
echo "ok: ссылка по почте, одноразовая, сеансы закрыты, администраторам — уведомление"

say "Без адреса почты — ссылка в файле на сервере"
pub POST /auth/recover-request -d '{"identifier":"Administrator"}' | jq -e .ok >/dev/null
F="$W/srv/server/password_reset_link.txt"
[ -f "$F" ] && [ "$(stat -c %a "$F")" = 600 ] || fail "файл ссылки (0600) не создан"
RT=$(grep -o 'token=[A-Za-z0-9_-]*' "$F" | head -1 | cut -d= -f2)
pub POST /auth/recover-reset -d "{\"token\":\"$RT\",\"password\":\"admin-new-pass\"}" | jq -e '.user == "Administrator"' >/dev/null || fail "сброс администратора по ссылке из файла"
[ ! -f "$F" ] || fail "файл ссылки должен удаляться после сброса"
curl -sk $API/auth/me -H "authorization: Bearer $TOKEN" | jq -r .error | grep -q "войдите заново" || fail "сеанс администратора не закрыт"
TOKEN=$(login Administrator admin-new-pass | jq -r .token); [ "$TOKEN" != null ] || fail "вход администратора с новым паролем"
# Своя смена пароля: этот сеанс продолжает работать с новым токеном.
NT=$(api PUT /users/Administrator/password -d '{"password":"admin-pass-3"}' | jq -r .token); [ "$NT" != null ] || fail "новый токен при смене своего пароля"
curl -sk $API/auth/me -H "authorization: Bearer $TOKEN" | jq -r .error | grep -q "войдите заново" || fail "старый токен после смены пароля"
TOKEN=$NT; api GET /auth/me | jq -e '.user == "Administrator"' >/dev/null || fail "новый токен не работает"

say "Ограничение частоты запросов сброса"
for i in $(seq 1 12); do C=$(curl -sk -o /dev/null -w '%{http_code}' $API/auth/recover-request -H 'content-type: application/json' -d '{"identifier":"x"}'); done
[ "$C" = 429 ] || fail "нет ограничения частоты ($C)"
echo "ok: после 10 запросов за 15 минут — 429"

say "Уведомление: подбор пароля"
for i in 1 2 3 4 5; do login oper wrong-pass >/dev/null; done
wait_mail "Подбор пароля" admin@test.local | jq -c '{subject}' || fail "уведомление о подборе пароля"

say "Уведомление: мало места (порог стенда — 101%)"
wait_mail "Мало места: сервер управления" admin@test.local | jq -c '{subject}' || fail "уведомление о месте"

say "Уведомления: агент не на связи и снова на связи, ошибка и успех копирования"
SRV_FP=$(api GET /status | jq -r .fingerprint)
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$SRV_FP" --name nt-host >/dev/null
HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
sleep 2
AGENT=$(api GET /agents | jq -r '.[0].id')
BAD=$(api POST /sites -d "{\"name\":\"Недоступная\",\"kind\":\"disk\",\"disk_path\":\"/mnt/hb-notify-bad\",\"public_url\":\"https://127.0.0.1:1\",\"password\":\"site-password-1\"}" | jq -r .id)
INST=$(api POST /instances -d "{\"name\":\"files-bad\",\"catalog_id\":\"files\",\"agent_id\":\"$AGENT\",\"site_id\":\"$BAD\",\"config\":{\"paths\":[\"$W/src\"]}}" | jq -r .id)
J=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); [ "$(wait_job "$J")" = failed ] || fail "копирование на недоступную площадку должно завершиться ошибкой"
M=$(wait_mail "Ошибка копирования" admin@test.local) || fail "уведомление об ошибке копирования"
echo "$M" | jq -c '{subject}'; echo "$M" | jq -r .body | grep -q "Конец журнала" || fail "в письме нет конца журнала"
echo "$M" | jq -r .body | grep -q "агент «nt-host»" || fail "в письме нет имени агента"
GOOD=$(api POST /sites -d "{\"name\":\"Диск\",\"kind\":\"disk\",\"disk_path\":\"/mnt/hb-notify-site\",\"public_url\":\"https://127.0.0.1:$SRV_PORT\",\"password\":\"site-password-1\"}" | jq -r .id)
[ "$(wait_job "$(api POST /sites/$GOOD/init | jq -r .job_id)")" = ok ] || fail "площадка"
INST2=$(api POST /instances -d "{\"name\":\"files-ok\",\"catalog_id\":\"files\",\"agent_id\":\"$AGENT\",\"site_id\":\"$GOOD\",\"config\":{\"paths\":[\"$W/src\"]}}" | jq -r .id)
[ "$(wait_job "$(api POST /instances/$INST2/backup -d '{}' | jq -r .job_id)")" = ok ] || fail "копирование"
wait_mail "Копия готова" admin@test.local | jq -c '{subject}' || fail "уведомление об успешном копировании"
kill "$(cat "$W/agent.pid")"; rm -f "$W/agent.pid"
wait_mail "Агент не на связи: nt-host" admin@test.local | jq -c '{subject}' || fail "уведомление «агент не на связи»"
N1=$(nmail "Агент не на связи"); sleep 3; [ "$(nmail "Агент не на связи")" = "$N1" ] || fail "о том же состоянии повторно писать не нужно"
HOPEBACKUP_DATA="$W/agent" "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
wait_mail "Агент снова на связи: nt-host" admin@test.local | jq -c '{subject}' || fail "уведомление «агент снова на связи»"
# Выключенное событие не отправляется.
api PUT /mail/notify -d '{"enabled":true,"recipients":["admin@test.local"],"events":["backup_failed"]}' >/dev/null
N2=$(nmail "Копия готова"); [ "$(wait_job "$(api POST /instances/$INST2/backup -d '{}' | jq -r .job_id)")" = ok ]; sleep 2
[ "$(nmail "Копия готова")" = "$N2" ] || fail "выключенное событие отправлено"
api GET /mail | jq -r '.log[:6][] | "  \(.ok) \(.subject)"'
echo "ok: уведомления о событиях и возврате в норму, без повторов, по выбранным событиям"

if [ -n "${BIN_TESTKEY:-}" ]; then
  say "Аварийный токен вендора (сборка с тестовым ключом)"
  kill "$(cat "$W/server.pid")"; sleep 1
  start_server "$(readlink -f "$BIN_TESTKEY")"
  REQ=$(pub GET /auth/recovery-request)
  FP=$(echo "$REQ" | jq -r .fingerprint); echo "$REQ" | jq -r .req | head -3
  b64u() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
  jwt() { # $1 — утверждения (JSON), $2 — закрытый ключ
    local h p s
    h=$(printf '{"alg":"RS256","typ":"JWT"}' | b64u); p=$(printf '%s' "$1" | b64u)
    s=$(printf '%s.%s' "$h" "$p" | openssl dgst -sha256 -sign "$2" -binary | b64u)
    echo "$h.$p.$s"
  }
  KEY="$HERE/mocks/vendor_test_key.pem"
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$W/other.pem" 2>/dev/null
  EXP=$(( $(date +%s) + 3600 ))
  claims() { jq -nc --arg sub "$1" --argjson exp "$2" --arg jti "$3" '{sub:$sub, exp:$exp, iss:"HOPE-LICENSE-SERVER", product:"HopeBackup", purpose:"admin-recovery", user:"Administrator", new_password:"from-vendor-token-1", jti:$jti}'; }
  emerg() { pub POST /auth/recover-emergency -d "$(jq -nc --arg t "$1" '{token:$t}')"; }
  emerg "$(jwt "$(claims WRONGFP $EXP j1)" "$KEY")" | jq -r .error | grep -q "другого сервера" || fail "токен чужого сервера принят"
  emerg "$(jwt "$(claims "$FP" $EXP j2)" "$W/other.pem")" | jq -r .error | grep -q "подпись" || fail "токен с чужой подписью принят"
  emerg "$(jwt "$(claims "$FP" $(( $(date +%s) - 60 )) j3)" "$KEY")" | jq -r .error | grep -q "истёк" || fail "истёкший токен принят"
  emerg "$(jwt "$(jq -nc --arg sub "$FP" --argjson exp $EXP '{sub:$sub, exp:$exp, purpose:"license", new_password:"x12345678"}')" "$KEY")" | jq -r .error | grep -q "не для восстановления" || fail "токен лицензии принят как токен восстановления"
  GOODTOK=$(jwt "$(claims "$FP" $EXP j-good)" "$KEY")
  emerg "$GOODTOK" | jq -e '.user == "Administrator"' >/dev/null || fail "верный токен не принят"
  login Administrator from-vendor-token-1 | jq -e .token >/dev/null || fail "вход с паролем из токена"
  emerg "$GOODTOK" | jq -r .error | grep -q "уже использован" || fail "повтор токена принят"
  echo "ok: аварийный токен — только этого сервера, подписанный вендором, в срок и один раз"
else
  echo "(аварийный токен: пропущено — задайте BIN_TESTKEY, сборка с --features testkey)"
fi

printf '\n\033[1;32mПРОВЕРКА ПОЧТЫ, ВОССТАНОВЛЕНИЯ ДОСТУПА И УВЕДОМЛЕНИЙ ПРОЙДЕНА\033[0m\n'
