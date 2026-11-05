# =====================================================================
# deploy.ps1 — установка/обновление HopeBackup на Linux-сервер с Windows.
#
#   powershell -ExecutionPolicy Bypass -File C:\HopeBackup\_deploy\deploy.ps1 -Ip 10.0.0.5 -User ubuntu -Mode server
#   ... -Mode node                                    # узел хранения на площадке
#   ... -Mode agent -Server https://10.0.0.5:4443 -Token … -Fingerprint …   # агент (команда — в панели)
#   ... -Mode agent ... -Vpn openvpn,wireguard                                # + пакеты VPN-коннекторов
#
# Нужны plink.exe и pscp.exe (PuTTY): ищутся в C:\HopeBackup\tools\PuTTY, C:\HopeDFS\tools\PuTTY,
# C:\HopeDC\build\PuTTY и в PATH. Пароль спрашивается при запуске и никуда не сохраняется.
# =====================================================================
param(
  [Parameter(Mandatory = $true)][string]$Ip,
  [string]$User = 'ubuntu',
  [string]$Password = '',
  [ValidateSet('server', 'node', 'agent')][string]$Mode = 'server',
  [string]$Server = '',
  [string]$Token = '',
  [string]$Fingerprint = '',
  # Пакеты VPN-коннекторов: openvpn,wireguard,l2tp,ipsec
  [string]$Vpn = ''
)
$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Gz = Join-Path $Root 'bin\hopebackup.gz'
$Inst = Join-Path $Root 'linux\install.sh'
$Log = Join-Path $PSScriptRoot ("deploy_{0}_{1}.log" -f $Ip, $Mode)
Remove-Item $Log -ErrorAction SilentlyContinue
function Log($m) { $m | Tee-Object -FilePath $Log -Append }

function Find-Tool($name) {
  foreach ($d in @("$Root\tools\PuTTY", 'C:\HopeDFS\tools\PuTTY', 'C:\HopeDC\build\PuTTY')) { if (Test-Path "$d\$name") { return "$d\$name" } }
  $c = Get-Command $name -ErrorAction SilentlyContinue
  if ($c) { return $c.Source }
  throw "Не найден $name — положите PuTTY (plink.exe, pscp.exe) в $Root\tools\PuTTY"
}
$Plink = Find-Tool 'plink.exe'
$Pscp = Find-Tool 'pscp.exe'
if (-not (Test-Path $Gz)) { throw "Нет бинарника $Gz" }
if ($Mode -eq 'agent' -and (-not $Server -or -not $Token)) { Log "Для агента укажите -Server и -Token (команда — в панели: Агенты → Подключить агент). Агент будет установлен без подключения." }
if (-not $Password) {
  $sec = Read-Host "Пароль $User@$Ip" -AsSecureString
  $Password = [Runtime.InteropServices.Marshal]::PtrToStringAuto([Runtime.InteropServices.Marshal]::SecureStringToBSTR($sec))
}

# Отпечаток ключа SSH-сервера (доверие при первом подключении).
$probe = (& $Plink -ssh -batch -pw $Password "$User@$Ip" "exit" 2>&1 | Out-String)
$HK = ([regex]::Match($probe, 'SHA256:[A-Za-z0-9+/=]+')).Value
$hkArgs = @()
if ($HK) { $hkArgs = @('-hostkey', $HK); Log "Ключ сервера: $HK" }

$check = (& $Plink -ssh -batch @hkArgs -pw $Password "$User@$Ip" "uname -m" 2>$null | Select-Object -Last 1)
if (-not $check) { Log "!!! Не удалось подключиться к $User@$Ip по SSH"; Log $probe; Read-Host 'Enter — выход'; exit 1 }
$Arch = switch ($check.Trim()) { 'x86_64' { 'x86_64' } 'amd64' { 'x86_64' } 'aarch64' { 'aarch64' } 'arm64' { 'aarch64' } default { '' } }
if (-not $Arch) { Log "!!! Архитектура $check не поддерживается (есть сборки для x86_64 и aarch64)"; Read-Host 'Enter — выход'; exit 1 }
Log "Архитектура: $Arch"
# Комплект сборок (bin\dist): серверу — весь (для машин других архитектур и Alpine), агенту и узлу — своей архитектуры.
$Dist = Join-Path $Root 'bin\dist'
$DistFiles = @()
if (Test-Path $Dist) {
  $DistFiles = Get-ChildItem $Dist -Filter 'hopebackup-*.gz' | Where-Object { $Mode -eq 'server' -or $_.Name -like "hopebackup-$Arch-*" }
}
if ($Arch -ne 'x86_64' -and -not $DistFiles) { Log "!!! Нет сборки для $Arch в $Dist"; Read-Host 'Enter — выход'; exit 1 }

