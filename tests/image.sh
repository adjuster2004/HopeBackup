#!/usr/bin/env bash
# Образ машины по SSH: снятие образа диска по SSH (root@localhost), дедупликация изменившихся блоков,
# восстановление образа файлом (raw и, если есть qemu-img, qcow2) и побайтовая сверка с источником.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/image.sh
#
# Нужны root и loop-устройства (как для реплики). disk-read читает диск по безопасному пути: снимок LVM
# через lvcreate -s (здесь тома LVM нет — диск читается целиком обычным чтением), без O_DIRECT/ioctl.
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-image}
SSH_PORT=${SSH_PORT:-2229}

[ "$(id -u)" = 0 ] || fail "нужен root (loop-устройства, чтение диска, sshd)"

cleanup() {
  umount "$W/tiny" 2>/dev/null || true
  [ -f "$W/sshd.pid" ] && kill "$(cat "$W/sshd.pid")" 2>/dev/null || true
  for l in $(losetup -a 2>/dev/null | grep "$W/" | cut -d: -f1); do losetup -d "$l" 2>/dev/null || true; done
  sed -i '/hb-image/d' /root/.ssh/authorized_keys 2>/dev/null || true
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
rm -rf "$W"; mkdir -p "$W"
export HOPEBACKUP_PASSWORD=image-repo-pass HOPEBACKUP_DATA="$W/data"

say "SSH на root@localhost (как цель образа без агента)"
ssh-keygen -q -t ed25519 -N '' -f "$W/id_ed25519"
mkdir -p /root/.ssh /run/sshd; chmod 700 /root/.ssh
sed -i '/hb-image/d' /root/.ssh/authorized_keys 2>/dev/null || true
printf '%s hb-image\n' "$(cut -d' ' -f1,2 "$W/id_ed25519.pub")" >> /root/.ssh/authorized_keys
/usr/sbin/sshd -p "$SSH_PORT" -o PidFile="$W/sshd.pid" -o PermitRootLogin=prohibit-password
wait_for 10 bash -c "echo > /dev/tcp/127.0.0.1/$SSH_PORT" || fail "sshd не поднялся"

say "Диск-источник: loop-устройство с ext4 и файлами"
truncate -s 256M "$W/disk.img"
LOOP=$(losetup -f --show "$W/disk.img")
mkfs.ext4 -q "$LOOP"
MNT="$W/mnt"; mkdir -p "$MNT"; mount "$LOOP" "$MNT"
head -c 40M /dev/urandom > "$MNT/data.bin"; echo "конфигурация" > "$MNT/app.conf"
sync; umount "$MNT"

KEY=$(python3 -c "import json,sys;print(json.dumps(open('$W/id_ed25519').read()))")
CFG="{\"host\":\"127.0.0.1\",\"port\":$SSH_PORT,\"user\":\"root\",\"auth\":\"key\",\"private_key\":$KEY,\"disks\":[\"$LOOP\"],\"lvm\":\"off\",\"freeze_small\":false}"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }

say "Первая копия образа по SSH"
R backup --plugin machine --instance srv --config "$CFG" | tee "$W/s1.txt" | tail -4
S1=$(last srv)
[ -n "$S1" ] || fail "первая копия не создала снапшот"

say "Изменение диска и вторая копия — только изменившиеся блоки"
mount "$LOOP" "$MNT"; head -c 8M /dev/urandom > "$MNT/more.bin"; sync; umount "$MNT"
R backup --plugin machine --instance srv --config "$CFG" | tee "$W/s2.txt" | tail -4
S2=$(last srv)
NEW=$(grep -o "новых [0-9.]* [КМГ]*Б" "$W/s2.txt" | head -1 || true)
echo "новых данных во второй копии: ${NEW:-?}"

say "Восстановление образа файлом (raw) и сверка с источником"
R restore --plugin machine --instance srv --snapshot "$S2" --params "{\"mode\":\"export\",\"format\":\"raw\",\"dir\":\"$W/out\"}" | tail -4
OUT="$W/out/$(basename "$LOOP").img"
[ -f "$OUT" ] || fail "файл образа не создан: $OUT"
SRC=$(dd if="$LOOP" bs=1M 2>/dev/null | md5sum | awk '{print $1}')
DST=$(dd if="$OUT" bs=1M 2>/dev/null | md5sum | awk '{print $1}')
[ "$SRC" = "$DST" ] || fail "образ не совпадает с источником: $SRC != $DST"
echo "ok: восстановленный raw-образ побайтово совпадает с диском-источником"

