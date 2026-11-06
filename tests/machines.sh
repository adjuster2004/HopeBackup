#!/usr/bin/env bash
# Проверка раздела «Серверы и ВМ» (нужны root, OpenSSH-сервер, jq, curl, openssl, sudo):
#   на этой машине поднимается свой sshd (порт 2223) и два пользователя:
#     hbpw  — вход по паролю (keyboard-interactive/password), sudo С паролем;
#     hbkey — вход только по ключу (пароли для него запрещены в sshd), sudo без пароля;
#   сервер управления подключается к «ВМ» по SSH, проверяет вход и права, снимает опись (ОС, ядро,
#   загрузка, железо, диски, сеть) и выдаёт анализ с рекомендациями;
#   ключ хоста закрепляется при первом подключении: после смены ключа подключение отклоняется, пока
#   ключ не сброшен; пароль для hbkey не подходит;
#   рекомендации превращаются в экземпляры (агент привязан вручную);
#   цель восстановления «сервер по SSH» с входом по паролю и sudo с паролем проходит проверку.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/machines.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-machines}
SSH_PORT=2223; SRV_PORT=17743
PW='Vm-Pass-2026!'

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  for f in sshd server agent secret; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done
  userdel -r hbpw 2>/dev/null || true; userdel -r hbkey 2>/dev/null || true
  rm -f /etc/sudoers.d/hb-machines /usr/local/sbin/systemctl /etc/systemd/system/hopebackup-agent.service
  [ -f "$W/opt.bak" ] || rm -rf /opt/hopebackup
  [ -d /var/lib/hopebackup.hbtest-bak ] && { rm -rf /var/lib/hopebackup; mv /var/lib/hopebackup.hbtest-bak /var/lib/hopebackup; } || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W" /run/sshd

say "Пользователи и sshd"
useradd -m -s /bin/bash hbpw; echo "hbpw:$PW" | chpasswd
useradd -m -s /bin/bash hbkey; usermod -p '*' hbkey
cat > /etc/sudoers.d/hb-machines <<EOF
hbpw ALL=(ALL) ALL
hbkey ALL=(ALL) NOPASSWD: ALL
EOF
chmod 440 /etc/sudoers.d/hb-machines; visudo -cf /etc/sudoers.d/hb-machines >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$W/hbkey" -C hb-test
install -d -m 700 -o hbkey -g hbkey /home/hbkey/.ssh
install -m 600 -o hbkey -g hbkey "$W/hbkey.pub" /home/hbkey/.ssh/authorized_keys
ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey1"; ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey2"
cat > "$W/sshd_config" <<EOF
Port $SSH_PORT
ListenAddress 127.0.0.1
PidFile $W/sshd.pid
UsePAM yes
KbdInteractiveAuthentication yes
PasswordAuthentication yes
PubkeyAuthentication yes
PermitRootLogin no
Match User hbkey
  PasswordAuthentication no
  KbdInteractiveAuthentication no
EOF
start_sshd() { [ -f "$W/sshd.pid" ] && kill "$(cat "$W/sshd.pid")" 2>/dev/null; sleep 0.5; /usr/sbin/sshd -f "$W/sshd_config" -h "$1"; sleep 0.5; }
start_sshd "$W/hostkey1"

say "Сервер управления"
HOPEBACKUP_TESTING=1 HOPEBACKUP_TEST_INIT=systemd HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat $W/srv/server/initial_admin_password.txt)\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() { local id=$1 st; for i in $(seq 1 300); do st=$(api GET /jobs/$id | jq -r .status); [ "$st" = ok ] && return 0; [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail -8; return 1; }; sleep 1; done; fail "задание не завершилось"; }

# Процесс с паролем в командной строке: в описи пароль должен быть скрыт.
sh -c 'sleep 900; true' hb-secret --password=Sekret-4242 & echo $! > "$W/secret.pid"

say "Проверка адресов: имя пользователя и адрес не могут быть опциями ssh"
api POST /machines -d "{\"name\":\"x\",\"host\":\"127.0.0.1\",\"user\":\"-oProxyCommand=touch /tmp/hb-pwn\",\"auth\":\"password\",\"password\":\"x\"}" | jq -r .error | grep -q "недопустимое имя" || fail "пользователь-опция принят (машина)"
api POST /targets -d "{\"name\":\"x\",\"kind\":\"ssh\",\"host\":\"127.0.0.1\",\"user\":\"-oProxyCommand=touch /tmp/hb-pwn\",\"password\":\"x\"}" | jq -r .error | grep -q "недопустимое имя" || fail "пользователь-опция принят (цель)"
api POST /targets -d "{\"name\":\"x\",\"kind\":\"ssh\",\"host\":\"-oProxyCommand=id\",\"user\":\"root\",\"password\":\"x\"}" | jq -r .error | grep -q "недопустимый адрес" || fail "адрес-опция принят (цель)"
[ ! -e /tmp/hb-pwn ] || fail "команда из имени пользователя выполнена"
echo "ok: опции ssh в пользователе и адресе отклоняются"

