#!/bin/sh
# =====================================================================
# hopebackup-install.sh — установка HopeBackup («Ковчег») на Linux.
#
# Положите рядом (например, в /tmp) архив hopebackup-<версия>-linux.tar.gz и этот скрипт, затем от root:
#
#   sh /tmp/hopebackup-install.sh                       # спросит, что ставить
#   sh /tmp/hopebackup-install.sh server                # сервер управления: панель https://<IP>:4443/
#   sh /tmp/hopebackup-install.sh node                  # узел площадки (хранилище копий), порт 8443
#   sh /tmp/hopebackup-install.sh agent --server https://10.0.0.5:4443 --token <токен> --fingerprint <отпечаток>
#   sh /tmp/hopebackup-install.sh status                # что установлено и работает ли
#   sh /tmp/hopebackup-install.sh uninstall [--purge]   # удалить (с --purge — и данные)
#
# Параметры:
#   --archive ФАЙЛ        архив (по умолчанию — самый новый hopebackup-*-linux.tar.gz рядом со скриптом)
#   --listen АДРЕС:ПОРТ   адрес службы (server: 0.0.0.0:4443, node: 0.0.0.0:8443)
#   --data КАТАЛОГ        каталог данных (server, agent: /var/lib/hopebackup; node: /var/lib/hopebackup-node)
#   --server URL --token ТОКЕН --fingerprint ОТПЕЧАТОК [--name ИМЯ]
#                         подключение агента (команду с ними показывает панель: Агенты → Подключить агент)
#   --vpn openvpn,wireguard,l2tp,ipsec   пакеты для VPN-коннекторов (server, agent)
#   --no-firewall         не открывать порт в файрволе
#   -y, --yes             не задавать вопросов (по умолчанию для всего, что не указано)
#
# Повторный запуск с той же ролью обновляет программу: данные и настройки сохраняются, прежняя версия
# остаётся в /opt/hopebackup/hopebackup.bak.* (три последние).
#
# Работает в sh (dash, busybox): Debian/Ubuntu, Astra, RHEL/Alma/Rocky/Oracle/CentOS 7+, РЕД ОС, ALT,
# SLES, Alpine. Службы — systemd или OpenRC. Архитектуры — x86_64 и aarch64 (сборки glibc и статические musl).
# =====================================================================
set -eu

INSTALL_DIR=/opt/hopebackup
ROLE=""; ARCHIVE=""; LISTEN=""; DATA=""; SERVER=""; TOKEN=""; FINGERPRINT=""; NAME=""; VPN=""
YES=0; PURGE=0; FIREWALL=1

say()  { printf '%s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
warn() { printf '!  %s\n' "$*" >&2; }
die()  { printf 'ОШИБКА: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

usage() { sed -n '3,27p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    server|node|agent|uninstall|status) ROLE="$1" ;;
    --archive) ARCHIVE="${2:?}"; shift ;;
    --listen) LISTEN="${2:?}"; shift ;;
    --data) DATA="${2:?}"; shift ;;
    --server) SERVER="${2:?}"; shift ;;
    --token) TOKEN="${2:?}"; shift ;;
    --fingerprint) FINGERPRINT="${2:?}"; shift ;;
    --name) NAME="${2:?}"; shift ;;
    --vpn) VPN="${2:?}"; shift ;;
    --no-firewall) FIREWALL=0 ;;
    --purge) PURGE=1 ;;
    -y|--yes) YES=1 ;;
    -h|--help) usage 0 ;;
    *) warn "неизвестный параметр: $1"; usage 1 ;;
  esac
  shift
done

[ "$(id -u)" -eq 0 ] || die "запустите от root: sudo sh $0 ${ROLE:-}"

