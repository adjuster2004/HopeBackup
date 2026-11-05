# HopeBackup — REST API сервера управления

Все адреса — относительно `https://<сервер>:4443`. Ответы — JSON. Ошибки: HTTP 4xx/5xx и тело
`{"error": "текст по-русски"}`. Авторизация панели: заголовок `Authorization: Bearer <token>` (из `/api/auth/login`).
Время — строки RFC 3339 (UTC). Размеры — байты (числа).

Роли: `admin` (всё), `operator` (просмотр + изменения, без пользователей), `restore` (просмотр + восстановление),
`auditor` (только просмотр). Права в `me.perms`: `view`, `edit`, `restore`, `admin`.

## Вход

| Метод | Путь | Тело / ответ |
|---|---|---|
| POST | `/api/auth/login` | `{user, password}` → `{token, user, role}` |
| GET | `/api/auth/me` | → `{user, role, perms: string[], version, hostname}` |

## Обзор

`GET /api/status` →
```json
{
  "version": "0.1.0", "hostname": "backup-01", "uptime": 3600,
  "counts": {"instances": 3, "agents": 2, "agents_online": 2, "sites": 1, "connectors": 1, "targets": 1},
  "jobs24": {"ok": 10, "failed": 1, "running": 0},
  "transfer24": {"read": 123456, "new": 23456, "uploaded": 12345},
  "worst_rpo": {"instance_id": "…", "name": "…", "seconds": 2820, "target_seconds": 900} | null,
  "recent_failures": [{"job_id": "…", "title": "…", "error": "…", "finished": "…"}],
  "next_runs": [{"instance_id": "…", "name": "…", "kind": "full", "at": "…"}]
}
```

## Каталог

`GET /api/catalog` →
```json
{
  "products": [{
    "id": "postgresql", "plugin": "postgresql", "title": "PostgreSQL 14+", "tile": "PG",
    "category": "СУБД", "description": "…", "modes": "полная · архив WAL · pg_dump",
    "available": true,
    "schema": {"type": "object", "properties": {"bin_dir": {"type": "string", "title": "…", "description": "…", "default": "…"},
                "port": {"type": "integer", …}, "password": {"type": "string", "format": "secret", …},
                "databases": {"type": "array", "items": {"type": "string"}, …}}},
    "defaults": {"port": 5432}
  }],
  "connector_kinds": [{"id": "ssh_tunnel", "title": "SSH-туннель (-L)", "description": "…", "available": true, "schema": {…}}],
  "target_kinds": [{"id": "ssh", "title": "Сервер по SSH", "available": true}, {"id": "agent", …}, {"id": "proxmox", "available": false}, …]
}
```
Схема — подмножество JSON Schema: типы `string`, `integer`, `boolean`, `array` строк; `format: "secret"` — поле
пароля (в ответах сервера секреты не возвращаются: вместо значения `"********"`, если задано).
Записи с `available: false` показываются в каталоге как «в разработке» и не выбираются.

## Экземпляры продуктов