say "ВМ с входом по паролю (sudo с паролем)"
M1=$(api POST /machines -d "{\"name\":\"vm-пароль\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"auth\":\"password\",\"password\":\"$PW\"}" | jq -r .id)
[ "$M1" != null ] || fail "машина не создана"
api GET /machines | jq -e '.[0].password == "********"' >/dev/null || fail "пароль не замаскирован"
T=$(api POST /machines/$M1/test); echo "$T" | jq -c .
echo "$T" | jq -e '.ok and .sudo == "sudo с паролем" and (.host_key|startswith("SHA256:"))' >/dev/null || fail "проверка входа по паролю"
J=$(api POST /machines/$M1/inventory | jq -r .job_id); wait_job "$J" || fail "опись по паролю"
api GET /jobs/$J | jq -r '.log[].m' | sed 's/^/  /' | head -8
INV=$(api GET /machines/$M1/inventory)
echo "$INV" | jq -c '{method, os: .inventory.system.os.pretty, family: .inventory.system.os.family, kernel: .inventory.system.kernel.release, arch: .inventory.system.kernel.arch, firmware: .inventory.system.boot.firmware, init: .inventory.system.boot.init, cpus: .inventory.system.hardware.cpus, root_fs: .inventory.system.storage.root_fstype, as_root: .inventory.as_root}'
echo "$INV" | jq -e '.method == "ssh" and .inventory.as_root and (.inventory.system.os.pretty|length>0) and (.inventory.system.kernel.release|length>0) and .inventory.system.hardware.cpus > 0' >/dev/null || fail "опись неполная"
echo "$INV" | jq -e '[.analysis.recommendations[].id] | (index("agent") != null and index("host") != null)' >/dev/null || fail "нет рекомендаций agent/host"
echo "$INV" | jq -r '.analysis.summary[] | "  \(.[0]): \(.[1])"'
echo "$INV" | jq -r '.analysis.recommendations[] | "  [\(.priority)] \(.title)"'
echo "$INV" | jq -r '"  оценка первой копии: \(.analysis.estimate.first) байт"'
ls "$W/srv/server/inventory/$M1.json" >/dev/null || fail "опись не сохранена"
echo "$INV" | jq -e '[.inventory.processes[].cmdline] | any(contains("hb-secret --password=********"))' >/dev/null || fail "процесс с паролем не найден или пароль не скрыт"
! grep -q "Sekret-4242" "$W/srv/server/inventory/$M1.json" && ! echo "$INV" | grep -q "Sekret-4242" || fail "пароль из командной строки попал в опись"
echo "ok: пароль из командной строки процесса скрыт"
[ -z "$(ls /var/tmp/hopebackup.* 2>/dev/null)" ] || fail "временный каталог программы описи не удалён"

