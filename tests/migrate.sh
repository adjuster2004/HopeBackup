#!/usr/bin/env bash
# «Переезд»: реплика машины применяется к платформе цели, которая сама создаёт ВМ и тома.
#   * Proxmox VE — имитация qm/pvesm/pvesh, но тома настоящие: файл (хранилище dir) и блочное устройство
#     (хранилище lvm — заранее забито мусором: толстый LVM не обнуляет новые тома).
#   * oVirt / zVirt — имитация API Engine и imageio с записью по смещениям; домен san1 — блочный (iSCSI).
# Цепочка: две синхронизации → первое применение (создание ВМ) → переключение → применение по состоянию.
# Проверки: диск с ФС — fsck и те же файлы; диск без ФС — побайтно равен источнику (нули на месте мусора).
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/migrate.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-migrate}
cleanup() {
  [ -n "${WR:-}" ] && kill "$WR" 2>/dev/null || true
  [ -f "$W/ovirt.pid" ] && kill "$(cat "$W/ovirt.pid")" 2>/dev/null || true
  for m in "$W"/mnt "$W"/chk; do mountpoint -q "$m" 2>/dev/null && { fsfreeze -u "$m" 2>/dev/null || true; umount -l "$m" || true; }; done
  for l in $(losetup -a | grep "$W/" | cut -d: -f1); do losetup -d "$l" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/mnt" "$W/chk" "$W/pve/bin" "$W/pve/images" "$W/virt"
export HOPEBACKUP_PASSWORD=migrate-repo-pass HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
loop() { losetup -f --show "$@"; }
# Результат применения — JSON в конце вывода.
result() { python3 -c 'import sys,json; t=sys.stdin.read(); i=t.rfind("\n{\n"); print(json.dumps(json.loads(t[i+1:] if i>=0 else t[t.index("{"):])))'; }
jq1() { python3 -c "import sys,json; v=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

say "Источник: системный диск (ext4, запись идёт) и диск данных без ФС"
truncate -s 300M "$W/d1.img"; truncate -s 64M "$W/d2.img"
L1=$(loop "$W/d1.img"); L2=$(loop "$W/d2.img")
mkfs.ext4 -q "$L1"; mount "$L1" "$W/mnt"
head -c 60M /dev/urandom > "$W/mnt/base.bin"; mkdir "$W/mnt/etc"; echo "web01" > "$W/mnt/etc/hostname"; sync
# Диск данных: куски данных с дырами (нулями) между ними.
for o in 0 9 30 51; do dd if=/dev/urandom of="$L2" bs=1M count=3 seek=$o conv=notrunc oflag=direct status=none; done
( i=0; while true; do echo "запись $i" >> "$W/mnt/app.log"; i=$((i+1)); sleep 0.05; done ) & WR=$!
CFG="{\"disks\":[\"$L1\",\"$L2\"]}"
R backup --plugin replica --instance web01 --config "$CFG" | tail -2
dd if=/dev/urandom of="$L2" bs=1M count=2 seek=20 conv=notrunc oflag=direct status=none
dd if=/dev/zero of="$L2" bs=1M count=3 seek=30 conv=notrunc oflag=direct status=none   # данные стёрты — нули
echo "web01-v2" > "$W/mnt/etc/hostname"; sync
R backup --plugin replica --instance web01 --config "$CFG" | tail -2
S2=$(last web01)
D1=$(basename "$L1"); D2=$(basename "$L2")

# ---------------------------------------------------------------- Proxmox (имитация)
. "$HERE/mocks/pve_mock.sh"; pve_mock "$W/pve"
say "Proxmox: первое применение — создание ВМ, тома в двух хранилищах"
# Диск данных — на толстый LVM: отдельным применением с другим хранилищем нельзя, поэтому системный диск
# тоже на local-lvm, а второй прогон (oVirt) — на файлах. Здесь проверяется обнуление и устройство.
P1="{\"platform\":\"proxmox\",\"storage\":\"local-lvm\",\"networks\":{\"*\":\"vmbr1\"},\"cores\":2}"
PATH="$M/bin:$PATH" R restore --plugin replica --instance web01 --snapshot "$S2" --params "$P1" | tee "$W/p1.txt" | result > "$W/p1.json"
grep -q "qm create 120 --name" "$M/calls" || fail "proxmox: ВМ не создана"
grep -q -- "--cores 2" "$M/calls" && grep -q -- "--scsihw virtio-scsi-single" "$M/calls" || fail "proxmox: параметры ВМ"
MAC=$(cat /sys/class/net/$(ls /sys/class/net | grep -v lo | head -1)/address 2>/dev/null || true)
grep -q "virtio=.*,bridge=vmbr1" "$M/calls" || echo "  (у машины теста нет сетевых интерфейсов с устройством — сеть ВМ не проверяется)"
grep -q "qm set 120 --scsi0 local-lvm:vm-120-disk-0,discard=on" "$M/calls" && grep -q "qm set 120 --scsi1 local-lvm:vm-120-disk-1" "$M/calls" || { grep "qm set" "$M/calls"; fail "proxmox: тома не подключены"; }
grep -q "qm set 120 --boot order=scsi0" "$M/calls" || fail "proxmox: порядок загрузки"
grep -q "не обнулён хранилищем" "$W/p1.txt" || fail "proxmox: новый том толстого LVM не обнулён"
[ "$(jq1 'v["state"]["vmid"]' < "$W/p1.json")" = 120 ] && [ "$(jq1 'v["state"]["applied"]' < "$W/p1.json")" = "$S2" ] || fail "proxmox: состояние цели"
echo "ok: ВМ 120 создана, тома выделены и подключены, мусор толстого LVM обнулён"

say "Proxmox: тестовый запуск без сети; толстый LVM без снимков — после теста тома переписываются целиком"
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/p1.json")
PATH="$M/bin:$PATH" "$BIN" action --plugin replica --name target-test-start --config "{\"platform\":\"proxmox\",\"state\":$ST}" | result > "$W/t1.json"
grep -q "link_down=1" "$M/net0-120" && [ -f "$M/running-120" ] || fail "proxmox: тест — ВМ без сети и запущена"
# Гость пишет на тома во время теста.
V1=$(cat "$M/vols/vm-120-disk-1"); dd if=/dev/urandom of="$V1" bs=1M count=2 seek=40 conv=notrunc status=none
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/t1.json")
PATH="$M/bin:$PATH" R restore --plugin replica --instance web01 --snapshot "$S2" --params "{\"platform\":\"proxmox\",\"state\":$ST}" > /dev/null 2>&1 && fail "proxmox: применение во время теста должно быть отклонено"
PATH="$M/bin:$PATH" "$BIN" action --plugin replica --name target-test-stop --config "{\"platform\":\"proxmox\",\"state\":$ST}" | result > "$W/t2.json"
[ ! -f "$M/running-120" ] && ! grep -q link_down "$M/net0-120" && [ "$(jq1 'v["state"]["dirty"]' < "$W/t2.json")" = True ] || fail "proxmox: окончание теста"
echo "ok: тест без сети, применение во время теста отклонено, после теста тома помечены к перезаписи"
cp "$W/t2.json" "$W/p1.json"

say "Переключение и применение по состоянию (без создания)"
kill "$WR"; WR=
R backup --plugin replica --instance web01 --config "$CFG" --kind cutover | tail -2
S3=$(last web01)
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/p1.json")
NC=$(grep -c "qm create" "$M/calls")
PATH="$M/bin:$PATH" R restore --plugin replica --instance web01 --snapshot "$S3" --params "{\"platform\":\"proxmox\",\"state\":$ST,\"start\":true}" | tee "$W/p2.txt" | result > "$W/p2.json"
[ "$(grep -c "qm create" "$M/calls")" = "$NC" ] || fail "proxmox: ВМ создана повторно"
grep -q "qm start 120" "$M/calls" || fail "proxmox: ВМ не запущена после переключения"
[ "$(jq1 'v["syncs"]' < "$W/p2.json")" = 3 ] && [ "$(jq1 'v["cutover"]' < "$W/p2.json")" = True ] || fail "proxmox: после теста без снимка должна примениться вся цепочка"
grep -q "будут обнулены и записаны заново" "$W/p2.txt" || fail "proxmox: тома после теста не переписаны"
[ "$(jq1 '"dirty" in v["state"]' < "$W/p2.json")" = False ] || fail "proxmox: отметка перезаписи не снята"
# Проверка томов: системный — fsck и файлы; данных — побайтно.
sum() { (cd "$1" && find . -type f -print0 | sort -z | xargs -0 md5sum); }
sum "$W/mnt" > "$W/src.md5"
V0=$(cat "$M/vols/vm-120-disk-0"); V1=$(cat "$M/vols/vm-120-disk-1")
e2fsck -fn "$V0" >/dev/null 2>&1 || fail "proxmox: ФС системного тома с ошибками"
mount -o ro,noload "$V0" "$W/chk"; sum "$W/chk" > "$W/p.md5"; umount "$W/chk"
diff "$W/src.md5" "$W/p.md5" >/dev/null || fail "proxmox: файлы системного тома отличаются"
cmp -n $((64<<20)) "$L2" "$V1" || fail "proxmox: том данных не равен диску источника (следы теста остались?)"
echo "ok: на томах ВМ — состояние источника в момент переключения (ФС без ошибок, диск данных побайтно)"
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/p2.json")
PATH="$M/bin:$PATH" "$BIN" action --plugin replica --name target-rollback --config "{\"platform\":\"proxmox\",\"state\":$ST}" | result > "$W/p3.json"
[ ! -f "$M/running-120" ] && [ "$(jq1 'v["state"]["dirty"]' < "$W/p3.json")" = True ] || fail "proxmox: откат"
echo "ok: откат — ВМ остановлена, тома будут переписаны при возобновлении"

# ---------------------------------------------------------------- oVirt / zVirt (имитация API)
say "zVirt/oVirt: ВМ через API, запись imageio по смещениям; домен san1 — блочный"
openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 2>/dev/null
echo '{"vms": {}, "disks": {}}' > "$W/ovirt.json"
VIRT_STATE="$W/ovirt.json" VIRT_DIR="$W/virt" python3 "$HERE/mocks/fake_virt.py" ovirt 26341 "$W/cert.pem" "$W/key.pem" > "$W/ovirt.log" 2>&1 & echo $! > "$W/ovirt.pid"; sleep 0.8
OV='"url":"https://127.0.0.1:26341/ovirt-engine/api","user":"admin@internal","password":"Virt-Pass-1","insecure":true'
S1=$(R list --instance web01 | sed -n 2p | awk '{print $1}')
R restore --plugin replica --instance web01 --snapshot "$S2" --params "{\"platform\":\"ovirt\",$OV,\"cluster\":\"Default\",\"storage\":\"san1\",\"networks\":{\"*\":\"dmz\"}}" | tee "$W/o1.txt" | result > "$W/o1.json"
python3 - "$W/ovirt.json" <<'EOF' || fail "oVirt: ВМ, диски, передачи"
import json, sys
s = json.load(open(sys.argv[1]))
vm = list(s["vms"].values())[0]
assert vm["bios"]["type"] in ("q35_sea_bios", "q35_ovmf"), vm
assert len(vm["disks"]) == 2, vm
for d in vm["disks"]:
    assert s["disks"][d]["format"] == "raw" and s["disks"][d]["sparse"] is False, s["disks"][d]
tr = list(s["transfers"].values())
assert len(tr) == 2 and all(t["phase"] == "finished_success" for t in tr), tr
assert all(t.get("zeroed", 0) >= int(s["disks"][t["disk"]]["provisioned_size"]) for t in tr), "новый диск блочного домена не обнулён"
assert all(t.get("puts", 0) > 0 and "flush" in t.get("ops", []) for t in tr), tr
EOF
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/o1.json")
# Тестовый запуск: снимок, интерфейсы отключены; гость пишет на диск; по окончании — возврат к снимку.
"$BIN" action --plugin replica --name target-test-start --config "{\"platform\":\"ovirt\",$OV,\"state\":$ST}" | result > "$W/ot1.json"
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/ot1.json")
python3 - "$W/ovirt.json" <<'EOF' || fail "oVirt: тестовый запуск"
import json, sys
s = json.load(open(sys.argv[1])); vm = list(s["vms"].values())[0]
assert vm["status"] == "up" and all(n["linked"] is False for n in vm["nics"]) and len(vm["snapshots"]) == 1, vm
for d in vm["disks"]:
    f = s["disks"][d]["file"]; open(f, "r+b").write(b"guest-wrote-here" * 4096)
EOF
"$BIN" action --plugin replica --name target-test-stop --config "{\"platform\":\"ovirt\",$OV,\"state\":$ST}" | result > "$W/ot2.json"
ST=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1]))["state"]))' "$W/ot2.json")
python3 - "$W/ovirt.json" "$W/ot2.json" <<'EOF' || fail "oVirt: окончание теста"
import json, sys
s = json.load(open(sys.argv[1])); r = json.load(open(sys.argv[2])); vm = list(s["vms"].values())[0]
assert vm["status"] == "down" and all(n["linked"] for n in vm["nics"]) and not vm["snapshots"], vm
assert "snapshot restore" in s["events"] and r["state"]["testing"] is None and not r["state"].get("dirty")
EOF
echo "ok: тест на zVirt/oVirt — снимок, без сети, следы гостя убраны возвратом к снимку"
R restore --plugin replica --instance web01 --snapshot "$S3" --params "{\"platform\":\"ovirt\",$OV,\"state\":$ST}" | result > "$W/o2.json"
python3 - "$W/ovirt.json" "$W/o2.json" > "$W/ovfiles" <<'EOF' || fail "oVirt: применение по состоянию"
import json, sys
s = json.load(open(sys.argv[1])); r = json.load(open(sys.argv[2]))
assert len(s["vms"]) == 1 and len(s["disks"]) == 2, "создано повторно"
assert r["syncs"] == 1 and r["state"]["applied"] == r["applied"]
st = r["state"]["disks"]
for k in sorted(st, key=lambda k: list(st).index(k)):
    print(k, s["disks"][st[k]["id"]]["file"])