Экземпляр:
```json
{
  "id": "pg-db-prod-01", "name": "PostgreSQL · db-prod-01", "catalog_id": "postgresql", "plugin": "postgresql",
  "agent_id": "…", "site_id": "…", "config": {…},
  "schedule": {"enabled": true, "full": "0 1 * * *", "dump": null},
  "retention": {"keep_last": 7},
  "rpo_target_min": 15,
  "created": "…",
  "runtime": {
    "status": "ok" | "warn" | "error" | "never",
    "last_backup": {"snapshot_id": "…", "time": "…", "kind": "full", "size": 1, "new_bytes": 1, "uploaded": 1} | null,
    "last_job": {"id": "…", "status": "ok", "finished": "…", "error": null} | null,
    "rpo_seconds": 120 | null,
    "wal": {"queued": 0, "last_shipped": "000000010000000000000005", "last_time": "…", "last_error": null, "shipped": 12} | null,
    "next_run": "…" | null
  }
}
```

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/instances` | → `Instance[]` |
| POST | `/api/instances` | `{name, catalog_id, agent_id, site_id, config, schedule, retention, rpo_target_min}` → `Instance` |
| GET | `/api/instances/:id` | → `Instance` |
| PUT | `/api/instances/:id` | как POST → `Instance` |
| DELETE | `/api/instances/:id` | → `{ok: true}` |
| POST | `/api/instances/:id/backup` | `{kind: "full" \| "dump" \| "files", retry?: bool}` → `{job_id}`; `retry: true` — повторять по политике при ошибке (`origin: "manual_retry"`) |
| POST | `/api/instances/:id/discover` | → `{job_id}` (результат — в `result` задания) |
| POST | `/api/instances/:id/action` | `{name: "setup-wal"}` — настроить архив WAL; `{name: "restart"}` — перезапустить PostgreSQL (агент сам выбирает способ; 400, пока у экземпляра идут задания) → `{job_id}` |
| GET | `/api/instances/:id/points` | → `{snapshots: SnapshotHeader[], wal: {count, first, last, bytes, last_time} \| null, pitr: {from, to} \| null}` |

`SnapshotHeader`: `{id, time, instance, product, kind, host, size, new_bytes, uploaded, chunks, extra}`.

## Площадки (узлы хранения)

Площадка: `{id, name, url, fingerprint, repo, connector_ids: string[], compression, has_token, has_password, has_prune_token, created}`.

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/sites` | → `Site[]` |
| POST | `/api/sites` | `{name, url, fingerprint, repo, token, password, prune_token?, connector_ids, compression}` → `Site` |
| PUT | `/api/sites/:id` | те же поля; пустые секреты — не менять |
| DELETE | `/api/sites/:id` | → `{ok}` |
| POST | `/api/sites/probe` | `{url}` → `{fingerprint}` — отпечаток сертификата узла для подтверждения |
| POST | `/api/sites/:id/test` | → `{ok, via, repo_exists, stats: RepoStats \| null}` |
| POST | `/api/sites/:id/init` | → `{job_id}` — создать репозиторий паролем площадки |

`RepoStats`: `{packs, pack_bytes, chunks, snapshots, objects, object_bytes}`.

## Коннекторы

Коннектор: `{id, name, kind: "ssh_tunnel" | "direct", params: {host, port, user, remote_host, remote_port, jump, has_key}, created, status: {up, local_port, error} | null}`.

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/connectors` | → `Connector[]` |
| POST | `/api/connectors` | `{name, kind, params: {host, port, user, private_key, remote_host, remote_port, jump}}` |
| PUT | `/api/connectors/:id` | те же поля; пустой `private_key` — не менять |
| DELETE | `/api/connectors/:id` | → `{ok}` |
| POST | `/api/connectors/:id/test` | → `{ok, local_port}` или ошибка |

## Агенты

Агент: `{id, name, host, os, version, enrolled, last_seen, online, wal: [{instance_id, queued, last_shipped, last_time, last_error, shipped}]}`.

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/agents` | → `Agent[]` |
| POST | `/api/agents/enroll-token` | `{note}` → `{token, expires, command, server_fingerprint}` |
| DELETE | `/api/agents/:id` | → `{ok}` |

## Цели восстановления

Цель: `{id, name, kind: "ssh" | "agent", host, port, user, agent_id, has_key, created}`.

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/targets` | → `Target[]` |
| POST | `/api/targets` | `{name, kind, host, port, user, private_key, agent_id}` |
| DELETE | `/api/targets/:id` | → `{ok}` |
| POST | `/api/targets/:id/test` | → `{ok, output}` |

## Восстановление

`POST /api/restore` (право `restore`):
```json
{
  "instance_id": "…", "snapshot_id": "…", "target_id": "…", "via": "direct" | "server",
  "params": {"pgdata": "/var/lib/postgresql/16/restore", "bin_dir": "", "os_user": "postgres",
             "target_time": "2026-10-24 14:31:00+03" | "", "port": 5433, "start": true,
             "mode": "cluster" | "dump", "database": "", "root": "/", "verify": true, "reload": false}
}
```
→ `{job_id}`. `via: "server"` — данные идут с площадки через сервер управления (обратный SSH-туннель), цели не нужен
прямой доступ к площадке.

## Переезд (0.8)

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/migrations` | Список: `id, name, platform, phase` (`syncing`, `testing`, `cutover`, `running`, `done`), `source_agent`, `target_agent`, `params` (секреты — `********`), `state` (ВМ и тома цели, `applied`, `dirty`, `testing`, `running`), `last_sync {id, time, cutover}`, `busy {job, step}`, `pending_apply`, `events[]` (новые первыми), `schedule`, `source_config` |
| POST | `/api/migrations` | `{name, source_agent, site_id, source: {disks[], stop_services[]} \| {source:"vsphere", url, user, password, vm, insecure, guest_user, guest_password, hard_poweroff}, sync_cron, target_agent, platform: "proxmox" \| "ovirt", params: {storage, networks: {"*": "vmbr0"}, name, cores, memory_mb, vmid; для oVirt: url, user, password, cluster, insecure}, auto_apply, shutdown_source}` — создаёт экземпляр «replica» на агенте источника |
| GET, PUT, DELETE | `/api/migrations/:id` | PUT: `{name, params, auto_apply, shutdown_source, target_agent, source}` (секреты «********» — не менять). DELETE запрещён, пока ВМ на цели работает после переключения; ВМ и копии не удаляются |
| POST | `/api/migrations/:id/:op` | `op`: `sync`, `apply`, `prepare` (подготовка к KVM), `test-start`, `test-stop`, `cutover`, `rollback`, `finish` → `{job_id}` первого задания; следующие шаги сервер ставит сам |

