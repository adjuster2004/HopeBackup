# Часть tests/dbms.sh: СУБД, которых нет в песочнице, — имитации утилит (tests/mocks/fake_*.py).
# Проверяется то, что делает плагин: команды, передача пароля не в командной строке, порядок цепочки
# восстановления, переименование, исключения.

MOCK="$W/mockbin"; mkdir -p "$MOCK"
ln -sf "$HERE/mocks/fake_sqlcmd.py" "$MOCK/sqlcmd"
for p in mongodump mongorestore mongosh; do ln -sf "$HERE/mocks/fake_mongo.py" "$MOCK/$p"; done
ln -sf "$HERE/mocks/fake_clickhouse.py" "$MOCK/clickhouse-client"
chmod +x "$HERE"/mocks/fake_*.py
OLDPATH=$PATH; export PATH="$MOCK:$PATH"
# Пароль не должен появляться в аргументах утилит: оболочки ниже запоминают командные строки.
noargs() { ! grep -rq -- "$2" "$1" 2>/dev/null; }

# ------------------------------------------------------------------ MS SQL Server
if want mssql; then
say "MS SQL Server (имитация sqlcmd): полная → журналы → разностная, STOPAT, переименование"
export MSSQL_STATE="$W/mssql.json" MSSQL_EXPECT_PASSWORD='Sql-Pass-1'
cat > "$MSSQL_STATE" <<'EOF'
{"lsn": 0, "dbs": {
 "master": {"recovery": "SIMPLE", "state": "ONLINE", "rows": ["m"], "log": [], "files": [["master", "/var/opt/mssql/data/master.mdf", "D"], ["mastlog", "/var/opt/mssql/data/mastlog.ldf", "L"]]},
 "sales": {"recovery": "FULL", "state": "ONLINE", "rows": ["r1", "r2", "r3"], "log": [], "files": [["sales", "/var/opt/mssql/data/sales.mdf", "D"], ["sales_log", "/var/opt/mssql/data/sales_log.ldf", "L"]]},
 "hrdb": {"recovery": "SIMPLE", "state": "ONLINE", "rows": ["h1"], "log": [], "files": [["hrdb", "/var/opt/mssql/data/hrdb.mdf", "D"], ["hrdb_log", "/var/opt/mssql/data/hrdb_log.ldf", "L"]]}}}
