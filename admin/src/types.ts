// Типы ответов API (docs/API.md).

export type Me = { user: string; role: string; perms: string[]; version: string; hostname: string }

export type Status = {
  version: string; hostname: string; uptime: number
  counts: { instances: number; agents: number; agents_online: number; sites: number; connectors: number; targets: number }
  jobs24: { ok: number; failed: number; running: number }
  transfer24: { read: number; new: number; uploaded: number }
  worst_rpo: { instance_id: string; name: string; seconds: number; target_seconds: number } | null
  /** Ошибки за сутки, повторы одного экземпляра (вид и режим) — одной строкой: ids всех заданий группы. */
  recent_failures: { job_id: string; title: string; error: string; finished: string; count?: number; ids?: string[] }[]
  /** Сколько всего нескрытых актуальных ошибок за сутки (в recent_failures — не больше 20). */
  recent_failures_total?: number
  next_runs: { instance_id: string; name: string; kind: string; at: string }[]
}

export type SchemaProp = {
  type: 'string' | 'integer' | 'boolean' | 'array'
  title?: string; description?: string; default?: any; format?: string
  items?: { type: string }
  enum?: any[]
  /** Особое поле: `machine` — машина из «Серверы и ВМ»; `instance:<плагин>` — экземпляр этого плагина. */
  'x-widget'?: string
}
export type Schema = { type?: 'object'; properties?: Record<string, SchemaProp>; required?: string[] }

export type CatalogProduct = {
  id: string; plugin: string; title: string; tile: string; category: string; description: string; modes: string
  available: boolean; schema: Schema; defaults: Record<string, any>; restore_schema?: Schema
  /** Виды копий помимо полной (журнал, инкремент) — кнопки и расписание. */
  kinds?: { id: string; title: string; hint?: string }[]
  /** Восстановление на момент времени по журналу. */
  pitr?: boolean
}
export type CatalogKind = { id: string; title: string; description?: string; available: boolean; schema?: Schema }
export type Catalog = { products: CatalogProduct[]; connector_kinds: CatalogKind[]; target_kinds: CatalogKind[] }

export type InstanceStatus = 'ok' | 'warn' | 'error' | 'never'
export type WalState = { queued: number; last_shipped: string | null; last_time: string | null; last_error: string | null; shipped: number }

export type Instance = {
  id: string; name: string; catalog_id: string; plugin: string; agent_id: string; site_id: string
  config: Record<string, any>
  schedule: { enabled: boolean; full: string; dump: string | null; incr?: string | null }
  retention: { keep_last: number }
  rpo_target_min: number
  created: string
  /** Своя политика повторов (нет — общая из настроек сервера). */
  retry?: RetryPolicy | null
  /** Закладка «Репликация»: площадки, куда уходят копии этого экземпляра (у «Образов qcow2» — готовые qcow2). */
  replicate?: ReplTarget[]
  /** Репликация задана администратором явно (сохранена закладка «Репликация»). */
  repl_set?: boolean
  /** Передавать при репликации площадки целиком; не задано — образ машины нет, остальные да. */
  site_repl?: boolean | null
  runtime: {
    status: InstanceStatus
    last_backup: { snapshot_id: string; time: string; kind: string; size: number; new_bytes: number; uploaded: number } | null
    last_job: { id: string; status: string; finished: string; error: string | null } | null
    /** Предупреждения последней удачной копии (строки журнала «внимание: …»). */
    warnings?: string[]
    rpo_seconds: number | null
    wal: WalState | null
    next_run: string | null
  }
}

export type InstanceBody = Pick<Instance, 'name' | 'catalog_id' | 'agent_id' | 'site_id' | 'config' | 'schedule' | 'retention' | 'rpo_target_min'> & { retry_mode?: 'inherit' | 'custom'; retry?: RetryPolicy | null }
export type RetryPolicy = { enabled: boolean; attempts: number; interval_min: number }

export type SnapshotHeader = {
  id: string; time: string; instance: string; product: string; kind: string; host: string
  size: number; new_bytes: number; uploaded: number; chunks: number; extra: any
}
export type Points = {
  snapshots: SnapshotHeader[]
  wal: { count: number; first: string; last: string; bytes: number; last_time: string } | null
  pitr: { from: string; to: string } | null
}

