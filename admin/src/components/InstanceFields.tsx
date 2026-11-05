// Поля экземпляра, общие для «Добавить продукт» и страницы экземпляра: агент, площадка, расписание, хранение.
import { Link } from 'react-router-dom'
import type { CatalogProduct, Agent, Instance, Site } from '../types'
import { cx, Field, Input, Select, Toggle, useApi } from './ui'
import { tr, trf } from '../i18n'

export const DEFAULT_FULL = '0 1 * * *'

export const CRON_PRESETS: [string, string][] = [
  ['0 1 * * *', tr("ежедневно в 01:00")],
  ['0 */4 * * *', tr("каждые 4 ч")],
  ['0 */2 * * *', tr("каждые 2 ч")],
  ['0 1 * * 0', tr("по воскресеньям в 01:00")],
]

/** Продукты, которые может выполнять сам сервер управления (без агента). */
export const SERVER_RUN = ['machine', 'qcow2images']
/** Продукты, которые выполняет только сервер управления (сборка и конвертация образов в его локальной папке). */
export const SERVER_ONLY = ['qcow2images']

/** Продукты, площадка которых — всегда площадка источника (откуда берётся копия). */
export const FOLLOWS_SOURCE = ['qcow2images']

/** Вместо выбора площадки — та же, что у источника. */
export function SiteFromSource({ source, sites }: { source?: string; sites: Site[] | null }) {
  const { data } = useApi<Instance[]>('/api/instances')
  const src = data?.find(i => i.id === source)
  const site = sites?.find(s => s.id === src?.site_id)
  return (
    <Field label={tr("Площадка")} hint={tr("Та же, что у источника: оттуда сервер берёт копию для сборки. Куда отправлять готовые qcow2 — после создания, на закладке «Репликация» (по умолчанию никуда).")}>
      <div className="px-3 py-2 rounded-lg border border-slate-200 bg-slate-50 text-sm text-slate-700">{site ? site.name : src ? src.site_id : tr("выберите источник ниже")}</div>
    </Field>
  )
}

/** Вместо выбора агента — пояснение: продукт выполняет сам сервер управления. */
export function ServerOnlyNote() {
  return (
    <Field label={tr("Кто выполняет")} hint={tr("Образы собирает из копии и конвертирует в qcow2 сам сервер HopeBackup — в своей локальной папке; агент не участвует. Готовые файлы он же отправляет на выбранные площадки.")}>
      <div className="px-3 py-2 rounded-lg border border-slate-200 bg-slate-50 text-sm text-slate-700">{tr("Сервер управления HopeBackup")}</div>
    </Field>
  )
}

/** Подпись исполнителя экземпляра: агент или сам сервер управления. */
export const agentLabel = (id: string, agents: Agent[] | null | undefined) => id === 'server' ? tr("сервер управления") : agents?.find(a => a.id === id)?.name || id || '—'

export function AgentSelect({ agents, value, onChange, disabled, allowServer }: { agents: Agent[] | null; value: string; onChange: (v: string) => void; disabled?: boolean; allowServer?: boolean }) {
  return (
    <Field label={allowServer ? tr("Кто выполняет") : tr("Агент")} hint={allowServer
      ? tr("Сервер управления подключается к машине по SSH сам — агент не нужен. Агент выбирайте, если машина доступна только из его сети.")
      : agents && agents.length === 0 ? <>{tr("Агентов нет —")} <Link to="/agents" className="text-blue-600 hover:underline">{tr("подключите агент")}</Link>  {tr("на сервере с продуктом.")}</> : tr("Сервер, на котором работает продукт.")}>
      <Select value={value} onChange={e => onChange(e.target.value)} disabled={disabled}>
        <option value="">{tr("— выберите агент —")}</option>
        {allowServer && <option value="server">{tr("Сервер управления (без агента)")}</option>}
        {(agents || []).map(a => <option key={a.id} value={a.id}>{a.name}{a.host && a.host !== a.name ? ` (${a.host})` : ''} · {a.online ? tr("в сети") : tr("не в сети")}{a.os ? ` · ${a.os}` : ''}</option>)}
      </Select>
    </Field>
  )
}

