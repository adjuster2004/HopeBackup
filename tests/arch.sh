#!/usr/bin/env bash
# Проверка сборок под разные архитектуры и библиотеки C (нужны root, OpenSSH-сервер, jq, curl, busybox-static;
# для ARM — qemu-user-static, для aarch64 glibc — libc6-arm64-cross):
#   комплект: x86_64/aarch64 × glibc/musl (каталог DIST, как bin/dist);
#   «машины» — окружения chroot без системной glibc, куда sshd пускает по паролю (ChrootDirectory):
#     alpine  — x86_64, musl (только busybox и статическая программа), пользователь без root;
#     arm     — aarch64, musl (uname -m → aarch64, программы ARM выполняет qemu через binfmt), вход под root;
#     armgnu  — aarch64, glibc 2.39 (библиотеки ARM из libc6-arm64-cross);
#   сервер управления выбирает сборку по архитектуре и библиотеке C, снимает опись, на «arm» ставит агента
#   (OpenRC), агент из chroot выходит на связь.
#
#   BIN=…/hopebackup DIST=bin/dist tests/arch.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
DIST=$(readlink -f "${DIST:-./bin/dist}")
W=${W:-/tmp/hb-arch}
R=/srv/hb-arch          # корни chroot (sshd требует, чтобы путь принадлежал root)
SSH_PORT=2224; SRV_PORT=17943
PW='Arch-Pass-2026!'

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  for f in sshd server agent; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done
  for r in alpine arm armgnu; do umount -l "$R/$r/proc" "$R/$r/dev" 2>/dev/null || true; done
  for u in hbalp hbarm hbarmg; do userdel -f "$u" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W" "$R"; mkdir -p "$W" /run/sshd

for t in x86_64-musl aarch64-musl aarch64-gnu; do [ -f "$DIST/hopebackup-$t.gz" ] || fail "нет $DIST/hopebackup-$t.gz"; done
[ -x /bin/busybox ] || fail "нужен busybox-static"
ARM=1
if [ -x /usr/bin/qemu-aarch64-static ]; then
  mountpoint -q /proc/sys/fs/binfmt_misc || mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc
  [ -e /proc/sys/fs/binfmt_misc/hb-aarch64 ] || echo ':hb-aarch64:M::\x7f\x45\x4c\x46\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\xb7\x00:\xff\xff\xff\xff\xff\xff\xff\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/usr/bin/qemu-aarch64-static:F' > /proc/sys/fs/binfmt_misc/register 2>/dev/null || true
else ARM=0; echo "(нет qemu-user-static — ARM пропущен)"; fi

say "Комплект сборок"
for t in x86_64-gnu x86_64-musl aarch64-gnu aarch64-musl; do
  f="$DIST/hopebackup-$t.gz"; [ -f "$f" ] || continue
  gunzip -c "$f" > "$W/hb-$t"; chmod 755 "$W/hb-$t"
  case $t in
    x86_64-*) v=$("$W/hb-$t" --version) ;;
    aarch64-gnu) [ $ARM = 1 ] && v=$(qemu-aarch64-static -L /usr/aarch64-linux-gnu "$W/hb-$t" --version) || v="(без qemu)" ;;
    aarch64-musl) [ $ARM = 1 ] && v=$("$W/hb-$t" --version) || v="(без qemu)" ;;
  esac
  printf '  %-14s %s · %s\n' "$t" "$(file -b "$W/hb-$t" | cut -d, -f1-2,4 | sed 's/ELF 64-bit LSB //')" "$v"
done

