#!/usr/bin/env bash
# Проверка плагина «Сервер Linux целиком» через `hopebackup repo` (нужны root, useradd, openssl, mount):
#   на этой машине создаются тестовый пользователь (пароль, ключ SSH, sudo), приложение /opt/hbapp,
#   которое работает фоновым sh-скриптом, задание cron со скриптом в /usr/local/bin, служба systemd,
#   «диск данных» (bind-монтирование) и журналы в /var/log;
#   опись должна найти скрипт, задание, службу и монтирование; копия — содержать /etc, ключи, приложение,
#   не содержать /var/log (общее исключение) и диск данных (не отмечен);
#   потом пользователь и приложение удаляются и восстанавливаются: пароль (хэш), ключи с правами 600,
#   sudo, файлы приложения; отмеченный диск — отдельной частью копии.
#
#   BIN=./hopebackup-core/target/release/hopebackup tests/host.sh
set -euo pipefail

BIN=$(readlink -f "${BIN:-./hopebackup-core/target/release/hopebackup}")
W=${W:-/tmp/hb-host}
U=hbtestuser

say() { printf '\n\033[1;34m== %s\033[0m\n' "$*"; }
fail() { printf '\n\033[1;31mОШИБКА: %s\033[0m\n' "$*"; exit 1; }
cleanup() {
  [ -f "$W/app.pid" ] && kill "$(cat "$W/app.pid")" 2>/dev/null || true
  pkill -f /opt/hbapp/run.sh 2>/dev/null || true
  mountpoint -q /mnt/hbdata 2>/dev/null && umount /mnt/hbdata || true
  userdel -r $U 2>/dev/null || true
  groupdel hbteam 2>/dev/null || true
  rm -rf /opt/hbapp /mnt/hbdata /usr/local/bin/hb-nightly.sh /etc/cron.d/hb-nightly /etc/sudoers.d/hbtest /etc/systemd/system/hb-worker.service /var/log/hbapp
}
[ "${KEEP:-0}" = 1 ] || trap cleanup EXIT
cleanup; rm -rf "$W"; mkdir -p "$W"
export HOPEBACKUP_PASSWORD=host-repo-password HOPEBACKUP_DATA="$W/data"
R() { "$BIN" repo --url "$W/repo" "$@"; }
R init

say "Подготовка: пользователь, приложение, cron, служба, диск данных, журналы"
groupadd -g 4711 hbteam
useradd -m -u 4712 -s /bin/bash -G hbteam $U
HASH=$(openssl passwd -6 -salt hbsalt 'S3cret-pass')
usermod -p "$HASH" $U
install -d -m 700 -o $U -g $U /home/$U/.ssh
echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHbTestKeyForHopeBackup hb@test" > /home/$U/.ssh/authorized_keys
chown $U:$U /home/$U/.ssh/authorized_keys; chmod 600 /home/$U/.ssh/authorized_keys
echo "$U ALL=(ALL) NOPASSWD: /usr/bin/systemctl" > /etc/sudoers.d/hbtest; chmod 440 /etc/sudoers.d/hbtest
mkdir -p /opt/hbapp/lib /var/log/hbapp
cat > /opt/hbapp/run.sh <<'EOF'
#!/bin/sh
while true; do date >> /var/log/hbapp/run.log; sleep 2; done
EOF
echo "config=1" > /opt/hbapp/lib/app.conf
chmod +x /opt/hbapp/run.sh
nohup sh /opt/hbapp/run.sh >/dev/null 2>&1 & echo $! > "$W/app.pid"
printf '#!/bin/bash\necho nightly\n' > /usr/local/bin/hb-nightly.sh; chmod +x /usr/local/bin/hb-nightly.sh
echo "30 2 * * * root /usr/local/bin/hb-nightly.sh >/dev/null 2>&1" > /etc/cron.d/hb-nightly
mkdir -p /etc/systemd/system
printf '[Unit]\nDescription=HB worker\n[Service]\nExecStart=/opt/hbapp/run.sh\n[Install]\nWantedBy=multi-user.target\n' > /etc/systemd/system/hb-worker.service
mkdir -p "$W/disk" /mnt/hbdata
dd if=/dev/urandom of="$W/disk/data.bin" bs=1M count=3 status=none
mount --bind "$W/disk" /mnt/hbdata
sleep 1