# Вопросы — с терминала, даже если скрипт пришёл через конвейер.
TTY=""
if [ "$YES" -eq 0 ] && [ -r /dev/tty ] && (exec </dev/tty) 2>/dev/null; then TTY=/dev/tty; fi
ask() { # ask "Вопрос" "по умолчанию" → ответ в $REPLY
  REPLY="$2"
  [ -n "$TTY" ] || return 0
  printf '%s [%s]: ' "$1" "$2" > "$TTY"
  read -r REPLY < "$TTY" || REPLY=""
  [ -n "$REPLY" ] || REPLY="$2"
}
ask_secret() {
  REPLY=""
  [ -n "$TTY" ] || return 0
  printf '%s: ' "$1" > "$TTY"
  stty -echo < "$TTY" 2>/dev/null || true
  read -r REPLY < "$TTY" || REPLY=""
  stty echo < "$TTY" 2>/dev/null || true
  printf '\n' > "$TTY"
}
confirm() { # confirm "Вопрос" → 0 = да
  [ -n "$TTY" ] || return 0
  printf '%s [Д/н]: ' "$1" > "$TTY"
  read -r a < "$TTY" || a=""
  case "$a" in н|Н|n|N|no|нет) return 1 ;; *) return 0 ;; esac
}

svc_name() { echo "hopebackup-$1"; }
# Слушает ли кто-то TCP-порт (ss или netstat; без них — «не знаю», считаем что нет).
port_listening() {
  if have ss; then ss -ltn 2>/dev/null | awk '{print $4}' | grep -q ":$1\$"
  elif have netstat; then netstat -ltn 2>/dev/null | awk '{print $4}' | grep -q ":$1\$"
  else return 1; fi
}
has_systemd() { have systemctl && [ -d /run/systemd/system ]; }
has_openrc() { [ -x /sbin/openrc-run ] && have rc-service; }
svc_exists() {
  [ -f "/etc/systemd/system/$1.service" ] || [ -f "/etc/init.d/$1" ]
}
svc_active() {
  if [ -f "/etc/systemd/system/$1.service" ] && has_systemd; then systemctl is-active --quiet "$1"; return; fi
  if [ -f "/etc/init.d/$1" ] && has_openrc; then rc-service "$1" status >/dev/null 2>&1; return; fi
  return 1
}
svc_stop() {
  if [ -f "/etc/systemd/system/$1.service" ] && has_systemd; then systemctl stop "$1" 2>/dev/null || true; fi
  if [ -f "/etc/init.d/$1" ] && has_openrc; then rc-service "$1" stop >/dev/null 2>&1 || true; fi
}

# ---------------------------------------------------------------- status
if [ "$ROLE" = status ]; then
  if [ -x "$INSTALL_DIR/hopebackup" ]; then say "Программа: $("$INSTALL_DIR/hopebackup" --version 2>/dev/null || echo '?') ($INSTALL_DIR/hopebackup)"; else say "Программа не установлена."; fi
  for r in server node agent; do
    s=$(svc_name $r)
    if svc_exists "$s"; then
      if svc_active "$s"; then say "  $s: работает"; else say "  $s: остановлена"; fi
    fi
  done
  exit 0
fi

# ---------------------------------------------------------------- uninstall
if [ "$ROLE" = uninstall ]; then
  step "Удаление HopeBackup"
  for r in server node agent; do
    s=$(svc_name $r)
    svc_exists "$s" || continue
    say "-- служба $s: остановка и удаление"
    svc_stop "$s"
    if [ -f "/etc/systemd/system/$s.service" ]; then
      has_systemd && systemctl disable "$s" >/dev/null 2>&1 || true
      rm -f "/etc/systemd/system/$s.service"
    fi
    if [ -f "/etc/init.d/$s" ]; then
      have rc-update && rc-update del "$s" default >/dev/null 2>&1 || true
      rm -f "/etc/init.d/$s"
    fi
  done
  has_systemd && systemctl daemon-reload || true
  # VPN-интерфейсы и соединения IPsec, если процесс их оставил.
  [ -x "$INSTALL_DIR/hopebackup" ] && "$INSTALL_DIR/hopebackup" vpn-cleanup >/dev/null 2>&1 || true
  rm -rf "$INSTALL_DIR"
  [ -L /usr/local/bin/hopebackup ] && rm -f /usr/local/bin/hopebackup
  say "-- программа удалена"
  DIRS="/var/lib/hopebackup /var/lib/hopebackup-node"
  if [ "$PURGE" -eq 1 ]; then
    say "Будут удалены данные: $DIRS"
    say "  (на узле площадки это ВСЕ КОПИИ; на сервере — настройки, пароли репозиториев, история)"
    if [ -n "$TTY" ]; then
      printf 'Для подтверждения введите УДАЛИТЬ: ' > "$TTY"; read -r a < "$TTY" || a=""
      [ "$a" = "УДАЛИТЬ" ] || die "отменено — данные не тронуты"
    fi
    rm -rf $DIRS
    say "-- данные удалены"
  else
    for d in $DIRS; do [ -d "$d" ] && say "-- данные сохранены: $d (удалить: sh $0 uninstall --purge)"; done
  fi
  say "Готово."
  exit 0
