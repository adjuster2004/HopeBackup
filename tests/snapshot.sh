#!/usr/bin/env bash
# Проверка снимков файловой системы перед файловой копией (нужны root, btrfs-progs, losetup):
#   btrfs (настоящий, на loop-диске): копия читается из снимка — файл, изменённый сразу после снимка
#   (команда «после снимка»), попадает в копию в прежнем виде, созданный после снимка — не попадает;
#   снимок удаляется; оставшийся от прерванной копии снимок убирается при следующей;
#   LVM (команды lvs/vgs/lvcreate/lvremove и монтирование имитируются: в ядре стенда нет device-mapper):
#   размер снимка — 10% тома (не меньше 1 ГиБ), снимок монтируется только для чтения и удаляется;
#   нет места в группе томов — в режиме auto копия «на ходу» с предупреждением, в режиме required — ошибка;
#   том без LVM/ZFS/btrfs: auto — копия с предупреждением, required — ошибка.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/snapshot.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-snap}
BT=/mnt/hb-btrfs
LV=/srv/hb-lvm

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  for m in $(grep -o "$W/snapmnt[^ ]*\|/run/hopebackup/snap[^ ]*" /proc/mounts | sort -r); do umount -l "$m" 2>/dev/null || true; done
  if mountpoint -q $BT; then for s in $BT/.hopebackup-snap/*; do [ -d "$s" ] && btrfs subvolume delete "$s" >/dev/null 2>&1; done; umount $BT; fi
  [ -n "${LOOP:-}" ] && losetup -d "$LOOP" 2>/dev/null || true
  rm -rf $LV
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"

export HOPEBACKUP_PASSWORD=snap-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
restore() { rm -rf "$W/out"; R restore --plugin files --instance "$1" --snapshot "$(R list | tail -1 | awk '{print $1}')" --dest "$W/out" >/dev/null; }

say "btrfs: копия из снимка"
command -v mkfs.btrfs >/dev/null || fail "нужен btrfs-progs"
truncate -s 1G "$W/btrfs.img"; mkfs.btrfs -q "$W/btrfs.img"
LOOP=$(losetup -f --show "$W/btrfs.img"); mkdir -p $BT; mount "$LOOP" $BT
mkdir -p $BT/data/sub; echo original > $BT/data/a.txt; head -c 20M /dev/urandom > $BT/data/sub/big.bin
CFG="{\"paths\":[\"$BT/data\"],\"snapshot\":\"required\",\"post_snapshot\":\"echo changed > $BT/data/a.txt; echo new > $BT/data/new.txt\"}"
R backup --instance bt --config "$CFG" | tee "$W/bt1.txt" | grep -E "снимок|архив|удалены" | sed 's/^/  /'
grep -q "снимок $BT: btrfs" "$W/bt1.txt" || fail "снимок btrfs не создан"
[ "$(cat $BT/data/a.txt)" = changed ] || fail "команда после снимка не выполнилась"
restore bt
[ "$(cat "$W/out$BT/data/a.txt")" = original ] || fail "файл в копии — не из снимка ($(cat "$W/out$BT/data/a.txt"))"
[ ! -e "$W/out$BT/data/new.txt" ] || fail "в копию попал файл, созданный после снимка"
cmp -s "$W/out$BT/data/sub/big.bin" $BT/data/sub/big.bin || fail "большой файл в копии отличается"
[ -z "$(ls -A $BT/.hopebackup-snap 2>/dev/null)" ] || fail "снимок не удалён"
[ -z "$(btrfs subvolume list $BT | grep hopebackup)" ] || fail "подтом снимка остался"
echo "ok: файлы прочитаны из снимка btrfs, снимок удалён"

say "btrfs: уборка снимка, оставшегося от прерванной копии"
mkdir -p $BT/.hopebackup-snap; btrfs subvolume snapshot -r $BT $BT/.hopebackup-snap/stale1 >/dev/null
R backup --instance bt --config "{\"paths\":[\"$BT/data\"]}" > "$W/bt2.txt"
grep -q "уборка: оставшийся снимок btrfs" "$W/bt2.txt" || fail "оставшийся снимок не убран"
[ -z "$(ls -A $BT/.hopebackup-snap 2>/dev/null)" ] || fail "каталог снимков не пуст"
echo "ok: оставшийся снимок убран"

say "LVM (имитация команд): снимок 10% тома, только чтение, удаление"
FAKE="$W/fakebin"; mkdir -p "$FAKE" "$W/snapdev" $LV
echo original > $LV/a.txt; mkdir -p $LV/d; echo x > $LV/d/x.txt
echo "100 1 253:7 / $LV rw,relatime - xfs /dev/mapper/vgt-data rw" > "$W/mountinfo"
echo 5368709120 > "$W/vgfree"
cat > "$FAKE/lvs" <<EOF
#!/bin/sh
echo "lvs \$*" >> $W/lvm.log
case "\$*" in
  *snap_percent*) echo "  swi-a-s--- 12.50" ;;
  */dev/mapper/vgt-data*) echo "  vgt|data|10737418240|-wi-ao----" ;;
  *) ;;