## Обновления (0.8.1, только администратор)

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/updates` | `current_version, build, target` (сборка сервера), `latest_version, update_available, notes, notes_kind, last_checked`, `channel, source_url, auto_check`, `offline_package {filename, version, size, file_sha256, kit[]}`, `target_version, target_is_downgrade, target_is_same`, `job {phase, progress, running, finished, failed, message, target_version, from_version}`, `updater_installed, updater_note`, `kit[]` (сборки комплекта), `agents[] {id, name, version, build, online, outdated, kit_has, update, last_job}` |
| POST | `/api/updates/check` | Проверить манифест → `{update_available, latest_version, note, kind}` |
| PUT | `/api/updates/config` | `{channel: "stable"\|"beta", source_url, auto_check}` |
| POST | `/api/updates/upload?filename=…&version=…` | Тело — архив выпуска `.tar.gz`, `hopebackup.gz` или программа (до 512 МБ) → `{version, size, file_sha256, kit[], note}` |
| POST | `/api/updates/upload/start` | Загрузка частями: `{filename, size, sha256, version, resume}` → `{id, received, size, chunk, resumed}`; `resume` — id прерванной загрузки того же файла |
| PUT | `/api/updates/upload/:id?offset=N` | Часть (до `chunk` байт) с позиции `N` → `{received, size}`; позиция не совпала — ничего не пишется, в ответе принятый объём |
| POST | `/api/updates/upload/:id/finish` | Сверка размера и SHA-256, проверка пакета → ответ как у `/api/updates/upload` |
| POST | `/api/updates/apply` | Установить загруженный пакет или найденную онлайн версию; ход — `job` в `GET /api/updates` |
| POST | `/api/updates/discard`, `/api/updates/dismiss` | Удалить загруженный пакет; скрыть итог установки |
| POST | `/api/updates/agents` | `{ids: []}` (пусто — все устаревшие на связи) → `{jobs: [{agent, name, job_id \| error}]}` — задания `self_update` |

Агент скачивает программу: `GET /api/agent/package/<сборка>` (токен агента), заголовок `x-hopebackup-version`.

## Задания

Задание: `{id, kind, title, instance_id, agent_id, status: "queued"|"running"|"ok"|"failed", created, started, finished, error, result, log?: [{t, m}]}`.

С 0.6 у задания копирования также: `origin` — `schedule` | `retry` | `catchup` | `manual` | `manual_retry`; `attempt` —
номер попытки (1 — первая); `mode` — вид копии; `interrupted` — прервано (падение агента или сервера), итог не получен;
`await_until` — до какого времени сервер ждёт итог от агента после своего перезапуска; `retry_decided` — решение о
повторе принято; `retry_at` — когда будет повтор; `retry_job` — id повторного задания или `"skipped"` (копия уже
есть). В `result` сверенного задания — `{snapshot, reconciled: true}`.

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/jobs?instance_id=&limit=` | → `Job[]` без `log`, новые первыми |
| GET | `/api/jobs/:id` | → `Job` с `log` |
| GET | `/api/suggestions` | → `[{agent_id, agent_name, id, title, reason, catalog_id, details, priority}]` — найденное агентами и ещё не защищённое |
| POST | `/api/suggestions/apply` | `{agent_id, id, site_id}` — создать экземпляр по находке → `{ok, id, name}` |
| POST | `/api/suggestions/dismiss` | `{agent_id, id}` — не предлагать эту находку для этого агента |
| POST | `/api/jobs/ack` | `{ids: [...]}` — скрыть ошибки заданий с «Обзора» (задание получает `ack: {user, at}`) → `{ok, hidden}` |