fi

# ---------------------------------------------------------------- выбор роли
if [ -z "$ROLE" ]; then
  [ -n "$TTY" ] || die "укажите роль: server | node | agent (или status, uninstall)"
  cat > "$TTY" <<'EOF'
Что установить на эту машину?
  1) server — сервер управления (панель, расписания, восстановление)
  2) node   — узел площадки (хранилище копий)
  3) agent  — агент на машине с данными (PostgreSQL, файлы, весь сервер…)
EOF
  ask "Номер или название" "1"
  case "$REPLY" in 1|server) ROLE=server ;; 2|node) ROLE=node ;; 3|agent) ROLE=agent ;; *) die "непонятный выбор: $REPLY" ;; esac
fi

case "$ROLE" in
  server) DATA="${DATA:-/var/lib/hopebackup}";      DEF_LISTEN=0.0.0.0:4443; DESC="сервер управления" ;;
  node)   DATA="${DATA:-/var/lib/hopebackup-node}"; DEF_LISTEN=0.0.0.0:8443; DESC="узел хранения площадки" ;;
  agent)  DATA="${DATA:-/var/lib/hopebackup}";      DEF_LISTEN="";           DESC="агент" ;;
esac
SVC=$(svc_name "$ROLE")
UPDATE=0; svc_exists "$SVC" && UPDATE=1
# При обновлении — прежний адрес службы.
if [ -z "$LISTEN" ] && [ "$UPDATE" -eq 1 ] && [ "$ROLE" != agent ]; then
  LISTEN=$(sed -n 's/.*--listen \([^ "]*\).*/\1/p' "/etc/systemd/system/$SVC.service" "/etc/init.d/$SVC" 2>/dev/null | head -1)
fi
# Узел берёт адрес из своей конфигурации.
if [ "$ROLE" = node ] && [ -f "$DATA/node.json" ]; then
  LISTEN=$(sed -n 's/.*"listen": *"\([^"]*\)".*/\1/p' "$DATA/node.json" | head -1)
fi
LISTEN="${LISTEN:-$DEF_LISTEN}"
L1="${LISTEN%%,*}"; PORT="${L1##*:}"

# ---------------------------------------------------------------- архив
step "HopeBackup: $([ "$UPDATE" -eq 1 ] && echo обновление || echo установка) — $DESC"
HERE=$(cd "$(dirname "$0")" && pwd)
if [ -z "$ARCHIVE" ]; then
  if [ -d "$HERE/dist" ]; then
    ARCHIVE="$HERE"   # скрипт запущен из распакованного архива
  else
    ARCHIVE=$(ls -t "$HERE"/hopebackup-*-linux.tar.gz 2>/dev/null | head -1 || true)
    [ -n "$ARCHIVE" ] || ARCHIVE=$(ls -t /tmp/hopebackup-*-linux.tar.gz 2>/dev/null | head -1 || true)
  fi
fi
[ -n "$ARCHIVE" ] && [ -e "$ARCHIVE" ] || die "не найден архив hopebackup-*-linux.tar.gz рядом со скриптом ($HERE) и в /tmp — укажите --archive ФАЙЛ"