say "ВМ только с ключом (sudo без пароля); пароль для неё не подходит"
KEY=$(jq -Rs . < "$W/hbkey")
M2=$(api POST /machines -d "{\"name\":\"vm-ключ\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"key\",\"private_key\":$KEY}" | jq -r .id)
api POST /machines/$M2/test | jq -e '.ok and .sudo == "sudo без пароля"' >/dev/null || fail "вход по ключу"
J=$(api POST /machines/$M2/inventory | jq -r .job_id); wait_job "$J" || fail "опись по ключу"
say "Смена пользователя и способа входа: пароль → ключ (сохранённые пароль и пароль sudo не мешают и не переносятся)"
M4=$(api POST /machines -d "{\"name\":\"vm-смена\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"auth\":\"password\",\"password\":\"$PW\",\"sudo_password\":\"$PW\"}" | jq -r .id)
api PUT /machines/$M4 -d "{\"name\":\"vm-смена\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"password\",\"password\":\"********\",\"sudo_password\":\"********\"}" | jq -r .error | grep -q "введите пароль заново" || fail "смена пользователя с сохранённым паролем должна требовать пароль"
R=$(api PUT /machines/$M4 -d "{\"name\":\"vm-смена\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"key\",\"password\":\"********\",\"private_key\":$KEY,\"key_passphrase\":\"\",\"sudo_password\":\"********\"}")
echo "$R" | jq -e '.auth == "key" and .user == "hbkey" and (.password == "") and (.sudo_password == "")' >/dev/null || fail "смена на вход по ключу не сохранена: $R"
api POST /machines/$M4/test | jq -e '.ok and .sudo == "sudo без пароля"' >/dev/null || fail "вход по ключу после смены"
api PUT /machines/$M4 -d "{\"name\":\"vm-смена\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"auth\":\"key\",\"private_key\":\"********\"}" | jq -r .error | grep -q "вставьте закрытый ключ заново" || fail "смена пользователя с сохранённым ключом должна требовать ключ"
api PUT /machines/$M4 -d "{\"name\":\"vm-смена\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"password\",\"password\":\"\"}" | jq -r .error | grep -q "укажите пароль" || fail "вход по паролю без пароля принят"
api DELETE /machines/$M4 >/dev/null
M3=$(api POST /machines -d "{\"name\":\"vm-ключ-паролем\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"password\",\"password\":\"x\"}" | jq -r .id)
api POST /machines/$M3/test | jq -r .error | tee "$W/m3.txt"
grep -q "вход не выполнен" "$W/m3.txt" || fail "пароль для пользователя только с ключом должен быть отклонён"

say "Смена ключа хоста: подключение отклоняется до сброса"
start_sshd "$W/hostkey2"
api POST /machines/$M2/test | jq -r .error | tee "$W/hk.txt"
grep -q "ключ хоста" "$W/hk.txt" || fail "смена ключа хоста не обнаружена"
api POST /machines/$M2/reset-key >/dev/null
api POST /machines/$M2/test | jq -e '.ok' >/dev/null || fail "после сброса ключа вход не выполнен"
echo "ok: ключ хоста закреплён и сбрасывается явно"