## Аудит и пользователи

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/audit?limit=` | → `[{time, user, action, detail}]`, новые первыми |
| GET | `/api/audit/page?q=&from=&to=&offset=&limit=` | Страница журнала с фильтром по всему журналу → `{items, total, offset, limit}`; `from`/`to` — RFC 3339 |
| GET | `/api/audit/export?q=&from=&to=&format=csv\|json` | Файл журнала с тем же фильтром: CSV (UTF-8 с BOM, разделитель «;», для Excel) или JSON; сама выгрузка пишется в журнал (`audit_export`) |
| GET | `/api/users` | → `[{name, role, created}]` |
| POST | `/api/users` | `{name, password, role}` |
| PUT | `/api/users/:name/password` | `{password}` |
| DELETE | `/api/users/:name` | → `{ok}` |

## Площадки: S3, репликация (0.2)

* `POST /api/sites` и `PUT /api/sites/:id` — также `kind: "node" | "s3"`, `s3: { provider: "yandex"|"aws"|"s3", endpoint, region, bucket, prefix, access_key, secret_key, admin_access_key, admin_secret_key, path_style, lock_mode: ""|"GOVERNANCE"|"COMPLIANCE", lock_days, storage_class, fingerprint }`, `replicas: [site_id]`, `replication_interval_min`. Секреты в ответе — `"********"`.
* `GET /api/sites` — дополнительно `replication: [{ site, state: { ok, time, finished, error, stats } }]`, `override` (доступ с этого узла).
* `POST /api/sites/:id/replicate` → `{ job_id }` — синхронизировать принимающие площадки сейчас.
* `POST /api/sites/:id/failover` `{ to }` → `{ instances }` — перевести экземпляры на принимающую площадку.
* `GET /api/instances/:id/points?site=<id>` — точки на основной или принимающей площадке; `POST /api/restore` — поле `site_id`.

## Коннекторы и цели (0.2)

* Виды коннекторов: `direct`, `ssh_tunnel`, `openvpn`, `wireguard`, `l2tp_ipsec`, `ipsec` (схемы — в `GET /api/catalog`, `connector_kinds`). `POST /api/connectors/wg-keygen` → `{ private_key, public_key }`. `POST /api/connectors/:id/test` → `{ ok, iface, address, routes, local_port, implementation, last_handshake_seconds, node: { ok, url, site, error } }`.
* Цели: `kind` — `ssh`, `agent`, `proxmox`, `vmware`, `yandex_cloud`, `aws`; `params` — по схеме из `target_kinds`. `PUT /api/targets/:id`. В `POST /api/restore` для ВМ-целей — `params.vm: { vm_name, cores, memory_mb, disk_gb, ip, destroy_on_failure }`; итог задания — `result.vm: { provider, id, name, ip, user, target_id }`.

## Узлы управления (0.2)

* `GET /api/cluster` → `{ node_id, name, role: "active"|"standby", epoch, self_url, active_url, last_sync, fingerprint, peers: [{ id, name, url, fingerprint, joined, self, sync }], site_overrides }`.
* `POST /api/cluster/invite` `{ self_url, name }` → `{ code, url, expires }` (активный узел, администратор).
* `POST /api/cluster/join` `{ code, self_url, name }` — подключить этот узел принимающим (каталог заменяется).
* `POST /api/cluster/takeover` → `{ epoch }` — принять управление (принимающий узел).
* `PUT /api/cluster/overrides/:site_id` `{ url, direct }` — доступ к площадке с этого узла.
* `DELETE /api/cluster/peers/:id`.
* `POST /api/cluster/offer` `{ self_url, name }` → `{ code, url, expires }` — код подключения нового узла (вводится на активном).
* `POST /api/cluster/adopt` `{ code, self_url, name, connector_id }` → `{ ok, id, name }` — активный узел подключает узел по его коду
  (сам заходит на него, через коннектор `connector_id`, если задан; тот же коннектор — для синхронизации и обновления).
* `POST /api/cluster/peers/:id/update` — обновить принимающий узел до версии активного; ход — в `peers[].update`
  (`{ running, ok, time, target, error }`), версия узла — `peers[].version`.
* Между узлами: `POST /api/cluster/peer/join` (одноразовый код), `POST /api/cluster/peer/sync` (подпись `x-hopebackup-sig` = HMAC-SHA256 секретом кластера; эпоха меньше своей → 409; ответ — `{ ok, version }`),
  `POST /api/cluster/peer/adopt` (одноразовый код подключения; только новый узел), `POST /api/cluster/peer/update` (архив выпуска;
  `x-hopebackup-ts`, `x-hopebackup-sha256`, `x-hopebackup-sig` = HMAC(`update|<id узла>|<ts>|<sha256>`); ±5 минут; только новее).
* На принимающем узле любые изменения, кроме восстановления, проверок и действий кластера, — `409`.

## Исключения, площадки на диске и в облаке, сервер целиком (0.3)

* `GET /api/settings` → `{ settings: { default_excludes: [..], retry, catch_up, catch_up_hours }, default_excludes_builtin: [..] }`; `PUT /api/settings` — только переданные поля из `{ default_excludes, retry, catch_up, catch_up_hours }` (администратор). Общие исключения подставляются в конфигурацию экземпляра как `global_exclude` (если у экземпляра не `use_global_exclude: false`); у экземпляра — `exclude`, `max_file_mb`.
* Площадка «диск сервера»: `POST /api/sites` `{ kind: "disk", name, password, disk_path: "/mnt/data/hopebackup", public_url: "https://сервер:4443" }` → `url = <public_url>/node/<id>`, токены и отпечаток сервера подставляются; в `GET /api/sites` — `disk: { path, total, used, free }`. Встроенный узел отвечает на `https://сервер:4443/node/<id>/v1/…` (токены узла).
* Облачная площадка: `POST /api/sites` `{ kind: "cloud", name, password, repo, cloud: { provider: "gdrive"|"yadisk"|"webdav", url, username, password, folder, folder_id, client_id, client_secret, access_token, oauth_flow, fingerprint } }`. Сервер проверяет доступ и заполняет `account`. Секреты в ответе — `"********"`, плюс `connected`.
* Вход OAuth по коду устройства: `POST /api/sites/oauth/start` `{ provider: "gdrive"|"yadisk", client_id, client_secret, site_id? }` → `{ flow, user_code, verification_url, expires_in }`; `GET /api/sites/oauth/:flow` → `{ status: "pending"|"done"|"error", error }`. Токены остаются на сервере: в `POST/PUT /api/sites` передаётся `cloud.oauth_flow`.
* `POST /api/sites/:id/test` для облака — дополнительно `cloud: { account, total, used, free }`.
* Плагин `host` («Сервер Linux целиком»): обнаружение возвращает опись `{ host, os, processes, apps, schedules, services, mounts, containers, users, listening, packages, products, db_data_dirs, warnings, plan }`; параметры восстановления — `restore_schema` в каталоге (`mode: users|files|full`, `users`, `streams`, `only`, `root`, `set_passwords`, `root_keys`, `sudoers`, `host_keys`, `install_packages`, `enable_services`, `start_services`).