WORK=$(mktemp -d /tmp/hopebackup-install.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT TERM
if [ -d "$ARCHIVE" ]; then
  PKG="$ARCHIVE"
else
  say "-- архив: $ARCHIVE"
  tar -xzf "$ARCHIVE" -C "$WORK" || die "архив не распаковывается: $ARCHIVE"
  PKG=$(find "$WORK" -mindepth 1 -maxdepth 2 -type d -name dist | head -1)
  [ -n "$PKG" ] || die "в архиве нет каталога dist/ со сборками"
  PKG=$(dirname "$PKG")
fi
[ -f "$PKG/VERSION" ] && say "-- версия в архиве: $(cat "$PKG/VERSION")"

# ---------------------------------------------------------------- сборка под эту машину
ARCH=$(uname -m)
case "$ARCH" in x86_64|amd64) ARCH=x86_64 ;; aarch64|arm64) ARCH=aarch64 ;; *) die "архитектура $ARCH не поддерживается (есть x86_64 и aarch64)" ;; esac
sha_ok() { # sha_ok файл.gz — сверка с файл.gz.sha256
  [ -f "$1.sha256" ] || { warn "нет контрольной суммы для $(basename "$1")"; return 0; }
  want=$(cut -d' ' -f1 < "$1.sha256")
  if have sha256sum; then got=$(sha256sum "$1" | cut -d' ' -f1)
  elif have openssl; then got=$(openssl dgst -sha256 "$1" | sed 's/.*= //')
  else warn "нет sha256sum — контрольная сумма не проверена"; return 0; fi
  [ "$want" = "$got" ]
}
BIN=""
for libc in gnu musl; do
  f="$PKG/dist/hopebackup-$ARCH-$libc.gz"
  [ -f "$f" ] || continue
  sha_ok "$f" || die "контрольная сумма $(basename "$f") не совпадает — архив повреждён"
  gunzip -c "$f" > "$WORK/hopebackup" && chmod 755 "$WORK/hopebackup"
  if "$WORK/hopebackup" --version >/dev/null 2>&1; then
    BIN="$WORK/hopebackup"
    [ "$libc" = musl ] && say "-- сборка $ARCH-musl (статическая: glibc нет или она старше 2.17)" || say "-- сборка $ARCH-gnu"
    break
  fi
done
[ -n "$BIN" ] || die "в архиве нет сборки, которая запускается на этой системе ($ARCH)"
NEWVER=$("$BIN" --version 2>/dev/null || echo '?')
say "-- $NEWVER"

# ---------------------------------------------------------------- проверка машины
step "Проверка машины"
mem_kb=$(awk '/MemAvailable/{print $2; exit}' /proc/meminfo 2>/dev/null || true)
[ -n "$mem_kb" ] || mem_kb=$(awk '/MemTotal/{print $2; exit}' /proc/meminfo 2>/dev/null || echo 0)
case "$ROLE" in agent) need_mb=256 ;; *) need_mb=512 ;; esac
say "-- свободно памяти: $((mem_kb / 1024)) МБ (нужно не меньше $need_mb МБ$([ "$ROLE" = node ] && echo ' и ещё ≈80 МБ на каждый 1 ТБ защищаемых данных'))"
[ "$mem_kb" -ge $((need_mb * 1024)) ] || warn "мало памяти: $((mem_kb / 1024)) МБ — добавьте ВМ памяти или подкачку"
mkdir -p "$DATA"
free_kb=$(df -Pk "$DATA" 2>/dev/null | awk 'NR==2{print $4}')
say "-- свободно на диске под $DATA: $((free_kb / 1024 / 1024)) ГБ"
case "$ROLE" in
  node) [ "$free_kb" -ge $((20 * 1024 * 1024)) ] || warn "под копии меньше 20 ГБ: нужно 1,5–2 объёма защищаемых данных (смонтируйте большой диск в $DATA или укажите --data)" ;;
  *)    [ "$free_kb" -ge $((1024 * 1024)) ] || warn "на диске меньше 1 ГБ свободно" ;;
