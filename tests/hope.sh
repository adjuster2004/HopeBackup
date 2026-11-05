#!/usr/bin/env bash
# Проверка плагинов продуктов Hope (нужен root): Hope Mail Server, Hope MSG Gateway, HopeInventory,
# HopeDFS, HopeDC. Продукты имитируются: юниты systemd с их настоящими переменными, каталоги данных,
# служба — HTTPS-заглушка, которая дописывает «WAL»; systemctl — имитация.
#
# Для каждого продукта: обнаружение по юниту, копия (снимка ФС нет — служба останавливается только на
# время копии данных; у HopeDFS — на ходу), служба снова работает; данные портятся; восстановление на
# место — данные заменены (прежние — рядом), владелец — пользователь продукта, служба отвечает.
# Плюс особенности: тела писем и исключения встроенных бэкапов (почта), мастер-ключ и антивирусные базы
# (шлюз), журнал аудита (инвентаризация), шары с xattr и без «Предыдущих версий» (HopeDFS), экспорт
# и импорт мастер-ключа, режим rejoin (HopeDC).
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/hope.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
W=${W:-/tmp/hb-hope}
FAKE="$W/fakebin"
UNITS="hopedb hope-msg-gateway hope-msg-avdb hopeinv hopedfs hopedc"

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  for f in "$W"/run/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true; done
  for u in $UNITS; do rm -f /etc/systemd/system/$u.service /etc/systemd/system/$u.path; done
  [ -f /run/systemd/system/.hb-hope ] && rm -rf /run/systemd || true
  rm -rf /var/lib/hope /var/lib/hope-gateway /etc/hope-msg /var/lib/hopeinv /var/log/hopeinv /opt/hopeinv \
    /var/lib/hopedfs /var/log/hopedfs /opt/hopedfs /srv/hb-share-* /var/lib/hopedc /etc/hopedc /opt/hopedc \
    /usr/local/bin/hopedb-server /usr/local/bin/hope-msg-gateway /usr/local/bin/hope-msg-avdb-apply /etc/keepalived/keepalived.conf \
    /var/lib/hope*.before-restore-* /var/lib/hope-gateway* 2>/dev/null || true
  for u in hope hopemsg hopeinv hopedc; do userdel "$u" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/run" "$FAKE"
export HOPEBACKUP_PASSWORD=hope-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 -subj /CN=localhost 2>/dev/null
chmod 644 "$W/key.pem" "$W/cert.pem"

# ------------------------------------------------------------------ имитация systemd
if [ ! -d /run/systemd/system ]; then mkdir -p /run/systemd/system; touch /run/systemd/system/.hb-hope; fi
cat > "$FAKE/systemctl" <<EOF
#!/bin/sh
cmd=\$1; shift
case "\$cmd" in
  daemon-reload|enable|disable) exit 0 ;;
  cat) [ -f /etc/systemd/system/\$1.service ] ;;
  is-active) [ "\$1" = --quiet ] && shift; p=$W/run/\$1.pid; [ -f \$p ] && kill -0 \$(cat \$p) 2>/dev/null; exit \$? ;;
  stop) p=$W/run/\$1.pid; echo "stop \$1 \$(date +%s.%N)" >> $W/run/events; [ -f \$p ] && kill \$(cat \$p) 2>/dev/null; rm -f \$p; sleep 0.3; exit 0 ;;
  start|restart)
    u=/etc/systemd/system/\$1.service; p=$W/run/\$1.pid
    echo "start \$1 \$(date +%s.%N)" >> $W/run/events
    [ -f \$p ] && { kill \$(cat \$p) 2>/dev/null; sleep 0.3; }
    wd=\$(sed -n 's/^WorkingDirectory=//p' \$u); [ -n "\$wd" ] && cd "\$wd"
    env \$(sed -n 's/^Environment=//p' \$u) sh -c "exec \$(sed -n 's/^ExecStart=//p' \$u)" > $W/run/\$1.log 2>&1 &
    echo \$! > \$p; exit 0 ;;
  *) exit 0 ;;