## Серверы и ВМ (0.4)

* `GET /api/machines` → `[{ id, name, host, port, user, auth: "password"|"key", password, private_key, key_passphrase, sudo_password (маска), host_key, agent_id, agent, notes, last: { time, ok, error, os, arch, method, sudo, recommendations, estimate } }]`.
* `POST /api/machines` `{ name, host, port, user, auth, password | private_key, key_passphrase, sudo_password, notes, agent_id? }`; `PUT /api/machines/:id` (маска или пусто — не менять; при смене адреса или пользователя секреты вводятся заново); `DELETE` (администратор).
* `POST /api/machines/:id/test` → `{ ok, host_key, sudo, sudo_note, root, uname, os, libc }`; `POST /api/machines/:id/reset-key`.
* `POST /api/machines/:id/inventory` → `{ job_id }`; `GET /api/machines/:id/inventory` → `{ time, method: "ssh"|"shell", inventory, analysis }` (пароли в fstab и командных строках заменены на `********`). `inventory.system` — `{ os, kernel, boot, hardware, storage, network, security, time, uptime_secs, locale, repos }`; `analysis` — `{ summary, recommendations: [{ id, priority, title, reason, action, catalog_id, config, schedule, rpo_min, keep_last, available, details }], warnings, restore_notes, estimate }`.
* `POST /api/machines/:id/install-agent` `{ server_url }` → `{ job_id }`; `POST /api/machines/:id/apply` `{ recommendations: [id], site_id }` → `{ created: [{ id, name }], updated: [{ id, name }], skipped: [{ id, reason }] }` (существующие экземпляры не дублируются).
* Цели «сервер по SSH»: поле `password` (вход по паролю и sudo -S), в ответе — `has_password`.
* Ответ опроса агента: `server_port` — основной порт сервера (агент переходит на него с прежнего).