say "Экземпляры по рекомендациям (агент привязан)"
SITE_DIR=/mnt/hb-machines-site; rm -rf $SITE_DIR
SITE=$(api POST /sites -d "{\"name\":\"Диск\",\"kind\":\"disk\",\"disk_path\":\"$SITE_DIR\",\"public_url\":\"https://127.0.0.1:$SRV_PORT\",\"password\":\"site-password-1\"}" | jq -r .id)
SRV_FP=$(api GET /status | jq -r .fingerprint)
ET=$(api POST /agents/enroll-token -d '{}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$SRV_FP" --name vm-agent >/dev/null
AGENT=$(api GET /agents | jq -r '.[0].id')
api POST /machines/$M2/apply -d "{\"recommendations\":[\"host\"],\"site_id\":\"$SITE\"}" | jq -r .error | grep -q "установите агента" || fail "без агента экземпляры создаваться не должны"
api PUT /machines/$M2 -d "{\"name\":\"vm-ключ\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"auth\":\"key\",\"private_key\":\"********\",\"agent_id\":\"$AGENT\"}" | jq -e '.agent.id' >/dev/null || fail "агент не привязан"
api GET /machines/$M2/inventory | jq -e '[.analysis.recommendations[].id] | index("agent") == null' >/dev/null || fail "с агентом рекомендация «установить агента» лишняя"
R=$(api POST /machines/$M2/apply -d "{\"recommendations\":[\"host\"],\"site_id\":\"$SITE\"}"); echo "$R" | jq -c .
INST=$(echo "$R" | jq -r '.created[0].id'); [ "$INST" != null ] || fail "экземпляр не создан"
R=$(api POST /machines/$M2/apply -d "{\"recommendations\":[\"host\"],\"site_id\":\"$SITE\"}"); echo "$R" | jq -c .
echo "$R" | jq -e '(.created|length) == 0 and (.skipped|length) == 1' >/dev/null || fail "повторное применение создало дубликат"
api GET /instances/$INST | jq -e '.catalog_id == "host" and .config.skip_db_data == true and (.config.home|length>0)' >/dev/null || fail "параметры экземпляра не из рекомендации"

say "Установка агента по SSH (systemctl подменён: на этой машине нет systemd)"
[ -d /opt/hopebackup ] && touch "$W/opt.bak"
[ -d /var/lib/hopebackup ] && mv /var/lib/hopebackup /var/lib/hopebackup.hbtest-bak
printf '#!/bin/sh\necho "systemctl $*" >> %s/systemctl.log\nexit 0\n' "$W" > /usr/local/sbin/systemctl; chmod 755 /usr/local/sbin/systemctl
api POST /machines/$M1/reset-key >/dev/null
J=$(api POST /machines/$M1/install-agent -d "{\"server_url\":\"https://127.0.0.1:$SRV_PORT\"}" | jq -r .job_id)
wait_job "$J" || fail "установка агента по SSH"
api GET /jobs/$J | jq -r '.log[].m' | sed 's/^/  /' | tail -6
[ -x /opt/hopebackup/hopebackup ] && [ -f /var/lib/hopebackup/agent.json ] || fail "агент не установлен"
grep -q "ExecStart=/opt/hopebackup/hopebackup agent run" /etc/systemd/system/hopebackup-agent.service || fail "нет юнита systemd"
grep -q "enable hopebackup-agent" "$W/systemctl.log" && grep -q "restart hopebackup-agent" "$W/systemctl.log" || fail "служба не включена"
NEWAGENT=$(api GET /machines | jq -r ".[] | select(.id==\"$M1\") | .agent_id")
api GET /agents | jq -e ".[] | select(.id==\"$NEWAGENT\")" >/dev/null || fail "машина не привязана к новому агенту"
# Агент из установленной программы выходит на связь (вместо systemd запускаем вручную).
HOPEBACKUP_DATA=/var/lib/hopebackup /opt/hopebackup/hopebackup agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
for i in $(seq 1 20); do [ "$(api GET /agents | jq -r ".[] | select(.id==\"$NEWAGENT\") | .last_seen")" != null ] && break; sleep 1; done
api GET /agents | jq -e ".[] | select(.id==\"$NEWAGENT\") | .last_seen != null" >/dev/null || fail "установленный агент не вышел на связь"
api GET /machines/$M1/inventory | jq -e '[.analysis.recommendations[].id] | index("agent") == null' >/dev/null || fail "после установки агента рекомендация осталась"
# Повторная установка: агент уже подключён — тот же агент, токен отозван.
J=$(api POST /machines/$M1/install-agent -d "{\"server_url\":\"https://127.0.0.1:$SRV_PORT\"}" | jq -r .job_id)
wait_job "$J" || fail "повторная установка агента"
[ "$(api GET /machines | jq -r ".[] | select(.id==\"$M1\") | .agent_id")" = "$NEWAGENT" ] || fail "после повторной установки агент сменился"
[ "$(api GET /agents | jq length)" = 2 ] || fail "повторная установка создала лишнего агента"
echo "ok: агент установлен по SSH, подключён и привязан к машине (повторная установка — тот же агент)"

say "Цель восстановления «сервер по SSH» с паролем и sudo с паролем"
TG=$(api POST /targets -d "{\"name\":\"по паролю\",\"kind\":\"ssh\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"password\":\"$PW\"}" | jq -r .id)
api GET /targets | jq -e ".[] | select(.id==\"$TG\") | .has_password and (.has_key|not)" >/dev/null || fail "цель с паролем"
api POST /targets/$TG/test | tee "$W/tg.txt" | jq -c .
jq -e '.ok' "$W/tg.txt" >/dev/null || fail "проверка цели по паролю"
# Смена адреса: сохранённый пароль на новый адрес не уходит — его надо ввести заново.
api PUT /targets/$TG -d "{\"name\":\"по паролю\",\"kind\":\"ssh\",\"host\":\"localhost\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"password\":\"********\"}" | jq -r .error | grep -q "введите пароль заново" || fail "пароль ушёл бы на новый адрес"
api PUT /targets/$TG -d "{\"name\":\"по паролю\",\"kind\":\"ssh\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbpw\",\"password\":\"********\"}" | jq -e '.has_password' >/dev/null || fail "без смены адреса пароль должен сохраниться"
# Ключ + пароль у пользователя с sudo NOPASSWD: пароль не должен попадать в stdin команды.
TK=$(api POST /targets -d "{\"name\":\"ключ и пароль\",\"kind\":\"ssh\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"hbkey\",\"private_key\":$KEY,\"password\":\"unused-pw\"}" | jq -r .id)
api POST /targets/$TK/test | jq -e '.ok' >/dev/null || fail "цель с ключом (sudo без пароля)"
echo "ok: цели по паролю и по ключу"

printf '\n\033[1;32mПРОВЕРКА «СЕРВЕРЫ И ВМ» ПРОЙДЕНА\033[0m\n'
