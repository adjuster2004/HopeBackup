#!/usr/bin/env bash
# Проверка плагинов СУБД (нужен root). Настоящие серверы: MariaDB 10.11 (дамп, mariabackup, binlog,
# восстановление на момент времени, одна база под другим именем) и Redis 7 (RDB по протоколу репликации,
# восстановление при включённом AOF, пароль). Имитации утилит: MS SQL Server (sqlcmd), MongoDB
# (mongodump/mongorestore), ClickHouse (clickhouse-client) — проверяют команды и порядок цепочки.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/dbms.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-dbms}
ONLY=${ONLY:-}
cleanup() {
  fake_systemd_cleanup "$W"
  pkill -x mariadbd 2>/dev/null && for _ in $(seq 50); do pgrep -x mariadbd >/dev/null || break; sleep 0.2; done || true
  redis-cli -p 16379 -a hb-redis-pass shutdown nosave >/dev/null 2>&1 || true
  rm -f /etc/systemd/system/mariadb.service /etc/systemd/system/redis-server.service
  rm -rf /var/lib/mysql /var/lib/mysql.before-restore-* /run/mysqld /var/lib/redis-hb /var/lib/redis-hb/*.before-restore-* /etc/redis/hb.conf
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"
fake_systemd "$W"
export HOPEBACKUP_PASSWORD=dbms-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
want() { [ "$ONLY" = "" ] || [[ " $ONLY " == *" $1 "* ]]; }

# ------------------------------------------------------------------ MariaDB
if want mysql; then
say "MariaDB: подготовка сервера"
mkdir -p /run/mysqld && chown mysql:mysql /run/mysqld
mariadb-install-db --user=mysql --datadir=/var/lib/mysql --auth-root-authentication-method=socket >/dev/null 2>&1
unit mariadb "User=mysql
ExecStart=/usr/sbin/mariadbd --datadir=/var/lib/mysql --socket=/run/mysqld/mysqld.sock --skip-networking --log-bin=/var/lib/mysql/mysql-bin --server-id=1 --innodb-buffer-pool-size=64M"
systemctl start mariadb
M() { mariadb --socket=/run/mysqld/mysqld.sock -N -B -e "$1"; }
wait_for 30 M "SELECT 1" || { cat "$W/run/mariadb.log"; fail "MariaDB не запустилась"; }
M "CREATE DATABASE shop; CREATE TABLE shop.orders (id INT PRIMARY KEY, note VARCHAR(100), t TIMESTAMP DEFAULT CURRENT_TIMESTAMP) ENGINE=InnoDB;
   CREATE DATABASE hr; CREATE TABLE hr.people (id INT PRIMARY KEY, name VARCHAR(50)); INSERT INTO hr.people VALUES (1,'Иванов'),(2,'Петрова');"
ins() { M "USE shop; INSERT INTO orders (id, note) SELECT seq, CONCAT('заказ ', seq) FROM seq_$1_to_$2"; }
cnt() { M "SELECT COUNT(*) FROM $1"; }
ins 1 100
CFG='{"socket":"/run/mysqld/mysqld.sock","user":"root","method":"dump"}'

say "MariaDB: дамп, журнал, восстановление на момент"
R backup --plugin mysql --instance my --config "$CFG" | tee "$W/my1.txt"
grep -q "позиция binlog копии: mysql-bin" "$W/my1.txt" || fail "позиция binlog не взята из дампа"
DUMP=$(last my)
ins 101 150
sleep 2; T1=$(date '+%Y-%m-%d %H:%M:%S'); sleep 2
ins 151 200; M "DROP TABLE hr.people"
R backup --plugin mysql --instance my --config "$CFG" --kind incr | tee "$W/my2.txt"
grep -q "архив binlog: 1 файл" "$W/my2.txt" || fail "архив binlog"
INCR=$(last my)
R show "$INCR" | grep -c "binlog/mysql-bin.000001" >/dev/null || fail "файл binlog не в копии"
R restore --plugin mysql --instance my --snapshot "$DUMP" --params "{\"socket\":\"/run/mysqld/mysqld.sock\",\"user\":\"root\",\"to_time\":\"$T1\"}" | tee "$W/my-r1.txt" | tail -4
[ "$(cnt shop.orders)" = 150 ] || fail "на момент $T1 ожидалось 150 заказов, есть $(cnt shop.orders)"
[ "$(cnt hr.people)" = 2 ] || fail "удалённая после момента таблица не восстановлена"
echo "ok: дамп + binlog до $T1 → 150 заказов, таблица hr.people на месте"
R restore --plugin mysql --instance my --snapshot "$INCR" --params '{"socket":"/run/mysqld/mysqld.sock","user":"root"}' > "$W/my-r2.txt"
[ "$(cnt shop.orders)" = 200 ] || fail "до конца копии журнала: ожидалось 200 заказов"
M "SHOW TABLES FROM hr" | grep -q people && fail "до конца журнала: таблица должна быть удалена (как в журнале)"
echo "ok: восстановление по копии журнала (основа — дамп) → 200 заказов, DROP TABLE повторён"
R restore --plugin mysql --instance my --snapshot "$DUMP" --params '{"socket":"/run/mysqld/mysqld.sock","user":"root","database":"shop","rename_to":"shop_copy"}' > /dev/null
[ "$(cnt shop_copy.orders)" = 100 ] || fail "одна база под другим именем"
[ "$(cnt shop.orders)" = 200 ] || fail "рабочая база не должна меняться при восстановлении копии рядом"
echo "ok: одна база из полного дампа под именем shop_copy"

say "MariaDB: mariabackup и binlog"
R backup --plugin mysql --instance myp --config '{"socket":"/run/mysqld/mysqld.sock","user":"root","method":"physical"}' | tail -3
PHYS=$(last myp)
ins 201 230
R backup --plugin mysql --instance myp --config '{"socket":"/run/mysqld/mysqld.sock","user":"root"}' --kind incr | tail -2
PINCR=$(last myp)
ins 231 260
R restore --plugin mysql --instance myp --snapshot "$PHYS" --params "{\"target_dir\":\"$W/xb\"}" > /dev/null
[ -f "$W/xb/ibdata1" ] && [ -d "$W/xb/shop" ] || fail "физическая копия в каталог"
R restore --plugin mysql --instance myp --snapshot "$PINCR" --params '{"socket":"/run/mysqld/mysqld.sock","user":"root"}' | tee "$W/myp-r.txt" | tail -5
systemctl is-active --quiet mariadb || fail "сервер не запущен после физического восстановления"
[ "$(cnt shop.orders)" = 230 ] || fail "физическая + binlog: ожидалось 230, есть $(cnt shop.orders)"
ls -d /var/lib/mysql.before-restore-* >/dev/null || fail "прежний каталог данных не сохранён"
[ "$(stat -c %U /var/lib/mysql/ibdata1)" = mysql ] || fail "владелец каталога данных"
echo "ok: mariabackup → замена каталога данных (прежний рядом) + binlog → 230 заказов"
systemctl stop mariadb
fi

# ------------------------------------------------------------------ Redis
if want redis; then
say "Redis 7: RDB без остановки, восстановление с AOF"
id redis >/dev/null 2>&1 || useradd -r -s /usr/sbin/nologin redis
mkdir -p /var/lib/redis-hb /etc/redis && chown redis:redis /var/lib/redis-hb
cat > /etc/redis/hb.conf <<'EOF'
port 16379
bind 127.0.0.1
dir /var/lib/redis-hb
dbfilename dump.rdb
appendonly yes
requirepass hb-redis-pass
EOF
chown redis /etc/redis/hb.conf
unit redis-server "User=redis
ExecStart=/usr/bin/redis-server /etc/redis/hb.conf"
systemctl start redis-server
RC() { REDISCLI_AUTH=hb-redis-pass redis-cli -p 16379 "$@"; }
wait_for 10 RC ping || fail "Redis не запустился"
for i in $(seq 1 500); do echo "SET key:$i значение-$i"; done | RC > /dev/null
RC HSET user:1 name "Иванов" > /dev/null; RC RPUSH q a b c > /dev/null
RCFG='{"host":"127.0.0.1","port":16379,"password":"hb-redis-pass"}'
R backup --plugin redis --instance rd --config "$RCFG" | tee "$W/rd.txt"
grep -q "ключей 502" "$W/rd.txt" || fail "Redis: число ключей"
RS=$(last rd)
RC FLUSHALL > /dev/null; RC SET only-after 1 > /dev/null
R restore --plugin redis --instance rd --snapshot "$RS" --params "$RCFG" | tee "$W/rd-r.txt" | tail -3
[ "$(RC DBSIZE)" = 502 ] || fail "Redis: после восстановления $(RC DBSIZE) ключей"
[ "$(RC GET key:77)" = "значение-77" ] && [ "$(RC HGET user:1 name)" = "Иванов" ] && [ "$(RC LLEN q)" = 3 ] || fail "Redis: значения"
[ "$(RC GET only-after)" = "" ] || fail "Redis: ключ после копии остался"
ls /var/lib/redis-hb/appendonlydir/ | grep -q base.rdb || fail "Redis: база AOF"
ls -d /var/lib/redis-hb/appendonlydir.before-restore-* >/dev/null || fail "Redis: прежний AOF не сохранён"
RC SET after-restore 1 >/dev/null; systemctl restart redis-server; wait_for 10 RC ping
[ "$(RC DBSIZE)" = 503 ] || fail "Redis: после перезапуска (AOF) ожидалось 503 ключа"
echo "ok: Redis — 502 ключа из копии, AOF продолжает работать после перезапуска"
R restore --plugin redis --instance rd --snapshot "$RS" --params "{\"target_dir\":\"$W/rdb\"}" >/dev/null
[ -s "$W/rdb/dump.rdb" ] && [ -f "$W/rdb/etc/redis/hb.conf" ] || fail "Redis: файл и конфигурация в каталог"
redis-check-rdb "$W/rdb/dump.rdb" | grep -q "RDB looks OK" || fail "Redis: файл RDB повреждён"
echo "ok: Redis — RDB проходит redis-check-rdb (EOF-метка снята)"
systemctl stop redis-server
fi

. "$HERE/dbms_mocks.sh"

printf '\n\033[1;32mПРОВЕРКА СУБД ПРОЙДЕНА\033[0m\n'