esac
EOF
cat > "$FAKE/vgs" <<EOF
#!/bin/sh
echo "vgs \$*" >> $W/lvm.log
cat $W/vgfree
EOF
cat > "$FAKE/lvcreate" <<EOF
#!/bin/sh
echo "lvcreate \$*" >> $W/lvm.log
n=""; prev=""; for a in "\$@"; do [ "\$prev" = "-n" ] && n="\$a"; prev="\$a"; done
mkdir -p $W/snapdev/\$n && cp -a $LV/. $W/snapdev/\$n/
EOF
cat > "$FAKE/lvremove" <<EOF
#!/bin/sh
echo "lvremove \$*" >> $W/lvm.log
for a in "\$@"; do case "\$a" in vgt/*) rm -rf $W/snapdev/\${a#vgt/} ;; esac; done
EOF
cat > "$FAKE/mount" <<EOF
#!/bin/sh
echo "mount \$*" >> $W/lvm.log
dev=""; for a in "\$@"; do case "\$a" in /dev/vgt/*) dev="\${a#/dev/vgt/}" ;; esac; last="\$a"; done
if [ -n "\$dev" ]; then exec $(command -v mount) --bind -o ro $W/snapdev/\$dev "\$last"; fi
exec $(command -v mount) "\$@"
EOF
chmod +x "$FAKE"/*
LVR() { PATH="$FAKE:$PATH" HOPEBACKUP_TESTING=1 HOPEBACKUP_TEST_MOUNTINFO="$W/mountinfo" R "$@"; }
LVR backup --instance lv --config "{\"paths\":[\"$LV\"],\"snapshot\":\"required\",\"post_snapshot\":\"echo changed > $LV/a.txt\"}" | tee "$W/lv1.txt" | grep -E "снимок|архив|удалены" | sed 's/^/  /'
grep -q "lvcreate -q -s -n hbsnap_[a-z0-9]*_0 -L 1073741824b vgt/data" "$W/lvm.log" || { cat "$W/lvm.log"; fail "снимок LVM: ожидался размер 1 ГиБ (10% от 10 ГиБ)"; }
grep -q "mount -t xfs -o ro,nouuid /dev/vgt/hbsnap_" "$W/lvm.log" || fail "снимок XFS монтируется с ro,nouuid"
grep -q "lvremove -q -f vgt/hbsnap_" "$W/lvm.log" || fail "снимок LVM не удалён"
grep -q "lv_attr,snap_percent" "$W/lvm.log" || fail "заполнение снимка не проверено"
restore lv
[ "$(cat "$W/out$LV/a.txt")" = original ] || fail "файл в копии — не из снимка LVM"
[ -z "$(grep "/run/hopebackup/snap" /proc/mounts)" ] || fail "снимок остался смонтированным"
echo "ok: снимок LVM создан (1 ГиБ), прочитан, размонтирован и удалён"

say "LVM: не хватает места в группе томов"
echo 104857600 > "$W/vgfree"; echo original > $LV/a.txt
LVR backup --instance lv --config "{\"paths\":[\"$LV\"],\"snapshot\":\"auto\"}" > "$W/lv2.txt"
grep -q "в группе томов vgt свободно 100.0 МБ" "$W/lv2.txt" || { cat "$W/lv2.txt"; fail "нет понятного сообщения о месте"; }
grep -q "копируется «на ходу»" "$W/lv2.txt" || fail "auto: копия должна идти без снимка"
if LVR backup --instance lv --config "{\"paths\":[\"$LV\"],\"snapshot\":\"required\"}" > "$W/lv3.txt" 2>&1; then fail "required без места должен завершиться ошибкой"; fi
grep -q "добавьте место в группу" "$W/lv3.txt" || { cat "$W/lv3.txt"; fail "нет совета добавить место"; }
echo "ok: без места — auto копирует «на ходу», required — ошибка с советом"

say "Том без LVM/ZFS/btrfs"
mkdir -p /var/tmp/hb-snap-plain && echo p > /var/tmp/hb-snap-plain/p.txt
R backup --instance plain --config '{"paths":["/var/tmp/hb-snap-plain"]}' > "$W/p1.txt"
grep -q "снимок невозможен" "$W/p1.txt" || fail "auto: нет предупреждения"
if R backup --instance plain --config '{"paths":["/var/tmp/hb-snap-plain"],"snapshot":"required"}' > "$W/p2.txt" 2>&1; then fail "required на обычном томе должен завершиться ошибкой"; fi
R backup --instance plain --config '{"paths":["/var/tmp/hb-snap-plain"],"snapshot":"off"}' > "$W/p3.txt"
! grep -q "снимок" "$W/p3.txt" || fail "off: снимков быть не должно"
rm -rf /var/tmp/hb-snap-plain
echo "ok: auto — предупреждение, required — ошибка, off — без снимков"

printf '\n\033[1;32mПРОВЕРКА СНИМКОВ ФАЙЛОВЫХ СИСТЕМ ПРОЙДЕНА\033[0m\n'