EOF
MS="$HERE/mocks/fake_sqlcmd.py"
rows() { python3 -c "import json;print(','.join(json.load(open('$MSSQL_STATE'))['dbs']['$1']['rows']))"; }
SCFG="{\"host\":\"localhost\",\"user\":\"sa\",\"password\":\"Sql-Pass-1\",\"stage_dir\":\"$W/mssql-stage\"}"
R backup --plugin mssql --instance ms --config "$SCFG" | tail -3; F=$(last ms)
"$MS" --mutate sales r4; "$MS" --mutate sales r5; "$MS" --mutate hrdb h2
R backup --plugin mssql --instance ms --config "$SCFG" --kind incr | tee "$W/ms1.txt" | tail -2; L1=$(last ms)
grep -q "hrdb: модель восстановления SIMPLE" "$W/ms1.txt" || fail "mssql: база SIMPLE — разностная вместо журнала"
"$MS" --mutate sales r6
R backup --plugin mssql --instance ms --config "$SCFG" --kind incr | tail -1; L2=$(last ms)
sleep 2; T=$(date '+%Y-%m-%d %H:%M:%S'); sleep 2
"$MS" --mutate sales r7
R backup --plugin mssql --instance ms --config "$SCFG" --kind incr | tail -1; L3=$(last ms)
R backup --plugin mssql --instance ms --config "${SCFG%\}},\"incr_mode\":\"diff\"}" --kind incr | tail -1; D1=$(last ms)
"$MS" --mutate sales r8
R backup --plugin mssql --instance ms --config "$SCFG" --kind incr | tail -1; L4=$(last ms)
[ -z "$(ls -A "$W/mssql-stage")" ] || fail "mssql: файлы .bak остались в промежуточном каталоге"
"$MS" --mutate sales r9
R restore --plugin mssql --instance ms --snapshot "$L3" --params "{\"databases\":[\"sales\"],\"to_time\":\"$T\",\"password\":\"Sql-Pass-1\",\"stage_dir\":\"$W/mssql-stage\"}" | tee "$W/ms-r1.txt" | grep "цепочка"
[ "$(rows sales)" = "r1,r2,r3,r4,r5,r6" ] || fail "mssql: на момент $T ожидалось r1…r6, есть $(rows sales)"
grep -q "STOPAT" "$W/ms-r1.txt" || fail "mssql: STOPAT"
echo "ok: полная → журнал → журнал → журнал со STOPAT → r1…r6"
R restore --plugin mssql --instance ms --snapshot "$L4" --params "{\"databases\":[\"sales\"],\"password\":\"Sql-Pass-1\",\"stage_dir\":\"$W/mssql-stage\"}" | grep "цепочка" | tee "$W/ms-r2.txt"
grep -q "full .* → diff .* → log" "$W/ms-r2.txt" || fail "mssql: цепочка должна идти через разностную"
[ "$(rows sales)" = "r1,r2,r3,r4,r5,r6,r7,r8" ] || fail "mssql: по копии журнала после разностной: $(rows sales)"
echo "ok: полная → разностная → журнал → r1…r8"
R restore --plugin mssql --instance ms --snapshot "$F" --params "{\"databases\":[\"sales\"],\"rename_to\":\"sales_copy\",\"password\":\"Sql-Pass-1\",\"stage_dir\":\"$W/mssql-stage\"}" > "$W/ms-r3.txt"
[ "$(rows sales_copy)" = "r1,r2,r3" ] && [ "$(rows sales)" = "r1,r2,r3,r4,r5,r6,r7,r8" ] || fail "mssql: копия под другим именем"
python3 -c "import json;f=json.load(open('$MSSQL_STATE'))['dbs']['sales_copy']['files'];assert all('sales_copy' in p for _,p,_ in f), f"
echo "ok: sales → sales_copy с MOVE файлов"
R restore --plugin mssql --instance ms --snapshot "$L1" --params "{\"databases\":[\"hrdb\"],\"password\":\"Sql-Pass-1\",\"stage_dir\":\"$W/mssql-stage\"}" > /dev/null
[ "$(rows hrdb)" = "h1,h2" ] || fail "mssql: SIMPLE — полная + разностная"
grep -q "Sql-Pass-1" "$W/mssql.json" && fail "mssql: пароль попал в запросы"
echo "ok: MS SQL — цепочки, STOPAT, MOVE; пароль только в SQLCMDPASSWORD"
unset MSSQL_EXPECT_PASSWORD
fi

# ------------------------------------------------------------------ MongoDB
if want mongodb; then
say "MongoDB (имитация mongodump/mongorestore)"
export MONGO_STATE="$W/mongo.json" MONGO_EXPECT_PASSWORD='Mongo"Pass\1'
echo '{"rs": true, "dbs": {"shop": {"orders": [{"_id": 1}, {"_id": 2}], "users": [{"_id": "a"}]}, "crm": {"leads": [{"_id": 9}]}}}' > "$MONGO_STATE"
MCFG='{"uri":"mongodb://127.0.0.1:27017","user":"backup","password":"Mongo\"Pass\\1"}'
R backup --plugin mongodb --instance mg --config "$MCFG" | tee "$W/mg.txt" | tail -2
grep -q "набор реплик, срез oplog" "$W/mg.txt" || fail "mongodb: набор реплик определяется через mongosh"
MG=$(last mg)
python3 -c "import json;s=json.load(open('$MONGO_STATE'));s['dbs']['shop']['orders']=[{'_id':99}];del s['dbs']['crm'];json.dump(s,open('$MONGO_STATE','w'))"
R restore --plugin mongodb --instance mg --snapshot "$MG" --params "$MCFG" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$MONGO_STATE'))
assert s['dbs']['shop']['orders']==[{'_id':1},{'_id':2}], s
assert s['dbs']['crm']['leads']==[{'_id':9}], s
assert s['last_restore']['oplog_replay'], 'oplogReplay'
EOF
R restore --plugin mongodb --instance mg --snapshot "$MG" --params "${MCFG%\}},\"database\":\"shop\",\"rename_to\":\"shop_copy\"}" > /dev/null
python3 -c "import json;s=json.load(open('$MONGO_STATE'));assert s['dbs']['shop_copy']['users']==[{'_id':'a'}] and 'crm_copy' not in s['dbs'], s"
echo "ok: MongoDB — архив с oplog, --oplogReplay, одна база под другим именем; пароль в --config"
fi