if command -v qemu-img >/dev/null 2>&1; then
  say "Выгрузка в qcow2 (qemu-img) и сверка"
  R restore --plugin machine --instance srv --snapshot "$S2" --params "{\"mode\":\"export\",\"format\":\"qcow2\",\"dir\":\"$W/out2\"}" | tail -3
  Q="$W/out2/$(basename "$LOOP").qcow2"
  [ -f "$Q" ] || fail "qcow2 не создан"
  qemu-img check "$Q" >/dev/null 2>&1 || fail "qcow2 повреждён"
  qemu-img convert -f qcow2 -O raw "$Q" "$W/q.raw"
  QD=$(dd if="$W/q.raw" bs=1M 2>/dev/null | md5sum | awk '{print $1}')
  [ "$QD" = "$SRC" ] || fail "qcow2 не совпадает с источником"
  echo "ok: qcow2 проверен qemu-img и совпадает с источником"
else
  echo "qemu-img нет — проверка qcow2 пропущена"
fi

say "Продукт «Образы qcow2»: набор в каталог, чужие каталоги рядом не трогаются"
# Каталог с чужими данными (как /mnt/data с каталогами hopebackup) — продукт удаляет только свои наборы.
IMG="$W/images"; mkdir -p "$IMG/hopebackup-data" "$IMG/20200101T000000Z"; echo "чужие данные" > "$IMG/hopebackup-data/keep.txt"; echo x > "$IMG/note.txt"
FMT=raw; command -v qemu-img >/dev/null 2>&1 && FMT=qcow2
QCFG="{\"source\":\"srv\",\"dir\":\"$IMG\",\"format\":\"$FMT\",\"keep\":1}"
R backup --plugin qcow2images --instance imgs --config "$QCFG" | tee "$W/q1.txt" | tail -3
grep -q "нужно свободного места" "$W/q1.txt" || fail "оценка места не выведена до создания"
SET=$(ls "$IMG" | grep -E '^[0-9]{8}T[0-9]{6}Z$' | grep -v 20200101T000000Z | head -1)
[ -n "$SET" ] || fail "набор образов не создан"
[ -f "$IMG/hopebackup-data/keep.txt" ] && [ -f "$IMG/note.txt" ] || fail "продукт удалил чужие данные в каталоге образов"
[ ! -d "$IMG/20200101T000000Z" ] || fail "старый набор сверх keep=1 не удалён"
F="$IMG/$SET/$(basename "$LOOP").$([ "$FMT" = raw ] && echo img || echo qcow2)"
[ -f "$F" ] || fail "файл образа набора не найден: $F"
if [ "$FMT" = qcow2 ]; then qemu-img convert -f qcow2 -O raw "$F" "$W/set.raw"; F="$W/set.raw"; fi
[ "$(dd if="$F" bs=1M 2>/dev/null | md5sum | awk '{print $1}')" = "$SRC" ] || fail "образ набора не совпадает с источником"
echo "ok: набор $SET ($FMT) совпадает с источником, чужие каталоги на месте"
R backup --plugin qcow2images --instance imgs --config "$QCFG" | tee "$W/q2.txt" | tail -2
grep -q "уже собран" "$W/q2.txt" || fail "без новой копии источника набор пересобран заново"

say "Нехватка места: образы не создаются"
mkdir -p "$W/tiny"; mount -t tmpfs -o size=1M tmpfs "$W/tiny"
if R backup --plugin qcow2images --instance imgs2 --config "{\"source\":\"srv\",\"dir\":\"$W/tiny\",\"format\":\"$FMT\"}" > "$W/q3.txt" 2>&1; then umount "$W/tiny"; fail "при нехватке места продукт не остановился"; fi
grep -q "недостаточно места" "$W/q3.txt" || { umount "$W/tiny"; fail "нет понятной ошибки о нехватке места: $(tail -2 "$W/q3.txt")"; }
[ -z "$(ls -A "$W/tiny")" ] || { umount "$W/tiny"; fail "при нехватке места остались файлы"; }
umount "$W/tiny"
echo "ok: при нехватке места образы не создаются, каталог пуст"

printf '\n\033[1;32mПРОВЕРКА ОБРАЗА МАШИНЫ ПРОЙДЕНА\033[0m\n'