esac
if [ -n "$PORT" ] && [ "$UPDATE" -eq 0 ] && port_listening "$PORT"; then
  die "порт $PORT уже занят другой программой — укажите --listen 0.0.0.0:<порт>"
fi
if have timedatectl; then
  timedatectl 2>/dev/null | grep -qiE 'synchronized: *yes' || warn "время не синхронизируется (NTP): расписания и восстановление на момент зависят от точного времени — timedatectl set-ntp true"
fi
if [ "$ROLE" != node ] && ! have ssh; then
  warn "нет клиента OpenSSH (ssh): он нужен для коннектора «SSH-туннель»$([ "$ROLE" = server ] && echo ' и восстановления по SSH')"
fi

# ---------------------------------------------------------------- агент: подключение
if [ "$ROLE" = agent ] && [ ! -f "$DATA/agent.json" ] && { [ -z "$SERVER" ] || [ -z "$TOKEN" ]; }; then
  if [ -n "$TTY" ]; then
    say "Подключение агента: адрес сервера, токен и отпечаток — в панели (Агенты → Подключить агент)."
    ask "Адрес сервера управления (https://IP:4443)" "$SERVER"; SERVER="$REPLY"
    [ -n "$TOKEN" ] || { ask_secret "Токен подключения"; TOKEN="$REPLY"; }
    [ -n "$FINGERPRINT" ] || { ask "Отпечаток сертификата сервера (SHA-256)" ""; FINGERPRINT="$REPLY"; }
  fi
  case "$SERVER" in https://*|http://*) ;; *) SERVER="" ;; esac
fi

# ---------------------------------------------------------------- программа
step "Установка программы"
mkdir -p "$INSTALL_DIR"
if [ -f "$INSTALL_DIR/hopebackup" ]; then
  say "-- было: $("$INSTALL_DIR/hopebackup" --version 2>/dev/null || echo '?')"
  cp -p "$INSTALL_DIR/hopebackup" "$INSTALL_DIR/hopebackup.bak.$(date +%Y%m%d%H%M%S)"
  ls -1t "$INSTALL_DIR"/hopebackup.bak.* 2>/dev/null | tail -n +4 | while read -r old; do rm -f "$old"; done
fi
# Через временный файл и переименование: работающая служба не мешает замене.
cp "$BIN" "$INSTALL_DIR/hopebackup.new" && chmod 755 "$INSTALL_DIR/hopebackup.new"
mv -f "$INSTALL_DIR/hopebackup.new" "$INSTALL_DIR/hopebackup"
ln -sf "$INSTALL_DIR/hopebackup" /usr/local/bin/hopebackup 2>/dev/null || true
say "-- $INSTALL_DIR/hopebackup (команда hopebackup)"
if [ "$ROLE" = server ]; then
  # Серверу — комплект сборок: опись и установка агентов на машины других архитектур и Alpine.
  mkdir -p "$INSTALL_DIR/dist"
  cp -f "$PKG"/dist/hopebackup-*.gz "$INSTALL_DIR/dist/" 2>/dev/null || true
  cp -f "$PKG"/dist/hopebackup-*.sha256 "$INSTALL_DIR/dist/" 2>/dev/null || true
  say "-- комплект сборок для машин: $(ls "$INSTALL_DIR/dist" | sed -n 's/^hopebackup-\(.*\)\.gz$/\1/p' | tr '\n' ' ')"
fi
# Сервер и агент делят /var/lib/hopebackup: каталог закрыт для чтения, но проходим — спул WAL внутри
# принадлежит пользователю СУБД. Узлу — только root.
case "$ROLE" in node) chmod 0700 "$DATA" ;; *) chmod 0711 "$DATA" ;; esac