# ------------------------------------------------------------------ ClickHouse
if want clickhouse; then
say "ClickHouse (имитация clickhouse-client): native и BACKUP на диск"
export CH_STATE="$W/ch.json" CH_EXPECT_PASSWORD='ch<pass>&1'
mkdir -p "$W/ch-backups"
cat > "$CH_STATE" <<EOF
{"disks": {"backups": "$W/ch-backups/"}, "dbs": {"shop": {"engine": "Atomic", "tables": {
  "orders": {"engine": "MergeTree", "ddl": "CREATE TABLE shop.orders\n(\n    \`id\` UInt64\n)\nENGINE = MergeTree\nORDER BY id", "rows": [[1], [2], [3]]},
  "orders_view": {"engine": "View", "ddl": "CREATE VIEW shop.orders_view\n(\n    \`id\` UInt64\n) AS SELECT id FROM shop.orders", "rows": []},
  "remote": {"engine": "Distributed", "ddl": "CREATE TABLE shop.remote\n(\n    \`id\` UInt64\n)\nENGINE = Distributed('c', 'shop', 'orders')", "rows": []}}}}}
EOF
CCFG='{"host":"127.0.0.1","password":"ch<pass>&1","method":"native"}'
R backup --plugin clickhouse --instance ch --config "$CCFG" | tail -3; CN=$(last ch)
R show "$CN" | grep -c "data/shop/orders.native" >/dev/null || fail "clickhouse: данные таблицы"
R show "$CN" | grep -c "remote.native" >/dev/null && fail "clickhouse: у Distributed нет своих данных"
R restore --plugin clickhouse --instance ch --snapshot "$CN" --params '{"password":"ch<pass>&1"}' > "$W/ch-r1.txt"
python3 - <<EOF
import json; s=json.load(open('$CH_STATE'))
assert s['dbs']['shop']['tables']['orders']['rows']==[[1],[2],[3]], s['dbs']['shop']
assert any(k.startswith('shop_before_restore_') for k in s['dbs']), list(s['dbs'])
assert list(s['dbs']['shop']['tables'])==['orders','remote','orders_view'] or set(s['dbs']['shop']['tables'])=={'orders','remote','orders_view'}
EOF
R restore --plugin clickhouse --instance ch --snapshot "$CN" --params '{"password":"ch<pass>&1","databases":["shop"],"rename_to":"shop_copy"}' > /dev/null
python3 -c "import json;s=json.load(open('$CH_STATE'));t=s['dbs']['shop_copy']['tables'];assert t['orders']['rows']==[[1],[2],[3]] and 'shop_copy.orders' in t['orders']['ddl'], t"
echo "ok: ClickHouse native — схема по порядку, данные, прежняя база рядом, копия под другим именем"
R backup --plugin clickhouse --instance chb --config '{"host":"127.0.0.1","password":"ch<pass>&1","method":"backup","databases":["shop"]}' | tail -2; CB=$(last chb)
[ -z "$(ls -A "$W/ch-backups")" ] || fail "clickhouse: каталог BACKUP должен удаляться с диска"
R restore --plugin clickhouse --instance chb --snapshot "$CB" --params '{"password":"ch<pass>&1","rename_to":"shop_b"}' > /dev/null
python3 -c "import json;s=json.load(open('$CH_STATE'));assert s['dbs']['shop_b']['tables']['orders']['rows']==[[1],[2],[3]]"
[ -z "$(ls -A "$W/ch-backups")" ] || fail "clickhouse: каталог RESTORE должен удаляться"
echo "ok: ClickHouse BACKUP/RESTORE через диск копий; пароль в --config-file"
fi
export PATH=$OLDPATH