say "Опись"
"$BIN" inventory 2>/dev/null > "$W/inv.json"
jq -e '.processes[] | select(.script=="/opt/hbapp/run.sh") | select(.app=="/opt/hbapp")' "$W/inv.json" >/dev/null || fail "не найден фоновый скрипт /opt/hbapp/run.sh"
jq -e '.schedules[] | select(.source=="/etc/cron.d/hb-nightly") | select(.scripts | index("/usr/local/bin/hb-nightly.sh"))' "$W/inv.json" >/dev/null || fail "не найдено задание cron"
jq -e '.services[] | select(.unit=="hb-worker.service")' "$W/inv.json" >/dev/null || fail "не найдена своя служба"
jq -e '.mounts[] | select(.target=="/mnt/hbdata" and .class=="bind")' "$W/inv.json" >/dev/null || fail "не найдено монтирование /mnt/hbdata"
jq -e ".users[] | select(.name==\"$U\" and .has_password and .sudo and (.ssh_files|index(\"authorized_keys\")))" "$W/inv.json" >/dev/null || fail "пользователь в описи неполный"
jq -r '.apps[] | "  приложение \(.path) ← \(.sources|join("; "))"' "$W/inv.json"
jq -r '.schedules[] | select(.packaged|not) | "  расписание \(.kind) \(.schedule): \(.command)"' "$W/inv.json"

say "Копия (общее исключение /var/log, диск данных не отмечен)"
CFG='{"home":"all","global_exclude":["/var/log","/var/cache","/proc","/sys","/dev","/run","/tmp"],"extra_paths":[],"mounts":[]}'
R backup --plugin host --instance srv --config "$CFG" | tee "$W/b1.txt"
S1=$(R list | tail -1 | awk '{print $1}')
R show "$S1" | tee "$W/show1.txt"
grep -q "system.tar" "$W/show1.txt" && grep -q "apps.tar" "$W/show1.txt" && grep -q "homes.tar" "$W/show1.txt" || fail "нет частей system/apps/homes"
grep -q "mount-" "$W/show1.txt" && fail "неотмеченный диск попал в копию"

say "Состав: в каталог, без замены системы"
R restore --plugin host --instance srv --snapshot "$S1" --dest "$W/out" --params "{\"mode\":\"files\",\"root\":\"$W/out\"}"
[ -f "$W/out/etc/cron.d/hb-nightly" ] || fail "нет /etc/cron.d/hb-nightly"
[ -f "$W/out/etc/shadow" ] || fail "нет /etc/shadow"
[ -f "$W/out/opt/hbapp/run.sh" ] && [ -f "$W/out/opt/hbapp/lib/app.conf" ] || fail "нет приложения /opt/hbapp"
[ -f "$W/out/usr/local/bin/hb-nightly.sh" ] || fail "нет скрипта cron"
[ -f "$W/out/home/$U/.ssh/authorized_keys" ] || fail "нет ключей пользователя"
[ -e "$W/out/var/log/hbapp" ] && fail "/var/log попал в копию вопреки исключению"
[ -e "$W/out/mnt/hbdata/data.bin" ] && fail "диск данных попал в копию"
echo "ok: состав верный"

say "Потеря: пользователь, sudo и приложение удалены"
kill "$(cat "$W/app.pid")" 2>/dev/null || true
userdel -r $U; groupdel hbteam
rm -rf /opt/hbapp /etc/sudoers.d/hbtest
id $U 2>/dev/null && fail "пользователь не удалён"

