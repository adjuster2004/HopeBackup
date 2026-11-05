# Имитация инструментов узла Proxmox VE (qm, pvesm, pvesh) для проверок «Переезда»: вызовы — в $M/calls,
# тома настоящие: хранилище local — файлы raw, local-lvm — толстый LVM (устройства loop, забитые мусором).
# Снимки недоступны (как у толстого LVM). Подключается: . tests/mocks/pve_mock.sh; pve_mock <каталог>.
pve_mock() {
M=$1; mkdir -p "$M/bin"
cat > "$M/bin/pvesh" <<EOF
#!/bin/sh
echo "pvesh \$*" >> $M/calls
[ "\$2" = /cluster/nextid ] && echo '"120"'
EOF
cat > "$M/bin/qm" <<EOF
#!/bin/sh
echo "qm \$*" >> $M/calls
case "\$1" in
  status) [ -f $M/vm-\$2 ] || { echo "Configuration file 'nodes/pve/qemu-server/\$2.conf' does not exist" >&2; exit 2; }; echo "status: stopped" ;;
  create) touch $M/vm-\$2; echo "virtio=BC:24:11:00:00:01,bridge=vmbr1" > $M/net0-\$2 ;;
  resize) exit 0 ;;
  config) echo "name: web01"; echo "net0: \$(cat $M/net0-\$2)"; echo "scsi0: local-lvm:vm-\$2-disk-0" ;;
  set) if [ "\$3" = --net0 ]; then echo "\$4" > $M/net0-\$2; fi ;;
  start) touch $M/running-\$2 ;;
  stop) rm -f $M/running-\$2 ;;
  snapshot) echo "snapshot feature is not available" >&2; exit 255 ;;
esac
EOF
# Хранилища: local — каталог (файлы raw), local-lvm — толстый LVM (устройства loop с мусором).
cat > "$M/bin/pvesm" <<EOF
#!/bin/sh
echo "pvesm \$*" >> $M/calls
case "\$1" in
  status) echo "Name Type Status Total Used Available %"; case "\$3" in local) echo "local dir active 1 1 1 1%";; local-lvm) echo "local-lvm lvm active 1 1 1 1%";; esac ;;
  list) ls $M/vols 2>/dev/null | sed "s/^/\$2:/" ;;
  alloc)
    mkdir -p $M/vols
    if [ "\$2" = local ]; then
      mkdir -p $M/images/\$3; truncate -s "\$5"K $M/images/\$3/\$4.raw; echo "$M/images/\$3/\$4.raw" > $M/vols/\$4
      echo "successfully created 'local:\$3/\$4.raw'"
    else
      head -c "\$5"K /dev/urandom > $M/lv-\$4; losetup -f --show $M/lv-\$4 > $M/vols/\$4
      echo "successfully created 'local-lvm:\$4'"
    fi ;;
  path) n=\$(echo "\$2" | sed 's|.*[:/]||; s|\.raw\$||'); cat $M/vols/\$n ;;
esac
EOF
chmod +x "$M/bin/"*
}