# ---------------------------------------------------------------- пакеты VPN
if [ -n "$VPN" ] && [ "$ROLE" != node ]; then
  step "Пакеты VPN-коннекторов: $VPN"
  PK=""
  for v in $(echo "$VPN" | tr ',' ' '); do
    case "$v" in
      openvpn)   PK="$PK openvpn" ;;
      wireguard) PK="$PK wireguard-tools" ;;
      l2tp)      PK="$PK strongswan xl2tpd ppp" ;;
      ipsec)     PK="$PK strongswan" ;;
      *) warn "VPN: неизвестный вид «$v» (openvpn, wireguard, l2tp, ipsec)" ;;
    esac
  done
  PK=$(echo $PK | tr ' ' '\n' | sort -u | tr '\n' ' ')
  if have apt-get; then
    DEB=$(echo "$PK" | sed 's/strongswan/strongswan-swanctl charon-systemd/')
    # shellcheck disable=SC2086
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q $DEB || warn "не все пакеты VPN установились"
  elif have dnf || have yum; then
    # shellcheck disable=SC2086
    $(command -v dnf || command -v yum) install -y $PK || warn "не все пакеты VPN установились (для RHEL нужен EPEL)"
  elif have apk; then
    # shellcheck disable=SC2086
    apk add --no-cache $PK || warn "не все пакеты VPN установились"
  elif have zypper; then
    # shellcheck disable=SC2086
    zypper -n install $PK || warn "не все пакеты VPN установились"
  else
    warn "менеджер пакетов не найден — поставьте вручную: $PK"
  fi
  case ",$VPN," in *,l2tp,*) has_systemd && systemctl disable --now xl2tpd >/dev/null 2>&1 || true ;; esac   # свой xl2tpd запускает HopeBackup
  case ",$VPN," in *,wireguard,*) modprobe wireguard 2>/dev/null || warn "модуля ядра wireguard нет — нужен wireguard-go" ;; esac
fi

# ---------------------------------------------------------------- роль
step "Настройка роли"
case "$ROLE" in
  node)
    if [ ! -f "$DATA/node.json" ]; then
      ( umask 077; HOPEBACKUP_NODE_DATA="$DATA" "$INSTALL_DIR/hopebackup" node init --data "$DATA" --listen "$LISTEN" > "$DATA/initial_tokens.txt" ) || die "не удалось создать конфигурацию узла"
      cat "$DATA/initial_tokens.txt"
      say "-- токены сохранены в $DATA/initial_tokens.txt (0600): перенесите их в панель и удалите файл"
    else
      say "-- конфигурация узла уже есть ($DATA/node.json) — токены не меняются"
    fi
    ;;
  agent)
    if [ -f "$DATA/agent.json" ] && { [ -z "$SERVER" ] || [ -z "$TOKEN" ]; }; then
      say "-- агент уже подключён ($DATA/agent.json)"
    elif [ -n "$SERVER" ] && [ -n "$TOKEN" ]; then
      # Команда подключения с новым сервером и токеном — переподключить (например, к другому серверу управления);
      # прежняя настройка остаётся рядом, а при отказе сервера не меняется (enroll пишет её только при успехе).
      if [ -f "$DATA/agent.json" ]; then
        OLD=$(sed -n 's/.*"server": *"\([^"]*\)".*/\1/p' "$DATA/agent.json" | head -1)
        BAK="$DATA/agent.json.before-$(date +%Y%m%d%H%M%S)"
        cp -p "$DATA/agent.json" "$BAK"
        say "-- агент был подключён к ${OLD:-другому серверу} — подключаю к $SERVER (прежняя настройка: $BAK)"
      fi
      TF=$(umask 077; mktemp "$WORK/token.XXXXXX")
      printf '%s' "$TOKEN" > "$TF"   # токен — файлом: не виден в списке процессов
      set -- agent enroll --server "$SERVER" --token-file "$TF"
      [ -n "$FINGERPRINT" ] && set -- "$@" --fingerprint "$FINGERPRINT"
      [ -n "$NAME" ] && set -- "$@" --name "$NAME"
      HOPEBACKUP_DATA="$DATA" "$INSTALL_DIR/hopebackup" "$@" || die "агент не подключился к $SERVER (проверьте адрес, токен — он одноразовый и живёт 24 ч — и отпечаток)"
      rm -f "$TF"
    else
      warn "агент не подключён: нужна команда из панели (Агенты → Подключить агент). Повторите: sh $0 agent --server … --token … --fingerprint …"
    fi
    ;;
  server)
    [ -f "$DATA/server/state.json" ] && say "-- данные сервера уже есть — настройки сохраняются" || say "-- новый сервер: пароль администратора будет создан при первом запуске"
    ;;