## Почта, уведомления, восстановление доступа (0.5)

Без входа (ограничение — 10 запросов за 15 минут с адреса на действие):

* `POST /api/auth/recover-request` `{ identifier }` (имя или адрес почты) → `{ ok: true }` всегда; ссылка — на почту учётной записи или в файл `password_reset_link.txt` на сервере.
* `GET /api/auth/recover-check?token=` → `{ ok, user, expires }` или 410.
* `POST /api/auth/recover-reset` `{ token, password }` → `{ ok, user }`; ссылка одноразовая, сеансы пользователя закрываются.
* `GET /api/auth/recovery-request` → `{ fingerprint, node, req, product }` — код запроса для вендора.
* `POST /api/auth/recover-emergency` `{ token }` (JWT RS256: `sub` = отпечаток, `exp`, `purpose: "admin-recovery"`, `product: "HopeBackup"`, `user`, `new_password`, `jti`) → `{ ok, user }`.

Администратор:

* `GET /api/mail` → `{ mail: { host, port, security, username, has_password, from, from_name, panel_url, panel_url_default, ca_pem, insecure_tls }, configured, notify: { enabled, recipients, events }, catalog: [{ id, group, title, text }], log: [{ at, event, subject, to, ok, error }] }`.
* `PUT /api/mail/smtp` `{ host, port, security: "starttls"|"tls"|"none", username, password (пусто — не менять), clear_password, from, from_name, panel_url, ca_pem, insecure_tls }`.
* `PUT /api/mail/notify` `{ enabled, recipients: [адрес], events: [id] }`.
* `POST /api/mail/test` `{ to, smtp? }` (настройки из формы, без сохранения); `POST /api/mail/test-notify` → `{ ok, to }`.
* Пользователи: `POST /api/users` принимает `email`; `PUT /api/users/:name` `{ role?, email? }`; `PUT /api/users/:name/password` → `{ ok, token }` (смена своего пароля — новый токен; сеансы закрываются). Токен сеанса содержит поколение сеансов пользователя: после смены или сброса пароля старые токены дают 401 «пароль изменён — войдите заново».
* `POST /api/machines/:id/test` дополнительно → `program` — какая сборка будет использована («aarch64 · musl (статическая)»).

## Повторы, догон, подготовка машины (0.6)

* Политика повторов `RetryPolicy`: `{ enabled: bool, attempts: 0–24, interval_min: 1–1440 }`; общая — `settings.retry`
  (по умолчанию `{true, 3, 60}`), догон — `settings.catch_up` (true), окно — `settings.catch_up_hours` (24).
* Экземпляр: `retry: RetryPolicy | null` (null — общая). В POST/PUT `/api/instances` — `retry_mode: "inherit" | "custom"`
  и `retry`; без `retry_mode` политика экземпляра не меняется.
* Анализ машины (`GET /api/machines/:id/inventory` → `analysis`): `prepare: [{ level: "bad"|"warn"|"info"|"ok", area,
  title, text, commands: [строка] }]` — что донастроить на машине (место под снимки LVM по группам томов, thin-пулы,
  btrfs/ZFS, память, процессор, место для агента, время, сеть). В `GET /api/machines` у `last` — `prepare_todo`: число
  пунктов `bad` и `warn`. В `inventory.system.storage` — `vgs`, `lvs`, `zpools`; в `hardware` — `mem_available`.
* Параметры файловых экземпляров (`files`, `host`): `snapshot: "auto"|"required"|"off"`, `snapshot_size_pct` (10),
  `pre_snapshot`, `post_snapshot`. В `extra` снапшота — `fs_snapshot` и `job` (id задания, по нему сервер сверяет
  потерянные итоги).

## Образ машины по SSH, развёртывание ВМ, скачивание образа (0.9)

Продукт **«Образ машины по SSH»** (`plugin: "machine"`, `catalog_id: "machine"`) снимает образ дисков
физической или виртуальной машины по SSH без агента: сервер управления (или агент) подключается к машине из
раздела «Серверы и ВМ», запускает на ней `hopebackup disk-read` и складывает диски блоками по 4 МиБ с
дедупликацией, шифрованием и репликацией. Тома LVM читаются согласованно через снимок (`lvcreate -s`),
остальные разделы — на ходу (с предупреждением), подкачка — нулями. В каждую следующую копию уходят только
изменившиеся блоки.

