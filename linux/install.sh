#!/usr/bin/env bash
# =====================================================================
# install.sh — установка HopeBackup (резервное копирование и восстановление).
#
# Один бинарник, три роли:
#   MODE=server — сервер управления: панель https://сервер:4443/, API, расписания, восстановление по SSH;
#   MODE=node   — узел хранения на площадке (репозитории копий), HTTPS :8443;
#   MODE=agent  — агент на хосте с продуктом (PostgreSQL, qdatabase, nginx…).
#
# Запуск (от root):
#   sudo BIN_SRC=/tmp/hopebackup MODE=server bash install.sh
#   sudo BIN_SRC=/tmp/hopebackup MODE=node bash install.sh
#   sudo BIN_SRC=/tmp/hopebackup MODE=agent SERVER=https://10.0.0.5:4443 TOKEN=… FINGERPRINT=… bash install.sh
#
# Переменные:
#   BIN_SRC      — откуда взять бинарник                  (/tmp/hopebackup)
#   DIST_SRC     — каталог комплекта сборок hopebackup-<арх>-<libc>.gz (x86_64/aarch64, gnu/musl)  (<каталог BIN_SRC>/dist):
#                  если BIN_SRC не запускается (ARM, Alpine, glibc старше 2.17), берётся подходящая сборка отсюда;
#                  серверу управления комплект нужен для описи и установки агентов на машины других архитектур
#   MODE         — server | node | agent                  (server)
#   LISTEN       — адрес сервера или узла                 (0.0.0.0:4443 / 0.0.0.0:8443)
#   SERVER, TOKEN, FINGERPRINT — подключение агента (команду с ними показывает панель: Агенты → Подключить)
#   VPN          — пакеты для VPN-коннекторов через запятую: openvpn,wireguard,l2tp,ipsec (для server и agent)
# =====================================================================
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "Запустите от root (sudo)."; exit 1; }

BIN_SRC="${BIN_SRC:-/tmp/hopebackup}"
MODE="${MODE:-server}"
INSTALL_DIR=/opt/hopebackup
USER_LISTEN="${LISTEN:-}"
case "$MODE" in
  server) SVC=hopebackup-server; DATA_DIR=/var/lib/hopebackup; LISTEN="${LISTEN:-0.0.0.0:4443}"
          # Обновление с версий, где панель была на 7543: старый порт остаётся вторым адресом, пока агенты
          # не перейдут на 4443 (сервер сообщает им новый порт, они переключаются сами).
          if [ -z "$USER_LISTEN" ] && grep -qs ':7543' /etc/systemd/system/hopebackup-server.service; then
            LISTEN="0.0.0.0:4443,0.0.0.0:7543"
            echo "-- панель переезжает на порт 4443; 7543 оставлен для агентов на время перехода (уберите позже: LISTEN=0.0.0.0:4443)"
          fi ;;
  node)   SVC=hopebackup-node;   DATA_DIR=/var/lib/hopebackup-node; LISTEN="${LISTEN:-0.0.0.0:8443}" ;;
  agent)  SVC=hopebackup-agent;  DATA_DIR=/var/lib/hopebackup; LISTEN="" ;;
  *) echo "MODE: server | node | agent"; exit 1 ;;
esac
PORTS="$(echo "$LISTEN" | tr ',' '\n' | sed 's/.*://' | tr '\n' ' ')"
PORT="${LISTEN%%,*}"; PORT="${PORT##*:}"

echo "== HopeBackup: установка роли «$MODE» =="
echo "   бинарник: $BIN_SRC → $INSTALL_DIR/hopebackup (ссылка /usr/local/bin/hopebackup)"
echo "   данные:   $DATA_DIR"
[ -n "$LISTEN" ] && echo "   адрес:    $LISTEN"