export type RepoStats = { packs: number; pack_bytes: number; chunks: number; snapshots: number; objects: number; object_bytes: number }
export type S3Site = {
  provider: 'yandex' | 'aws' | 's3'; endpoint: string; region: string; bucket: string; prefix: string
  access_key: string; secret_key: string; admin_access_key: string; admin_secret_key: string
  path_style: boolean; lock_mode: string; lock_days: number; storage_class: string; fingerprint: string
}
export type CloudSite = {
  provider: 'gdrive' | 'yadisk' | 'webdav'; title?: string; url: string; username: string; password: string; folder: string; folder_id: string
  client_id: string; client_secret: string; refresh_token: string; access_token: string; token_expires: number; account: string
  fingerprint: string; endpoint: string; connected?: boolean; oauth_flow?: string
}
export type DiskInfo = { path: string; total: number; used: number; free: number }
export type CloudInfo = { account?: string; total?: number | null; used?: number | null; free?: number | null; folder_id?: string }
export type Site = {
  id: string; name: string; kind: 'node' | 's3' | 'disk' | 'cloud'; url: string; fingerprint: string; repo: string; connector_ids: string[]; compression: number
  has_token: boolean; has_password: boolean; has_prune_token: boolean; s3: S3Site | null; created: string
  cloud?: CloudSite | null; disk?: DiskInfo | null; node_disk?: { total: number; used: number; free: number; time: string; images?: NodeImages | null } | null
  replicas: string[]; replication_interval_min: number; replication: { site: string; state: ReplState | null }[]
  override?: { url: string; direct: boolean } | null
}
/** Готовые образы qcow2 на узле площадки (продукт «Образы qcow2»). */
export type NodeImages = { root: string; sets: { instance: string; set: string; complete: boolean; path: string; files: { name: string; size: number; used: number }[] }[] }
export type SiteTest = { ok: boolean; via: string; repo_exists: boolean; stats: RepoStats | null; cloud?: CloudInfo | null }
export type Settings = { settings: { default_excludes: string[]; retry: RetryPolicy; catch_up: boolean; catch_up_hours: number }; default_excludes_builtin: string[] }

export type Connector = {
  id: string; name: string; kind: string
  params: { host: string; port: number; user: string; remote_host: string; remote_port: number; jump: string; has_key: boolean; public_key?: string | null } & Record<string, any>
  created: string
  sites?: string[]
  /** Агенты, которые пишут на площадки через этот коннектор (им нужны те же пакеты VPN). */
  agents?: { id: string; name: string; can_install: boolean }[]
  status: { up: boolean; local_port?: number | null; iface?: string; address?: string | null; routes?: string[]; implementation?: string; shared?: boolean; error: string | null; time?: string } | null
}
export type ConnectorTest = {
  ok: boolean; local_port?: number; iface?: string; address?: string; routes?: string[]; implementation?: string; last_handshake_seconds?: number | null
  node?: { ok: boolean; url?: string; site: string; error?: string }
}

export type Agent = {
  id: string; name: string; host: string; os: string; version: string; enrolled: string; last_seen: string; online: boolean
  wal: ({ instance_id: string } & WalState)[]
  /** Возможности версии агента: `pg-restart` — перезапуск PostgreSQL из панели (с 0.8.3). */
  caps?: string[]
}
export type EnrollToken = { token: string; expires: string; command: string; server_fingerprint: string }

export type Target = { id: string; name: string; kind: string; host: string; port: number; user: string; agent_id: string; has_key: boolean; has_password?: boolean; params?: Record<string, any>; created: string }
export const VM_KINDS = ['proxmox', 'vmware', 'yandex_cloud', 'aws']

export type JobStatus = 'queued' | 'running' | 'ok' | 'failed'
export type Job = { mode?: string; ack?: { user: string; at: string };
  id: string; kind: string; title: string; instance_id: string | null; agent_id: string | null; status: JobStatus
  created: string; started: string | null; finished: string | null; error: string | null; result: any
  log?: { t: string; m: string }[]
  /** schedule | retry | catchup | manual */
  origin?: string; attempt?: number; interrupted?: boolean; retry_at?: string | null; retry_job?: string | null; await_until?: string | null
}

export type AuditEntry = { time: string; user: string; action: string; detail: string }
export type User = { name: string; role: string; email?: string; created: string }

export type PeerSync = { ok: boolean; time: string; error: string | null; version?: string | null }
export type PeerUpdate = { running: boolean; ok: boolean | null; time: string; target: string; error: string | null }
export type Peer = { id: string; name: string; url: string; fingerprint: string; joined: string; self: boolean; sync: PeerSync | null; update: PeerUpdate | null; connector_id: string; version: string | null }
export type Cluster = {
  node_id: string; name: string; role: 'active' | 'standby'; epoch: number; self_url: string; active_url: string; last_sync: string | null; fingerprint: string; version: string
  peers: Peer[]
  site_overrides: Record<string, { url: string; direct: boolean }>
}
export type ReplState = { ok: boolean | null; time: string; finished: string; error: string | null; last_ok?: string | null; running?: boolean; started?: string; progress?: string; stats?: { packs: number; pack_bytes: number; snapshots: number; objects: number; object_bytes: number } }

export type ReplTarget = { site_id: string; enabled: boolean; when: 'after_job' | 'cron'; cron: string; keep: number }
export type ReplStatus = {
  kind: 'inst_repl' | 'image_ship'; targets: ReplTarget[] | null; own_site: string; keep_default: number
  last: Record<string, { job_id: string; status: string; started: string | null; finished: string | null; error: string | null; result: any }>
  site_level: { id: string; name: string }[]
  repl_set?: boolean; site_repl?: boolean | null; site_repl_excluded?: boolean
}

/** Продукт исключён из репликации площадки целиком (как Instance::site_repl_excluded на сервере). */
export const siteReplExcluded = (i: Pick<Instance, 'plugin' | 'site_repl'>) => i.plugin !== 'replica' && (i.site_repl ?? i.plugin !== 'machine') === false