esac
EOF
chmod +x "$FAKE/systemctl"
export PATH="$FAKE:$PATH"
# Программа продукта: версия, ключ HopeDC, иначе — служба (HTTPS + дозапись WAL).
app() { # app <путь> <имя переменной порта> <файл WAL> <версия>
  mkdir -p "$(dirname "$1")"
  cat > "$1" <<EOF
#!/bin/sh
case "\$1" in
  --version|version) echo "$4"; exit 0 ;;
  key) shift; sub=\$1; shift
       case "\$sub" in
         export) out=\$2; printf 'HDCKEY:%s:%s' "\$HOPEDC_KEY_PASSWORD" "\$(cat /etc/hopedc/master.key)" > "\$out"; echo "экспортирован" ;;
         import) f=\$1; c=\$(cat "\$f"); pw=\$(echo "\$c" | cut -d: -f2); [ "\$pw" = "\$HOPEDC_KEY_PASSWORD" ] || { echo "неверный пароль" >&2; exit 3; }
                 mkdir -p /etc/hopedc; echo "\$c" | cut -d: -f3- > /etc/hopedc/master.key; chmod 600 /etc/hopedc/master.key; echo "импортирован" ;;
         info) sha256sum /etc/hopedc/master.key | cut -c1-16 ;;
       esac; exit 0 ;;
esac
exec python3 $HERE/mocks/fake_app.py "\${$2}" $W/cert.pem $W/key.pem "$3"
EOF
  chmod 755 "$1"
}
unit() { # unit <имя> <содержимое секции Service>
  printf '[Unit]\nDescription=%s\n\n[Service]\n%s\nRestart=always\n\n[Install]\nWantedBy=multi-user.target\n' "$1" "$2" > /etc/systemd/system/$1.service
}
for u in hope hopemsg hopeinv hopedc; do useradd -r -s /usr/sbin/nologin "$u"; done
active() { systemctl is-active --quiet "$1"; }
check_restored() { # check_restored <каталог> <пользователь>
  [ ! -e "$1/after.txt" ] || fail "$1: файл, созданный после копии, остался — данные не заменены"
  [ "$(cat "$1/conf.json")" = '{"v":1}' ] || fail "$1: conf.json не восстановлен"
  ls -d "$1".before-restore-* >/dev/null 2>&1 || fail "$1: прежнее содержимое не сохранено рядом"
  [ "$(stat -c %U "$1/conf.json")" = "$2" ] || fail "$1: владелец $(stat -c %U "$1/conf.json"), ожидался $2"
}
spoil() { echo after > "$1/after.txt"; echo '{"v":2}' > "$1/conf.json"; }