DIST_SRC="${DIST_SRC:-$(dirname "$BIN_SRC")/dist}"
ARCH="$(uname -m)"; case "$ARCH" in arm64) ARCH=aarch64 ;; amd64) ARCH=x86_64 ;; esac
runs() { [ -f "$1" ] && "$1" --version >/dev/null 2>&1; }
if ! runs "$BIN_SRC"; then
  # Другая архитектура, Alpine (musl) или glibc старше 2.17: подходящая сборка из комплекта.
  FOUND=""
  for t in "$ARCH-gnu" "$ARCH-musl"; do
    for f in "$DIST_SRC/hopebackup-$t" "$DIST_SRC/hopebackup-$t.gz"; do
      [ -f "$f" ] || continue
      C="/tmp/hopebackup-$t.$$"
      case "$f" in *.gz) gunzip -c "$f" > "$C" ;; *) cp "$f" "$C" ;; esac
      chmod 755 "$C"
      if runs "$C"; then FOUND="$C"; echo "-- сборка $t (основная не подходит этой системе)"; break 2; fi
      rm -f "$C"
    done
  done
  if [ -z "$FOUND" ]; then
    [ -f "$BIN_SRC" ] || echo "❌ Не найден бинарник: $BIN_SRC (задайте BIN_SRC=...)"
    echo "❌ Бинарник не запускается на этой системе ($ARCH), а в комплекте $DIST_SRC нет подходящей сборки (hopebackup-$ARCH-gnu.gz или hopebackup-$ARCH-musl.gz)"
    exit 1
  fi
  BIN_SRC="$FOUND"
fi

# 1. Каталоги и бинарник.
mkdir -p "$INSTALL_DIR" "$DATA_DIR"
chmod 0755 "$DATA_DIR"   # агенту: спул WAL внутри принадлежит пользователю СУБД, ему нужен проход по пути
if [ -f "$INSTALL_DIR/hopebackup" ]; then
  cp -a "$INSTALL_DIR/hopebackup" "$INSTALL_DIR/hopebackup.bak.$(date +%s)"
  ls -1t "$INSTALL_DIR"/hopebackup.bak.* 2>/dev/null | tail -n +4 | xargs -r rm -f   # хранить 3 копии
fi
install -m 0755 "$BIN_SRC" "$INSTALL_DIR/hopebackup"
ln -sf "$INSTALL_DIR/hopebackup" /usr/local/bin/hopebackup
# Серверу управления — комплект сборок для машин других архитектур (ARM) и Alpine.
if [ "$MODE" = server ] && ls "$DIST_SRC"/hopebackup-*-* >/dev/null 2>&1; then
  mkdir -p "$INSTALL_DIR/dist"
  cp -f "$DIST_SRC"/hopebackup-*-* "$INSTALL_DIR/dist/"
  echo "-- комплект сборок для машин: $(ls "$INSTALL_DIR/dist" | sed 's/hopebackup-//; s/\.gz$//' | tr '\n' ' ')"
fi
echo "-- версия: $("$INSTALL_DIR/hopebackup" --version 2>/dev/null || echo '?')"

# Нужные программы роли.
command -v ssh >/dev/null 2>&1 || echo "⚠️  Нет клиента OpenSSH (ssh): нужен для коннектора «SSH-туннель» и восстановления по SSH."

# Пакеты VPN-коннекторов (по запросу): apt (Debian/Ubuntu/Astra) или dnf/yum (RHEL-семейство).
if [ -n "${VPN:-}" ] && [ "$MODE" != node ]; then
  PK=""
  for v in ${VPN//,/ }; do
    case "$v" in
      openvpn)   PK="$PK openvpn" ;;
      wireguard) PK="$PK wireguard-tools"; modprobe wireguard 2>/dev/null || PK="$PK wireguard-go" ;;
      l2tp)      PK="$PK strongswan-swanctl charon-systemd xl2tpd ppp" ;;
      ipsec)     PK="$PK strongswan-swanctl charon-systemd" ;;
      *) echo "⚠️  VPN: неизвестный вид «$v» (openvpn, wireguard, l2tp, ipsec)" ;;
    esac
  done
  if [ -n "$PK" ]; then
    echo "-- пакеты VPN:$PK"
    if command -v apt-get >/dev/null 2>&1; then
      DEBIAN_FRONTEND=noninteractive apt-get install -y -q $(echo $PK | tr ' ' '\n' | sort -u) || echo "⚠️  не все пакеты VPN установились"
      # Свой экземпляр xl2tpd запускает HopeBackup: системная служба заняла бы порт 1701.
      case ",$VPN," in *,l2tp,*) systemctl disable --now xl2tpd >/dev/null 2>&1 || true ;; esac
      case ",$VPN," in *,l2tp,*|*,ipsec,*) systemctl enable --now strongswan >/dev/null 2>&1 || true ;; esac
    elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; then
      PM=$(command -v dnf || command -v yum)
      $PM install -y $(echo $PK | sed 's/strongswan-swanctl charon-systemd/strongswan/; s/wireguard-go//') || echo "⚠️  не все пакеты VPN установились (для RHEL нужен EPEL)"
      case ",$VPN," in *,l2tp,*|*,ipsec,*) systemctl enable --now strongswan >/dev/null 2>&1 || true ;; esac
    fi
  fi
