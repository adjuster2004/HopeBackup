#!/usr/bin/env bash
# Проверка плагинов инфраструктуры и платформ (нужен root).
# Настоящие: OpenLDAP (slapd, slapcat/slapadd), Zabbix на MariaDB, Grafana на SQLite, снимок etcd.
# Файловые продукты с имитацией службы: Jenkins, Nexus. Имитации CLI/API: Keycloak (kc.sh), Docker,
# Harbor (docker compose, harbor-db), Kubernetes (kubectl), OpenSearch (Snapshot API), FreeIPA.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/apps.sh      ONLY="ldap docker" — выборочно
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-apps}
ONLY=${ONLY:-}
want() { [ "$ONLY" = "" ] || [[ " $ONLY " == *" $1 "* ]]; }
cleanup() {
  fake_systemd_cleanup "$W"
  for p in "$W"/*.pid; do [ -f "$p" ] && kill "$(cat "$p")" 2>/dev/null || true; done
  pkill -x mariadbd 2>/dev/null && for _ in $(seq 50); do pgrep -x mariadbd >/dev/null || break; sleep 0.2; done || true
  if [ -d "$W/orig-ldap" ]; then rm -rf /var/lib/ldap /var/lib/ldap.before-restore-*; cp -a "$W/orig-ldap" /var/lib/ldap; fi
  for u in slapd mariadb zabbix-server grafana-server jenkins nexus keycloak; do rm -f /etc/systemd/system/$u.service; done
  rm -rf /var/lib/mysql /var/lib/mysql.before-restore-* /run/mysqld /etc/zabbix /etc/grafana /var/lib/grafana-hb /var/lib/jenkins-hb* /opt/nexus /opt/sonatype-work /opt/keycloak-hb /opt/harbor-hb /var/lib/ipa/backup/ipa-* 2>/dev/null || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"
fake_systemd "$W"
export HOPEBACKUP_PASSWORD=apps-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
MOCK="$W/mockbin"; mkdir -p "$MOCK"; chmod +x "$HERE"/mocks/*.py
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 -subj /CN=localhost 2>/dev/null
httpd() { echo "python3 -m http.server $1 --bind 127.0.0.1 --directory /tmp"; }
for u in jenkins nexus keycloak grafana zabbix; do id "$u" >/dev/null 2>&1 || useradd -r -s /usr/sbin/nologin "$u"; done

# ------------------------------------------------------------------ OpenLDAP
if want ldap; then
say "OpenLDAP: slapcat → slapadd"
pkill -x slapd 2>/dev/null || true; sleep 0.5
cp -a /var/lib/ldap "$W/orig-ldap"
cat > "$W/people.ldif" <<'EOF'
dn: ou=people,dc=nodomain
objectClass: organizationalUnit
ou: people

dn: uid=ivanov,ou=people,dc=nodomain
objectClass: inetOrgPerson
uid: ivanov
cn: Иван Иванов
sn: Иванов
mail: ivanov@corp.ru
EOF
slapadd -q -n 1 -l "$W/people.ldif" 2>/dev/null; chown -R openldap:openldap /var/lib/ldap
unit slapd 'ExecStart=/usr/sbin/slapd -d 0 -h ldap://127.0.0.1:3890/ -F /etc/ldap/slapd.d -u openldap -g openldap'
systemctl start slapd
LS() { ldapsearch -x -LLL -H ldap://127.0.0.1:3890 -b dc=nodomain "$@"; }
wait_for 10 LS uid=ivanov || { cat "$W/run/slapd.log"; fail "slapd не запустился"; }
R backup --plugin freeipa --instance ldap --config '{"mode":"openldap"}' | tee "$W/ldap.txt" | tail -4
grep -q "dc=nodomain (mdb)" "$W/ldap.txt" || fail "ldap: базы из cn=config"
L1=$(last ldap)
systemctl stop slapd
printf 'dn: uid=after,ou=people,dc=nodomain\nobjectClass: inetOrgPerson\nuid: after\ncn: x\nsn: x\n' > "$W/after.ldif"
slapadd -q -n 1 -l "$W/after.ldif" 2>/dev/null; chown -R openldap:openldap /var/lib/ldap
systemctl start slapd; wait_for 10 LS uid=after
R restore --plugin freeipa --instance ldap --snapshot "$L1" --params '{}' > "$W/ldap-r.txt"
wait_for 10 LS uid=ivanov || fail "ldap: slapd не поднялся после восстановления"
[ -z "$(LS uid=after)" ] || fail "ldap: запись, добавленная после копии, осталась"
LS uid=ivanov | grep -q "mail: ivanov@corp.ru" || fail "ldap: запись не восстановлена"
ls -d /var/lib/ldap.before-restore-* >/dev/null || fail "ldap: прежняя база не сохранена"
[ "$(stat -c %U /var/lib/ldap/data.mdb)" = openldap ] || fail "ldap: владелец базы"
R restore --plugin freeipa --instance ldap --snapshot "$L1" --params "{\"target_dir\":\"$W/ldif\"}" > /dev/null
grep -q "olcDatabase={1}mdb" "$W/ldif/config.ldif" && grep -q "uid=ivanov" "$W/ldif/db-1.ldif" || fail "ldap: LDIF в каталог"
echo "ok: OpenLDAP — slapcat cn=config и базы, slapadd в чистый каталог, прежний рядом"
systemctl stop slapd
fi

# ------------------------------------------------------------------ Zabbix (MariaDB) и Grafana (SQLite)
if want zabbix; then
say "Zabbix на MariaDB"
mkdir -p /run/mysqld && chown mysql:mysql /run/mysqld
mariadb-install-db --user=mysql --datadir=/var/lib/mysql --auth-root-authentication-method=socket >/dev/null 2>&1
unit mariadb "User=mysql
ExecStart=/usr/sbin/mariadbd --datadir=/var/lib/mysql --socket=/run/mysqld/mysqld.sock --skip-networking"
systemctl start mariadb
M() { mariadb --socket=/run/mysqld/mysqld.sock -N -B -e "$1"; }
wait_for 30 M "SELECT 1" || fail "MariaDB не запустилась"
M "CREATE DATABASE zabbix CHARACTER SET utf8mb4 COLLATE utf8mb4_bin; CREATE USER zabbix@localhost IDENTIFIED BY 'Zbx-Pass-1'; GRANT ALL PRIVILEGES ON *.* TO zabbix@localhost;
   USE zabbix; CREATE TABLE hosts (hostid INT PRIMARY KEY, host VARCHAR(64)); CREATE TABLE items (itemid INT PRIMARY KEY, hostid INT, key_ VARCHAR(64));
   CREATE TABLE history (itemid INT, clock INT, value DOUBLE); CREATE TABLE history_uint LIKE history; CREATE TABLE trends (itemid INT, clock INT, value_avg DOUBLE);
   INSERT INTO hosts VALUES (1,'web01'),(2,'db01'); INSERT INTO items VALUES (10,1,'cpu'),(11,2,'disk');
   INSERT INTO history SELECT 10, seq, seq/10 FROM seq_1_to_1000; INSERT INTO trends VALUES (10, 1, 5.5);"
mkdir -p /etc/zabbix/alertscripts
cat > /etc/zabbix/zabbix_server.conf <<'EOF'
LogFile=/var/log/zabbix/zabbix_server.log
DBHost=localhost
DBName=zabbix
DBUser=zabbix
DBPassword=Zbx-Pass-1
DBSocket=/run/mysqld/mysqld.sock
AlertScriptsPath=/etc/zabbix/alertscripts
EOF
printf '#!/bin/sh\necho notify\n' > /etc/zabbix/alertscripts/telegram.sh; chmod 755 /etc/zabbix/alertscripts/telegram.sh
unit zabbix-server "ExecStart=sleep 100000"
systemctl start zabbix-server
R backup --plugin zabbix --instance zbx --config '{"product":"zabbix","db_type":"mysql","history":false}' | tee "$W/zbx.txt" | tail -4
grep -q "без данных: history, history_uint, trends" "$W/zbx.txt" || fail "zabbix: исключение истории"
Z1=$(last zbx)
R show "$Z1" | grep -c "schema.sql" >/dev/null && R show "$Z1" | grep -c "data.sql" >/dev/null || fail "zabbix: структура и данные отдельно"
R backup --plugin zabbix --instance zbxf --config '{"product":"zabbix","db_type":"mysql"}' > /dev/null; Z2=$(last zbxf)
M "DELETE FROM zabbix.hosts; DROP TABLE zabbix.items"; rm /etc/zabbix/alertscripts/telegram.sh
R restore --plugin zabbix --instance zbx --snapshot "$Z1" --params '{"product":"zabbix","db_type":"mysql"}' | tee "$W/zbx-r.txt" | tail -3
[ "$(M 'SELECT COUNT(*) FROM zabbix.hosts')" = 2 ] && [ "$(M 'SELECT COUNT(*) FROM zabbix.items')" = 2 ] || fail "zabbix: таблицы не восстановлены"
[ "$(M 'SELECT COUNT(*) FROM zabbix.history')" = 0 ] || fail "zabbix: история не должна восстанавливаться из копии без истории"
[ -x /etc/zabbix/alertscripts/telegram.sh ] || fail "zabbix: скрипты оповещений"
systemctl is-active --quiet zabbix-server || fail "zabbix: служба не запущена"
R restore --plugin zabbix --instance zbxf --snapshot "$Z2" --params '{"product":"zabbix","db_type":"mysql","database":"zabbix_copy","restore_config":false}' > /dev/null
[ "$(M 'SELECT COUNT(*) FROM zabbix_copy.history')" = 1000 ] || fail "zabbix: полная копия в другую базу"
echo "ok: Zabbix — без истории (структура есть, данных нет), полная — в zabbix_copy, конфигурация и скрипты"
systemctl stop zabbix-server; systemctl stop mariadb

say "Grafana на SQLite"
mkdir -p /etc/grafana/provisioning/datasources /var/lib/grafana-hb/plugins/clock-panel
cat > /etc/grafana/grafana.ini <<'EOF'
[paths]
data = /var/lib/grafana-hb
[server]
http_port = 13000
[database]
type = sqlite3
EOF
echo "apiVersion: 1" > /etc/grafana/provisioning/datasources/zbx.yaml
echo plugin > /var/lib/grafana-hb/plugins/clock-panel/module.js
sqlite3 /var/lib/grafana-hb/grafana.db "CREATE TABLE dashboard (id INTEGER PRIMARY KEY, title TEXT); INSERT INTO dashboard (title) VALUES ('Серверы'), ('Сеть');"
chown -R grafana:grafana /var/lib/grafana-hb
unit grafana-server "User=grafana
ExecStart=$(httpd 13000)"
systemctl start grafana-server
R backup --plugin zabbix --instance gf --config '{"product":"grafana"}' | tail -3; G1=$(last gf)
sqlite3 /var/lib/grafana-hb/grafana.db "DELETE FROM dashboard"; rm -rf /var/lib/grafana-hb/plugins/clock-panel
R restore --plugin zabbix --instance gf --snapshot "$G1" --params '{"product":"grafana"}' | tee "$W/gf-r.txt" | tail -3
[ "$(sqlite3 /var/lib/grafana-hb/grafana.db 'SELECT COUNT(*) FROM dashboard')" = 2 ] || fail "grafana: дашборды"
[ -f /var/lib/grafana-hb/plugins/clock-panel/module.js ] || fail "grafana: плагины"
ls /var/lib/grafana-hb/grafana.db.before-restore-* >/dev/null || fail "grafana: прежняя база рядом"
[ "$(stat -c %U /var/lib/grafana-hb/grafana.db)" = grafana ] || fail "grafana: владелец"
grep -q '"healthy": true' "$W/gf-r.txt" || fail "grafana: служба не отвечает"
echo "ok: Grafana — grafana.db через sqlite3 .backup, плагины и provisioning"
systemctl stop grafana-server
fi

# ------------------------------------------------------------------ Jenkins и Nexus
if want jenkins; then
say "Jenkins"
J=/var/lib/jenkins-hb
mkdir -p $J/jobs/app/builds/1/archive $J/workspace/app $J/secrets $J/plugins $J/caches/git
echo '<hudson><version>2.479.1</version></hudson>' > $J/config.xml
echo '<project/>' > $J/jobs/app/config.xml; echo "BUILD OK" > $J/jobs/app/builds/1/log
head -c 200000 /dev/urandom > $J/jobs/app/builds/1/archive/app.jar; echo src > $J/workspace/app/main.c
echo KEY > $J/secrets/master.key; echo jpi > $J/plugins/git.jpi; echo c > $J/caches/git/x
chown -R jenkins:jenkins $J
unit jenkins "User=jenkins
Environment=JENKINS_HOME=$J
Environment=JENKINS_PORT=18080
ExecStart=$(httpd 18080)"
systemctl start jenkins
: > "$W/run/events"
R backup --plugin jenkins --instance jk --config '{}' | tail -3; JK=$(last jk)
! grep -q "stop jenkins" "$W/run/events" || fail "jenkins: по умолчанию — без остановки"
echo '<project>changed</project>' > $J/jobs/app/config.xml; rm -rf $J/secrets
R restore --plugin jenkins --instance jk --snapshot "$JK" --params '{}' > "$W/jk-r.txt"
grep -q "<project/>" $J/jobs/app/config.xml && [ -f $J/secrets/master.key ] && [ -f $J/jobs/app/builds/1/log ] || fail "jenkins: восстановление"
[ ! -e $J/workspace ] && [ ! -e $J/jobs/app/builds/1/archive ] && [ ! -e $J/caches ] || fail "jenkins: рабочие каталоги, артефакты и кэши не копируются"
[ "$(stat -c %U $J/config.xml)" = jenkins ] || fail "jenkins: владелец"
grep -q '"healthy": true' "$W/jk-r.txt" || fail "jenkins: служба не отвечает"
: > "$W/run/events"
R backup --plugin jenkins --instance jk --config '{"consistency":"stop"}' > /dev/null
grep -q "stop jenkins" "$W/run/events" || fail "jenkins: consistency=stop"
echo "ok: Jenkins — JENKINS_HOME без workspace/кэшей/артефактов, на ходу; stop — с остановкой"
systemctl stop jenkins
fi

if want nexus; then
say "Nexus"
mkdir -p /opt/nexus/bin /opt/nexus/etc /opt/sonatype-work/nexus3/{db/component,blobs/default/content/vol-01,log,etc,tmp,elasticsearch}
printf '#!/bin/sh\nexec %s\n' "$(httpd 18081)" > /opt/nexus/bin/nexus; chmod +x /opt/nexus/bin/nexus
printf -- '-Xms1g\n-Dkaraf.data=../sonatype-work/nexus3\n' > /opt/nexus/bin/nexus.vmoptions
echo "application-port=18081" > /opt/sonatype-work/nexus3/etc/nexus.properties
echo "component-db" > /opt/sonatype-work/nexus3/db/component/c.pcl
head -c 100000 /dev/urandom > /opt/sonatype-work/nexus3/blobs/default/content/vol-01/b1.bytes
echo log > /opt/sonatype-work/nexus3/log/nexus.log; echo idx > /opt/sonatype-work/nexus3/elasticsearch/x
chown -R nexus:nexus /opt/sonatype-work
unit nexus "User=nexus
ExecStart=/opt/nexus/bin/nexus run"
systemctl start nexus
: > "$W/run/events"
R backup --plugin nexus --instance nx --config '{}' | tail -4; NX=$(last nx)
grep -q "stop nexus" "$W/run/events" || fail "nexus: без снимка ФС — короткая остановка для БД"
R show "$NX" | grep -c "blobs.tar" >/dev/null || fail "nexus: blob отдельной частью"
echo broken > /opt/sonatype-work/nexus3/db/component/c.pcl; rm /opt/sonatype-work/nexus3/blobs/default/content/vol-01/b1.bytes
R restore --plugin nexus --instance nx --snapshot "$NX" --params '{}' > "$W/nx-r.txt"
[ "$(cat /opt/sonatype-work/nexus3/db/component/c.pcl)" = component-db ] && [ -f /opt/sonatype-work/nexus3/blobs/default/content/vol-01/b1.bytes ] || fail "nexus: восстановление"
[ ! -e /opt/sonatype-work/nexus3/log/nexus.log ] && [ ! -e /opt/sonatype-work/nexus3/elasticsearch ] || fail "nexus: журналы и индекс поиска не копируются"
grep -q '"healthy": true' "$W/nx-r.txt" || fail "nexus: служба не отвечает"
echo "ok: Nexus — БД и настройки при остановке, blob на ходу, без журналов и индекса поиска"
systemctl stop nexus
fi

# ------------------------------------------------------------------ Keycloak (имитация kc.sh)
if want keycloak; then
say "Keycloak (имитация kc.sh)"
K=/opt/keycloak-hb; mkdir -p $K/bin $K/conf $K/providers $K/themes/corp
export KC_STATE="$W/kc.json"
echo '{"realms": {"master": {"users": ["admin"]}, "corp": {"users": ["ivanov", "petrova"]}}}' > "$KC_STATE"; chmod 666 "$KC_STATE"
cat > $K/bin/kc.sh <<EOF
#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
st = "$KC_STATE"
def opt(f): return a[a.index(f) + 1] if f in a else None
if a[0] == "start":
    os.execvp("python3", ["python3", "-m", "http.server", "18082", "--bind", "127.0.0.1", "--directory", "/tmp"])
s = json.load(open(st))
if a[0] == "export":
    d = opt("--dir"); assert opt("--users") == "realm_file"
    open(os.path.join(d, ".who"), "w").write(str(os.getuid()))
    for r, v in s["realms"].items():
        json.dump({"realm": r, "users": v["users"]}, open(os.path.join(d, r + "-realm.json"), "w"))
    print("Export finished successfully")
elif a[0] == "import":
    d = opt("--dir")
    for f in sorted(os.listdir(d)):
        if f.endswith("-realm.json"):
            r = json.load(open(os.path.join(d, f))); s["realms"][r["realm"]] = {"users": r["users"]}
    s["imported_override"] = opt("--override"); json.dump(s, open(st, "w"))
    print("Import finished successfully")
EOF
chmod 755 $K/bin/kc.sh
printf 'db=postgres\ndb-url=jdbc:postgresql://db/keycloak\nhttp-port=18082\n' > $K/conf/keycloak.conf
echo "theme" > $K/themes/corp/theme.properties
chown -R keycloak:keycloak $K
unit keycloak "User=keycloak
ExecStart=$K/bin/kc.sh start"
systemctl start keycloak
R backup --plugin keycloak --instance kc --config "{\"home\":\"$K\"}" | tee "$W/kc.txt" | tail -3
grep -q "плагина СУБД" "$W/kc.txt" || fail "keycloak: подсказка про копию базы"
KC=$(last kc)
python3 -c "import json;s=json.load(open('$KC_STATE'));del s['realms']['corp'];json.dump(s,open('$KC_STATE','w'))"
rm -rf $K/themes/corp
R restore --plugin keycloak --instance kc --snapshot "$KC" --params '{}' > "$W/kc-r.txt"
python3 -c "import json;s=json.load(open('$KC_STATE'));assert s['realms']['corp']['users']==['ivanov','petrova'] and s['imported_override']=='true', s"
[ -f $K/themes/corp/theme.properties ] || fail "keycloak: темы"
systemctl is-active --quiet keycloak || fail "keycloak: служба не запущена"
echo "ok: Keycloak — выгрузка realm от пользователя службы, import --override, конфигурация"
systemctl stop keycloak
fi

# ------------------------------------------------------------------ Docker и Harbor (имитация docker)
if want docker; then
say "Docker (имитация CLI): тома с паузой, compose, образы"
ln -sf "$HERE/mocks/fake_docker.py" "$MOCK/docker"
export DOCKER_STATE="$W/docker.json"
D="$W/docker"; mkdir -p "$D/volumes/pgdata/_data/base" "$D/volumes/appdata/_data" "$D/projects/shop"
echo "PG_VERSION 16" > "$D/volumes/pgdata/_data/PG_VERSION"; head -c 300000 /dev/urandom > "$D/volumes/pgdata/_data/base/16384"
echo app > "$D/volumes/appdata/_data/settings.json"
printf 'services:\n  web:\n    image: nginx:1.27\n' > "$D/projects/shop/compose.yml"; echo "TOKEN=1" > "$D/projects/shop/.env"
cat > "$DOCKER_STATE" <<EOF
{"root": "$D/volumes", "volumes": {
  "pgdata": {"Driver": "local", "Mountpoint": "$D/volumes/pgdata/_data", "Labels": {"com.docker.compose.project": "shop"}},
  "appdata": {"Driver": "local", "Mountpoint": "$D/volumes/appdata/_data", "Labels": {}},
  "buildx_cache": {"Driver": "local", "Mountpoint": "$D/volumes/buildx/_data", "Labels": {}}},
 "containers": {"shop-db-1": {"State": "running", "Volumes": ["pgdata"], "Project": "shop", "Image": "postgres:16"},
                "shop-web-1": {"State": "running", "Volumes": ["appdata"], "Project": "shop", "Image": "nginx:1.27"},
                "shop-worker-1": {"State": "restarting", "Volumes": ["appdata"], "Project": "shop", "Image": "nginx:1.27"},
                "shop-cron-1": {"State": "running", "PauseFails": "restarting", "Project": "shop", "Image": "nginx:1.27"}},
 "projects": {"shop": {"ConfigFiles": "$D/projects/shop/compose.yml"}}, "images": ["postgres:16", "nginx:1.27"]}
EOF
PATH="$MOCK:$PATH" R backup --plugin docker --instance dk --config '{"exclude_volumes":["buildx_*"],"images":"used"}' | tee "$W/dk.txt" | tail -4
DK=$(last dk)
python3 - <<EOF
import json; e=json.load(open('$DOCKER_STATE'))['events']
assert e.index('pause shop-db-1') < e.index('unpause shop-db-1'), e
cs = json.load(open('$DOCKER_STATE'))['containers']
# Контейнеры в цикле перезапусков не приостанавливаются и копию не ломают; остальные вернулись в работу.
assert 'pause shop-worker-1' not in e and 'unpause shop-cron-1' not in e, e
assert all(c['State']=='running' for n, c in cs.items() if n not in ('shop-worker-1', 'shop-cron-1')), cs
EOF
grep -q "контейнер shop-worker-1 (проект shop) перезапускается в цикле" "$W/dk.txt" || fail "docker: нет предупреждения о контейнере в цикле перезапусков"
grep -q "контейнер shop-cron-1 (во время копии) перезапускается в цикле" "$W/dk.txt" || fail "docker: нет предупреждения о контейнере, ушедшем в перезапуск при паузе"
R show "$DK" | grep -c "volumes/buildx_cache" >/dev/null && fail "docker: исключённый том скопирован"
R show "$DK" | grep -c "images/postgres_16.tar" >/dev/null || fail "docker: образ"
rm -rf "$D/volumes/pgdata/_data/base"; echo "changed" > "$D/projects/shop/.env"
python3 -c "import json;s=json.load(open('$DOCKER_STATE'));s['events']=[];del s['volumes']['appdata'];json.dump(s,open('$DOCKER_STATE','w'))"
PATH="$MOCK:$PATH" R restore --plugin docker --instance dk --snapshot "$DK" --params '{}' > "$W/dk-r.txt"
[ -f "$D/volumes/pgdata/_data/base/16384" ] || fail "docker: содержимое тома"
ls -d "$D/volumes/pgdata/_data.before-restore-"* >/dev/null || fail "docker: прежнее содержимое тома рядом"
[ -f "$D/volumes/appdata/_data/settings.json" ] || fail "docker: удалённый том создан заново"
[ "$(cat "$D/projects/shop/.env")" = "TOKEN=1" ] || fail "docker: проект compose"
python3 - <<EOF
import json; s=json.load(open('$DOCKER_STATE')); e=s['events']
assert 'create appdata' in e and e.index('stop shop-db-1') < e.index('start shop-db-1'), e
assert any(x.startswith('compose -p shop -f') and x.endswith('up -d') for x in e), e
assert set(s['loaded'])=={'postgres:16','nginx:1.27'}, s.get('loaded')
EOF
echo "ok: Docker — тома с паузой, исключения, compose up, образы, том создан заново"

say "Harbor (имитация docker compose и harbor-db)"
H=/opt/harbor-hb; HD="$W/harbor-data"; mkdir -p $H/common/config/core "$HD/secret/core" "$HD/registry/docker/registry/v2/blobs/sha256/ab" "$HD/ca_download"
printf 'hostname: 127.0.0.1:18443\nhttps:\n  port: 443\ndata_volume: %s\n' "$HD" > $H/harbor.yml
echo "services: {}" > $H/docker-compose.yml; echo "CORE_SECRET=x" > $H/common/config/core/env; echo v2.11.1 > $H/VERSION
echo "secretkey" > "$HD/secret/keys/secretkey" 2>/dev/null || { mkdir -p "$HD/secret/keys"; echo secretkey > "$HD/secret/keys/secretkey"; }
head -c 100000 /dev/urandom > "$HD/registry/docker/registry/v2/blobs/sha256/ab/data"
python3 "$HERE/mocks/fake_app.py" 18443 "$W/cert.pem" "$W/key.pem" & echo $! > "$W/harbor-app.pid"
cat > "$DOCKER_STATE" <<EOF
{"root": "$D/volumes", "volumes": {}, "projects": {}, "containers": {"harbor-db": {"State": "running", "db": ["project library", "repo library/nginx"]}, "harbor-core": {"State": "running"}}}
EOF
PATH="$MOCK:$PATH" R backup --plugin nexus --instance hb --config "{\"product\":\"harbor\",\"harbor_dir\":\"$H\"}" | tail -4; HB=$(last hb)
python3 -c "import json;s=json.load(open('$DOCKER_STATE'));s['containers']['harbor-db']['db']=['broken'];s['events']=[];json.dump(s,open('$DOCKER_STATE','w'))"
rm -rf "$HD/registry/docker/registry/v2/blobs" "$HD/secret/keys"
PATH="$MOCK:$PATH" R restore --plugin nexus --instance hb --snapshot "$HB" --params '{}' > "$W/hb-r.txt"
python3 - <<EOF
import json; s=json.load(open('$DOCKER_STATE')); e=s['events']
assert s['containers']['harbor-db']['db']==['project library','repo library/nginx'], s['containers']['harbor-db']
st=[i for i,x in enumerate(e) if x.endswith(' stop')]; db=[i for i,x in enumerate(e) if 'up -d postgresql' in x]; up=[i for i,x in enumerate(e) if x.endswith('up -d')]
assert st and db and up and st[0] < db[0] < e.index('psql load') < up[-1], e
EOF
[ -f "$HD/registry/docker/registry/v2/blobs/sha256/ab/data" ] && [ -f "$HD/secret/keys/secretkey" ] || fail "harbor: хранилище образов и ключи"
echo "ok: Harbor — pg_dumpall из harbor-db, ключи, хранилище; восстановление: stop → файлы → база → up"
fi

# ------------------------------------------------------------------ Kubernetes (имитация kubectl) + настоящий etcd
if want kube; then
say "Kubernetes (имитация kubectl) и снимок etcd"
ln -sf "$HERE/mocks/fake_kubectl.py" "$MOCK/kubectl"
export KUBE_STATE="$W/kube.json"; echo "apiVersion: v1" > "$W/kubeconfig"
etcd --data-dir "$W/etcd" --listen-client-urls http://127.0.0.1:23790 --advertise-client-urls http://127.0.0.1:23790 --listen-peer-urls http://127.0.0.1:23800 > "$W/etcd.log" 2>&1 & echo $! > "$W/etcd.pid"
wait_for 20 env ETCDCTL_API=3 etcdctl --endpoints=http://127.0.0.1:23790 put /registry/test 1 || fail "etcd не запустился"
mkdir -p "$W/k8s/pv-data"; echo "rows" > "$W/k8s/pv-data/db.dat"
python3 - <<EOF
import json
md = lambda n, ns=None, **k: dict({"name": n, "uid": "u-" + n, "resourceVersion": "7", "creationTimestamp": "2026-10-01T00:00:00Z", "managedFields": [{}], **({"namespace": ns} if ns else {})}, **k)
s = {"resources": {"namespaced": ["deployments.apps", "replicasets.apps", "services", "configmaps", "secrets", "persistentvolumeclaims", "events", "widgets.example.com"],
                   "cluster": ["namespaces", "persistentvolumes", "customresourcedefinitions.apiextensions.k8s.io", "nodes"]},
     "applied": [], "objects": {
  "namespaces": [{"kind": "Namespace", "metadata": md("shop"), "status": {"phase": "Active"}}, {"kind": "Namespace", "metadata": md("kube-system")}],
  "deployments.apps": [{"kind": "Deployment", "metadata": md("web", "shop", annotations={"deployment.kubernetes.io/revision": "3"}), "spec": {"replicas": 2}, "status": {"readyReplicas": 2}},
                       {"kind": "Deployment", "metadata": md("coredns", "kube-system"), "spec": {}}],
  "replicasets.apps": [{"kind": "ReplicaSet", "metadata": md("web-5d9", "shop", ownerReferences=[{"kind": "Deployment", "name": "web"}])}],
  "services": [{"kind": "Service", "metadata": md("web", "shop"), "spec": {"clusterIP": "10.96.0.15", "clusterIPs": ["10.96.0.15"], "ports": [{"port": 80}]}},
               {"kind": "Service", "metadata": md("kubernetes", "default"), "spec": {"clusterIP": "10.96.0.1"}}],
  "configmaps": [{"kind": "ConfigMap", "metadata": md("app", "shop"), "data": {"k": "v"}}, {"kind": "ConfigMap", "metadata": md("kube-root-ca.crt", "shop"), "data": {}}],
  "secrets": [{"kind": "Secret", "type": "Opaque", "metadata": md("db-pass", "shop"), "data": {"p": "cA=="}}, {"kind": "Secret", "type": "kubernetes.io/service-account-token", "metadata": md("tok", "shop")}],
  "persistentvolumes": [{"kind": "PersistentVolume", "metadata": md("pv-data"), "spec": {"hostPath": {"path": "$W/k8s/pv-data"}, "claimRef": {"namespace": "shop", "name": "data", "uid": "x", "resourceVersion": "1"}}}],
  "persistentvolumeclaims": [{"kind": "PersistentVolumeClaim", "metadata": md("data", "shop"), "spec": {"volumeName": "pv-data"}, "status": {"phase": "Bound"}}],
  "customresourcedefinitions.apiextensions.k8s.io": [{"kind": "CustomResourceDefinition", "metadata": md("widgets.example.com"), "spec": {"group": "example.com"}}],
  "widgets.example.com": [{"kind": "Widget", "metadata": md("w1", "shop"), "spec": {"size": 3}}],
  "events": [{"kind": "Event", "metadata": md("e1", "shop")}],
  "nodes": [{"kind": "Node", "metadata": md("minikube")}]}}
json.dump(s, open("$KUBE_STATE", "w"))
EOF
KCFG="{\"kubeconfig\":\"$W/kubeconfig\",\"etcd\":\"on\",\"etcd_endpoints\":\"http://127.0.0.1:23790\"}"
PATH="$MOCK:$PATH" R backup --plugin kubernetes --instance k8s --config "$KCFG" | tee "$W/k8s.txt" | tail -4
K8=$(last k8s)
R show "$K8" | grep -c "resources/events" >/dev/null && fail "k8s: события не копируются"
R show "$K8" | grep -c "etcd.db" >/dev/null && R show "$K8" | grep -c "pv/pv-data.tar" >/dev/null || fail "k8s: etcd и данные тома"
python3 -c "import json;s=json.load(open('$KUBE_STATE'));s['objects']={};s['applied']=[];json.dump(s,open('$KUBE_STATE','w'))"
rm -rf "$W/k8s/pv-data"
PATH="$MOCK:$PATH" R restore --plugin kubernetes --instance k8s --snapshot "$K8" --params "{\"kubeconfig\":\"$W/kubeconfig\"}" | tee "$W/k8s-r.txt" | tail -3
python3 - <<EOF
import json; a=[(o['kind'], o['metadata']['name']) for o in json.load(open('$KUBE_STATE'))['applied']]
k=[x[0] for x in a]
assert ('ReplicaSet','web-5d9') not in a and ('Secret','tok') not in a and ('ConfigMap','kube-root-ca.crt') not in a and ('Service','kubernetes') not in a, a
assert ('Deployment','coredns') not in a and ('Namespace','kube-system') not in a, a
assert k.index('Namespace') < k.index('CustomResourceDefinition') < k.index('PersistentVolume') < k.index('PersistentVolumeClaim') < k.index('Secret') < k.index('Service') < k.index('Widget') < k.index('Deployment'), k
svc=[o for o in json.load(open('$KUBE_STATE'))['applied'] if o['kind']=='Service'][0]
assert 'clusterIP' not in svc['spec'], svc
EOF
[ "$(cat "$W/k8s/pv-data/db.dat")" = rows ] || fail "k8s: данные тома hostPath"
R restore --plugin kubernetes --instance k8s --snapshot "$K8" --params "{\"target_dir\":\"$W/k8s-files\"}" > /dev/null
ETCDCTL_API=3 etcdctl snapshot status "$W/k8s-files/etcd.db" -w json 2>/dev/null | grep -q totalKey || fail "k8s: снимок etcd не читается"
echo "ok: Kubernetes — объекты по порядку без служебных полей и производных, тома hostPath, снимок etcd"
kill "$(cat "$W/etcd.pid")"
fi

# ------------------------------------------------------------------ OpenSearch (имитация Snapshot API)
if want opensearch; then
say "OpenSearch (имитация Snapshot API)"
export OS_STATE="$W/os.json" OS_PATH_REPO="$W/os-repos" OS_AUTH="admin:Os-Pass-1"
mkdir -p "$OS_PATH_REPO/hb"
echo '{"repos": {}, "indices": {"logs-1": {"docs": [1,2,3]}, "logs-2": {"docs": [4]}, ".kibana": {"docs": [0]}}}' > "$OS_STATE"
python3 "$HERE/mocks/fake_opensearch.py" 19200 "$W/cert.pem" "$W/key.pem" & echo $! > "$W/os.pid"
sleep 1
OCFG="{\"url\":\"https://127.0.0.1:19200\",\"user\":\"admin\",\"password\":\"Os-Pass-1\",\"insecure\":true,\"repo_path\":\"$OS_PATH_REPO/hb\"}"
R backup --plugin opensearch --instance os --config "$OCFG" | tail -3
R backup --plugin opensearch --instance os --config "$OCFG" | tail -1; OS=$(last os)
[ "$(ls "$OS_PATH_REPO/hb" | grep -c '^snap-')" = 1 ] || fail "opensearch: в локальном репозитории — только последний снимок"
python3 -c "import json;s=json.load(open('$OS_STATE'));s['indices']['logs-1']['docs']=[];json.dump(s,open('$OS_STATE','w'))"
R restore --plugin opensearch --instance os --snapshot "$OS" --params "${OCFG%\}},\"indices\":[\"logs-1\"]}" > "$W/os-r.txt"
python3 -c "import json;s=json.load(open('$OS_STATE'));assert s['indices']['logs-1']['docs']==[1,2,3] and '.kibana' in s['indices'], s"
R restore --plugin opensearch --instance os --snapshot "$OS" --params "${OCFG%\}},\"rename_prefix\":\"restored_\"}" > /dev/null
python3 -c "import json;s=json.load(open('$OS_STATE'));assert s['indices']['restored_logs-2']['docs']==[4] and 'restored_.kibana' not in s['indices'], s"
[ -z "$(ls -d "$OS_PATH_REPO/hb/hopebackup-restore-"* 2>/dev/null)" ] || fail "opensearch: временный репозиторий не удалён"
echo "ok: OpenSearch — снимок в fs, каталог в копию, замена закрытием, восстановление с префиксом"
kill "$(cat "$W/os.pid")"
fi

# ------------------------------------------------------------------ FreeIPA (имитация ipa-backup/ipa-restore)
if want freeipa; then
say "FreeIPA (имитация ipa-backup / ipa-restore)"
cat > "$MOCK/ipa-backup" <<'EOF'
#!/bin/sh
d=/var/lib/ipa/backup/ipa-data-$(date +%Y-%m-%d-%H-%M-%S); mkdir -p $d; echo "$*" > /tmp/hb-apps/ipa-backup.args; echo ldif > $d/userRoot.ldif; echo "Backed up to $d"
EOF
cat > "$MOCK/ipa-restore" <<EOF
#!/bin/sh
echo "\$*" > $W/ipa-restore.args; ls "\$4" > $W/ipa-restore.files; echo "The ipa-restore command was successful"
EOF
chmod +x "$MOCK/ipa-backup" "$MOCK/ipa-restore"; mkdir -p /var/lib/ipa/backup
PATH="$MOCK:$PATH" R backup --plugin freeipa --instance ipa --config '{"mode":"freeipa"}' | tail -2; IP=$(last ipa)
[ -z "$(ls /var/lib/ipa/backup/)" ] || fail "freeipa: каталог ipa-backup должен удаляться"
if PATH="$MOCK:$PATH" R restore --plugin freeipa --instance ipa --snapshot "$IP" --params '{}' > /dev/null 2>&1; then fail "freeipa: без пароля DM восстановление должно остановиться"; fi
PATH="$MOCK:$PATH" R restore --plugin freeipa --instance ipa --snapshot "$IP" --params '{"dm_password":"DM-Pass-1"}' > /dev/null
grep -q -- "--unattended --password DM-Pass-1 /var/lib/ipa/backup/ipa-data-" "$W/ipa-restore.args" && grep -q userRoot.ldif "$W/ipa-restore.files" || fail "freeipa: ipa-restore"
grep -q -- "--data --online" "$W/ipa-backup.args" || fail "freeipa: по умолчанию — данные на ходу"
echo "ok: FreeIPA — ipa-backup --data --online → копия, ipa-restore из распакованного каталога"
fi

printf '\n\033[1;32mПРОВЕРКА ПРИЛОЖЕНИЙ ПРОЙДЕНА\033[0m\n'
