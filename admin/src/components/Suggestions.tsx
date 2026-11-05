// Найдено на серверах, но не защищено: агенты раз в несколько часов смотрят, что работает на машине
// (GitLab, СУБД, Vault…), и Ковчег предлагает создать экземпляр штатными средствами продукта.
import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { ChevronDown, Radar, ShieldPlus } from 'lucide-react'
import { post } from '../api'
import type { Site } from '../types'
import { Button, Select, useApi, useToast } from './ui'
import { notifyInstances } from './common'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

const OPEN_KEY = 'hopebackup_card:suggestions'

type Suggestion = { agent_id: string; agent_name: string; id: string; title: string; reason: string; catalog_id: string; details?: string[] }

export function Suggestions({ compact, className }: { compact?: boolean; className?: string }) {
  const perms = usePerms()
  const toast = useToast()
  const nav = useNavigate()
  const { data, reload } = useApi<Suggestion[]>('/api/suggestions', [], 30000)
  const { data: sites } = useApi<Site[]>('/api/sites', [], 60000)
  const [site, setSite] = useState('')
  const [busy, setBusy] = useState('')
  // Свёрнут или развёрнут — запоминается в браузере (как сворачиваемые карточки).
  const [open, setOpen] = useState<boolean>(() => { try { return localStorage.getItem(OPEN_KEY) !== '0' } catch { return true } })
  if (!data || data.length === 0 || !perms.edit) return null
  const siteId = site || (sites?.length === 1 ? sites[0].id : '')
  const key = (s: Suggestion) => `${s.agent_id}/${s.id}`
  const apply = async (s: Suggestion) => {
    if (!siteId) return toast.err(tr("Выберите площадку для копий"))
    setBusy(key(s))
    try {
      const r = await post<{ id: string; name: string }>('/api/suggestions/apply', { agent_id: s.agent_id, id: s.id, site_id: siteId })
      toast.ok(trf("Создан экземпляр «{0}» — копия по расписанию; первую можно запустить сразу", r.name))
      notifyInstances(); reload()
      nav(`/instances/${encodeURIComponent(r.id)}`)
    } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const dismiss = async (s: Suggestion) => {
    setBusy(key(s) + '-x')
    try { await post('/api/suggestions/dismiss', { agent_id: s.agent_id, id: s.id }); reload() } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const shown = compact ? data.slice(0, 5) : data
  const toggle = () => { const n = !open; setOpen(n); try { localStorage.setItem(OPEN_KEY, n ? '1' : '0') } catch { /* */ } }
  return (
    <div className={`rounded-xl border border-blue-200 bg-blue-50/60 px-4 py-3 text-sm shadow-sm ${className || ''}`}>
      <div className={`flex flex-wrap items-center justify-between gap-2 ${open ? 'mb-2' : ''}`}>
        <button type="button" onClick={toggle} aria-expanded={open} title={open ? tr("Свернуть") : tr("Развернуть")}
          className="flex items-center gap-2 font-semibold text-slate-900 text-left hover:text-blue-700 -ml-1 px-1 rounded focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-500">
          <ChevronDown size={18} className={`text-slate-400 transition-transform shrink-0 ${open ? '' : '-rotate-90'}`} />
          <Radar size={18} className="text-blue-600" />{tr("Найдено на серверах — не защищено (")}{data.length})
          {!open && <span className="font-normal text-xs text-slate-500">{tr("— нажмите, чтобы развернуть")}</span>}
        </button>
        {open && (sites?.length || 0) > 1 && (
          <Select className="!w-56 !py-1.5 text-xs" value={site} onChange={e => setSite(e.target.value)} aria-label={tr("Площадка для копий")}>
            <option value="">{tr("Площадка для копий…")}</option>
            {sites!.map(x => <option key={x.id} value={x.id}>{x.name}</option>)}
          </Select>
        )}
      </div>
      {open && <>
      <ul className="space-y-2">
        {shown.map(s => (
          <li key={key(s)} className="flex flex-wrap items-start gap-3 bg-white rounded-lg border border-blue-100 px-3 py-2">
            <div className="flex-1 min-w-[240px]">
              <div className="font-medium text-slate-900">{s.title} <span className="font-normal text-slate-500">{tr("на сервере")} {s.agent_name}</span></div>
              <div className="text-xs text-slate-600">{s.reason}</div>
              {(s.details || []).length > 0 && <div className="text-[11px] text-slate-400 mt-0.5">{s.details!.join(' · ')}</div>}
            </div>
            <div className="flex items-center gap-2 shrink-0">
              <Button kind="primary" className="!py-1.5 text-xs" icon={<ShieldPlus size={14} />} loading={busy === key(s)} onClick={() => apply(s)}>{tr("Защитить")}</Button>
              <button className="text-xs text-slate-500 hover:text-slate-800 hover:underline" disabled={busy === key(s) + '-x'} onClick={() => dismiss(s)}
                title={tr("Больше не предлагать для этого сервера (например, копируется другим способом)")}>{tr("Не предлагать")}</button>
            </div>
          </li>
        ))}
      </ul>
      {compact && data.length > shown.length && <div className="text-xs text-slate-500 mt-2">{tr("и ещё")} {data.length - shown.length}  {tr("— на странице «Продукты»")}</div>}
      <div className="text-[11px] text-slate-500 mt-2">{tr("Агенты смотрят, что работает на их серверах, раз в 6 часов. Экземпляр создаётся с рекомендованными параметрами и ночным расписанием — их можно поменять на странице экземпляра.")}</div>
      </>}
    </div>
  )
}