esac

# ---------------------------------------------------------------- файрвол
if [ -n "$PORT" ] && [ "$FIREWALL" -eq 1 ]; then
  P="$PORT/tcp"
  if have ufw && ufw status 2>/dev/null | grep -qiw active; then
    ufw allow "$P" >/dev/null && say "-- ufw: открыт $P"
  elif have firewall-cmd && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port="$P" >/dev/null && firewall-cmd --reload >/dev/null && say "-- firewalld: открыт $P"
  elif have nft && nft list chain inet filter input >/dev/null 2>&1; then
    nft list chain inet filter input | grep -q "tcp dport $PORT accept" || nft insert rule inet filter input tcp dport "$PORT" accept
    [ -f /etc/nftables.conf ] && nft list ruleset > /etc/nftables.conf
    say "-- nftables: открыт $P"
  elif have iptables && iptables -S INPUT 2>/dev/null | grep -qE '^-P INPUT (DROP|REJECT)|-j (DROP|REJECT)'; then
    iptables -C INPUT -p tcp --dport "$PORT" -j ACCEPT 2>/dev/null || iptables -I INPUT -p tcp --dport "$PORT" -j ACCEPT
    have netfilter-persistent && netfilter-persistent save >/dev/null 2>&1 || true
    say "-- iptables: открыт $P"
  else
    say "-- активный файрвол не найден — порт $P не трогаю"
  fi
fi

# ---------------------------------------------------------------- служба
step "Служба $SVC"
case "$ROLE" in
  server) EXEC="$INSTALL_DIR/hopebackup server --listen $LISTEN" ;;
  node)   EXEC="$INSTALL_DIR/hopebackup node run --data $DATA" ;;
  agent)  EXEC="$INSTALL_DIR/hopebackup agent run" ;;
esac
STARTABLE=1
[ "$ROLE" = agent ] && [ ! -f "$DATA/agent.json" ] && STARTABLE=0
LOGCMD=""
if has_systemd; then
  {
    echo "[Unit]"
    echo "Description=HopeBackup — $DESC"
    echo "After=network-online.target local-fs.target"
    echo "Wants=network-online.target"
    echo
    echo "[Service]"
    echo "Type=simple"
    echo "ExecStart=$EXEC"
    echo "Environment=HOPEBACKUP_DATA=$DATA"
    echo "Environment=HOPEBACKUP_NODE_DATA=$DATA"
    echo "Environment=HOPEBACKUP_LOG=hopebackup=info,warn"
    echo "# От root: чтение данных любых владельцев, утилиты СУБД от их пользователя, снимки ФС, VPN-интерфейсы."
    echo "User=root"
    [ "$ROLE" != node ] && echo "ExecStopPost=-$INSTALL_DIR/hopebackup vpn-cleanup"
    echo "Restart=always"
    echo "RestartSec=3"
    echo "LimitNOFILE=1048576"
    echo "KillMode=mixed"
    echo "TimeoutStopSec=30"
    echo
    echo "[Install]"
    echo "WantedBy=multi-user.target"
  } > "/etc/systemd/system/$SVC.service"
  systemctl daemon-reload
  systemctl enable "$SVC" >/dev/null 2>&1 || true
  if [ "$STARTABLE" -eq 1 ]; then systemctl restart "$SVC"; fi
  LOGCMD="journalctl -u $SVC -n 40 --no-pager"
