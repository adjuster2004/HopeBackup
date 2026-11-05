#!/usr/bin/env bash
# Проверка VPN-коннекторов (нужны root, iproute2, iptables, openvpn, openssl, wireguard-tools, jq, curl;
# для WireGuard без модуля ядра — boringtun-cli или wireguard-go):
#   «площадка» в отдельном сетевом пространстве: OpenVPN-сервер, WireGuard-сервер и узел хранения,
#   доступ к узлу снаружи закрыт — только через туннель. Сервер управления и агент на хосте:
#   площадка через OpenVPN и через WireGuard → проверка, создание репозитория, копия файлов агентом.
#   Профиль OpenVPN нарочно содержит опасные директивы и сервер раздаёт redirect-gateway — маршрутизация
#   хоста не должна измениться.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/vpn.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-vpn}
NS=hbsite
SRV_PORT=17643; NODE_PORT=18444

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
inns() { ip netns exec $NS "$@"; }

cleanup() {
  for f in agent server; do [ -f "$W/$f.pid" ] && kill "$(cat "$W/$f.pid")" 2>/dev/null || true; done
  sleep 1
  [ -f "$W/agent.pid" ] && HOPEBACKUP_DATA="$W/agent" "$BIN" vpn-cleanup >/dev/null 2>&1 || true
  ip netns pids $NS 2>/dev/null | xargs -r kill 2>/dev/null || true
  sleep 0.5
  ip netns del $NS 2>/dev/null || true
  ip link del veth-hb 2>/dev/null || true
  for i in $(ls /sys/class/net | grep '^hb' || true); do ip link del "$i" 2>/dev/null || true; done
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT

say "Сетевое пространство площадки"
cleanup
rm -rf "$W"; mkdir -p "$W"
ip netns add $NS
ip link add veth-hb type veth peer name veth-site
ip link set veth-site netns $NS
ip addr add 192.168.77.1/24 dev veth-hb; ip link set veth-hb up
inns ip addr add 192.168.77.2/24 dev veth-site; inns ip link set veth-site up; inns ip link set lo up
ROUTES_BEFORE=$(ip route show | sort)

say "PKI и сервер OpenVPN"
cd "$W"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out ca.crt -days 2 -subj /CN=hb-test-ca 2>/dev/null
mk() { openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout $1.key -out $1.csr -subj /CN=$1 2>/dev/null
  printf "basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyAgreement\nextendedKeyUsage=%s\n" "$2" > $1.ext
  openssl x509 -req -in $1.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out $1.crt -days 2 -extfile $1.ext 2>/dev/null; }
mk server serverAuth; mk client clientAuth
cat > server.conf <<EOF
dev tun
proto udp
port 1194
local 192.168.77.2
server 10.8.0.0 255.255.255.0
topology subnet
ca $W/ca.crt
cert $W/server.crt
key $W/server.key
dh none
keepalive 5 30
data-ciphers AES-256-GCM:AES-128-GCM
push "redirect-gateway def1"
push "dhcp-option DNS 10.8.0.1"
push "route 10.99.0.0 255.255.0.0"
verb 3
EOF
inns openvpn --config server.conf --daemon --log "$W/ovpn-server.log" --writepid "$W/ovpn-server.pid"
PROFILE=$(cat <<EOF
client
dev tun
proto udp
remote 192.168.77.2 1194
nobind
remote-cert-tls server
script-security 2
up /bin/false
redirect-gateway def1
<ca>
$(cat ca.crt)
</ca>
<cert>
$(cat client.crt)
</cert>
<key>
$(cat client.key)
</key>
EOF
)

say "Сервер WireGuard (userspace, если нет модуля ядра)"
read SK_SRV PK_SRV < <("$BIN" wg-genkey)
read SK_CLI PK_CLI < <("$BIN" wg-genkey)
if inns ip link add wgs0 type wireguard 2>/dev/null; then echo "WireGuard: модуль ядра"; else
  WGU=$(command -v wireguard-go || command -v boringtun-cli || true); [ -n "$WGU" ] || fail "нет WireGuard: ни модуля ядра, ни userspace"
  echo "WireGuard: $WGU"
  inns env WG_SUDO=1 "$WGU" $([ "$(basename $WGU)" = boringtun-cli ] && echo "-f --disable-drop-privileges" || echo "-f") wgs0 > "$W/wg-server.log" 2>&1 &
  for i in $(seq 1 50); do inns test -e /sys/class/net/wgs0 && break; sleep 0.1; done
fi
printf '[Interface]\nPrivateKey = %s\nListenPort = 51820\n\n[Peer]\nPublicKey = %s\nAllowedIPs = 10.66.0.2/32\n' "$SK_SRV" "$PK_CLI" > wgs.conf
inns wg setconf wgs0 wgs.conf
inns ip addr add 10.66.0.1/24 dev wgs0; inns ip link set wgs0 up

say "Узел хранения на площадке (снаружи закрыт)"
inns "$BIN" node init --data "$W/node" --listen 0.0.0.0:$NODE_PORT > node-init.txt
NODE_FP=$(grep 'Отпечаток' node-init.txt | awk '{print $NF}')
NODE_ADMIN=$(grep 'администратора' node-init.txt | awk '{print $NF}')
NODE_APPEND=$(grep 'append' node-init.txt | awk '{print $NF}')
inns iptables -A INPUT -i veth-site -p tcp --dport $NODE_PORT -j DROP
inns env HOPEBACKUP_LOG=hopebackup=info "$BIN" node run --data "$W/node" > "$W/node.log" 2>&1 &
sleep 1
curl -sk -m 3 https://192.168.77.2:$NODE_PORT/v1/health && fail "узел доступен напрямую — проверка бессмысленна" || echo "напрямую узел недоступен — как и задумано"

say "Сервер управления и агент"
HOPEBACKUP_DATA="$W/srv" "$BIN" server --listen 127.0.0.1:$SRV_PORT > "$W/server.log" 2>&1 & echo $! > "$W/server.pid"
for i in $(seq 1 50); do [ -f "$W/srv/server/initial_admin_password.txt" ] && curl -sk https://127.0.0.1:$SRV_PORT/api/health >/dev/null && break; sleep 0.2; done
PW=$(cat "$W/srv/server/initial_admin_password.txt")
API=https://127.0.0.1:$SRV_PORT/api
TOKEN=$(curl -sk $API/auth/login -H 'content-type: application/json' -d "{\"user\":\"Administrator\",\"password\":\"$PW\"}" | jq -r .token)
api() { local m=$1 p=$2; shift 2; curl -sk -X "$m" "$API$p" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
wait_job() {
  local id=$1 st
  for i in $(seq 1 300); do
    st=$(api GET /jobs/$id | jq -r .status)
    [ "$st" = ok ] && return 0
    [ "$st" = failed ] && { api GET /jobs/$id | jq -r '.log[].m' | tail -20; fail "задание $id: $(api GET /jobs/$id | jq -r .error)"; }
    sleep 1
  done
  fail "задание $id не завершилось"
}
SRV_FP=$(api GET /status | jq -r .fingerprint)
ET=$(api POST /agents/enroll-token -d '{"note":"vpn"}' | jq -r .token)
HOPEBACKUP_DATA="$W/agent" "$BIN" agent enroll --server https://127.0.0.1:$SRV_PORT --token "$ET" --fingerprint "$SRV_FP" --name vpn-host >/dev/null
HOPEBACKUP_DATA="$W/agent" HOPEBACKUP_LOG=hopebackup=info "$BIN" agent run > "$W/agent.log" 2>&1 & echo $! > "$W/agent.pid"
sleep 2
AGENT=$(api GET /agents | jq -r '.[0].id')

mkdir -p "$W/data"; for i in $(seq 1 50); do head -c 200000 /dev/urandom > "$W/data/f$i.bin"; done

check_site() { # $1 имя, $2 тип, $3 параметры коннектора (JSON), $4 URL узла
  say "Коннектор $2"
  local CID SID R JOB INST
  CID=$(api POST /connectors -d "{\"name\":\"$1\",\"kind\":\"$2\",\"params\":$3}" | jq -r .id)
  [ "$CID" != null ] || fail "создание коннектора $2"
  SID=$(api POST /sites -d "{\"name\":\"$1\",\"url\":\"$4\",\"fingerprint\":\"$NODE_FP\",\"repo\":\"$2\",\"token\":\"$NODE_APPEND\",\"prune_token\":\"$NODE_ADMIN\",\"password\":\"vpn-test-password\",\"connector_ids\":[\"$CID\"]}" | jq -r .id)
  R=$(api POST /connectors/$CID/test); echo "$R" | jq -c '{ok,iface,address,routes,node,implementation,last_handshake_seconds}'
  [ "$(echo "$R" | jq -r .node.ok)" = true ] || fail "$2: узел через туннель не отвечает: $R"
  JOB=$(api POST /sites/$SID/init | jq -r .job_id); wait_job "$JOB"
  INST=$(api POST /instances -d "{\"name\":\"files-$2\",\"catalog_id\":\"files\",\"agent_id\":\"$AGENT\",\"site_id\":\"$SID\",\"config\":{\"paths\":[\"$W/data\"]}}" | jq -r .id)
  JOB=$(api POST /instances/$INST/backup -d '{}' | jq -r .job_id); wait_job "$JOB"
  api GET /jobs/$JOB | jq -r '.log[].m' | grep -E "коннектор|готово" || true
  R=$(api POST /sites/$SID/test); [ "$(echo "$R" | jq -r .repo_exists)" = true ] || fail "$2: репозиторий не виден"
  echo "$2: снапшотов на площадке $(echo "$R" | jq -r .stats.snapshots)"
  # Канал переподключился сам и маршруты пропали — при следующем использовании они возвращаются.
  local IF; IF=$(api GET /connectors | jq -r --arg id "$CID" '.[] | select(.id==$id) | .status.iface // empty')
  if [ -n "$IF" ]; then
    local BEFORE; BEFORE=$(ip -4 route show dev "$IF" | grep -v 'proto kernel' | awk '{print $1}' | sort)
    for r in $BEFORE; do ip route del "$r" dev "$IF"; done
    R=$(api POST /sites/$SID/test); [ "$(echo "$R" | jq -r .repo_exists)" = true ] || fail "$2: после потери маршрутов площадка недоступна: $R"
    [ "$(ip -4 route show dev "$IF" | grep -v 'proto kernel' | awk '{print $1}' | sort)" = "$BEFORE" ] || fail "$2: маршруты не восстановлены"
    echo "$2: потерянные маршруты ($(echo $BEFORE | tr '\n' ' ')) восстановлены"
  fi
  # Сторож сервера сам возвращает маршруты поднятого канала — без обращений к площадке.
  if [ -n "$IF" ] && [ "$2" = wireguard ]; then
    for r in $BEFORE; do ip route del "$r" dev "$IF"; done
    for i in $(seq 1 45); do [ "$(ip -4 route show dev "$IF" | grep -v 'proto kernel' | awk '{print $1}' | sort)" = "$BEFORE" ] && break; sleep 1; done
    [ "$(ip -4 route show dev "$IF" | grep -v 'proto kernel' | awk '{print $1}' | sort)" = "$BEFORE" ] || fail "$2: сторож не вернул маршруты"
    echo "$2: сторож канала вернул маршруты сам"
  fi
}

check_site "ЦОД-VPN · OpenVPN" openvpn "$(jq -n --arg p "$PROFILE" '{profile:$p}')" "https://10.8.0.1:$NODE_PORT"
check_site "ЦОД-VPN · WireGuard" wireguard "$(jq -n --arg sk "$SK_CLI" --arg pk "$PK_SRV" '{private_key:$sk,address:"10.66.0.2/32",peer_public_key:$pk,endpoint:"192.168.77.2:51820",keepalive:5}')" "https://10.66.0.1:$NODE_PORT"

say "Маршрутизация хоста не изменилась (кроме маршрутов до узла)"
ROUTES_AFTER=$(ip route show | grep -v ' dev hb' | sort)
[ "$ROUTES_BEFORE" = "$ROUTES_AFTER" ] || { diff <(echo "$ROUTES_BEFORE") <(echo "$ROUTES_AFTER") || true; fail "маршруты хоста изменились"; }
ip route show | grep ' dev hb' || true
ip route show | grep -q '10.99.0.0' && fail "принят маршрут, присланный сервером OpenVPN"
ip route show | grep -q '^0.0.0.0/1\|^128.0.0.0/1' && fail "принят redirect-gateway"
echo "ok: только /32 до узла через туннели"

say "Общий канал: сервер и агент на одном хосте"
grep -h "уже поднят другим процессом" "$W/agent.log" "$W/server.log" | head -3 || true
N=$(ls /sys/class/net | grep -c '^hb' || true)
[ "$N" -eq 2 ] || fail "ожидалось 2 интерфейса hb* (OpenVPN и WireGuard), есть $N"

printf '\n\033[1;32mПРОВЕРКА VPN-КОННЕКТОРОВ ПРОЙДЕНА\033[0m\n'
