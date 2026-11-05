import { useEffect, useMemo, useState } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'
import { Plus, Search, PackagePlus } from 'lucide-react'
import { post } from '../api'
import type { Agent, CatalogProduct, Instance, InstanceBody, Site } from '../types'
import { Alert, Button, Card, cx, Empty, Field, InDev, Input, PageHeader, Spinner, useApi, useToast } from '../components/ui'
import { notifyInstances, Tile, useCatalog } from '../components/common'
import { SchemaForm, schemaClean, schemaInitial } from '../components/SchemaForm'
import { AgentSelect, DEFAULT_FULL, FOLLOWS_SOURCE, SERVER_ONLY, SERVER_RUN, ScheduleFields, ServerOnlyNote, SiteFromSource, SiteSelect, type ScheduleValue } from '../components/InstanceFields'
import { ImageNeeds } from '../components/ImageNeeds'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

const usesWal = (p: CatalogProduct) => /wal/i.test(p.modes || '')

export function AddProductPage() {
  const nav = useNavigate()
  const toast = useToast()
  const perms = usePerms()
  const { catalog, error } = useCatalog()
  const { data: agents } = useApi<Agent[]>('/api/agents')
  const { data: sites } = useApi<Site[]>('/api/sites')
  const [search, setSearch] = useState('')
  const [sel, setSel] = useState<CatalogProduct | null>(null)
  const [name, setName] = useState('')
  const [nameTouched, setNameTouched] = useState(false)
  const [agentId, setAgentId] = useState('')
  const [siteId, setSiteId] = useState('')
  const [config, setConfig] = useState<Record<string, any>>({})
  const [sched, setSched] = useState<ScheduleValue>({ schedule: { enabled: true, full: DEFAULT_FULL, dump: null }, retention: { keep_last: 7 }, rpo_target_min: 1440 })
  const [busy, setBusy] = useState(false)

  // Одна площадка / один агент — выбрать сразу.
  useEffect(() => { if (!siteId && sites?.length === 1) setSiteId(sites[0].id) }, [sites])
  useEffect(() => { if (!agentId && agents?.length === 1) setAgentId(agents[0].id) }, [agents])
  // Имя по умолчанию: «Продукт · хост агента».
  useEffect(() => {
    if (!sel || nameTouched) return
    const a = agents?.find(x => x.id === agentId)
    setName(`${sel.title.replace(/\s+\d+\+?$/, '')}${a && !SERVER_RUN.includes(sel.plugin) ? ` · ${a.host || a.name}` : ''}`)
  }, [sel, agentId, agents, nameTouched])

  const [sp] = useSearchParams()
  const pick = (p: CatalogProduct) => {
    if (!p.available) return
    setSel(p)
    setConfig(schemaInitial(p.schema, p.defaults))
    // Безагентные продукты по умолчанию выполняет сам сервер управления.
    if (SERVER_RUN.includes(p.plugin)) setAgentId('server')
    else if (agentId === 'server') setAgentId(agents?.length === 1 ? agents[0].id : '')
    // Есть копия журнала (binlog, журнал транзакций) — по умолчанию каждые 15 минут.
    setSched(s => ({ ...s, rpo_target_min: usesWal(p) ? 15 : p.kinds?.length ? 60 : 1440, schedule: { ...s.schedule, incr: p.kinds?.length ? '*/15 * * * *' : null } }))
  }  // Ссылка из описи сервера: /products/new?catalog=postgresql
  useEffect(() => {
    const c = sp.get('catalog'); if (!c || !catalog || sel) return
    const p = catalog.products.find(x => x.id === c); if (!p) return
    pick(p)
    // Из «Серверы и ВМ»: /products/new?catalog=machine&machine=<id>
    const m = sp.get('machine'); if (m) setConfig(v => ({ ...v, machine_id: m }))
  }, [catalog])


  const groups = useMemo(() => {
    const qs = search.trim().toLowerCase()
    const m = new Map<string, CatalogProduct[]>()
    for (const p of catalog?.products || []) {
      if (p.id === 'replica') continue  // создаётся на странице «Переезд»
      if (qs && !`${p.title} ${p.id} ${p.description} ${p.category} ${p.modes}`.toLowerCase().includes(qs)) continue
      const k = p.category || tr("Другое")
      if (!m.has(k)) m.set(k, [])
      m.get(k)!.push(p)
    }
    for (const l of m.values()) l.sort((a, b) => Number(b.available) - Number(a.available))
    return [...m.entries()]
  }, [catalog, search])

  const submit = async () => {
    if (!sel) return
    if (!name.trim()) return toast.err(tr("Укажите имя экземпляра"))
    const agent = SERVER_ONLY.includes(sel.plugin) ? 'server' : agentId
    if (!agent) return toast.err(tr("Выберите агент"))
    const follows = FOLLOWS_SOURCE.includes(sel.plugin)
    if (follows && !config.source) return toast.err(tr("Выберите источник — экземпляр «Образ машины по SSH»"))
    if (!siteId && !follows) return toast.err(tr("Выберите площадку"))
    if (!sched.schedule.full.trim()) return toast.err(tr("Укажите расписание полной копии"))
    const body: InstanceBody = {
      name: name.trim(), catalog_id: sel.id, agent_id: agent, site_id: siteId,
      config: schemaClean(sel.schema, config), schedule: sched.schedule, retention: sched.retention, rpo_target_min: sched.rpo_target_min,
    }
    setBusy(true)
    try {
      const r = await post<Instance>('/api/instances', body)
      notifyInstances()
      toast.ok(trf("Экземпляр «{0}» добавлен — его раздел появился в меню", r.name))
      nav(`/instances/${encodeURIComponent(r.id)}`)
    } catch (e) { toast.err(e) } finally { setBusy(false) }
  }

  if (!perms.edit) return <Alert tone="warn">{tr("Добавлять продукты может роль с правом изменения.")}</Alert>
  return (
    <div>
      <PageHeader title={tr("Добавить продукт")} icon={<PackagePlus size={22} />}
        subtitle={tr("Выберите плагин из каталога. После добавления экземпляр появится в разделе «Продукты».")} />
      {error && <div className="mb-4"><Alert tone="error">{error}</Alert></div>}
      <div className="flex flex-wrap gap-6 items-start">
        <Card className="flex-[3_1_560px] min-w-0">
          <label className="flex items-center gap-2 px-3 py-2 border border-slate-300 rounded-lg text-slate-500 mb-5 focus-within:ring-2 focus-within:ring-blue-500/30 focus-within:border-blue-500">
            <Search size={16} />
            <input className="flex-1 outline-none text-sm text-slate-800 bg-transparent" placeholder={tr("Поиск: postgres, файлы…")} value={search} onChange={e => setSearch(e.target.value)} />
          </label>
          {!catalog ? <Spinner /> : groups.length === 0 ? <Empty title={tr("Ничего не найдено")} text={tr("Измените строку поиска.")} /> : (
            <div className="space-y-5">
              {groups.map(([cat, items]) => (
                <div key={cat}>
                  <div className="text-xs font-semibold uppercase tracking-wider text-slate-500 mb-2.5">{cat}</div>
                  <div className="grid gap-2.5" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(190px, 1fr))' }}>
                    {items.map(p => (
                      <button key={p.id} type="button" onClick={() => pick(p)} disabled={!p.available} title={p.available ? p.description : tr("В разработке")}
                        className={cx('text-left flex gap-2.5 items-start p-3 rounded-xl bg-white',
                          sel?.id === p.id ? 'border-2 border-blue-600 ring-4 ring-blue-600/10' : 'border border-slate-200 hover:border-slate-300',
                          !p.available && 'opacity-55 grayscale cursor-not-allowed bg-slate-50')}>
                        <Tile product={p} />
                        <span className="min-w-0">
                          <span className="block text-sm font-semibold text-slate-900">{p.title}</span>
                          <span className="block text-xs text-slate-500 mt-0.5">{p.description}</span>
                          {!p.available && <span className="block mt-1"><InDev /></span>}
                        </span>
                      </button>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          )}
        </Card>

        <Card className="flex-[2_1_400px] min-w-0" padded={false}
          title={sel ? <span className="flex items-center gap-3"><Tile product={sel} /><span><span className="block">{tr("Новый экземпляр:")} {sel.title}</span><span className="block text-xs font-normal text-slate-500">{sel.modes}</span></span></span> : tr("Новый экземпляр")}>
          {!sel ? (
            <Empty icon={<Plus size={26} />} title={tr("Выберите продукт")} text={tr("Нажмите на плитку в каталоге слева — здесь появится форма с параметрами плагина.")} />
          ) : (
            <div className="p-5 space-y-4">
              {agents && agents.length === 0 && <Alert tone="warn">{tr("Нет ни одного агента.")} <Link to="/agents" className="underline font-medium">{tr("Подключите агент")}</Link>  {tr("на сервере, где работает")} {sel.title}.</Alert>}
              {sites && sites.length === 0 && <Alert tone="warn">{tr("Площадок пока нет —")} <Link to="/sites" className="underline font-medium">{tr("добавьте узел хранения")}</Link>.</Alert>}
              <Field label={tr("Имя экземпляра")}><Input value={name} onChange={e => { setName(e.target.value); setNameTouched(true) }} placeholder="PostgreSQL · db-prod-01" /></Field>
              {SERVER_ONLY.includes(sel.plugin) ? <ServerOnlyNote /> : <AgentSelect agents={agents} value={agentId} onChange={setAgentId} allowServer={SERVER_RUN.includes(sel.plugin)} />}
              {FOLLOWS_SOURCE.includes(sel.plugin) ? <SiteFromSource source={config.source} sites={sites} /> : <SiteSelect sites={sites} value={siteId} onChange={setSiteId} />}
              <div className="pt-2 border-t border-slate-100">
                <div className="text-xs font-semibold uppercase tracking-wider text-slate-500 mb-3 mt-2">{tr("Параметры плагина")} {sel.plugin}</div>
                <SchemaForm schema={sel.schema} value={config} onChange={setConfig} />
                {sel.plugin === 'machine' && config.machine_id && <div className="mt-3"><ImageNeeds mode="machine" machineId={config.machine_id} siteId={siteId} /></div>}
              </div>
              <div className="pt-2 border-t border-slate-100">
                <div className="text-xs font-semibold uppercase tracking-wider text-slate-500 mb-3 mt-2">{tr("Расписание и хранение")}</div>
                <ScheduleFields value={sched} onChange={setSched} product={sel} />
              </div>
              <div className="flex justify-end gap-2 pt-2">
                <Button onClick={() => setSel(null)}>{tr("Отмена")}</Button>
                <Button kind="primary" icon={<Plus size={16} />} loading={busy} onClick={submit}>{tr("Добавить")}</Button>
              </div>
            </div>
          )}
        </Card>
      </div>
    </div>
  )
}