# Окружение chroot: busybox, пользователь, признак библиотеки C, /proc и /dev.
mkroot() { # $1 — имя, $2 — пользователь, $3 — uid, $4 — арх (uname -m), $5 — libc (musl|gnu)
  local r="$R/$1"
  mkdir -p "$r"/{bin,sbin,etc/init.d,lib,usr/local/bin,var/tmp,tmp,proc,dev,root,home/$2,run}
  chmod 1777 "$r/tmp" "$r/var/tmp"; chmod 755 "$R" "$r"
  cp /bin/busybox "$r/bin/busybox"
  for a in $(/bin/busybox --list); do [ -e "$r/bin/$a" ] || ln -s busybox "$r/bin/$a"; done
  rm -f "$r/bin/uname"; printf '#!/bin/sh\nif [ "$1" = -m ]; then echo %s; else exec /bin/busybox uname "$@"; fi\n' "$4" > "$r/bin/uname"; chmod 755 "$r/bin/uname"
  if [ "$5" = musl ]; then : > "$r/lib/ld-musl-$4.so.1"; printf 'NAME="Alpine Linux"\nID=alpine\nVERSION_ID=3.20.3\nPRETTY_NAME="Alpine Linux v3.20 (%s)"\n' "$4" > "$r/etc/os-release"; echo 3.20.3 > "$r/etc/alpine-release"
  else printf '#!/bin/sh\n[ "$1" = GNU_LIBC_VERSION ] && echo "glibc 2.39"\n' > "$r/bin/getconf"; chmod 755 "$r/bin/getconf"; printf 'NAME="Debian GNU/Linux"\nID=debian\nVERSION_ID=12\nPRETTY_NAME="Debian GNU/Linux 12 (bookworm, %s)"\n' "$4" > "$r/etc/os-release"; fi
  printf 'root:x:0:0:root:/root:/bin/sh\n%s:x:%s:%s::/home/%s:/bin/sh\n' "$2" "$3" "$3" "$2" > "$r/etc/passwd"
  printf 'root:x:0:\n%s:x:%s:\n' "$2" "$3" > "$r/etc/group"
  chown "$3:$3" "$r/home/$2"
  # OpenRC-заглушки (служба агента ставится через rc-update/rc-service).
  printf '#!/bin/sh\necho "rc-update $*" >> /var/log/rc.log\n' > "$r/sbin/rc-update"; printf '#!/bin/sh\necho "rc-service $*" >> /var/log/rc.log\n' > "$r/sbin/rc-service"
  mkdir -p "$r/var/log"; chmod 755 "$r/sbin/rc-update" "$r/sbin/rc-service"; : > "$r/sbin/openrc-run"; chmod 755 "$r/sbin/openrc-run"
  # Оболочка: в ARM-окружениях — dash (busybox-ash вызывает свои команды напрямую, мимо подменённого uname).
  if [ "${6:-busybox}" = dash ]; then
    mkdir -p "$r/lib/x86_64-linux-gnu" "$r/lib64"
    cp /usr/bin/dash "$r/bin/dash"; rm -f "$r/bin/sh"; ln -s dash "$r/bin/sh"
    for l in $(ldd /usr/bin/dash | grep -o '/[^ ]*'); do case "$l" in */ld-linux-x86-64.so.2) cp -L "$l" "$r/lib64/" ;; *) cp -L "$l" "$r/lib/x86_64-linux-gnu/" ;; esac; done
  fi
  mount --bind /proc "$r/proc"; mount --bind /dev "$r/dev"
  useradd -M -d "/home/$2" -s /bin/sh -u "$3" -o "$2" 2>/dev/null || true; echo "$2:$PW" | chpasswd
}
say "Машины: chroot без системной glibc"
mkroot alpine hbalp 2101 x86_64 musl
[ $ARM = 1 ] && mkroot arm hbarm 0 aarch64 musl dash
if [ $ARM = 1 ]; then
  mkroot armgnu hbarmg 2103 aarch64 gnu dash
  for d in lib lib/aarch64-linux-gnu usr/aarch64-linux-gnu/lib; do mkdir -p "$R/armgnu/$d"; cp -a /usr/aarch64-linux-gnu/lib/. "$R/armgnu/$d/"; done
  cp -L /usr/aarch64-linux-gnu/lib/ld-linux-aarch64.so.1 "$R/armgnu/lib/ld-linux-aarch64.so.1"
fi
ls "$R/alpine/lib" "$R/alpine/usr/lib" 2>/dev/null | grep -v ld-musl | grep -q . && fail "в chroot alpine не должно быть библиотек"
echo "  alpine: $(ls "$R/alpine/bin" | wc -l) команд busybox, библиотек нет"

ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
cat > "$W/sshd_config" <<EOF
Port $SSH_PORT
ListenAddress 127.0.0.1
PidFile $W/sshd.pid
UsePAM yes
KbdInteractiveAuthentication yes
PasswordAuthentication yes
PermitRootLogin no
Match User hbalp
  ChrootDirectory $R/alpine
Match User hbarm
  ChrootDirectory $R/arm
  PermitRootLogin yes
Match User hbarmg
  ChrootDirectory $R/armgnu
EOF
/usr/sbin/sshd -f "$W/sshd_config" -h "$W/hostkey"; sleep 0.5

