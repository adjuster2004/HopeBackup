import { useMemo } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { FilterX, Plus, RefreshCw, Search, ShieldCheck } from 'lucide-react'
import { fmtAgo, fmtBytes, fmtTime } from '../api'
import type { Agent, Instance, Site } from '../types'
import { Alert, Badge, Button, Card, Empty, Input, PageHeader, Select, Spinner, Table, Td, useApi, RefreshButton } from '../components/ui'
import { INSTANCE_STATUS, InstanceBadge, kindLabel, RpoValue, Tile, useCatalog } from '../components/common'
import { usePerms } from '../perms'
import { Suggestions } from '../components/Suggestions'
import { tr } from '../i18n'

export function ProductsPage() {
  const nav = useNavigate()
  const perms = usePerms()
  const { data, error, reload } = useApi<Instance[]>('/api/instances', [], 5000)
  const { data: sites } = useApi<Site[]>('/api/sites')
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 15000)
  const { product } = useCatalog()
  const siteName = (id: string) => sites?.find(x => x.id === id)?.name || id || '—'
  const agent = (id: string) => agents?.find(x => x.id === id)
  // Фильтры живут в адресе страницы (?type=&agent=&status=&q=): ссылку можно отправить, «Назад» их сохраняет.
  const [sp, setSp] = useSearchParams()
  const f = { type: sp.get('type') || '', agent: sp.get('agent') || '', status: sp.get('status') || '', q: sp.get('q') || '' }
  const setF = (k: keyof typeof f, v: string) => { const n = new URLSearchParams(sp); if (v) n.set(k, v); else n.delete(k); setSp(n, { replace: true }) }
  const list = data || []
  const typeTitle = (i: Instance) => product(i.catalog_id)?.title || i.plugin
  const typeKey = (i: Instance) => i.catalog_id || i.plugin
  const agentName = (id: string) => id === 'server' ? tr("сервер управления") : agent(id)?.name || id || '—'
  // Варианты — только из имеющихся экземпляров, с количеством.
  const opts = (key: (i: Instance) => string, label: (i: Instance) => string) => {
    const m = new Map<string, { label: string; n: number }>()
    for (const i of list) { const k = key(i); const e = m.get(k); if (e) e.n++; else m.set(k, { label: label(i), n: 1 }) }
    return [...m.entries()].sort((a, b) => a[1].label.localeCompare(b[1].label, 'ru'))
  }
  const types = useMemo(() => opts(typeKey, typeTitle), [list, product])
  const servers = useMemo(() => opts(i => i.agent_id, i => agentName(i.agent_id)), [list, agents])
  const statusOk = (i: Instance) => !f.status || (f.status === 'problem' ? ['error', 'warn'].includes(i.runtime?.status || '') : i.runtime?.status === f.status)
  const shown = list.filter(i => (!f.type || typeKey(i) === f.type) && (!f.agent || i.agent_id === f.agent) && statusOk(i)
    && (!f.q || `${i.name} ${typeTitle(i)} ${agentName(i.agent_id)}`.toLowerCase().includes(f.q.toLowerCase())))
  const filtered = !!(f.type || f.agent || f.status || f.q)
  return (
    <div>
      <PageHeader title={tr("Продукты")} icon={<ShieldCheck size={22} />}
        subtitle={tr("Экземпляры продуктов под защитой: состояние, последняя копия и RPO. Нажмите на строку, чтобы открыть экземпляр.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => nav('/products/new')}>{tr("Добавить продукт")}</Button>}
        </>} />
      <Suggestions className="mb-4" />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<ShieldCheck size={28} />} title={tr("Продуктов пока нет")}
          text={tr("Выберите плагин из каталога и укажите агент и площадку — экземпляр появится в этом списке.")}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => nav('/products/new')}>{tr("Добавить продукт")}</Button>} /></Card>
      ) : (
        <Card padded={false}>
          <div className="flex flex-wrap items-center gap-2 px-4 py-3 border-b border-slate-100">
            <div className="relative w-full sm:w-60">
              <Search size={15} className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400 pointer-events-none" />
              <Input className="!pl-9" placeholder={tr("Поиск по имени")} value={f.q} onChange={e => setF('q', e.target.value)} />
            </div>
            <Select className="w-full sm:w-52" value={f.type} onChange={e => setF('type', e.target.value)} aria-label={tr("Тип продукта")}>
              <option value="">{tr("Все типы продуктов")}</option>
              {types.map(([k, v]) => <option key={k} value={k}>{v.label} ({v.n})</option>)}
            </Select>
            <Select className="w-full sm:w-52" value={f.agent} onChange={e => setF('agent', e.target.value)} aria-label={tr("Сервер")}>
              <option value="">{tr("Все серверы")}</option>
              {servers.map(([k, v]) => <option key={k} value={k}>{v.label} ({v.n})</option>)}
            </Select>
            <Select className="w-full sm:w-44" value={f.status} onChange={e => setF('status', e.target.value)} aria-label={tr("Состояние")}>
              <option value="">{tr("Любое состояние")}</option>
              <option value="problem">{tr("С проблемами")}</option>
              {Object.entries(INSTANCE_STATUS).map(([k, v]) => <option key={k} value={k}>{v.label}</option>)}
            </Select>
            {filtered && <span className="flex items-center gap-3 whitespace-nowrap ml-1">
              <span className="text-xs text-slate-500">{tr("показано")} {shown.length}  {tr("из")} {list.length}</span>
              <button type="button" className="inline-flex items-center gap-1 text-xs text-blue-600 hover:underline" onClick={() => setSp(new URLSearchParams(), { replace: true })}><FilterX size={13} />{tr("Сбросить")}</button>
            </span>}
          </div>
          {shown.length === 0 ? <div className="px-4 py-10 text-center text-sm text-slate-500">{tr("Под фильтр ничего не подходит.")} <button type="button" className="text-blue-600 hover:underline" onClick={() => setSp(new URLSearchParams(), { replace: true })}>{tr("Сбросить фильтры")}</button></div> :
          <Table head={[tr("Экземпляр"), tr("Агент"), tr("Площадка"), tr("Последняя копия"), 'RPO', tr("Следующий запуск"), tr("Состояние")]}>
            {shown.map(i => {
              const p = product(i.catalog_id)
              const a = agent(i.agent_id)
              const lb = i.runtime?.last_backup
              return (
                <tr key={i.id} className="hover:bg-slate-50 cursor-pointer" onClick={() => nav(`/instances/${encodeURIComponent(i.id)}`)}>
                  <Td>
                    <div className="flex items-center gap-3">
                      <Tile product={p} fallback={i.plugin} />
                      <div className="min-w-0"><div className="font-medium text-slate-900">{i.name}</div><div className="text-xs text-slate-500">{p?.title || i.plugin}</div></div>
                    </div>
                  </Td>
                  <Td>{a ? <span className="flex items-center gap-2"><span className={`w-2 h-2 rounded-full ${a.online ? 'bg-emerald-500' : 'bg-red-500'}`} />{a.name}</span> : <span className="text-slate-400">{agentName(i.agent_id)}</span>}</Td>
                  <Td className="text-slate-600">{siteName(i.site_id)}</Td>
                  <Td>{lb ? <><div>{fmtTime(lb.time)}</div><div className="text-xs text-slate-500">{kindLabel(lb.kind)} · {fmtBytes(lb.size)}</div></> : <span className="text-slate-400">{tr("ещё не было")}</span>}</Td>
                  <Td><RpoValue inst={i} /></Td>
                  <Td>{!i.schedule?.enabled ? <Badge>{tr("выключено")}</Badge> : i.runtime?.next_run ? <span title={fmtTime(i.runtime.next_run)}>{fmtAgo(i.runtime.next_run)}</span> : '—'}</Td>
                  <Td><InstanceBadge status={i.runtime?.status} /></Td>
                </tr>
              )
            })}
          </Table>}
        </Card>
      )}
    </div>
  )
}
