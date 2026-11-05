// Общие элементы предметной области: состояния, плитки продуктов, подписи типов, кэш каталога.
import { useEffect, useState } from 'react'
import { api, fmtDuration } from '../api'
import type { Catalog, CatalogProduct, Instance, InstanceStatus, JobStatus } from '../types'
import { Badge, cx, StatusDot, type Tone } from './ui'
import { trTree, tr, trf } from '../i18n'

// ---------------------------------------------------------------- состояния
export const INSTANCE_STATUS: Record<InstanceStatus, { label: string; tone: Tone }> = {
  ok: { label: tr("В норме"), tone: 'green' },
  warn: { label: tr("Внимание"), tone: 'amber' },
  error: { label: tr("Ошибка"), tone: 'red' },
  never: { label: tr("Нет копий"), tone: 'slate' },
}
export const instTone = (s?: string): Tone => INSTANCE_STATUS[s as InstanceStatus]?.tone || 'slate'
export const InstanceBadge = ({ status }: { status?: string }) => {
  const s = INSTANCE_STATUS[status as InstanceStatus]
  return <Badge tone={s?.tone || 'slate'}><StatusDot tone={s?.tone || 'slate'} />{s?.label || status || '—'}</Badge>
}

export const JOB_STATUS: Record<JobStatus, { label: string; tone: Tone }> = {
  queued: { label: tr("В очереди"), tone: 'slate' },
  running: { label: tr("Выполняется"), tone: 'blue' },
  ok: { label: tr("Успешно"), tone: 'green' },
  failed: { label: tr("Ошибка"), tone: 'red' },
}
export const JobBadge = ({ status }: { status?: string }) => {
  const s = JOB_STATUS[status as JobStatus]
  return <Badge tone={s?.tone || 'slate'}>{status === 'running' && <span className="w-1.5 h-1.5 rounded-full bg-blue-500 animate-pulse" />}{s?.label || status || '—'}</Badge>
}
export const jobActive = (s?: string) => s === 'queued' || s === 'running'

/** Подписи видов копий и заданий (неизвестные показываются как есть). */
const KIND: Record<string, string> = {
  full: tr("полная"), dump: tr("дамп"), files: tr("файлы"), wal: tr("архив WAL"), incr: tr("журнал / инкремент"), incremental: tr("инкремент"), diff: tr("разностная"), binlog: tr("журнал binlog"), physical: tr("физическая"), log: tr("журнал"),
  backup: tr("копия"), restore: tr("восстановление"), discover: tr("обнаружение"), action: tr("действие"), init: tr("создание репозитория"),
  'setup-wal': tr("настройка WAL"), prune: tr("очистка"), test: tr("проверка"),
}
export const kindLabel = (k?: string | null) => (k && KIND[k]) || k || '—'

// ---------------------------------------------------------------- RPO
export function RpoValue({ inst }: { inst: Instance }) {
  const s = inst.runtime?.rpo_seconds
  if (s === null || s === undefined) return <span className="text-slate-400">—</span>
  const target = (inst.rpo_target_min || 0) * 60
  const bad = target > 0 && s > target
  return (
    <span className={cx('whitespace-nowrap', bad ? 'text-red-700 font-semibold' : 'text-slate-800')} title={target ? trf("цель {0}", fmtDuration(target)) : undefined}>
      {fmtDuration(s)}{inst.runtime?.wal ? <span className="text-slate-400 font-normal"> (WAL)</span> : null}
      {target > 0 && <span className="block text-xs text-slate-400 font-normal">{tr("цель")} {fmtDuration(target)}</span>}
    </span>
  )
}

// ---------------------------------------------------------------- плитки продуктов
const CAT_TONE: [RegExp, string][] = [
  [/субд|баз/i, 'bg-blue-100 text-blue-700'],
  [/инфра/i, 'bg-violet-100 text-violet-700'],
  [/платф/i, 'bg-orange-100 text-orange-800'],
  [/хран|машин|файл/i, 'bg-teal-100 text-teal-800'],
]
export const tileClass = (category?: string) => CAT_TONE.find(([r]) => r.test(category || ''))?.[1] || 'bg-slate-100 text-slate-700'

export function Tile({ product, fallback, size = 34, className }: { product?: CatalogProduct | null; fallback?: string; size?: number; className?: string }) {
  const text = product?.tile || (fallback || '?').slice(0, 2)
  return (
    <span style={{ width: size, height: size }}
      className={cx('rounded-lg inline-flex items-center justify-center font-bold shrink-0', size < 30 ? 'text-[9px]' : 'text-[11px]', tileClass(product?.category), className)}>
      {text}
    </span>
  )
}

// ---------------------------------------------------------------- каталог (кэш на сеанс)
let catalogCache: Catalog | null = null
let catalogPromise: Promise<Catalog> | null = null
export function loadCatalog(): Promise<Catalog> {
  if (catalogCache) return Promise.resolve(catalogCache)
  if (!catalogPromise) catalogPromise = api<Catalog>('/api/catalog').then(c => { catalogCache = trTree(c); return catalogCache }).catch(e => { catalogPromise = null; throw e })
  return catalogPromise
}
export function useCatalog() {
  const [cat, setCat] = useState<Catalog | null>(catalogCache)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => { let alive = true; loadCatalog().then(c => alive && setCat(c)).catch(e => alive && setError(e.message)); return () => { alive = false } }, [])
  return { catalog: cat, error, product: (id?: string) => cat?.products.find(p => p.id === id) || null }
}

/** Сообщить меню, что список экземпляров изменился. */
export const INSTANCES_EVENT = 'hopebackup-instances'
export const notifyInstances = () => window.dispatchEvent(new Event(INSTANCES_EVENT))

/** Красивое JSON-представление результата задания. */
export const pretty = (v: any) => { try { return typeof v === 'string' ? v : JSON.stringify(v, null, 2) } catch { return String(v) } }