Log "=== 1. Копирование hopebackup.gz и install.sh ==="
$sha = (Get-FileHash -Algorithm SHA256 $Gz).Hash.ToLower()
& $Pscp -batch @hkArgs -pw $Password $Gz "$User@${Ip}:/tmp/hopebackup.gz" 2>&1 | Tee-Object -FilePath $Log -Append
& $Pscp -batch @hkArgs -pw $Password $Inst "$User@${Ip}:/tmp/hopebackup-install.sh" 2>&1 | Tee-Object -FilePath $Log -Append
if ($DistFiles) {
  & $Plink -ssh -batch @hkArgs -pw $Password "$User@$Ip" "rm -rf /tmp/hopebackup-dist; mkdir -p /tmp/hopebackup-dist" 2>&1 | Out-Null
  foreach ($f in $DistFiles) {
    Log "   + $($f.Name)"
    & $Pscp -batch @hkArgs -pw $Password $f.FullName "$User@${Ip}:/tmp/hopebackup-dist/$($f.Name)" 2>&1 | Tee-Object -FilePath $Log -Append
  }
}

$script = @'
set -e
S=$(sha256sum /tmp/hopebackup.gz | awk '{print $1}')
[ "$S" = "__SHA__" ] || { echo "SHA_FAIL $S"; exit 1; }
gunzip -f /tmp/hopebackup.gz && chmod +x /tmp/hopebackup
sed -i 's/\r$//' /tmp/hopebackup-install.sh
command -v bash >/dev/null 2>&1 || { command -v apk >/dev/null 2>&1 && echo '__PASS__' | sudo -S -p '' apk add -q bash; }
echo '__PASS__' | sudo -S -p '' env BIN_SRC=/tmp/hopebackup DIST_SRC=/tmp/hopebackup-dist MODE=__MODE__ SERVER='__SERVER__' TOKEN='__TOKEN__' FINGERPRINT='__FP__' VPN='__VPN__' bash /tmp/hopebackup-install.sh
echo DEPLOY_OK
'@
$script = $script.Replace('__SHA__', $sha).Replace('__PASS__', $Password.Replace("'", "'\''")).Replace('__MODE__', $Mode).Replace('__SERVER__', $Server).Replace('__TOKEN__', $Token).Replace('__FP__', $Fingerprint).Replace('__VPN__', ($Vpn -replace '[^a-z0-9,]', '')).Replace("`r`n", "`n")
$b64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script))
Log "=== 2. Установка роли «$Mode» на $Ip ==="
& $Plink -ssh -batch @hkArgs -pw $Password "$User@$Ip" "echo $b64 | base64 -d > /tmp/hb-deploy.sh && sh /tmp/hb-deploy.sh; rm -f /tmp/hb-deploy.sh" 2>&1 | Tee-Object -FilePath $Log -Append

if (Select-String -Path $Log -Pattern 'DEPLOY_OK' -Quiet) {
  Log ""
  switch ($Mode) {
    'server' { Log "Готово. Панель: https://${Ip}:4443/ (пароль Administrator — выше в журнале)" }
    'node'   { Log "Готово. Узел площадки: https://${Ip}:8443 — токены и отпечаток выше; добавьте площадку в панели." }
    'agent'  { Log "Готово. Агент установлен на $Ip — проверьте раздел «Агенты» в панели." }
  }
} else {
  Log "!!! Установка не завершилась — см. $Log"
}
Read-Host 'Enter — закрыть'
