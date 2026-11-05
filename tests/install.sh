#!/usr/bin/env bash
# Проверка установочного скрипта linux/hopebackup-install.sh (нужны root, busybox-static):
#   1) systemd (имитация systemctl): узел, сервер, агент с подключением к серверу, статус, обновление
#      с сохранением токенов и пароля, удаление с данными;
#   2) Alpine без glibc (chroot только с busybox, имитация OpenRC): выбор статической сборки musl, узел,
#      служба OpenRC, удаление; скрипт выполняется в busybox sh.
#
#   tests/install.sh            (архив — bin/release/hopebackup-*-linux.tar.gz)
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd); ROOT=$(dirname "$HERE")
ARCHIVE=${ARCHIVE:-$(ls -t "$ROOT"/bin/release/hopebackup-*-linux.tar.gz | head -1)}
SCRIPT="$ROOT/linux/hopebackup-install.sh"
W=${W:-/tmp/hb-install}
FAKE="$W/fakebin"
CH="$W/alpine"

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  for f in "$W"/run/*.pid "$CH"/run/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true; done
  umount -l "$CH/proc" "$CH/dev" 2>/dev/null || true
  rm -rf /run/systemd/system/.hb-test /etc/systemd/system/hopebackup-*.service
  [ -f /run/systemd/system/.hb-created ] && rm -rf /run/systemd || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
[ -f "$ARCHIVE" ] || fail "нет архива: $ARCHIVE"
[ -d /opt/hopebackup ] && fail "на этой машине уже есть /opt/hopebackup — проверка установки его бы заменила"
cleanup; rm -rf "$W"; mkdir -p "$W/run" "$FAKE" "$W/tmp"
cp "$ARCHIVE" "$SCRIPT" "$W/tmp/"
S="$W/tmp/$(basename "$SCRIPT")"

# ------------------------------------------------------------------ имитация systemd
if [ ! -d /run/systemd/system ]; then mkdir -p /run/systemd/system; touch /run/systemd/system/.hb-created; fi
cat > "$FAKE/systemctl" <<EOF
#!/bin/sh
# Имитация systemctl: службы — фоновые процессы по ExecStart и Environment из юнита.
cmd=\$1; shift
[ "\$cmd" = "--quiet" ] && { cmd=\$1; shift; }
case "\$cmd" in
  daemon-reload|enable|disable) exit 0 ;;
  is-active) [ "\$1" = --quiet ] && shift; p=$W/run/\$1.pid; [ -f \$p ] && kill -0 \$(cat \$p) 2>/dev/null; exit \$? ;;
  stop) p=$W/run/\$1.pid; [ -f \$p ] && kill \$(cat \$p) 2>/dev/null; rm -f \$p; sleep 1; exit 0 ;;
  restart|start)
    u=/etc/systemd/system/\$1.service; p=$W/run/\$1.pid
    [ -f \$p ] && { kill \$(cat \$p) 2>/dev/null; sleep 1; }
    env \$(sed -n 's/^Environment=//p' \$u) sh -c "exec \$(sed -n 's/^ExecStart=//p' \$u)" > $W/run/\$1.log 2>&1 &
    echo \$! > \$p; exit 0 ;;
  *) exit 0 ;;
esac
EOF
printf '#!/bin/sh\ntail -n 40 %s/run/*.log\n' "$W" > "$FAKE/journalctl"
chmod +x "$FAKE"/*
export PATH="$FAKE:$PATH"

say "systemd: узел площадки"
sh "$S" node --listen 0.0.0.0:18943 -y | tee "$W/node.txt"
grep -q "служба hopebackup-node работает" "$W/node.txt" || fail "узел не запустился"
[ "$(stat -c %a /var/lib/hopebackup-node/initial_tokens.txt)" = 600 ] || fail "токены узла — не 0600"
curl -sk https://127.0.0.1:18943/v1/health | grep -q '"role":"node"' || fail "узел не отвечает"
grep -q "ExecStart=/opt/hopebackup/hopebackup node run --data /var/lib/hopebackup-node" /etc/systemd/system/hopebackup-node.service || fail "юнит узла"
grep -q "сборка x86_64-gnu\|сборка aarch64-gnu" "$W/node.txt" || fail "на glibc ожидалась сборка gnu"
echo "ok: узел установлен и отвечает"

say "systemd: сервер управления"
sh "$S" server --listen 0.0.0.0:18944 -y | tee "$W/server.txt"
grep -q "Первый вход:   Administrator / " "$W/server.txt" || fail "нет пароля администратора"
PW=$(cat /var/lib/hopebackup/server/initial_admin_password.txt)
curl -sk https://127.0.0.1:18944/api/health | grep -q ok || fail "сервер не отвечает"
[ "$(ls /opt/hopebackup/dist/*.gz | wc -l)" = 4 ] || fail "серверу нужен комплект из 4 сборок"
[ "$(stat -c %a /var/lib/hopebackup)" = 711 ] || fail "каталог данных сервера и агента — 0711"
echo "ok: сервер установлен, комплект сборок на месте"

say "systemd: агент с подключением к серверу"
API=https://127.0.0.1:18944/api
TOK=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$PW\"}" | jq -r .token)
ENR=$(curl -sk -X POST $API/agents/enroll-token -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{}')
sh "$S" agent --server https://127.0.0.1:18944 --token "$(echo "$ENR" | jq -r .token)" --fingerprint "$(echo "$ENR" | jq -r .server_fingerprint)" --name inst-agent -y | tee "$W/agent.txt"
grep -q "Агент подключён" "$W/agent.txt" || fail "агент не подключился"
for i in $(seq 1 30); do curl -sk $API/agents -H "authorization: Bearer $TOK" | jq -e '.[] | select(.name=="inst-agent" and .online)' >/dev/null && break; sleep 1; done
curl -sk $API/agents -H "authorization: Bearer $TOK" | jq -e '.[] | select(.name=="inst-agent" and .online)' >/dev/null || fail "агент не на связи"
# Повторная команда подключения с новым токеном переподключает агента (а не оставляет прежнюю настройку молча).
ENR=$(curl -sk -X POST $API/agents/enroll-token -H "authorization: Bearer $TOK" -H 'content-type: application/json' -d '{}')
sh "$S" agent --server https://127.0.0.1:18944 --token "$(echo "$ENR" | jq -r .token)" --fingerprint "$(echo "$ENR" | jq -r .server_fingerprint)" --name inst-agent-2 -y > "$W/agent2.txt" 2>&1
grep -q "подключаю к https://127.0.0.1:18944" "$W/agent2.txt" || { cat "$W/agent2.txt"; fail "повторное подключение агента"; }
ls /var/lib/hopebackup/agent.json.before-* >/dev/null 2>&1 || fail "прежняя настройка агента не сохранена"
grep -q '"name": "inst-agent-2"' /var/lib/hopebackup/agent.json || fail "агент не переподключён"
for i in $(seq 1 30); do curl -sk $API/agents -H "authorization: Bearer $TOK" | jq -e '.[] | select(.name=="inst-agent-2" and .online)' >/dev/null && break; sleep 1; done
curl -sk $API/agents -H "authorization: Bearer $TOK" | jq -e '.[] | select(.name=="inst-agent-2" and .online)' >/dev/null || fail "переподключённый агент не на связи"
rm -f /var/lib/hopebackup/agent.json.before-*
echo "ok: агент переподключается повторной командой, прежняя настройка сохраняется"
! ps -ef | grep -v grep | grep -q "$(echo "$ENR" | jq -r .token)" || fail "токен виден в списке процессов"
echo "ok: агент подключён и на связи"

say "Статус и обновление"
sh "$S" status | tee "$W/status.txt"
[ "$(grep -c 'работает' "$W/status.txt")" = 3 ] || fail "статус: ожидались три работающие службы"
T1=$(sha256sum /var/lib/hopebackup-node/node.json)
sh "$S" node -y > "$W/node2.txt"
grep -q "обновление" "$W/node2.txt" && grep -q "токены не меняются" "$W/node2.txt" || fail "обновление узла"
[ "$T1" = "$(sha256sum /var/lib/hopebackup-node/node.json)" ] || fail "обновление изменило конфигурацию узла"
sh "$S" server -y > "$W/server2.txt"
grep -q "https://.*:18944/" "$W/server2.txt" || fail "при обновлении сервер должен остаться на своём порту"
[ "$(ls /opt/hopebackup/hopebackup.bak.* | wc -l)" -ge 1 ] || fail "нет копии прежней версии"
curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$PW\"}" | jq -e .token >/dev/null || fail "после обновления пароль не подходит"
echo "ok: обновление сохраняет данные, порт и пароль"

say "Удаление"
sh "$S" uninstall -y > "$W/un1.txt"
[ ! -e /opt/hopebackup ] && [ ! -e /etc/systemd/system/hopebackup-node.service ] || fail "программа или службы не удалены"
[ -d /var/lib/hopebackup-node ] || fail "без --purge данные должны остаться"
sh "$S" uninstall --purge -y > "$W/un2.txt"
[ ! -e /var/lib/hopebackup-node ] && [ ! -e /var/lib/hopebackup ] || fail "--purge не удалил данные"
for f in "$W"/run/*.pid; do [ -f "$f" ] && kill -0 "$(cat "$f")" 2>/dev/null && fail "процесс остался: $f"; done
echo "ok: удаление (данные — только с --purge)"

# ------------------------------------------------------------------ Alpine без glibc: busybox и OpenRC
say "Alpine (busybox, musl, OpenRC): узел"
[ -x /bin/busybox ] || fail "нужен busybox-static"
mkdir -p "$CH"/{bin,sbin,etc/init.d,proc,dev,tmp,run,var/log,var/lib,usr/local/bin,opt}
cp /bin/busybox "$CH/bin/busybox"
for a in $(/bin/busybox --list); do [ -e "$CH/bin/$a" ] || ln -s busybox "$CH/bin/$a"; done
cp "$ARCHIVE" "$SCRIPT" "$CH/tmp/"
mount -t proc proc "$CH/proc"; mount --bind /dev "$CH/dev"
cat > "$CH/sbin/openrc-run" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$CH/bin/rc-update" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$CH/bin/rc-service" <<'EOF'
#!/bin/sh
# Имитация OpenRC: переменные службы из /etc/init.d/<служба>, процесс — в фоне.
s=$1; a=$2; p=/run/$s.pid
case "$a" in
  status) [ -f $p ] && kill -0 $(cat $p) 2>/dev/null ;;
  stop) [ -f $p ] && kill $(cat $p) 2>/dev/null; rm -f $p; sleep 1 ;;
  start|restart)
    [ -f $p ] && { kill $(cat $p) 2>/dev/null; sleep 1; }
    ( . /etc/init.d/$s; exec $command $command_args ) > /var/log/$s.log 2>&1 &
    echo $! > $p ;;
esac
EOF
chmod +x "$CH/sbin/openrc-run" "$CH/bin/rc-update" "$CH/bin/rc-service"
chroot "$CH" /bin/sh /tmp/hopebackup-install.sh node --listen 0.0.0.0:18945 -y | tee "$W/alpine.txt"
grep -q "сборка .*-musl" "$W/alpine.txt" || fail "в Alpine без glibc ожидалась статическая сборка musl"
grep -q "служба hopebackup-node работает" "$W/alpine.txt" || fail "служба OpenRC не запустилась"
grep -q "^command_args=\"node run --data /var/lib/hopebackup-node\"" "$CH/etc/init.d/hopebackup-node" || fail "скрипт OpenRC"
curl -sk https://127.0.0.1:18945/v1/health | grep -q '"role":"node"' || fail "узел в Alpine не отвечает"
chroot "$CH" /bin/sh /tmp/hopebackup-install.sh uninstall --purge -y > "$W/alpine-un.txt"
[ ! -e "$CH/opt/hopebackup" ] && [ ! -e "$CH/etc/init.d/hopebackup-node" ] && [ ! -e "$CH/var/lib/hopebackup-node" ] || fail "удаление в Alpine"
echo "ok: Alpine — сборка musl, служба OpenRC, удаление"

printf '\n\033[1;32mПРОВЕРКА УСТАНОВКИ ПРОЙДЕНА\033[0m\n'
