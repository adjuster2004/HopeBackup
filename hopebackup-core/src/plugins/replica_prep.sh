#!/bin/sh
# Подготовка Linux к запуску на KVM (Proxmox, zVirt/oVirt, OpenStack, OpenNebula) — на работающем источнике.
# Всё безопасно для текущей платформы: изменения лишь добавляют возможности.
#   1) драйверы virtio — в initramfs (иначе ядро не найдёт корневой диск на новой платформе);
#   2) имена сетевых интерфейсов закрепляются за MAC-адресами (на цели MAC сохраняются, а имя по
#      шине PCI сменилось бы, и настройки сети не применились бы);
#   3) в fstab устройства /dev/sdX заменяются на UUID (на virtio диски называются /dev/vdX);
#   4) qemu-guest-agent — если есть в репозитории пакетов (заморозка ФС и адреса для гипервизора).
# Повторный запуск ничего не ломает. Резервные копии изменённых файлов — рядом, с суффиксом .hb-prep.
set -u
MODS="virtio virtio_pci virtio_blk virtio_scsi virtio_net virtio_ring virtio_console sd_mod"
say() { echo "$*"; }
backup() { [ -f "$1" ] && [ ! -f "$1.hb-prep" ] && cp -p "$1" "$1.hb-prep"; }

# --- 1. initramfs
if command -v dracut >/dev/null 2>&1; then
  backup /etc/dracut.conf.d/hopebackup-virtio.conf
  mkdir -p /etc/dracut.conf.d
  printf 'add_drivers+=" %s "\n' "$MODS" > /etc/dracut.conf.d/hopebackup-virtio.conf
  if dracut -f --regenerate-all >/dev/null 2>&1 || dracut -f >/dev/null 2>&1; then say "initramfs: dracut, добавлены virtio"; else say "! dracut завершился с ошибкой"; fi
elif command -v update-initramfs >/dev/null 2>&1; then
  backup /etc/initramfs-tools/modules
  mkdir -p /etc/initramfs-tools
  for m in $MODS; do grep -qx "$m" /etc/initramfs-tools/modules 2>/dev/null || echo "$m" >> /etc/initramfs-tools/modules; done
  if update-initramfs -u -k all >/dev/null 2>&1; then say "initramfs: initramfs-tools, добавлены virtio"; else say "! update-initramfs завершился с ошибкой"; fi
elif command -v mkinitcpio >/dev/null 2>&1; then
  backup /etc/mkinitcpio.conf
  grep -q '^MODULES=.*virtio_blk' /etc/mkinitcpio.conf || sed -i 's/^MODULES=(\(.*\))/MODULES=(\1 virtio virtio_pci virtio_blk virtio_scsi virtio_net)/' /etc/mkinitcpio.conf
  mkinitcpio -P >/dev/null 2>&1 && say "initramfs: mkinitcpio, добавлены virtio" || say "! mkinitcpio завершился с ошибкой"
else
  say "! не найден dracut, update-initramfs или mkinitcpio — проверьте, что virtio в ядре или initramfs"
fi

# --- 2. имена интерфейсов по MAC
n=0
for d in /sys/class/net/*; do
  name=$(basename "$d"); [ "$name" = lo ] && continue; [ -e "$d/device" ] || continue
  mac=$(cat "$d/address" 2>/dev/null); [ -n "$mac" ] || continue
  if [ -d /etc/systemd/network ] || command -v systemctl >/dev/null 2>&1; then
    mkdir -p /etc/systemd/network
    printf '# HopeBackup: имя интерфейса сохраняется на новой платформе\n[Match]\nMACAddress=%s\n\n[Link]\nName=%s\n' "$mac" "$name" > "/etc/systemd/network/10-hopebackup-$name.link"
  fi
  mkdir -p /etc/udev/rules.d
  grep -q "$mac" /etc/udev/rules.d/70-hopebackup-net.rules 2>/dev/null || \
    echo "SUBSYSTEM==\"net\", ACTION==\"add\", ATTR{address}==\"$mac\", NAME=\"$name\"" >> /etc/udev/rules.d/70-hopebackup-net.rules
  n=$((n+1))
done
say "сеть: имена $n интерфейсов закреплены за MAC"
# Имена .link применяются из initramfs тоже — обновлён выше; для initramfs-tools — ещё раз, если сеть настроена сейчас.

# --- 3. fstab: /dev/sdX → UUID
if [ -f /etc/fstab ] && grep -qE '^/dev/(sd|hd|xvd)[a-z]+[0-9]*[[:space:]]' /etc/fstab; then
  backup /etc/fstab
  tmp=$(mktemp)
  while IFS= read -r line; do
    dev=$(echo "$line" | awk '{print $1}')
    case "$dev" in
      /dev/sd*|/dev/hd*|/dev/xvd*) u=$(blkid -s UUID -o value "$dev" 2>/dev/null); if [ -n "$u" ]; then echo "$line" | sed "s|^$dev|UUID=$u|"; else echo "$line"; fi ;;
      *) echo "$line" ;;
    esac
  done < /etc/fstab > "$tmp" && cat "$tmp" > /etc/fstab; rm -f "$tmp"
  say "fstab: устройства заменены на UUID"
else
  say "fstab: уже по UUID/LABEL/LVM"
fi

# --- 4. qemu-guest-agent
if ! command -v qemu-ga >/dev/null 2>&1 && [ ! -x /usr/bin/qemu-ga ] && [ ! -x /usr/sbin/qemu-ga ]; then
  if command -v apt-get >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get install -y -q qemu-guest-agent >/dev/null 2>&1
  elif command -v dnf >/dev/null 2>&1; then dnf install -y -q qemu-guest-agent >/dev/null 2>&1
  elif command -v yum >/dev/null 2>&1; then yum install -y -q qemu-guest-agent >/dev/null 2>&1
  elif command -v zypper >/dev/null 2>&1; then zypper -q -n install qemu-guest-agent >/dev/null 2>&1
  fi
fi
if command -v qemu-ga >/dev/null 2>&1 || [ -x /usr/bin/qemu-ga ] || [ -x /usr/sbin/qemu-ga ]; then
  command -v systemctl >/dev/null 2>&1 && systemctl enable qemu-guest-agent >/dev/null 2>&1
  say "qemu-guest-agent: установлен (запустится на KVM)"
else
  say "! qemu-guest-agent не установлен (нет в репозиториях) — без него не будет заморозки ФС гипервизором"
fi
# Ядро: есть ли virtio вообще.
k=$(uname -r)
if [ -d "/lib/modules/$k" ] && ! find "/lib/modules/$k" -name 'virtio_blk*' | grep -q . && ! grep -q virtio_blk "/lib/modules/$k/modules.builtin" 2>/dev/null; then
  say "! в ядре $k нет модуля virtio_blk — нужен другой пакет ядра"
fi
echo "HB-PREP-DONE"
