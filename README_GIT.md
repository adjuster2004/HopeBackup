# HopeBackup «Ковчег» — работа с репозиторием на GitHub

Как положить HopeBackup «Ковчег» из `C:\HopeBackup` на github.com и дальше выпускать версии. Команды — для
Windows (PowerShell или Git Bash), папка проекта — `C:\HopeBackup`.

## Что попадает в репозиторий, а что нет

В репозиторий идут **исходники**: ядро (`hopebackup-core`), панель (`admin/src`), тесты (`tests`),
документация в Markdown (`docs/*.md`), установщики (`linux`), развёртывание с Windows (`_deploy/*.ps1, *.cmd`),
`Cargo.lock` и `package-lock.json` (точные версии зависимостей). Около 4 МБ.

Не идут (правила — в `.gitignore`):

| Что | Почему | Где взять |
|---|---|---|
| `bin/` — программы и архив выпуска | десятки МБ на каждую версию; GitHub не принимает файлы больше 100 МБ | **GitHub Releases** (ниже) |
| `admin/dist/` — собранная панель | собирается из `admin/src` | `cd admin && npm ci && npm run build` |
| `docs/*.html` | собирается из `docs/*.md` | `python3 hopebackup-core/tools/gen_docs.py` |
| `hopebackup-core/target*/`, `admin/node_modules/` | сборка и зависимости | `cargo build`, `npm ci` |
| `_deploy/*.log`, `*.log` | **в журнале развёртывания — пароль администратора и токены узла** | — |
| `*.pem`, `*.key`, `.env`, `initial_admin_password.txt`, `node.json`, `state.json` | секреты | — |

Исключения из правила про `*.pem`: открытый ключ вендора `hopebackup-core/src/server/vendor_public.pem` (нужен
сборке) и тестовые ключи `tests/mocks/vendor_test_*.pem` — ими подписываются только тестовые пакеты, рабочие
сборки им не доверяют.

`.gitattributes` закрепляет окончания строк: скрипты `.sh` всегда с LF (с CRLF установщик на Linux не
запустится), `.ps1` и `.cmd` — с CRLF.

**Закрытый ключ подписи обновлений вендора в эту папку не класть никогда** — ни в репозиторий, ни рядом.

## 1. Один раз: GitHub

1. Войдите на github.com (или зарегистрируйтесь).
2. **Скрыть личную почту в коммитах:** Settings → Emails → «Keep my email addresses private». Ниже на этой
   странице будет адрес вида `12345678+имя@users.noreply.github.com` — его и укажете в Git (шаг 2).
3. **Создать репозиторий:** «+» → New repository →
   * Repository name: `HopeBackup`;
   * **Private** — рекомендуется: в документации и тестах есть подробности устройства системы;
   * **без** README, .gitignore и лицензии (всё уже есть в папке) → Create repository.
   Адрес будет `https://github.com/<имя>/HopeBackup.git`.

## 2. Один раз: компьютер с Windows

1. Установите **Git for Windows** — git-scm.com → Download. В установщике можно оставить всё по умолчанию;
   в нём есть Git Credential Manager — вход в GitHub через браузер, без паролей в командной строке.
2. Откройте PowerShell и задайте имя и почту для коммитов:

   ```powershell
   git config --global user.name "Ваше Имя"
   git config --global user.email "12345678+имя@users.noreply.github.com"
   git config --global core.quotepath off      # русские имена файлов в git status — буквами, а не кодами
   git config --global init.defaultBranch main
   ```

3. (По желанию) **GitHub CLI** — gh: cli.github.com. Удобен для выпусков: `gh auth login` → GitHub.com →
   HTTPS → войти через браузер.

Вход по SSH-ключу вместо браузера: `ssh-keygen -t ed25519 -C "hopebackup"` → содержимое
`%USERPROFILE%\.ssh\id_ed25519.pub` — в GitHub → Settings → SSH and GPG keys → New SSH key; адрес
репозитория тогда `git@github.com:<имя>/HopeBackup.git`.

## 3. Один раз: первая отправка