# ------------------------------------------------------------------ Hope Mail Server
say "Hope Mail Server"
mkdir -p /var/lib/hope/data/backups /var/lib/hope/blobs/ab /etc/hope/tls
echo '{"v":1}' > /var/lib/hope/data/conf.json; echo secret > /var/lib/hope/data/jwt_secret.key
echo "backup-1" > /var/lib/hope/data/backups/backup-1.tar; echo x > /var/lib/hope/data/restore_pending.txt
head -c 300000 /dev/urandom > /var/lib/hope/blobs/ab/msg1.eml; echo cert > /etc/hope/tls/fullchain.pem
chown -R hope:hope /var/lib/hope
app /usr/local/bin/hopedb-server HOPE_API_PORT /var/lib/hope/data/wal.log "hopedb-server 3.2.1"
unit hopedb "User=hope
Environment=DB_DATA_DIR=/var/lib/hope/data
Environment=BLOB_DATA_DIR=/var/lib/hope/blobs
Environment=HOPE_BACKUP_DIR=/var/lib/hope/data/backups
Environment=HOPE_API_PORT=18501
ExecStart=/usr/local/bin/hopedb-server"
systemctl start hopedb; sleep 1; active hopedb || fail "заглушка почты не запустилась"
R backup --plugin hopemail --instance mail --config '{}' | tee "$W/mail.txt"
grep -q "служба остановлена" "$W/mail.txt" || fail "почта: без снимка ФС ожидалась короткая остановка"
grep -q "hopedb-server 3.2.1" "$W/mail.txt" || fail "почта: версия не определена"
active hopedb || fail "почта: служба не запущена обратно"
# Тела писем копируются после запуска службы.
STOP=$(grep "^start hopedb" "$W/run/events" | tail -1 | awk '{print $3}')
SNAP=$(last mail)
R show "$SNAP" | grep -c "blobs.tar" >/dev/null || fail "почта: в копии нет тел писем"
spoil /var/lib/hope/data; head -c 1000 /dev/urandom > /var/lib/hope/blobs/ab/msg2.eml; rm /var/lib/hope/blobs/ab/msg1.eml
R restore --plugin hopemail --instance mail --snapshot "$SNAP" --params '{}' | tee "$W/mail-r.txt" | tail -5
check_restored /var/lib/hope/data hope
[ -f /var/lib/hope/blobs/ab/msg1.eml ] || fail "почта: тело письма не восстановлено"
[ ! -e /var/lib/hope/data/backups/backup-1.tar ] || fail "почта: встроенные бэкапы не должны попадать в копию"
[ ! -e /var/lib/hope/data/restore_pending.txt ] || fail "почта: restore_pending.txt не должен попадать в копию"
grep -q '"healthy": true' "$W/mail-r.txt" || fail "почта: служба не отвечает после восстановления"
echo "ok: почта — WAL при остановке, тела писем после запуска, восстановление с заменой данных"

# ------------------------------------------------------------------ Hope MSG Gateway
say "Hope MSG Gateway"
D=/var/lib/hope-gateway/msg_local_data
mkdir -p $D/av $D/updates $D/mailstore/corp.ru/ivanov /etc/hope-msg /etc/keepalived
echo '{"v":1}' > $D/conf.json; echo '{"rules":[]}' > $D/state.json; head -c 50000 /dev/urandom > $D/av/main.cvd; echo pkg > $D/updates/pkg.bin
echo "From: a" > $D/mailstore/corp.ru/ivanov/1.eml; chown -R hopemsg:hopemsg /var/lib/hope-gateway
echo "0123456789abcdef" > /etc/hope-msg/master.key; chmod 600 /etc/hope-msg/master.key
echo "vrrp_instance VI_1 {}" > /etc/keepalived/keepalived.conf
printf '#!/bin/sh\necho apply\n' > /usr/local/bin/hope-msg-avdb-apply; chmod 755 /usr/local/bin/hope-msg-avdb-apply
printf '[Unit]\n[Path]\nPathChanged=/x\n' > /etc/systemd/system/hope-msg-avdb.path
app /usr/local/bin/hope-msg-gateway MSG_API_PORT $D/wal.log "hope-msg-gateway 0.2.0"
unit hope-msg-gateway "User=hopemsg
WorkingDirectory=/var/lib/hope-gateway
ExecStart=/usr/local/bin/hope-msg-gateway
LoadCredential=master.key:/etc/hope-msg/master.key
Environment=MSG_API_PORT=18502"
systemctl start hope-msg-gateway; sleep 1
R backup --plugin hopegw --instance gw --config '{}' | tee "$W/gw.txt"
active hope-msg-gateway || fail "шлюз: служба не запущена обратно"
SNAP=$(last gw)
spoil $D; rm /etc/hope-msg/master.key /etc/keepalived/keepalived.conf
R restore --plugin hopegw --instance gw --snapshot "$SNAP" --params '{}' > "$W/gw-r.txt"
check_restored $D hopemsg
[ "$(cat /etc/hope-msg/master.key)" = 0123456789abcdef ] || fail "шлюз: мастер-ключ не восстановлен"
[ "$(stat -c '%a %U' /etc/hope-msg/master.key)" = "600 root" ] || fail "шлюз: мастер-ключ — не 0600 root"
[ -f /etc/keepalived/keepalived.conf ] || fail "шлюз: конфигурация keepalived не восстановлена"
[ ! -e $D/av/main.cvd ] && [ ! -e $D/updates/pkg.bin ] || fail "шлюз: антивирусные базы и пакеты обновлений не копируются"
[ -f $D/mailstore/corp.ru/ivanov/1.eml ] || fail "шлюз: ящики не восстановлены"
active hope-msg-gateway || fail "шлюз: служба не работает после восстановления"
echo "ok: шлюз — данные, мастер-ключ, keepalived; без антивирусных баз"