fi

# 2. Роль.
case "$MODE" in
  node)
    if [ ! -f "$DATA_DIR/node.json" ]; then
      HOPEBACKUP_NODE_DATA="$DATA_DIR" "$INSTALL_DIR/hopebackup" node init --data "$DATA_DIR" --listen "$LISTEN" | tee "$DATA_DIR/initial_tokens.txt"
      chmod 0600 "$DATA_DIR/initial_tokens.txt"
      echo "-- токены сохранены в $DATA_DIR/initial_tokens.txt — перенесите их в панель и удалите файл"
    fi
    ;;
  agent)
    if [ ! -f "$DATA_DIR/agent.json" ]; then
      if [ -n "${SERVER:-}" ] && [ -n "${TOKEN:-}" ]; then
        # Токен — файлом 0600, а не аргументом: так он не виден в списке процессов.
        TOKEN_FILE=$(umask 077; mktemp)
        printf '%s' "$TOKEN" > "$TOKEN_FILE"
        HOPEBACKUP_DATA="$DATA_DIR" "$INSTALL_DIR/hopebackup" agent enroll --server "$SERVER" --token-file "$TOKEN_FILE" ${FINGERPRINT:+--fingerprint "$FINGERPRINT"} || { rm -f "$TOKEN_FILE"; exit 1; }
        rm -f "$TOKEN_FILE"
      else
        echo "⚠️  Агент не подключён к серверу. Выполните команду из панели (Агенты → Подключить агент), затем: systemctl restart $SVC"
      fi
    fi
    ;;
esac

# 3. Файрвол (только для ролей, которые слушают порт).
for PORT in $PORTS; do
  P="$PORT/tcp"
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -qiw active; then
    echo "-- ufw: открываю $P"; ufw allow "$P" >/dev/null
  elif command -v firewall-cmd >/dev/null 2>&1 && systemctl is-active --quiet firewalld; then
    echo "-- firewalld: открываю $P"; firewall-cmd --permanent --add-port="$P" >/dev/null; firewall-cmd --reload >/dev/null
  elif command -v nft >/dev/null 2>&1 && systemctl is-active --quiet nftables && nft list chain inet filter input >/dev/null 2>&1; then
    echo "-- nftables: открываю $P"
    nft list chain inet filter input | grep -q "tcp dport $PORT accept" || nft insert rule inet filter input tcp dport "$PORT" accept
    nft list ruleset > /etc/nftables.conf
  elif command -v iptables >/dev/null 2>&1 && iptables -S INPUT 2>/dev/null | grep -qE '^-P INPUT (DROP|REJECT)|-j (DROP|REJECT)'; then
    echo "-- iptables: открываю $P"
    iptables -C INPUT -p tcp --dport "$PORT" -j ACCEPT 2>/dev/null || iptables -I INPUT -p tcp --dport "$PORT" -j ACCEPT
    command -v netfilter-persistent >/dev/null && netfilter-persistent save >/dev/null 2>&1 || true
  else
    echo "-- активный файрвол не найден — порты не трогаю"
  fi
done
PORT="${LISTEN%%,*}"; PORT="${PORT##*:}"