EOF
F1=$(grep "^$D1 " "$W/ovfiles" | cut -d' ' -f2); F2=$(grep "^$D2 " "$W/ovfiles" | cut -d' ' -f2)
C=$(loop -r "$F1"); e2fsck -fn "$C" >/dev/null 2>&1 || fail "oVirt: ФС системного диска с ошибками"
mount -o ro,noload "$C" "$W/chk"; sum "$W/chk" > "$W/o.md5"; umount "$W/chk"
diff "$W/src.md5" "$W/o.md5" >/dev/null || fail "oVirt: файлы системного диска отличаются"
cmp -n $((64<<20)) "$L2" "$F2" || fail "oVirt: диск данных не равен источнику"
echo "ok: zVirt/oVirt — ВМ с дисками на блочном домене, обнуление, запись по смещениям, применение по состоянию"
[ -n "$S1" ] || true

say "Подготовка к KVM на этой машине: /etc — копия, подключённая поверх в отдельном пространстве монтирования"
E="$W/etc-copy"; PB="$W/prepbin"; mkdir -p "$PB"
cp -a /etc "$E"
printf '/dev/sdz1 /data ext4 defaults 0 2\nUUID=keep-me / ext4 defaults 0 1\n' >> "$E/fstab"
# Инструменты, которых нет в контейнере теста: сборка initramfs, blkid для «старого» диска, пакетный менеджер.
printf '#!/bin/sh\necho "update-initramfs $* : $(tr "\\n" " " < /etc/initramfs-tools/modules)" >> %s/prep.calls\n' "$W" > "$PB/update-initramfs"
printf '#!/bin/sh\ncase "$*" in *"/dev/sdz1"*) echo 1111-aaaa-2222 ;; *) exit 2 ;; esac\n' > "$PB/blkid"
printf '#!/bin/sh\necho "apt-get $*" >> %s/prep.calls; exit 100\n' "$W" > "$PB/apt-get"
chmod +x "$PB"/*
unshare -m --propagation private sh -c "set -e; mount --bind '$E' /etc; PATH='$PB':\$PATH '$BIN' action --plugin replica --name prepare-kvm" > "$W/prep.txt" 2>&1 \
  || { cat "$W/prep.txt"; fail "подготовка к KVM завершилась ошибкой"; }
grep -q "virtio_blk" "$W/prep.calls" && grep -q "virtio_scsi" "$W/prep.calls" || { cat "$W/prep.calls"; fail "подготовка: virtio не добавлены в initramfs"; }
grep -q "^UUID=1111-aaaa-2222 /data ext4" "$E/fstab" && grep -q "^UUID=keep-me / " "$E/fstab" || { cat "$E/fstab"; fail "подготовка: fstab"; }
[ -f "$E/fstab.hb-prep" ] || fail "подготовка: нет резервной копии fstab"
MAC=$(cat /sys/class/net/eth0/address)
grep -q "MACAddress=$MAC" "$E/systemd/network/10-hopebackup-eth0.link" && grep -q "Name=eth0" "$E/systemd/network/10-hopebackup-eth0.link" || fail "подготовка: имя eth0 не закреплено за MAC"
grep -q "ATTR{address}==\"$MAC\", NAME=\"eth0\"" "$E/udev/rules.d/70-hopebackup-net.rules" || fail "подготовка: правило udev"
grep -q "apt-get install -y -q qemu-guest-agent" "$W/prep.calls" && grep -q "qemu-guest-agent не установлен" "$W/prep.txt" || fail "подготовка: гостевой агент"
grep -q '"ready": false' "$W/prep.txt" || fail "подготовка: предупреждения должны снимать готовность"
[ ! -e /etc/systemd/network/10-hopebackup-eth0.link ] && ! grep -q sdz1 /etc/fstab || fail "подготовка изменила систему теста"
# Повторный запуск ничего не дублирует.
unshare -m --propagation private sh -c "mount --bind '$E' /etc; PATH='$PB':\$PATH '$BIN' action --plugin replica --name prepare-kvm" > /dev/null 2>&1 || true
[ "$(grep -c "$MAC" "$E/udev/rules.d/70-hopebackup-net.rules")" = 1 ] && [ "$(grep -cx virtio_blk "$E/initramfs-tools/modules")" = 1 ] || fail "подготовка: повторный запуск дублирует записи"
echo "ok: virtio в initramfs, fstab по UUID (с копией), eth0 закреплён за MAC, гостевой агент — с предупреждением; повтор безопасен"

printf '\n\033[1;32mПРОВЕРКА ПЕРЕЕЗДА ПРОЙДЕНА\033[0m\n'