Параметры экземпляра: `{machine_id, disks[], lvm: "auto"|"required"|"off", freeze_small, freeze_timeout,
block_mib}`. Доступ SSH берётся по `machine_id` из раздела «Серверы и ВМ». Экземпляр запускается на агенте
или на сервере управления (`agent_id: "server"` — только для продуктов `machine` и `qcow2images`; другим
продуктам нужен агент, иначе `400`). Копии, выполненные сервером, как и копии агентов, запускают очистку по
хранению и репликацию на принимающие площадки.

Продукт **«Образы qcow2»** (`plugin: "qcow2images"`) берёт последнюю копию образа машины и собирает готовые
файлы в локальной папке сервера управления по своему расписанию, храня последние N наборов. Выполняет только
сервер управления (`agent_id: "server"`; другой — `400`); площадка — всегда площадка источника (`site_id` в
запросе не учитывается; без `source` — `400`; источник нельзя удалить, пока на него ссылаются). Параметры: `{source, dir, format, keep}`.
qcow2 собирается с закреплённой раскладкой (`preallocation=metadata`, разрежённо). Площадки для готовых qcow2 —
репликация экземпляра (ниже): после каждой сборки (или по своему расписанию) последний набор отправляется на
включённые узлы HopeBackup только изменившимися участками — задание `image_ship`.

**Репликация экземпляра** (закладка «Репликация»): `Instance.replicate = [{site_id, enabled, when: "after_job"|"cron",
cron, keep}]` (по умолчанию пусто — никуда).

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/instances/:id/replication` | `{kind: "inst_repl"\|"image_ship", targets, last: {<site_id>: {job_id, status, started, finished, error, result}}, site_level: [{id, name}], own_site, keep_default, repl_set, site_repl, site_repl_excluded}` — `site_level` — принимающие площадки площадки экземпляра (репликация площадки целиком); `site_repl_excluded` — продукт не передаётся при ней (`site_repl` не задан — по умолчанию: образ машины исключён) |
| PUT | `/api/instances/:id/replication` | `{targets: [...], site_repl?: bool}` — сохранить (своя площадка — `400`, кроме qcow2; для qcow2 — только узлы; cron проверяется); `site_repl` — передавать ли продукт при репликации площадки целиком; запись в аудит `replication_set` |
| POST | `/api/instances/:id/replicate` | `{site_id}` → `{job_id}` — реплицировать сейчас |
| POST | `/api/instances/:id/ship` | на все включённые площадки → `{job_id, jobs}` |

Задание `inst_repl` (зашифрованные копии): снапшоты экземпляра, которых нет на площадке, и недостающие блоки
(новыми pack-файлами, без расшифровки), сегменты `wal/<экземпляр>`; репозиторий на площадке создаётся копией
конфигурации; после — очистка экземпляра на площадке по `keep` (токен очистки). Результат
`{snapshots, chunks, bytes, wal, created_repo, pruned}`. `GET /api/server/mounts` — подключённые
диски сервера `[{mount, fstype, device, total, free, system, readonly}]` для выбора папки.

Узел площадки, чтение для репликации (с 0.9.5; токен на чтение; узел старше 0.9.5 — блоки по одному через `GET /v1/r/:repo/blob/:id`,
индекс — из `GET /v1/r/:repo/pack/:id`, версия узла — из `GET /v1/health`):

| Метод | Путь | Назначение |
|---|---|---|
| POST | `/v1/r/:repo/blobs` | `{ids: ["<hex>", …]}` (до 256) → подряд «длина u32 BE + блок» в порядке запроса; ответ ≈ до 32 МиБ, остальные блоки — следующим запросом |
| GET | `/v1/r/:repo/pack/:id/index` | Индекс pack-файла без данных `[{id, offset, len}]` |

Узел площадки, образы qcow2 (токен `append`; удаление старых наборов — только `admin`):

| Метод | Путь | Назначение |
|---|---|---|
| GET | `/v1/r/:repo/images` | Наборы образов `{root, sets: [{instance, set, complete, path, files: [{name, size, used}]}]}` |
| POST | `/v1/r/:repo/images/begin` | `{instance, set, file, size, base}` — начать приём: копия файла из набора `base` (разрежённо) или пустой; повтор продолжает |
| GET | `/v1/r/:repo/images/sums?instance&set&file` | Суммы участков по 4 МиБ (16 байт BLAKE3 на участок; нулевой участок — нули) |
| PUT | `/v1/r/:repo/images/chunk?instance&set&file&offset&raw` | Участок: тело — zstd, `raw` — открытая длина |
| POST | `/v1/r/:repo/images/zero` | `{instance, set, file, offset, len}` — обнулить (дыра) |
| POST | `/v1/r/:repo/images/finish` | `{instance, set, files, keep}` — набор принят; с токеном `admin` — удалить наборы сверх `keep` |
До создания оценивает место (ненулевые блоки × размер блока; для qcow2/vmdk/vhd — плюс временный raw самого
большого диска на время сборки, и 5% запаса) и свободное место в каталоге; при нехватке — не начинает, а
недособранный набор удаляет. Если новой копии источника не было, набор не пересобирается. Хранение последних
N удаляет **только** подкаталоги наборов вида `ГГГГММДДTччммссZ` — прочие файлы и каталоги в `dir` не
трогаются.

**Восстановление** (через `POST /api/restore` с целью-агентом) параметрами:
```json
{"mode": "deploy" | "export",
 "platform": "proxmox" | "libvirt" | "ovirt" | "openstack",
 "storage": "…", "cluster": "…", "flavor": "…", "networks": {"eth0": "vmbr0"},
 "name": "…", "start": false,
 "format": "qcow2" | "vmdk" | "vhd" | "raw", "dir": "/путь"}