say "Восстановление пользователей: пароль, группы, ключи SSH, sudo"
R restore --plugin host --instance srv --snapshot "$S1" --dest / --params "{\"mode\":\"users\",\"users\":[\"$U\"],\"root_keys\":false}"
id $U | grep -q hbteam || fail "пользователь не в группе hbteam"
[ "$(id -u $U)" = 4712 ] || fail "UID не сохранён"
[ "$(getent shadow $U | cut -d: -f2)" = "$HASH" ] || fail "хэш пароля не восстановлен"
[ "$(stat -c '%a %U' /home/$U/.ssh/authorized_keys)" = "600 $U" ] || fail "права authorized_keys: $(stat -c '%a %U' /home/$U/.ssh/authorized_keys)"
[ "$(stat -c '%a' /home/$U/.ssh)" = 700 ] || fail "права ~/.ssh"
[ -f /etc/sudoers.d/hbtest ] || fail "правило sudo не восстановлено"
echo "ok: $U восстановлен с паролем, ключами и sudo"

say "Защита: ~/.ssh пользователя — ссылка на /root/.ssh, восстановление не пишет через неё"
mkdir -p /root/.ssh; touch /root/.ssh/authorized_keys
BEFORE=$(sha256sum /root/.ssh/authorized_keys | cut -d' ' -f1)
rm -rf /home/$U/.ssh; ln -s /root/.ssh /home/$U/.ssh; chown -h $U:$U /home/$U/.ssh
R restore --plugin host --instance srv --snapshot "$S1" --dest / --params "{\"mode\":\"users\",\"users\":[\"$U\"],\"root_keys\":false}" | tee "$W/sym.txt" | grep -i "ссылка" || true
[ "$(sha256sum /root/.ssh/authorized_keys | cut -d' ' -f1)" = "$BEFORE" ] || fail "запись через ссылку пользователя в /root/.ssh"
[ "$(stat -c %U /root/.ssh)" = root ] || fail "владелец /root/.ssh изменился"
grep -q "символьная ссылка" "$W/sym.txt" || fail "нет предупреждения о ссылке"
rm -f /home/$U/.ssh
echo "ok: через ссылку пользователя ничего не записано"

say "Ключи root дописываются, а не заменяют текущие"
cp /root/.ssh/authorized_keys "$W/root_keys.bak"
echo "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICurrentRestoreKey current@target" >> /root/.ssh/authorized_keys
R restore --plugin host --instance srv --snapshot "$S1" --dest / --params "{\"mode\":\"users\",\"users\":[\"$U\"],\"root_keys\":true}" >/dev/null
grep -q "current@target" /root/.ssh/authorized_keys || fail "текущий ключ root потерян"
cp "$W/root_keys.bak" /root/.ssh/authorized_keys
echo "ok: текущий ключ root на месте"

say "Восстановление приложения на место (только /opt/hbapp)"
R restore --plugin host --instance srv --snapshot "$S1" --dest / --params '{"mode":"files","root":"/","streams":["apps"],"only":["/opt/hbapp"]}'
[ -x /opt/hbapp/run.sh ] && grep -q "config=1" /opt/hbapp/lib/app.conf || fail "приложение не восстановлено"
echo "ok: /opt/hbapp на месте"

say "Отмеченный диск — отдельной частью"
R backup --plugin host --instance srv --config '{"home":"ssh","auto_apps":false,"mounts":["/mnt/hbdata"],"exclude":["/var/log"]}' | tail -2
S2=$(R list | tail -1 | awk '{print $1}')
R show "$S2" | grep -q "mount-mnt_hbdata.tar" || fail "нет части mount-mnt_hbdata.tar"
R restore --plugin host --instance srv --snapshot "$S2" --dest "$W/out2" --params "{\"mode\":\"files\",\"root\":\"$W/out2\",\"streams\":[\"mount-mnt_hbdata\"]}"
cmp "$W/out2/mnt/hbdata/data.bin" "$W/disk/data.bin" || fail "диск данных восстановлен неверно"
echo "ok: диск данных совпал"

printf '\n\033[1;32mПРОВЕРКА ПЛАГИНА «СЕРВЕР LINUX ЦЕЛИКОМ» ПРОЙДЕНА\033[0m\n'