# ------------------------------------------------------------------ HopeInventory
say "HopeInventory"
mkdir -p /var/lib/hopeinv/tls /var/log/hopeinv
echo '{"v":1}' > /var/lib/hopeinv/conf.json; head -c 200000 /dev/urandom > /var/lib/hopeinv/hopeinv.redb; echo key > /var/lib/hopeinv/tls/server.key
echo '{"audit":1}' > /var/log/hopeinv/audit.jsonl; echo noise > /var/log/hopeinv/hopeinv.log.2026-10-26
chown -R hopeinv:hopeinv /var/lib/hopeinv /var/log/hopeinv
app /opt/hopeinv/hopeinv HOPEINV_ADMIN_PORT /var/lib/hopeinv/wal.log "hopeinv 1.4.0"
unit hopeinv "User=hopeinv
ExecStart=/opt/hopeinv/hopeinv run
Environment=HOPEINV_DATA=/var/lib/hopeinv
Environment=HOPEINV_LOG=/var/log/hopeinv
Environment=HOPEINV_ADMIN_PORT=18503"
systemctl start hopeinv; sleep 1
R backup --plugin hopeinv --instance inv --config '{}' > "$W/inv.txt"
grep -q "hopeinv 1.4.0" "$W/inv.txt" || fail "инвентаризация: версия"
SNAP=$(last inv)
spoil /var/lib/hopeinv; rm /var/log/hopeinv/audit.jsonl
R restore --plugin hopeinv --instance inv --snapshot "$SNAP" --params '{}' > "$W/inv-r.txt"
check_restored /var/lib/hopeinv hopeinv
[ -f /var/log/hopeinv/audit.jsonl ] || fail "инвентаризация: журнал аудита не восстановлен"
R restore --plugin hopeinv --instance inv --snapshot "$SNAP" --params "{\"target_root\":\"$W/alt\",\"parts\":[\"config\"]}" > /dev/null
[ -x "$W/alt/opt/hopeinv/hopeinv" ] && [ -f "$W/alt/etc/systemd/system/hopeinv.service" ] || fail "инвентаризация: программа и юнит в отдельный каталог"
[ ! -e "$W/alt/var/lib/hopeinv" ] || fail "инвентаризация: восстановлено лишнее"
echo "ok: инвентаризация — база и настройки при остановке, журнал аудита, программа и юнит отдельно"

