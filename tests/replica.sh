#!/usr/bin/env bash
# Блочная реплика («Переезд»): настоящие блочные устройства (loop) с ext4 и XFS под нагрузкой записи.
# Первая синхронизация, изменения, переключение с заморозкой ФС; применение цепочки к тому цели (файл
# образа и блочное устройство); на цели — fsck без ошибок и те же файлы.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/replica.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-replica}
cleanup() {
  [ -n "${WR:-}" ] && kill "$WR" 2>/dev/null || true
  for m in "$W"/mnt* "$W"/chk; do mountpoint -q "$m" 2>/dev/null && { fsfreeze -u "$m" 2>/dev/null || true; umount -l "$m" || true; }; done
  for l in $(losetup -a | grep "$W/" | cut -d: -f1); do losetup -d "$l" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
rm -rf "$W"; mkdir -p "$W/mnt1" "$W/mnt2" "$W/chk"
export HOPEBACKUP_PASSWORD=replica-repo-pass HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
loop() { losetup -f --show "$@"; }

say "Источник: два диска (ext4 и XFS), запись идёт во время синхронизации"
truncate -s 400M "$W/d1.img"; truncate -s 300M "$W/d2.img"
L1=$(loop "$W/d1.img"); L2=$(loop "$W/d2.img")
mkfs.ext4 -q "$L1"; mkfs.xfs -q "$L2" 2>/dev/null || mkfs.ext4 -q "$L2"
mount "$L1" "$W/mnt1"; mount "$L2" "$W/mnt2"
head -c 80M /dev/urandom > "$W/mnt1/base.bin"; mkdir "$W/mnt1/etc"; echo "конфиг 1" > "$W/mnt1/etc/app.conf"
head -c 40M /dev/urandom > "$W/mnt2/db.bin"; sync
# Нагрузка: файл дописывается всё время.
( i=0; while true; do echo "запись $i" >> "$W/mnt1/journal.log"; i=$((i+1)); sleep 0.05; done ) & WR=$!
CFG="{\"disks\":[\"$L1\",\"$L2\"]}"
R backup --plugin replica --instance srv --config "$CFG" | tee "$W/s1.txt" | tail -3
S1=$(last srv)
head -c 5M /dev/urandom > "$W/mnt1/new.bin"; echo "конфиг 2" > "$W/mnt1/etc/app.conf"; dd if=/dev/urandom of="$W/mnt2/db.bin" bs=1M count=2 seek=10 conv=notrunc status=none; sync
R backup --plugin replica --instance srv --config "$CFG" | tee "$W/s2.txt" | tail -3
S2=$(last srv)
CH=$(grep -o "изменено блоков [0-9]* из" "$W/s2.txt" | awk '{s+=$3} END {print s}')
[ "$CH" -lt 20 ] || fail "вторая синхронизация передала слишком много блоков: $CH"
echo "ok: вторая синхронизация — только изменённые блоки ($CH)"

say "Цель: файл образа и блочное устройство; применение первых двух синхронизаций"
truncate -s 300M "$W/t2.img"; T2=$(loop "$W/t2.img")
D1=$(basename "$L1"); D2=$(basename "$L2")
TG="{\"$D1\":\"$W/t1.img\",\"$D2\":\"$T2\"}"
R restore --plugin replica --instance srv --snapshot "$S2" --params "{\"targets\":$TG}" | tail -4

say "Переключение: остановка нагрузки, заморозка ФС, последняя синхронизация"
kill "$WR"; WR=
R backup --plugin replica --instance srv --config "$CFG" --kind cutover | tee "$W/s3.txt" | tail -4
grep -q "файловые системы заморожены" "$W/s3.txt" || fail "заморозка ФС при переключении"
S3=$(last srv)
touch "$W/mnt1/after-thaw" || fail "ФС осталась замороженной"
R restore --plugin replica --instance srv --snapshot "$S3" --params "{\"targets\":$TG,\"applied\":\"$S2\"}" | tail -3
sum() { (cd "$1" && find . -type f ! -name after-thaw -print0 | sort -z | xargs -0 md5sum); }
sum "$W/mnt1" > "$W/src1.md5"; sum "$W/mnt2" > "$W/src2.md5"
C1=$(loop -r "$W/t1.img")
fsck -fn "$C1" >/dev/null 2>&1 || e2fsck -fn "$C1" >/dev/null 2>&1 || fail "ФС цели (ext4) с ошибками"
mount -o ro,noload "$C1" "$W/chk"; sum "$W/chk" > "$W/t1.md5"; umount "$W/chk"
mount -o ro,norecovery,nouuid "$T2" "$W/chk" 2>/dev/null || mount -o ro,noload "$T2" "$W/chk"; sum "$W/chk" > "$W/t2.md5"; umount "$W/chk"
diff "$W/src1.md5" "$W/t1.md5" >/dev/null || { diff "$W/src1.md5" "$W/t1.md5" | head; fail "файлы диска 1 на цели отличаются"; }
diff "$W/src2.md5" "$W/t2.md5" >/dev/null || fail "файлы диска 2 на цели отличаются"
grep -q "journal.log" "$W/t1.md5" && grep -q "app.conf" "$W/t1.md5" || fail "нет файлов на цели"
echo "ok: на цели те же файлы, что на источнике в момент переключения; ФС цели без ошибок"

printf '\n\033[1;32mПРОВЕРКА РЕПЛИКИ ПРОЙДЕНА\033[0m\n'