say "Сервер управления с комплектом сборок"
HOPEBACKUP_TESTING=1 HOPEBACKUP_TEST_INIT=openrc HOPEBACKUP_DIST="$DIST" HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$(cat $W/srv/server/initial_admin_password.txt)\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() { local id=$1 st; for i in $(seq 1 600); do st=$(api GET /jobs/$id | jq -r .status); [ "$st" = ok ] && return 0; [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail -8; return 1; }; sleep 1; done; fail "задание не завершилось"; }

machine() { # $1 — имя, $2 — пользователь; печатает id
  api POST /machines -d "{\"name\":\"$1\",\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"$2\",\"auth\":\"password\",\"password\":\"$PW\"}" | jq -r .id
}
inventory() { # $1 — id машины, $2 — ожидаемая сборка, $3 — ожидаемая архитектура ядра
  local T J INV
  T=$(api POST /machines/$1/test); echo "  проверка: $(echo "$T" | jq -r '"\(.os) · \(.sudo) · программа: \(.program)"')"
  echo "$T" | jq -e --arg p "$2" '.ok and (.program|startswith($p))' >/dev/null || fail "выбрана не та сборка (ждали $2)"
  J=$(api POST /machines/$1/inventory | jq -r .job_id); wait_job "$J" || fail "опись"
  api GET /jobs/$J | jq -r '.log[].m' | grep -E "машина:|опись:" | sed 's/^/    /'
  INV=$(api GET /machines/$1/inventory)
  echo "$INV" | jq -e --arg a "$3" '.method == "ssh" and .inventory.system.kernel.arch == $a and (.inventory.system.os.pretty|length>0)' >/dev/null || fail "опись не программой ($(echo "$INV" | jq -c '{method, arch: .inventory.system.kernel.arch}'))"
  echo "$INV" | jq -r '"    ОС: \(.inventory.system.os.pretty) · ядро \(.inventory.system.kernel.release) · \(.inventory.system.kernel.arch) · рекомендаций \(.analysis.recommendations|length)"'
}

say "Alpine x86_64 (musl, без gcompat): опись статической сборкой"
MA=$(machine alpine hbalp); inventory "$MA" "x86_64 · musl" x86_64
[ -z "$(ls "$R/alpine/var/tmp" "$R/alpine/home/hbalp" 2>/dev/null | grep hopebackup)" ] || fail "временный каталог программы не удалён"

if [ $ARM = 1 ]; then
  say "ARM64 glibc: опись сборкой aarch64-gnu"
  MG=$(machine arm-glibc hbarmg); inventory "$MG" "aarch64 · glibc" aarch64

  say "ARM64 musl (Alpine ARM): опись и установка агента под root"
  MR=$(machine arm-alpine hbarm); inventory "$MR" "aarch64 · musl" aarch64
  J=$(api POST /machines/$MR/install-agent -d "{\"server_url\":\"https://127.0.0.1:$SRV_PORT\"}" | jq -r .job_id)
  wait_job "$J" || fail "установка агента на ARM"
  api GET /jobs/$J | jq -r '.log[].m' | tail -4 | sed 's/^/    /'
  file -b "$R/arm/opt/hopebackup/hopebackup" | grep -q "ARM aarch64" || fail "установлена не ARM-сборка"
  grep -q "rc-update add hopebackup-agent" "$R/arm/var/log/rc.log" || fail "служба OpenRC не добавлена"
  AID=$(api GET /machines | jq -r ".[] | select(.id==\"$MR\") | .agent_id"); [ -n "$AID" ] && [ "$AID" != null ] || fail "агент не привязан к машине"
  chroot "$R/arm" /bin/env HOPEBACKUP_DATA=/var/lib/hopebackup /opt/hopebackup/hopebackup agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
  for i in $(seq 1 40); do [ "$(api GET /agents | jq -r ".[] | select(.id==\"$AID\") | .online")" = true ] && break; sleep 1; done
  api GET /agents | jq -e ".[] | select(.id==\"$AID\") | .online" >/dev/null || { tail -5 "$W/agent.log"; fail "ARM-агент не вышел на связь"; }
  api GET /agents | jq -r ".[] | select(.id==\"$AID\") | \"    агент \(.name): \(.os), \(.version), в сети\""
fi

printf '\n\033[1;32mПРОВЕРКА СБОРОК ARM И ALPINE ПРОЙДЕНА\033[0m\n'