elif has_openrc; then
  cat > "/etc/init.d/$SVC" <<EOF
#!/sbin/openrc-run
description="HopeBackup — $DESC"
command=${EXEC%% *}
command_args="${EXEC#* }"
command_background=true
pidfile=/run/$SVC.pid
supervisor=supervise-daemon
respawn_delay=3
output_log=/var/log/$SVC.log
error_log=/var/log/$SVC.log
export HOPEBACKUP_DATA=$DATA
export HOPEBACKUP_NODE_DATA=$DATA
export HOPEBACKUP_LOG=hopebackup=info,warn
depend() { need net; }
EOF
  chmod 755 "/etc/init.d/$SVC"
  rc-update add "$SVC" default >/dev/null 2>&1 || true
  if [ "$STARTABLE" -eq 1 ]; then rc-service "$SVC" restart >/dev/null 2>&1 || rc-service "$SVC" start >/dev/null 2>&1 || true; fi
  LOGCMD="tail -n 40 /var/log/$SVC.log"
else
  warn "нет ни systemd, ни OpenRC — служба не создана. Запуск вручную:"
  say "   HOPEBACKUP_DATA=$DATA HOPEBACKUP_NODE_DATA=$DATA nohup $EXEC >/var/log/$SVC.log 2>&1 &"
  STARTABLE=0
fi

if [ "$STARTABLE" -eq 1 ]; then
  i=0; up=0
  while [ $i -lt 15 ]; do
    if svc_active "$SVC"; then
      # Слушающей службе — дождаться порта.
      if [ -z "$PORT" ]; then up=1; break; fi
      if have ss || have netstat; then port_listening "$PORT" && { up=1; break; }; else sleep 2; up=1; break; fi
    fi
    i=$((i + 1)); sleep 1
  done
  if [ "$up" -eq 1 ]; then say "-- служба $SVC работает"; else warn "служба $SVC не запустилась:"; [ -n "$LOGCMD" ] && sh -c "$LOGCMD" >&2 || true; exit 1; fi
elif [ "$ROLE" = agent ]; then
  say "-- служба $SVC создана; запустится после подключения агента"
fi

# ---------------------------------------------------------------- итог
IP=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.*src \([0-9.]*\).*/\1/p' | head -1)
[ -n "$IP" ] || IP=$(hostname -i 2>/dev/null | awk '{print $1}')
step "Готово: $NEWVER"
case "$ROLE" in
  server)
    say "Панель:        https://${IP:-<IP-сервера>}:$PORT/"
    if [ -f "$DATA/server/initial_admin_password.txt" ]; then
      say "Первый вход:   Administrator / $(cat "$DATA/server/initial_admin_password.txt")  (смените пароль; файл удалите)"
    fi
    FP=$(HOPEBACKUP_NODE_DATA="$DATA/server" "$INSTALL_DIR/hopebackup" node fingerprint --data "$DATA/server" 2>/dev/null || true)
    [ -n "$FP" ] && say "Отпечаток:     $FP  (сверьте при первом входе в браузере)"
    say "Дальше:        Площадки → Добавить площадку; Серверы и ВМ → Добавить сервер (опись и установка агентов по SSH)"
    ;;
  node)
    FP=$("$INSTALL_DIR/hopebackup" node fingerprint --data "$DATA" 2>/dev/null || true)
    say "Узел площадки: https://${IP:-<IP-узла>}:$PORT"
    say "Отпечаток:     $FP"
    say "Дальше:        в панели — Площадки → Добавить площадку (адрес, отпечаток, токены из $DATA/initial_tokens.txt)"
    say "Копии хранятся в $DATA/repos — это должен быть большой диск (1,5–2 объёма защищаемых данных)"
    ;;
  agent)
    if [ -f "$DATA/agent.json" ]; then say "Агент подключён — он появится в панели в разделе «Агенты»."; fi
    ;;
esac
say "Состояние: sh $0 status   Удаление: sh $0 uninstall"