export function SiteSelect({ sites, value, onChange, disabled }: { sites: Site[] | null; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  return (
    <Field label={tr("Площадка")} hint={sites && sites.length === 0 ? <>{tr("Площадок пока нет —")} <Link to="/sites" className="text-blue-600 hover:underline">{tr("добавьте узел хранения")}</Link>.</> : tr("Узел хранения, куда пойдут копии.")}>
      <Select value={value} onChange={e => onChange(e.target.value)} disabled={disabled}>
        <option value="">{tr("— выберите площадку —")}</option>
        {(sites || []).map(s => <option key={s.id} value={s.id}>{s.name} · {s.url}</option>)}
      </Select>
    </Field>
  )
}

export function CronInput({ label, value, onChange, hint, disabled, placeholder }: { label: string; value: string; onChange: (v: string) => void; hint?: React.ReactNode; disabled?: boolean; placeholder?: string }) {
  return (
    <Field label={label} hint={hint ?? tr("Формат cron: минута час день месяц день_недели")}>
      <Input className="font-mono text-[13px]" value={value} onChange={e => onChange(e.target.value)} disabled={disabled} placeholder={placeholder} />
      {!disabled && (
        <span className="flex flex-wrap gap-1.5 mt-1.5">
          {CRON_PRESETS.map(([c, l]) => (
            <button key={c} type="button" onClick={() => onChange(c)}
              className={cx('text-[11px] px-2 py-0.5 rounded-md border', value === c ? 'border-blue-300 bg-blue-50 text-blue-700' : 'border-slate-200 text-slate-500 hover:bg-slate-50')}>{l}</button>
          ))}
        </span>
      )}
    </Field>
  )
}

export type ScheduleValue = Pick<Instance, 'schedule' | 'retention' | 'rpo_target_min'>

export function ScheduleFields({ value, onChange, disabled, product }: { value: ScheduleValue; onChange: (v: ScheduleValue) => void; disabled?: boolean; product?: CatalogProduct | null }) {
  const s = value.schedule
  const extra = product?.kinds?.[0]
  const pg = !product || product.plugin === 'postgresql'
  const setS = (p: Partial<Instance['schedule']>) => onChange({ ...value, schedule: { ...s, ...p } })
  return (
    <div className="space-y-4">
      <Toggle checked={s.enabled} onChange={v => setS({ enabled: v })} label={tr("Расписание включено")} hint={tr("Выключенное расписание не запускает копии; вручную — по кнопке.")} disabled={disabled} />
      <div className="grid sm:grid-cols-2 gap-4">
        <CronInput label={tr("Полная копия")} value={s.full} onChange={v => setS({ full: v })} disabled={disabled} />
        {pg && <CronInput label={tr("Логический дамп (необязательно)")} value={s.dump || ''} onChange={v => setS({ dump: v.trim() ? v : null })} disabled={disabled}
          placeholder={tr("не делать")} hint={tr("Пусто — не делать. Дамп позволяет восстановить одну базу.")} />}
        {extra && <CronInput label={trf("{0} (необязательно)", extra.title)} value={s.incr || ''} onChange={v => setS({ incr: v.trim() ? v : null })} disabled={disabled}
          placeholder={tr("не делать")} hint={trf("Пусто — не делать. {0}Например, */15 * * * * — каждые 15 минут.", extra.hint ? extra.hint[0].toUpperCase() + extra.hint.slice(1) + '. ' : '')} />}
      </div>
      <div className="grid sm:grid-cols-2 gap-4">
        <Field label={tr("Хранить последних копий")} hint={tr("Старые точки удаляются при очистке площадки.")}>
          <Input type="number" min={1} value={value.retention.keep_last} disabled={disabled}
            onChange={e => onChange({ ...value, retention: { ...value.retention, keep_last: parseInt(e.target.value, 10) || 0 } })} />
        </Field>
        <Field label={tr("Цель RPO, мин")} hint={tr("При превышении экземпляр помечается «Внимание» на обзоре.")}>
          <Input type="number" min={1} value={value.rpo_target_min} disabled={disabled}
            onChange={e => onChange({ ...value, rpo_target_min: parseInt(e.target.value, 10) || 0 })} />
        </Field>
      </div>
    </div>
  )
}
