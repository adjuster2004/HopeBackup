@chcp 65001 >nul
@echo off
rem Пример: deploy.cmd 10.0.0.5 ubuntu server
rem         deploy.cmd 10.0.0.9 ubuntu node
rem         deploy.cmd 10.0.0.21 ubuntu agent https://10.0.0.5:4443 ТОКЕН ОТПЕЧАТОК
rem         deploy.cmd 10.0.0.21 ubuntu agent https://10.0.0.5:4443 ТОКЕН ОТПЕЧАТОК openvpn,wireguard
if "%~1"=="" (
  echo Использование: deploy.cmd IP [пользователь] [server^|node^|agent] [сервер токен отпечаток]
  pause
  exit /b 1
)
set U=%~2
if "%U%"=="" set U=ubuntu
set M=%~3
if "%M%"=="" set M=server
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0deploy.ps1" -Ip %1 -User %U% -Mode %M% -Server "%~4" -Token "%~5" -Fingerprint "%~6" -Vpn "%~7"
