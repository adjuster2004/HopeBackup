# Общее для проверок плагинов приложений (подключается через «. tests/lib.sh»).
#   say/fail — вывод; fake_systemd <каталог> — имитация systemctl (юниты из /etc/systemd/system,
#   ExecStart/Environment/WorkingDirectory/User, pid-файлы); unit <имя> <строки секции [Service]>.

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }

fake_systemd() {
  local W=$1 FAKE=$1/fakebin
  mkdir -p "$W/run" "$FAKE"
  if [ ! -d /run/systemd/system ]; then mkdir -p /run/systemd/system; touch /run/systemd/system/.hb-fake; fi
  cat > "$FAKE/systemctl" <<EOF
#!/bin/sh
cmd=\$1; shift
[ "\$1" = --quiet ] && shift
case "\$cmd" in
  daemon-reload|enable|disable) exit 0 ;;
  cat) [ -f /etc/systemd/system/\$1.service ] ;;
  is-active) p=$W/run/\$1.pid; [ -f \$p ] && kill -0 \$(cat \$p) 2>/dev/null; exit \$? ;;
  stop)
    p=$W/run/\$1.pid; echo "stop \$1 \$(date +%s.%N)" >> $W/run/events
    if [ -f \$p ]; then pid=\$(cat \$p); kill \$pid 2>/dev/null; i=0; while kill -0 \$pid 2>/dev/null && [ \$i -lt 300 ]; do sleep 0.1; i=\$((i+1)); done; fi
    rm -f \$p; exit 0 ;;
  start|restart)
    u=/etc/systemd/system/\$1.service; p=$W/run/\$1.pid
    echo "start \$1 \$(date +%s.%N)" >> $W/run/events
    if [ -f \$p ]; then kill \$(cat \$p) 2>/dev/null; sleep 0.5; fi
    wd=\$(sed -n 's/^WorkingDirectory=//p' \$u); [ -n "\$wd" ] && cd "\$wd"
    usr=\$(sed -n 's/^User=//p' \$u)
    ex=\$(sed -n 's/^ExecStart=//p' \$u)
    if [ -n "\$usr" ] && [ "\$usr" != root ]; then
      env \$(sed -n 's/^Environment=//p' \$u) setpriv --reuid="\$usr" --regid="\$usr" --init-groups sh -c "exec \$ex" > $W/run/\$1.log 2>&1 &
    else
      env \$(sed -n 's/^Environment=//p' \$u) sh -c "exec \$ex" > $W/run/\$1.log 2>&1 &
    fi
    echo \$! > \$p; sleep 0.3; exit 0 ;;
  *) exit 0 ;;
esac
EOF
  chmod +x "$FAKE/systemctl"
  export PATH="$FAKE:$PATH"
}

fake_systemd_cleanup() {
  local W=$1
  for f in "$W"/run/*.pid; do [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true; done
  [ -f /run/systemd/system/.hb-fake ] && rm -rf /run/systemd || true
}

unit() { # unit <имя> <содержимое секции Service>
  printf '[Unit]\nDescription=%s\n\n[Service]\n%s\n\n[Install]\nWantedBy=multi-user.target\n' "$1" "$2" > /etc/systemd/system/$1.service
}

wait_for() { # wait_for <секунд> <команда…>
  local n=$1; shift
  for _ in $(seq 1 $((n * 5))); do "$@" >/dev/null 2>&1 && return 0; sleep 0.2; done
  return 1
}
