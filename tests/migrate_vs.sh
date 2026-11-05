#!/usr/bin/env bash
# «Переезд» без агента в гостевой ОС: источник — ВМ VMware vSphere (имитация API vCenter), диски которой
# отдаются экспортом NFC в настоящем формате VMDK streamOptimized (собирается qemu-img из образа с ext4).
# Синхронизации → изменения → переключение (штатное выключение гостевой ОС) → применение к тому цели;
# том цели побайтно равен диску ВМ. Подготовка к KVM — через гостевые операции (загрузка и запуск сценария).
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/migrate_vs.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-migvs}
cleanup() {
  [ -f "$W/vs.pid" ] && kill "$(cat "$W/vs.pid")" 2>/dev/null || true
  mountpoint -q "$W/mnt" 2>/dev/null && umount -l "$W/mnt" || true
  for l in $(losetup -a | grep "$W/" | cut -d: -f1); do losetup -d "$l" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/mnt" "$W/virt"
command -v qemu-img >/dev/null || fail "нужен qemu-img"
export HOPEBACKUP_PASSWORD=migvs-repo-pass HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
result() { python3 -c 'import sys,json; t=sys.stdin.read(); i=t.rfind("\n{\n"); print(json.dumps(json.loads(t[i+1:] if i>=0 else t[t.index("{"):])))'; }
# Диск ВМ: образ raw с ext4; экспорт — его копия в streamOptimized.
edit_disk() { local l; l=$(losetup -f --show "$W/disk.raw"); mount "$l" "$W/mnt"; "$@"; umount "$W/mnt"; losetup -d "$l"; }
export_vmdk() { qemu-img convert -f raw -O vmdk -o subformat=streamOptimized "$W/disk.raw" "$W/virt/disk.vmdk"; }

say "ВМ vSphere «app01»: диск 96 МиБ (ext4), экспорт — VMDK streamOptimized"
truncate -s 96M "$W/disk.raw"; mkfs.ext4 -q -F "$W/disk.raw"
edit_disk sh -c "head -c 20M /dev/urandom > '$W/mnt/data.bin'; mkdir '$W/mnt/etc'; echo app01 > '$W/mnt/etc/hostname'"
export_vmdk
echo "  VMDK: $(stat -c %s "$W/virt/disk.vmdk") байт (сжатый, без пустых областей)"
openssl req -x509 -newkey rsa:2048 -nodes -subj /CN=127.0.0.1 -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 2>/dev/null
cat > "$W/vs.json" <<EOF
{"vms": {"vm-10": {"name": "app01", "state": "poweredOn", "disks": ["d1"], "cid": 0, "macs": ["00:50:56:aa:bb:01"], "cpus": 4, "memory_mb": 4096, "hostname": "app01.corp", "firmware": "efi"}},
 "disks": {"d1": {"file": "$W/virt/disk.vmdk", "capacity": $((96<<20))}}, "snaps": {}, "tasks": {}, "leases": {}, "events": [], "quiesce_ok": true}
EOF
VIRT_STATE="$W/vs.json" VIRT_DIR="$W/virt" python3 "$HERE/mocks/fake_virt.py" vsphere 26351 "$W/cert.pem" "$W/key.pem" > "$W/vs.log" 2>&1 & echo $! > "$W/vs.pid"; sleep 0.8
CFG='{"source":"vsphere","url":"https://127.0.0.1:26351","user":"administrator@vsphere.local","password":"Virt-Pass-1","insecure":true,"vm":"app01","guest_user":"root","guest_password":"Guest-Pass-1"}'

say "Синхронизация 1 (снимок с заморозкой ФС, экспорт, разбор VMDK)"
R backup --plugin replica --instance app01 --config "$CFG" | tee "$W/s1.txt" | tail -3
S1=$(last app01)
grep -q "snapshot quiesce" <(python3 -c "import json; print('\n'.join(json.load(open('$W/vs.json'))['events']))") || fail "снимок без заморозки ФС"
python3 -c "import json; s=json.load(open('$W/vs.json')); assert not s['snaps'], 'снимок не удалён'" || fail "снимок ВМ не удалён"

say "Изменения в ВМ → синхронизация 2: только изменённые блоки"
edit_disk sh -c "echo app01-v2 > '$W/mnt/etc/hostname'; head -c 3M /dev/urandom > '$W/mnt/new.bin'"
export_vmdk
R backup --plugin replica --instance app01 --config "$CFG" | tee "$W/s2.txt" | tail -2
CH=$(grep -o "изменено блоков [0-9]* из" "$W/s2.txt" | awk '{print $3}')
[ "$CH" -le 6 ] || fail "синхронизация 2 передала слишком много блоков: $CH"
echo "ok: изменено блоков $CH из 24"

say "Переключение: штатное выключение гостевой ОС и последняя синхронизация"
edit_disk sh -c "echo 'последние данные' > '$W/mnt/etc/final'"
export_vmdk
R backup --plugin replica --instance app01 --config "$CFG" --kind cutover | tee "$W/s3.txt" | tail -3
S3=$(last app01)
python3 -c "import json; s=json.load(open('$W/vs.json')); assert s['vms']['vm-10']['state']=='poweredOff' and 'shutdown guest' in s['events'] and 'snapshot' in s['events'][-3:] or any(e=='snapshot' for e in s['events'])" || fail "ВМ не выключена штатно перед последней синхронизацией"
grep -q "выключение гостевой ОС" "$W/s3.txt" || fail "нет выключения гостевой ОС в журнале"

say "Применение всей цепочки к тому цели: побайтно равен диску ВМ"
R restore --plugin replica --instance app01 --snapshot "$S3" --params "{\"targets\":{\"vmdk2000\":\"$W/target.img\"}}" | result > "$W/a.json"
python3 -c "import json; r=json.load(open('$W/a.json')); assert r['syncs']==3, r" || fail "применены не все синхронизации"
cmp "$W/disk.raw" "$W/target.img" || fail "том цели не равен диску ВМ"
echo "ok: том цели = диск ВМ на момент переключения ($(stat -c %s "$W/target.img") байт)"

say "ВМ на Proxmox (имитация узла) по сведениям из vSphere: 4 vCPU, 4 ГиБ, UEFI, тот же MAC"
. "$HERE/mocks/pve_mock.sh"; pve_mock "$W/pve"
PATH="$M/bin:$PATH" R restore --plugin replica --instance app01 --snapshot "$S3" --params '{"platform":"proxmox","storage":"local"}' | result > "$W/p.json"
grep "qm create" "$M/calls" | tee "$W/qmc.txt" | sed 's/^/  /'
grep -q -- "--cores 4 --memory 4096" "$W/qmc.txt" && grep -q -- "--bios ovmf --machine q35" "$W/qmc.txt" && grep -q -- "--net0 virtio=00:50:56:aa:bb:01,bridge=vmbr0" "$W/qmc.txt" && grep -q -- "--name app01.corp" "$W/qmc.txt" || fail "параметры ВМ на цели не по источнику"
cmp "$W/disk.raw" "$(cat "$M/vols/vm-120-disk-0")" || fail "том ВМ на Proxmox не равен диску"
echo "ok: ВМ создана по сведениям vSphere, том равен диску"

say "Откат: ВМ источника включается через vCenter"
"$BIN" action --plugin replica --name resume --config "$CFG" | result > "$W/on.json"
python3 -c "import json; s=json.load(open('$W/vs.json')); assert s['vms']['vm-10']['state']=='poweredOn'" || fail "ВМ источника не включена"
echo "ok: ВМ источника включена"

say "Подготовка к KVM без агента: сценарий через гостевые операции vSphere"
"$BIN" action --plugin replica --name prepare-kvm --config "$CFG" | result > "$W/prep.json"
python3 - "$W/vs.json" "$W/prep.json" "$HERE/../hopebackup-core/src/plugins/replica_prep.sh" <<'PYEOF' || fail "подготовка через гостевые операции"
import json, sys
s = json.load(open(sys.argv[1])); r = json.load(open(sys.argv[2])); script = open(sys.argv[3]).read()
up = [v for k, v in s["guest_files"].items() if k.endswith(".sh")]
assert up and up[0] == script, "в гостевую ОС загружен не тот сценарий"
assert any(e.startswith("guest run -c 'sh /tmp/hb-prep-") for e in s["events"]), s["events"]
assert r["ready"] is True and any("virtio" in d for d in r["done"]), r
PYEOF
BAD=$(echo "$CFG" | sed 's/Guest-Pass-1/wrong/')
"$BIN" action --plugin replica --name prepare-kvm --config "$BAD" > "$W/bad.txt" 2>&1 && fail "неверный пароль гостевой ОС принят"
grep -q "Failed to authenticate" "$W/bad.txt" || fail "ошибка входа в гостевую ОС не показана"
echo "ok: сценарий загружен и выполнен от root, итог разобран; неверный пароль — понятная ошибка"

printf '\n\033[1;32mПРОВЕРКА ПЕРЕЕЗДА ИЗ VSPHERE БЕЗ АГЕНТА ПРОЙДЕНА\033[0m\n'