```powershell
cd C:\HopeBackup
git init
git add -A
git status            # проверьте список: нет bin\, admin\dist\, *.log, target
```

Перед первым коммитом убедитесь, что секретов нет:

```powershell
git ls-files | Select-String -Pattern "\.log$|password|\.env|state\.json|node\.json"
```

Вывод должен быть пустым. Затем:

```powershell
git commit -m "HopeBackup 0.9.4"
git remote add origin https://github.com/<имя>/HopeBackup.git
git push -u origin main
```

При первом `push` откроется окно входа в GitHub — войдите, Git запомнит вход в «Диспетчере учётных данных
Windows».

## 4. Один раз: настройки репозитория на GitHub

В репозитории → **Settings**:

* **General → Features:** Issues — по желанию (задачи), Wiki — не нужна (документация в `docs/`).
* **Rules → Rulesets → New branch ruleset** для `main`: включите «Restrict deletions» и «Block force pushes» —
  ветку `main` нельзя будет случайно удалить или переписать. Если работаете не один — ещё «Require a pull
  request before merging».
* **Collaborators** — пригласить коллег (роль Write — отправлять изменения, Read — только читать).
* **Code security:** включите **Dependabot alerts** (уязвимости в зависимостях Rust и npm) и, если доступно
  для вашего плана, **Secret scanning** и **Push protection** — GitHub не даст отправить коммит с токеном или
  ключом.

## 5. Каждая новая версия

Когда новая версия записана в `C:\HopeBackup` (исходники, `bin\`, документация):

```powershell
cd C:\HopeBackup
git status                         # что изменилось
git add -A
git commit -m "0.9.5: кратко, что нового"
git tag v0.9.5
git push
git push --tags
```

**Выпуск с программами** (они не в git) — GitHub → Releases → «Draft a new release» → тег `v0.9.5` → заголовок
`HopeBackup 0.9.5` → в описание — раздел версии из `docs/05_Дорожная_карта.md` → прикрепите файлы:

* `hopebackup-0.9.5-linux.tar.gz` (архив выпуска — он присылается в чат, в `bin\release` его нет);
* `bin\release\SHA256SUMS` и `bin\release\hopebackup-install.sh`.

Или одной командой через GitHub CLI (архив предварительно положите в `bin\release`):

```powershell
gh release create v0.9.5 bin\release\hopebackup-0.9.5-linux.tar.gz bin\release\SHA256SUMS bin\release\hopebackup-install.sh `
  --title "HopeBackup 0.9.5" --notes "Что нового — docs/05_Дорожная_карта.md"
```

Release — то, что ставится на серверы: архив и `hopebackup-install.sh` (см. `docs/01_Установка.md`), или архив —
в разделе «Обновления» панели.

## 6. Работа с другого компьютера и сборка из репозитория

```bash
git clone https://github.com/<имя>/HopeBackup.git
cd HopeBackup
git pull                                   # забрать свежие изменения
```

Сборка на Linux (нужны Rust — rustup.rs, Node.js 22):

```bash
cd admin && npm ci && npm run build && cd ..          # панель → admin/dist (иначе в программе заглушка панели)
cd hopebackup-core && cargo build --release           # программа → target/release/hopebackup
python3 tools/gen_docs.py                             # HTML-документация (по желанию)
```

Подробнее о сборке, стенде и проверках — «Как запускать.txt»; сквозные проверки `tests/*.sh` требуют root.

## 7. Если в репозиторий попал секрет

1. **Сразу смените его** — пароль, токен узла (новый токен: `hopebackup node token …`), ключ. Удаление из
   истории не отменяет того, что репозиторий могли уже скачать.
2. Удалите файл и добавьте правило в `.gitignore`; из истории — `git filter-repo` (github.com/newren/git-filter-repo)
   и `git push --force` (временно разрешив его в Rulesets).

## 8. Если репозиторий подключить к Claude

Если GitHub подключён к Claude, а репозиторий выдан сессии, изменения можно отправлять прямо в репозиторий —
отдельной веткой с pull request, который вы проверяете и сливаете на GitHub. Тогда шаг 5 (кроме выпуска с
программами) делать вручную не нужно.
