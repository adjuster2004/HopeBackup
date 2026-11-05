#!/usr/bin/env bash
# Проверка «Образ ВМ целиком» (нужен root). Настоящие: libvirt/KVM (QEMU без KVM-ускорения) — работающая
# ВМ с цепочкой qcow2 и raw-диском, внешний снимок и слияние, восстановление под новым именем, с заменой,
# файлами; то же через SSH (свой sshd, пользователь с sudo). Узлы OpenNebula, SpaceVM, VMmanager — тот же
# libvirt с имитацией API платформы. Имитации: Proxmox VE (pvesh/vzdump/qm/qmrestore), oVirt (API backups и
# imageio), OpenStack (Keystone/Nova/Glance/Cinder), Nutanix Prism v3, XAPI (XCP-ng), vSphere (SOAP + NFC).
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/vm.sh       ONLY="libvirt vmware" — выборочно
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"
W=${W:-/tmp/hb-vm}
ONLY=${ONLY:-}
want() { [ "$ONLY" = "" ] || [[ " $ONLY " == *" $1 "* ]]; }
V() { virsh -q -c qemu:///system "$@"; }
cleanup() {
  for p in "$W"/*.pid; do [ -f "$p" ] && kill "$(cat "$p")" 2>/dev/null || true; done
  for d in hbweb hbweb2 hbweb3 one-7 spa-dom vmm-dom; do V destroy $d >/dev/null 2>&1 || true; V undefine $d >/dev/null 2>&1 || true; done
  userdel -r hbvirt 2>/dev/null || true; rm -f /etc/sudoers.d/hb-vm
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W/img" "$W/mock" /run/sshd
export HOPEBACKUP_PASSWORD=vm-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init >/dev/null
last() { R list --instance "$1" | tail -1 | awk '{print $1}'; }
pgrep -x libvirtd >/dev/null || { libvirtd -d; virtlogd -d 2>/dev/null || true; sleep 2; }
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 -subj /CN=localhost 2>/dev/null
domxml() { # domxml <имя> <файл qcow2> <файл raw> [uuid]
  cat <<EOF
<domain type='qemu'><name>$1</name>${4:+<uuid>$4</uuid>}<memory unit='MiB'>48</memory><vcpu>1</vcpu><os><type arch='x86_64' machine='pc'>hvm</type></os>
<devices><disk type='file' device='disk'><driver name='qemu' type='qcow2'/><source file='$2'/><target dev='vda' bus='virtio'/></disk>
${3:+<disk type='file' device='disk'><driver name='qemu' type='raw'/><source file='$3'/><target dev='vdb' bus='virtio'/></disk>}
<disk type='file' device='cdrom'><target dev='hdc' bus='ide'/><readonly/></disk>
<interface type='user'><mac address='52:54:00:12:34:56'/></interface><graphics type='vnc' port='-1' listen='127.0.0.1'/></devices></domain>
EOF
}
mkdisks() { # mkdisks <префикс> → <префикс>-base.qcow2, <префикс>.qcow2 (на базе), <префикс>-data.raw
  qemu-img create -q -f qcow2 "$1-base.qcow2" 64M; qemu-io -c "write -P 0x41 0 4M" "$1-base.qcow2" >/dev/null
  qemu-img create -q -f qcow2 -b "$1-base.qcow2" -F qcow2 "$1.qcow2"; qemu-io -c "write -P 0x42 8M 2M" "$1.qcow2" >/dev/null
  qemu-img create -q -f raw "$1-data.raw" 32M; qemu-io -f raw -c "write -P 0x43 0 1M" "$1-data.raw" >/dev/null
}

# ------------------------------------------------------------------ libvirt (настоящий)
if want libvirt; then
say "libvirt: работающая ВМ, цепочка qcow2 + raw, внешний снимок"
mkdisks "$W/img/web"
domxml hbweb "$W/img/web.qcow2" "$W/img/web-data.raw" > "$W/hbweb.xml"
V define "$W/hbweb.xml" >/dev/null; V start hbweb >/dev/null
R backup --plugin vm --instance lv --config '{"platform":"libvirt","vms":["hbweb"]}' | tee "$W/lv.txt" | tail -6
grep -q "заморозка ФС гостя недоступна" "$W/lv.txt" || fail "libvirt: без гостевого агента — снимок без заморозки"
LV=$(last lv)
R show "$LV" | grep -c "disk-vda.0.qcow2" >/dev/null && R show "$LV" | grep -c "disk-vda.1.qcow2" >/dev/null && R show "$LV" | grep -c "disk-vdb.0.raw" >/dev/null || fail "libvirt: слои дисков"
V domblklist hbweb | grep -q "$W/img/web.qcow2" || fail "libvirt: оверлей не слит обратно"
[ -z "$(ls "$W/img" | grep '\.hb')" ] || fail "libvirt: файлы оверлеев остались"
[ "$(V domstate hbweb)" = running ] || fail "libvirt: ВМ должна продолжать работать"
R restore --plugin vm --instance lv --snapshot "$LV" --params "{\"new_name\":\"hbweb2\",\"storage\":\"$W/restore\"}" | tail -3
V dumpxml hbweb2 | grep -q "$W/restore/hbweb2-vda.qcow2" || fail "libvirt: пути дисков новой ВМ"
V dumpxml hbweb2 | grep -q "52:54:00:12:34:56" && fail "libvirt: MAC новой ВМ должен быть новым"
qemu-img info "$W/restore/hbweb2-vda.qcow2" | grep -q "backing file: $W/restore/hbweb2-vda.base0.qcow2" || fail "libvirt: цепочка qcow2 не собрана"
qemu-img compare -U -q "$W/img/web.qcow2" "$W/restore/hbweb2-vda.qcow2" && qemu-img compare -U -q -f raw -F raw "$W/img/web-data.raw" "$W/restore/hbweb2-vdb.raw" || fail "libvirt: содержимое дисков"
V start hbweb2 >/dev/null && [ "$(V domstate hbweb2)" = running ] || fail "libvirt: восстановленная ВМ не запускается"
echo "ok: libvirt — снимок без остановки, слои qcow2, новая ВМ запускается, диски совпадают"
V destroy hbweb >/dev/null
R restore --plugin vm --instance lv --snapshot "$LV" --params '{"vm":"hbweb","replace":true}' > /dev/null
ls "$W/img/web.qcow2.before-restore-"* >/dev/null || fail "libvirt: прежний диск рядом"
qemu-img compare -U -q "$W/img/web.qcow2" "$W/restore/hbweb2-vda.qcow2" || fail "libvirt: замена на месте"
R restore --plugin vm --instance lv --snapshot "$LV" --params "{\"target_dir\":\"$W/export\"}" > /dev/null
[ -f "$W/export/hbweb/domain.xml" ] && [ -f "$W/export/hbweb/disk-vdb.0.raw" ] && [ -f "$W/export/hbweb/hopebackup-vm.json" ] || fail "libvirt: файлы в каталог"
echo "ok: libvirt — замена на месте (прежние диски рядом), выгрузка файлами"

say "libvirt по SSH (пользователь с sudo, вход по ключу)"
useradd -m -s /bin/bash hbvirt; usermod -p '*' hbvirt
echo "hbvirt ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/hb-vm; chmod 440 /etc/sudoers.d/hb-vm
ssh-keygen -q -t ed25519 -N '' -f "$W/sshkey"; ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
install -d -m 700 -o hbvirt -g hbvirt /home/hbvirt/.ssh; install -m 600 -o hbvirt -g hbvirt "$W/sshkey.pub" /home/hbvirt/.ssh/authorized_keys
printf 'Port 2224\nListenAddress 127.0.0.1\nPidFile %s/sshd.pid\nUsePAM yes\nPasswordAuthentication no\nPubkeyAuthentication yes\n' "$W" > "$W/sshd_config"
/usr/sbin/sshd -f "$W/sshd_config" -h "$W/hostkey"; sleep 0.5
V start hbweb >/dev/null
SCFG=$(python3 -c "import json;print(json.dumps({'platform':'libvirt','vms':['hbweb'],'ssh_host':'127.0.0.1','ssh_port':2224,'ssh_user':'hbvirt','ssh_key':open('$W/sshkey').read(),'quiesce':False}))")
R backup --plugin vm --instance lvs --config "$SCFG" | tee "$W/lvs.txt" | tail -3
grep -q "\$ \[127.0.0.1\] cat" "$W/lvs.txt" || fail "libvirt/SSH: диски по SSH"
LS=$(last lvs)
R restore --plugin vm --instance lvs --snapshot "$LS" --params "$(echo "$SCFG" | python3 -c "import json,sys;d=json.load(sys.stdin);d.update(new_name='hbweb3',storage='$W/restore-ssh');print(json.dumps(d))")" | tail -2
qemu-img compare -U -q "$W/restore/hbweb2-vda.qcow2" "$W/restore-ssh/hbweb3-vda.qcow2" || fail "libvirt/SSH: содержимое дисков"
V domstate hbweb3 >/dev/null || fail "libvirt/SSH: ВМ не определена"
echo "ok: libvirt по SSH — потоки дисков в обе стороны через sudo"
fi

# ------------------------------------------------------------------ OpenNebula, SpaceVM, VMmanager: API + libvirt узла
if want nodes; then
say "OpenNebula / Брест: XML-RPC + libvirt узла"
mkdisks "$W/img/one"
domxml one-7 "$W/img/one.qcow2" "" > "$W/one.xml"; V define "$W/one.xml" >/dev/null; V start one-7 >/dev/null
export VIRT_STATE="$W/one.json" VIRT_DIR="$W/mock"
echo '{"vms": {"7": {"name": "brest-app"}}, "images": {}, "templates": {}, "created": []}' > "$VIRT_STATE"
python3 "$HERE/mocks/fake_virt.py" opennebula 26330 "$W/cert.pem" "$W/key.pem" & echo $! > "$W/one.pid"; sleep 0.7
OCFG='{"platform":"opennebula","url":"http://127.0.0.1:26330/RPC2","user":"oneadmin","password":"Virt-Pass-1","vms":["brest-app"]}'
R backup --plugin vm --instance one --config "$OCFG" | tee "$W/one.txt" | tail -3
grep -q "домен one-7" "$W/one.txt" || fail "opennebula: домен узла"
ONE=$(last one)
R restore --plugin vm --instance one --snapshot "$ONE" --params "${OCFG%\}},\"new_name\":\"brest-app-r\",\"storage\":\"105\"}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
im=list(s['images'].values()); assert len(im)==1 and im[0]['fmt']=='514649fb', im   # qcow2, одна цепочка → один образ
t=list(s['templates'].values())[0]; assert 'DISK=[IMAGE_ID="100"]' in t and 'NETWORK_ID="3"' in t and 'MEMORY="512"' in t, t
assert s['created'][0]['name']=='brest-app-r', s['created']
EOF
echo "ok: OpenNebula — описание по XML-RPC, диски через libvirt узла, восстановление образами и шаблоном"
kill "$(cat "$W/one.pid")"; V destroy one-7 >/dev/null

for P in spacevm vmmanager; do
say "$P: API платформы + libvirt узла"
U=$(python3 -c "import uuid;print(uuid.uuid4())")
mkdisks "$W/img/$P"
domxml "${P:0:3}-dom" "$W/img/$P.qcow2" "$W/img/$P-data.raw" "$U" > "$W/$P.xml"; V define "$W/$P.xml" >/dev/null; V start "${P:0:3}-dom" >/dev/null 2>&1 || true
D=$(V domname "$U")
export VIRT_STATE="$W/$P.json"
if [ $P = spacevm ]; then echo "{\"vms\": {\"$U\": {\"verbose_name\": \"vdi-01\", \"user_power_state\": 3, \"node\": {\"id\": \"n1\"}}}}" > "$VIRT_STATE"; PORT=26331; CFG='{"platform":"spacevm","url":"https://127.0.0.1:26331","user":"","password":"jwt-1","insecure":true,"vms":["vdi-01"]}'
else echo "{\"vms\": {\"15\": {\"name\": \"vm15\", \"state\": \"active\", \"node\": {\"id\": 1}, \"uuid\": \"$U\"}}}" > "$VIRT_STATE"; PORT=26332; CFG='{"platform":"vmmanager","url":"https://127.0.0.1:26332","user":"admin@corp","password":"x","insecure":true,"vms":["vm15"]}'; fi
python3 "$HERE/mocks/fake_virt.py" $P $PORT "$W/cert.pem" "$W/key.pem" & echo $! > "$W/$P.pid"; sleep 0.7
R backup --plugin vm --instance $P --config "$CFG" | tee "$W/$P.txt" | tail -2
grep -q "домен libvirt $D" "$W/$P.txt" || fail "$P: домен найден по UUID"
S=$(last $P)
R restore --plugin vm --instance $P --snapshot "$S" --params "${CFG%\}},\"storage\":\"$W/$P-restore\"}" > /dev/null
[ -f "$W/$P-restore/domain.xml" ] && qemu-img compare -U -q -f raw -F raw "$W/img/$P-data.raw" "$W/$P-restore/disk-vdb.0.raw" || fail "$P: файлы ВМ на узле"
echo "ok: $P — ВМ и узел из API, диски через libvirt узла, восстановление файлами на узел"
kill "$(cat "$W/$P.pid")"; V destroy "$D" >/dev/null 2>&1 || true
done
fi

# ------------------------------------------------------------------ Proxmox VE (имитация CLI узла)
if want proxmox; then
say "Proxmox VE (имитация pvesh/vzdump/qmrestore)"
M="$W/pve"; mkdir -p "$M/bin"; NODE=$(hostname)
cat > "$M/bin/pvesh" <<EOF
#!/bin/sh
case "\$2" in
  /cluster/resources) echo '[{"vmid":101,"name":"web","node":"$NODE","type":"qemu","status":"running","template":0},{"vmid":102,"name":"ct","node":"$NODE","type":"lxc","status":"running","template":0},{"vmid":900,"name":"tpl","node":"$NODE","type":"qemu","template":1}]' ;;
  /cluster/nextid) echo '"105"' ;;
esac
EOF
cat > "$M/bin/vzdump" <<EOF
#!/bin/sh
echo "\$*" >> $M/vzdump.args
printf 'VMA\0'; head -c 3000000 /dev/zero | tr '\0' 'v'; echo "vm \$1"
EOF
cat > "$M/bin/qm" <<EOF
#!/bin/sh
echo "qm \$*" >> $M/qm.args
[ "\$1" = config ] && printf 'cores: 2\nmemory: 2048\nscsi0: local-lvm:vm-\$2-disk-0,size=32G\n'
exit 0
EOF
cp "$M/bin/qm" "$M/bin/pct"; sed -i 's/qm \$\*/pct $*/' "$M/bin/pct"
cat > "$M/bin/qmrestore" <<EOF
#!/bin/sh
echo "\$*" >> $M/qmrestore.args; cat > $M/restored-\$2.vma
EOF
chmod +x "$M/bin/"*
PATH="$M/bin:$PATH" R backup --plugin vm --instance pve --config '{"platform":"proxmox","vms":["web","102"]}' | tail -4; PV=$(last pve)
grep -q "101 --mode snapshot --stdout --compress 0" "$M/vzdump.args" || fail "proxmox: vzdump без сжатия в stdout"
R show "$PV" | grep -c "dump.vma" >/dev/null && R show "$PV" | grep -c "dump.tar" >/dev/null || fail "proxmox: VMA и tar контейнера"
PATH="$M/bin:$PATH" R restore --plugin vm --instance pve --snapshot "$PV" --params '{"platform":"proxmox","vm":"web","new_name":"web-copy","storage":"local-zfs"}' > /dev/null
grep -q -- "- 105 --storage local-zfs --unique 1" "$M/qmrestore.args" || { cat "$M/qmrestore.args"; fail "proxmox: qmrestore из stdin на новый vmid"; }
head -c 4 "$M/restored-105.vma" | grep -q VMA && grep -q "qm set 105 --name web-copy" "$M/qm.args" || fail "proxmox: данные и имя"
echo "ok: Proxmox — vzdump --stdout в репозиторий, qmrestore - на новый vmid с --unique, имя"
fi

# ------------------------------------------------------------------ API-платформы (имитации)
mkimg() { head -c "$2" /dev/urandom > "$1"; }
apiplat() { # apiplat <платформа> <порт>
  export VIRT_STATE="$W/$1.json" VIRT_DIR="$W/mock"
  python3 "$HERE/mocks/fake_virt.py" "$1" "$2" "$W/cert.pem" "$W/key.pem" > "$W/$1.log" 2>&1 & echo $! > "$W/$1.pid"; sleep 0.7
}
ev() { python3 -c "import json;print(json.load(open('$VIRT_STATE'))['events'])"; }

if want ovirt; then
say "oVirt / zVirt (имитация API backups и imageio)"
mkimg "$W/mock/ov1" 3000000; mkimg "$W/mock/ov2" 1000000
cat > "$W/ovirt.json" <<EOF
{"vms": {"vm-1": {"name": "app01", "status": "up", "cluster": {"id": "c1"}, "memory": 4294967296, "disks": ["dk-1", "dk-2"], "nics": [{"name": "nic1", "interface": "virtio", "vnic_profile": {"id": "vp-1"}}]}},
 "disks": {"dk-1": {"name": "app01_os", "provisioned_size": 3000000, "format": "cow", "sparse": true, "file": "$W/mock/ov1", "storage_domains": {"storage_domain": [{"id": "sd-1"}]}},
           "dk-2": {"name": "app01_data", "provisioned_size": 1000000, "format": "raw", "sparse": false, "file": "$W/mock/ov2", "storage_domains": {"storage_domain": [{"id": "sd-1"}]}}}}
EOF
apiplat ovirt 26333
CFG='{"platform":"ovirt","url":"https://127.0.0.1:26333/ovirt-engine/api","user":"admin@internal","password":"Virt-Pass-1","insecure":true,"vms":["app01"]}'
R backup --plugin vm --instance ov --config "$CFG" | tail -3; OV=$(last ov)
python3 -c "import json;e=json.load(open('$VIRT_STATE'))['events'];assert e==['backup start','transfer download done','transfer download done','backup finalize'], e"
R restore --plugin vm --instance ov --snapshot "$OV" --params "${CFG%\}},\"new_name\":\"app01-r\",\"storage\":\"data2\",\"network\":\"dmz\",\"start\":true}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
v=[x for x in s['vms'].values() if x['name']=='app01-r'][0]
assert v['status']=='up' and len(v['disks'])==2 and v['nics'][0]['vnic_profile']['id']=='vp-2', v
a=[s['disks'][d] for d in v['disks']]
assert open(a[0]['file'],'rb').read()==open('$W/mock/ov1','rb').read() and open(a[1]['file'],'rb').read()==open('$W/mock/ov2','rb').read()
assert a[0]['storage_domains']['storage_domain'][0]['name']=='data2'
EOF
echo "ok: oVirt — копия через API backups/imageio без остановки, новая ВМ с дисками и профилем сети"
kill "$(cat "$W/ovirt.pid")"
fi

if want openstack; then
say "OpenStack (имитация Keystone/Nova/Glance/Cinder)"
mkimg "$W/mock/os-root" 2000000; mkimg "$W/mock/os-vol" 1500000
cat > "$W/openstack.json" <<EOF
{"vms": {"srv-1": {"name": "web", "status": "ACTIVE", "image": {"id": "base-img"}, "root_disk": "d1", "flavor": {"original_name": "m1.small"}, "addresses": {"net-1": []}, "os-extended-volumes:volumes_attached": []},
         "srv-2": {"name": "db", "status": "ACTIVE", "image": "", "flavor": {"original_name": "m1.large"}, "addresses": {"net-1": []}, "os-extended-volumes:volumes_attached": [{"id": "vol-1"}]}},
 "disks": {"d1": {"file": "$W/mock/os-root"}}, "images": {}, "volumes": {"vol-1": {"size": 10, "bootable": "true", "status": "in-use", "file": "$W/mock/os-vol", "attachments": [{"device": "/dev/vda"}]}}}
EOF
apiplat openstack 26334
CFG='{"platform":"openstack","url":"https://127.0.0.1:26334","user":"admin","password":"Virt-Pass-1","project":"admin","insecure":true,"vms":["web","db"]}'
R backup --plugin vm --instance ost --config "$CFG" | tail -3; OS=$(last ost)
python3 -c "import json;s=json.load(open('$VIRT_STATE'));assert not s['images'], s['images']"
R restore --plugin vm --instance ost --snapshot "$OS" --params "${CFG%\}},\"vm\":\"web\",\"new_name\":\"web-r\"}" > /dev/null
R restore --plugin vm --instance ost --snapshot "$OS" --params "${CFG%\}},\"vm\":\"db\",\"new_name\":\"db-r\",\"flavor\":\"m1.large\"}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
w=[v for v in s['vms'].values() if v['name']=='web-r'][0]['request']; d=[v for v in s['vms'].values() if v['name']=='db-r'][0]['request']
assert w['flavorRef']=='f1' and open(s['images'][w['imageRef']]['file'],'rb').read()==open('$W/mock/os-root','rb').read(), w
vid=d['block_device_mapping_v2'][0]['uuid']; assert d['flavorRef']=='f2' and open(s['volumes'][vid]['file'],'rb').read()==open('$W/mock/os-vol','rb').read(), d
assert d['block_device_mapping_v2'][0]['boot_index']==0
EOF
echo "ok: OpenStack — снимок Nova и выгрузка тома Cinder, временные образы удалены, ВМ из образа и с тома"
kill "$(cat "$W/openstack.pid")"
fi

if want nutanix; then
say "Nutanix AHV (имитация Prism v3)"
mkimg "$W/mock/nx1" 2500000
cat > "$W/nutanix.json" <<EOF
{"vms": {"vm-a": {"spec": {"name": "sql01", "cluster_reference": {"kind": "cluster", "uuid": "cl-1"}, "resources": {"num_sockets": 2, "num_vcpus_per_socket": 1, "memory_size_mib": 4096,
  "disk_list": [{"uuid": "vd-1", "device_properties": {"device_type": "DISK", "disk_address": {"adapter_type": "SCSI", "device_index": 0}}}, {"uuid": "cd-1", "device_properties": {"device_type": "CDROM"}}],
  "nic_list": [{"subnet_reference": {"kind": "subnet", "uuid": "sn-1"}}]}}}},
 "disks": {"vd-1": {"file": "$W/mock/nx1"}}, "images": {}}
EOF
apiplat nutanix 26335
CFG='{"platform":"nutanix","url":"https://127.0.0.1:26335","user":"admin","password":"Virt-Pass-1","insecure":true,"vms":["sql01"]}'
R backup --plugin vm --instance nx --config "$CFG" | tail -2; NX=$(last nx)
R restore --plugin vm --instance nx --snapshot "$NX" --params "${CFG%\}},\"new_name\":\"sql01-r\"}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
assert s['events'][:2]==['image from vm_disk','delete image'], s['events']
v=[x for x in s['vms'].values() if x['spec']['name']=='sql01-r'][0]['spec']
img=v['resources']['disk_list'][0]['data_source_reference']['uuid']
assert len(v['resources']['disk_list'])==1 and open(s['images'][img]['file'],'rb').read()==open('$W/mock/nx1','rb').read() and v['resources']['memory_size_mib']==4096, v
EOF
echo "ok: Nutanix — образ из диска ВМ без остановки, ВМ с диском-клоном образа"
kill "$(cat "$W/nutanix.pid")"
fi

if want xen; then
say "XCP-ng / XAPI (имитация)"
mkimg "$W/mock/xva" 2200000
echo "{\"vms\": {\"OpaqueRef:vm1\": {\"name_label\": \"db01\", \"uuid\": \"u-1\", \"power_state\": \"Running\", \"file\": \"$W/mock/xva\"}}, \"snaps\": {}, \"tasks\": {}}" > "$W/xen.json"
apiplat xen 26336
CFG='{"platform":"xen","url":"https://127.0.0.1:26336","user":"root","password":"Virt-Pass-1","insecure":true,"vms":["db01"]}'
R backup --plugin vm --instance xe --config "$CFG" | tee "$W/xe.txt" | tail -2; XE=$(last xe)
grep -q "снимок с заморозкой недоступен" "$W/xe.txt" || fail "xen: без заморозки"
python3 -c "import json;s=json.load(open('$VIRT_STATE'));assert s['events']==['snapshot','destroy snapshot','destroy vdi'] and not s['snaps'], s"
R restore --plugin vm --instance xe --snapshot "$XE" --params "${CFG%\}},\"new_name\":\"db01-r\",\"storage\":\"fast\",\"start\":true}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
v=[x for x in s['vms'].values() if x['name_label']=='db01-r'][0]
assert v['sr']=='OpaqueRef:sr-fast' and v['power_state']=='Running' and open(v['file'],'rb').read()==open('$W/mock/xva','rb').read(), v
EOF
echo "ok: XAPI — снимок → XVA без сжатия → удаление снимка и дисков; импорт в SR, имя, запуск"
kill "$(cat "$W/xen.pid")"
fi

if want vmware; then
say "VMware vSphere (имитация SOAP и NFC)"
mkimg "$W/mock/vd-a" 1800000; mkimg "$W/mock/vd-b" 700000
echo "{\"vms\": {\"vm-10\": {\"name\": \"erp\", \"state\": \"poweredOn\", \"disks\": [\"vd-a\", \"vd-b\"], \"cid\": 1}}, \"disks\": {\"vd-a\": {\"file\": \"$W/mock/vd-a\"}, \"vd-b\": {\"file\": \"$W/mock/vd-b\"}}, \"snaps\": {}, \"tasks\": {}, \"leases\": {}}" > "$W/vsphere.json"
apiplat vsphere 26337
CFG='{"platform":"vmware","url":"https://127.0.0.1:26337","user":"backup@vsphere.local","password":"Virt-Pass-1","insecure":true,"vms":["erp"]}'
R backup --plugin vm --instance vs --config "$CFG" | tee "$W/vs1.txt" | tail -3
grep -q "CBT включён" "$W/vs1.txt" && grep -q "без заморозки" "$W/vs1.txt" && grep -q "CBT: изменено 128.0 КБ с создания дисков" "$W/vs1.txt" || fail "vmware: CBT и снимок без заморозки"
R backup --plugin vm --instance vs --config "$CFG" | tee "$W/vs2.txt" | tail -2; VS=$(last vs)
grep -q "CBT: изменено 8.0 КБ с прошлой копии" "$W/vs2.txt" || fail "vmware: изменения CBT с прошлой копии"
python3 -c "import json;s=json.load(open('$VIRT_STATE'));e=s['events'];assert not s['snaps'] and e.count('remove snapshot')==2 and e.count('HttpNfcLeaseComplete')==2, e"
R restore --plugin vm --instance vs --snapshot "$VS" --params "${CFG%\}},\"new_name\":\"erp-r\",\"storage\":\"ds2\",\"network\":\"DMZ\",\"node\":\"esxi01.corp\",\"start\":true}" > /dev/null
python3 - <<EOF
import json; s=json.load(open('$VIRT_STATE'))
v=[x for x in s['vms'].values() if x['name']=='erp-r'][0]
assert v['state']=='poweredOn' and v['datastore']=='datastore-2' and v['netmap']==[['VM Network','network-2']], v
up=v['uploaded']; assert open(up[0],'rb').read()==open('$W/mock/vd-a','rb').read() and open(up[1],'rb').read()==open('$W/mock/vd-b','rb').read()
EOF
echo "ok: vSphere — снимок, экспорт NFC, OVF, учёт CBT, удаление снимка; ImportVApp в datastore и сеть, включение"
kill "$(cat "$W/vsphere.pid")"
fi

printf '\n\033[1;32mПРОВЕРКА ОБРАЗОВ ВМ ПРОЙДЕНА\033[0m\n'