```
`deploy` создаёт ВМ с параметрами источника (vCPU, память, BIOS/UEFI, сетевые карты с теми же MAC) на
Proxmox VE, libvirt/KVM, oVirt/zVirt или OpenStack (образы в Glance, запуск при заданном `flavor`).
`export` выгружает файлы образа (qcow2/vmdk/vhd/raw) в каталог. Сборка файлов — потоком, нулевые блоки —
«дырами» (разрежённо, без промежуточного файла полного размера).

**Скачивание образа** (только администратор, каждое скачивание — в журнал аудита):

| Метод | Путь | Тело / ответ |
|---|---|---|
| GET | `/api/image/requirements?machine_id\|instance_id&site_id&dir&keep&keep_remote&sites&format` | (просмотр) Сколько места понадобится под образы машины: `basis` — `copy` (по последней копии экземпляра), `inventory` (по описи дисков машины: `disks`, `disk_total`, `used`, `estimate` с запасом 25%) или `none` (нет описи и копий); проверки `repo` (хранилище площадки `site_id`), `local` (папка `dir` на сервере: `keep` наборов + временный raw; `system` — системный диск), `remote` (площадки `sites` через запятую, `keep_remote` + принимаемый) — у каждой `{need, free, ok}`. |
| GET | `/api/image/estimate?instance_id&format` | (право просмотра) Оценка места под образы последней копии экземпляра «Образ машины по SSH» → `{snapshot, time, format, disks: [{name, size, used}], total, largest, need}`; без копий — `{snapshot: null}`. Её показывает форма «Образов qcow2» до создания. |
| GET | `/api/image/disks?instance_id&site_id&snapshot_id` | → `{disks: [{name, size, blocks}], machine}` |
| POST | `/api/image/download` | `{instance_id, site_id, snapshot_id, disk, format}` → `{token, size, name, url}` — размер показывается до скачивания |
| GET | `/api/image/dl/:token` | Отдаёт собранный файл образа. Ссылка одноразовая и с коротким сроком — сам токен авторизует запрос (выдан администратором). Файл собирается во временный каталог сервера: сначала проверяется свободное место (для qcow2/vmdk/vhd — вдвое против оценки), при сбое недособранный файл удаляется. |

## Протокол агентов (не для панели)

| Метод | Путь | Назначение |
|---|---|---|
| POST | `/api/agent/enroll` | `{token, name, host, os, version}` → `{agent_id, agent_token}` |
| POST | `/api/agent/poll` | отчёт агента → задания и список экземпляров для отправки WAL (длинный опрос до 25 с) |
| POST | `/api/agent/jobs/:id/log` | `{lines: string[]}` |
| POST | `/api/agent/jobs/:id/finish` | `{ok, result, error, log?: string[]}` — `log`: строки журнала, не отправленные, пока сервер был недоступен. Принимается и для задания, которое сервер уже счёл прерванным |

В отчёте агента (`poll`) с 0.6: `pending` — id заданий, итог которых лежит в outbox агента (сервер не считает их
прерванными), `started` — время запуска процесса агента (задание, начатое раньше, после перезапуска агента
сразу считается прерванным).