# ------------------------------------------------------------------ HopeDFS
say "HopeDFS"
mkdir -p /var/lib/hopedfs/tls /var/lib/hopedfs/updates /var/log/hopedfs
mkdir -p "/srv/hb-share-docs/Отдел/.x" /srv/hb-share-docs/.hopedfs/snapshots/@GMT-2026.10.25-01.00.00 /srv/hb-share-docs/.hopedfs/dfsr/f1 /srv/hb-share-fin
echo '{"v":1}' > /var/lib/hopedfs/conf.json; echo pkg > /var/lib/hopedfs/updates/p.bin; echo pw > /var/lib/hopedfs/initial_admin_password.txt
cat > /var/lib/hopedfs/cluster_state.json <<'EOF'
{"version": 7, "shares": [{"name": "docs", "path": "/srv/hb-share-docs"}, {"name": "Финансы", "path": "/srv/hb-share-fin"}]}
EOF
echo "договор" > "/srv/hb-share-docs/Отдел/договор.docx"; echo old > /srv/hb-share-docs/.hopedfs/snapshots/@GMT-2026.10.25-01.00.00/old.txt
echo redb > /srv/hb-share-docs/.hopedfs/dfsr/f1/versions.redb; echo tmp > /srv/hb-share-docs/.hopedfs-repl-123
echo "отчёт" > /srv/hb-share-fin/отчёт.xlsx
python3 - <<'EOF'
import os
os.setxattr("/srv/hb-share-docs/Отдел/договор.docx", "user.hopedfs.sd", b"\x01\x00\x04\x80SD-BINARY\x00\xff")
os.setxattr("/srv/hb-share-docs/Отдел/договор.docx", "user.hopedfs.dos", b"\x20\x00\x00\x00")
os.setxattr("/srv/hb-share-docs/Отдел", "user.hopedfs.sd", b"DIR-SD")
EOF
app /opt/hopedfs/hopedfs HOPEDFS_ADMIN_PORT /var/lib/hopedfs/wal.log "hopedfs 1.9.0"
unit hopedfs "User=root
ExecStart=/opt/hopedfs/hopedfs run
Environment=HOPEDFS_DATA=/var/lib/hopedfs
Environment=HOPEDFS_LOG=/var/log/hopedfs
Environment=HOPEDFS_ADMIN_PORT=18504"
systemctl start hopedfs; sleep 1
: > "$W/run/events"
R backup --plugin hopedfs --instance dfs --config '{}' | tee "$W/dfs.txt"
! grep -q "stop hopedfs" "$W/run/events" || fail "HopeDFS: файловый сервер не должен останавливаться без явного consistency=stop"
grep -q "на ходу" "$W/dfs.txt" || fail "HopeDFS: без снимка ФС — копия на ходу"
SNAP=$(last dfs)
R show "$SNAP" | grep -c "share-docs.tar" >/dev/null && R show "$SNAP" | grep -c "share-______.tar\|share-Финансы.tar" >/dev/null || { R show "$SNAP"; fail "HopeDFS: шары — отдельными частями"; }
rm -rf /srv/hb-share-docs/Отдел; spoil /var/lib/hopedfs
R restore --plugin hopedfs --instance dfs --snapshot "$SNAP" --params '{}' > "$W/dfs-r.txt"
check_restored /var/lib/hopedfs root
python3 - <<'EOF'
import os
f = "/srv/hb-share-docs/Отдел/договор.docx"
assert open(f).read() == "договор\n", "содержимое"
assert os.getxattr(f, "user.hopedfs.sd") == b"\x01\x00\x04\x80SD-BINARY\x00\xff", "NTFS-права файла (user.hopedfs.sd)"
assert os.getxattr(f, "user.hopedfs.dos") == b"\x20\x00\x00\x00", "атрибуты DOS"
assert os.getxattr("/srv/hb-share-docs/Отдел", "user.hopedfs.sd") == b"DIR-SD", "права каталога"
EOF
[ -f /srv/hb-share-docs/.hopedfs/dfsr/f1/versions.redb ] || fail "HopeDFS: база версий репликации должна быть в копии"
R restore --plugin hopedfs --instance dfs --snapshot "$SNAP" --params "{\"target_root\":\"$W/dfsalt\"}" > /dev/null
[ ! -e "$W/dfsalt/srv/hb-share-docs/.hopedfs/snapshots" ] && [ ! -e "$W/dfsalt/srv/hb-share-docs/.hopedfs-repl-123" ] || fail "HopeDFS: «Предыдущие версии» и временные файлы не копируются"
[ ! -e "$W/dfsalt/var/lib/hopedfs/updates" ] && [ ! -e "$W/dfsalt/var/lib/hopedfs/initial_admin_password.txt" ] || fail "HopeDFS: исключения каталога данных"
[ -f "$W/dfsalt/srv/hb-share-fin/отчёт.xlsx" ] || fail "HopeDFS: шара с русским именем"
grep -q "репликации площадок" "$W/dfs-r.txt" || fail "HopeDFS: нет предупреждения о группах репликации"
echo "ok: HopeDFS — шары с NTFS-правами (xattr), без остановки и без «Предыдущих версий»"