# 4. Служба: systemd или OpenRC (Alpine).
case "$MODE" in
  server) EXEC="$INSTALL_DIR/hopebackup server --listen $LISTEN"; DESC="сервер управления" ;;
  node)   EXEC="$INSTALL_DIR/hopebackup node run --data $DATA_DIR"; DESC="узел хранения площадки" ;;
  agent)  EXEC="$INSTALL_DIR/hopebackup agent run"; DESC="агент" ;;
esac
if ! command -v systemctl >/dev/null 2>&1 && [ -x /sbin/openrc-run ]; then
  cat > /etc/init.d/$SVC <<EOF
#!/sbin/openrc-run
description="HopeBackup — $DESC"
command=${EXEC%% *}
command_args="${EXEC#* }"
command_background=true
pidfile=/run/$SVC.pid
supervisor=supervise-daemon
output_log=/var/log/$SVC.log
error_log=/var/log/$SVC.log
export HOPEBACKUP_DATA=$DATA_DIR
export HOPEBACKUP_NODE_DATA=$DATA_DIR
export HOPEBACKUP_LOG=hopebackup=info,warn
depend() { need net; }
EOF
  chmod 755 /etc/init.d/$SVC
  rc-update add $SVC default >/dev/null 2>&1 || true
  if [ "$MODE" = agent ] && [ ! -f "$DATA_DIR/agent.json" ]; then echo "-- служба $SVC (OpenRC) создана, запустится после подключения агента"; exit 0; fi
  rc-service $SVC restart
  sleep 3
  if rc-service $SVC status >/dev/null 2>&1; then echo "✅ Служба $SVC (OpenRC) запущена."; else echo "❌ Служба не запустилась:"; tail -n 40 /var/log/$SVC.log 2>/dev/null; exit 1; fi
  [ "$MODE" = server ] && [ -f "$DATA_DIR/server/initial_admin_password.txt" ] && echo "Первый вход: Administrator / $(cat "$DATA_DIR/server/initial_admin_password.txt")"
  exit 0
fi
cat > /etc/systemd/system/$SVC.service <<EOF
[Unit]
Description=HopeBackup — $DESC
After=network-online.target local-fs.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$EXEC
Environment=HOPEBACKUP_DATA=$DATA_DIR
Environment=HOPEBACKUP_NODE_DATA=$DATA_DIR
Environment=HOPEBACKUP_LOG=hopebackup=info,warn
# От root: чтение данных продуктов любых владельцев, смена пользователя для утилит СУБД, VPN-интерфейсы.
User=root
$( [ "$MODE" != node ] && echo "# Убрать VPN-интерфейсы и IPsec-соединения, если процесс завершился, не опустив их.
ExecStopPost=-$INSTALL_DIR/hopebackup vpn-cleanup" )
Restart=always
RestartSec=3
LimitNOFILE=1048576
KillMode=mixed
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable "$SVC" >/dev/null 2>&1
if [ "$MODE" = agent ] && [ ! -f "$DATA_DIR/agent.json" ]; then
  echo "-- служба $SVC создана, запустится после подключения агента"
  exit 0
fi
systemctl restart "$SVC"
sleep 3
if systemctl is-active --quiet "$SVC"; then
  echo "✅ Служба $SVC запущена."
else
  echo "❌ Служба не запустилась:"; journalctl -u "$SVC" -n 40 --no-pager; exit 1
fi

IP=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -1)
case "$MODE" in
  server)
    echo
    echo "Панель: https://${IP:-<IP-сервера>}:$PORT/"
    [ -f "$DATA_DIR/server/initial_admin_password.txt" ] && echo "Первый вход: Administrator / $(cat "$DATA_DIR/server/initial_admin_password.txt")"
    echo "Отпечаток сертификата сервера: $(HOPEBACKUP_NODE_DATA=$DATA_DIR/server "$INSTALL_DIR/hopebackup" node fingerprint --data "$DATA_DIR/server" 2>/dev/null)"
    ;;
  node)
    echo
    echo "Узел площадки: https://${IP:-<IP-узла>}:$PORT  (отпечаток: $("$INSTALL_DIR/hopebackup" node fingerprint --data "$DATA_DIR"))"
    echo "Добавьте его в панели: Площадки → Добавить площадку."
    ;;
esac