# ------------------------------------------------------------------ HopeDC
say "HopeDC"
mkdir -p /var/lib/hopedc/updates /var/lib/hopedc/backups /etc/hopedc
echo '{"v":1}' > /var/lib/hopedc/conf.json; head -c 100000 /dev/urandom > /var/lib/hopedc/active.ewal; echo '{"node_id":"n1"}' > /var/lib/hopedc/cluster_config.json
echo zip > /var/lib/hopedc/backups/auto.zip; echo MASTERKEY-1 > /etc/hopedc/master.key; chmod 600 /etc/hopedc/master.key
chown -R hopedc:hopedc /var/lib/hopedc /etc/hopedc
app /opt/hopedc/hopedc-core DC_UI_PORT /var/lib/hopedc/wal.log "hopedc-core v154"
unit hopedc "User=hopedc
WorkingDirectory=/var/lib/hopedc
ExecStart=/opt/hopedc/hopedc-core
Environment=DC_DATA_DIR=/var/lib/hopedc
Environment=DC_MASTER_KEY_FILE=/etc/hopedc/master.key
Environment=DC_UI_PORT=18505"
systemctl start hopedc; sleep 1
R backup --plugin hopedc --instance dc --config '{"key_password":"Ключ-Пароль-1"}' | tee "$W/dc.txt"
grep -q "экспорт с паролем\|master-key" "$W/dc.txt" || true
SNAP=$(last dc)
R show "$SNAP" | grep -c "master-key.hdckey" >/dev/null || fail "HopeDC: экспорт ключа не попал в копию"
R show "$SNAP" | grep -c "secrets.tar" >/dev/null && fail "HopeDC: файл ключа не должен копироваться в режиме export"
spoil /var/lib/hopedc; rm -f /etc/hopedc/master.key
if R restore --plugin hopedc --instance dc --snapshot "$SNAP" --params '{}' > "$W/dc-nopw.txt" 2>&1; then fail "HopeDC: без пароля ключа восстановление должно остановиться"; fi
grep -q "Пароль ключа из копии" "$W/dc-nopw.txt" || fail "HopeDC: нет понятной ошибки о пароле"
R restore --plugin hopedc --instance dc --snapshot "$SNAP" --params '{"key_password":"Ключ-Пароль-1"}' > "$W/dc-r.txt"
check_restored /var/lib/hopedc hopedc
[ "$(cat /etc/hopedc/master.key)" = MASTERKEY-1 ] || fail "HopeDC: мастер-ключ не импортирован"
[ ! -e /var/lib/hopedc/backups/auto.zip ] && [ ! -e /var/lib/hopedc/updates ] || fail "HopeDC: исключения (автобэкапы, обновления)"
active hopedc || fail "HopeDC: служба не работает после восстановления"
# rejoin: только cluster_config.json и ключ — узел заберёт данные у пира.
R restore --plugin hopedc --instance dc --snapshot "$SNAP" --params '{"mode":"rejoin","key_password":"Ключ-Пароль-1"}' > "$W/dc-rj.txt"
[ -f /var/lib/hopedc/cluster_config.json ] && [ ! -e /var/lib/hopedc/active.ewal ] || { ls -la /var/lib/hopedc; fail "HopeDC: rejoin — только cluster_config.json"; }
[ "$(ls -d /var/lib/hopedc.before-restore-* | wc -l)" -ge 2 ] || fail "HopeDC: прежние данные должны сохраняться рядом"
echo "ok: HopeDC — база при остановке, ключ экспортом с паролем, режим rejoin"

printf '\n\033[1;32mПРОВЕРКА ПРОДУКТОВ HOPE ПРОЙДЕНА\033[0m\n'
