import { useState } from 'react'
import { ArrowRight, CheckCircle2, FlaskConical, Pencil, Play, Plus, RefreshCw, Repeat, RotateCcw, Square, Trash2, Truck, Wrench, Zap } from 'lucide-react'
import { del, fmtTime, post, put, SECRET_MASK } from '../api'
import type { Agent, Site } from '../types'
import { Alert, Badge, Button, Card, ChoiceCard, Confirm, Empty, Field, IconButton, Input, Modal, PageHeader, Select, Spinner, StatusDot, Toggle, cx, useApi, useToast, RefreshButton } from '../components/ui'
import { JobLogModal } from '../components/JobLog'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

type MigEvent = { t: string; what: string; job: string; ok: boolean }
export type Migration = {
  id: string; name: string; instance_id: string; target_agent: string; target_agent_name: string; source_agent: string; source_agent_name: string
  platform: 'proxmox' | 'ovirt'; params: Record<string, any>; state: Record<string, any> | null
  phase: 'syncing' | 'testing' | 'cutover' | 'running' | 'done'; auto_apply: boolean; shutdown_source: boolean
  last_sync: { id: string; time: string; cutover: boolean } | null; busy: { job: string; step: string } | null; pending_apply: boolean
  events: MigEvent[]; created: string; schedule: { enabled: boolean; full?: string | null }; source_config: Record<string, any>; site_id: string
}

const PHASES: { id: Migration['phase']; label: string }[] = [
  { id: 'syncing', label: tr("Синхронизации") }, { id: 'testing', label: tr("Тест") }, { id: 'cutover', label: tr("Переключение") }, { id: 'running', label: tr("Работает на цели") }, { id: 'done', label: tr("Завершён") },
]
const PLATFORM: Record<string, string> = { proxmox: 'Proxmox VE', ovirt: tr("zVirt / oVirt / РОСА") }
const STEP: Record<string, string> = {
  apply: tr("применение на цели"), 'cutover-sync': tr("последняя синхронизация"), 'cutover-apply': tr("применение переключения"), poweroff: tr("выключение источника"),
  start: tr("запуск ВМ"), 'test-start': tr("тестовый запуск"), 'test-stop': tr("окончание теста"), rollback: tr("откат"), finish: tr("завершение"),
}

function vmOf(m: Migration): string {
  const s = m.state || {}
  if (m.platform === 'proxmox') return s.vmid ? trf("ВМ {0} · {1}", s.vmid, s.storage || '') : ''
  return s.vm_id ? trf("ВМ {0} · {1}", String(s.vm_id).slice(0, 8), s.storage || '') : ''
}

export function MigrationsPage() {
  const perms = usePerms()
  const toast = useToast()
  const { data, error, reload } = useApi<Migration[]>('/api/migrations', [], 4000)
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 15000)
  const { data: sites } = useApi<Site[]>('/api/sites', [])
  const [edit, setEdit] = useState<Migration | 'new' | null>(null)
  const [log, setLog] = useState<string | null>(null)
  const [ask, setAsk] = useState<{ m: Migration; op: string } | null>(null)
  const [remove, setRemove] = useState<Migration | null>(null)
  const [busy, setBusy] = useState('')
  const step = async (m: Migration, op: string) => {
    setBusy(m.id + op)
    try { const r = await post<{ job_id: string }>(`/api/migrations/${encodeURIComponent(m.id)}/${op}`); setLog(r.job_id); reload() }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doRemove = async () => {
    const m = remove!; setRemove(null)
    try { await del(`/api/migrations/${encodeURIComponent(m.id)}`); toast.ok(trf("Переезд «{0}» удалён", m.name)); reload() } catch (e) { toast.err(e) }
  }
  const asks: Record<string, { title: string; yes: string; danger?: boolean; text: (m: Migration) => React.ReactNode }> = {
    cutover: { title: tr("Переключить на цель?"), yes: tr("Переключить"), danger: true, text: m => <>
      {m.source_config?.source === 'vsphere' ? <>{tr("Гостевая ОС ВМ «")}<b>{m.source_config.vm}</b>{tr("» будет штатно выключена, затем выполнится последняя синхронизация выключенной ВМ.")}</>
        : <>{tr("Остановятся службы приложений")}{(m.source_config?.stop_services || []).length ? <> (<b>{m.source_config.stop_services.join(', ')}</b>)</> : ''}{tr(", файловые системы источника будут заморожены на время последней синхронизации.")}</>}
      
      {tr("Затем изменения применятся к ВМ на цели,")} {m.shutdown_source ? <><b>{tr("источник будет выключен")}</b>  {tr("и ВМ на цели запущена с теми же MAC-адресами.")}</> : <>{tr("и ВМ на цели запустится —")} <b>{tr("источник не выключается")}</b>{tr(": следите за конфликтом адресов.")}</>}
      <div className="mt-2 text-slate-500">{tr("Простой — время последней синхронизации и загрузки ВМ. Откат возможен до завершения переезда.")}</div></> },
    rollback: { title: tr("Откатить переезд?"), yes: tr("Откатить"), danger: true, text: m => <>{tr("ВМ на цели будет остановлена, её тома возвращены к моменту переключения (или помечены к перезаписи, если хранилище не умеет снимки).")} {m.shutdown_source ? tr("Включите источник вручную — синхронизации продолжатся.") : tr("Службы источника будут запущены снова.")}</> },
    finish: { title: tr("Завершить переезд?"), yes: tr("Завершить"), text: () => <>{tr("Снимок отката на цели будет удалён — вернуться к источнику после этого можно только новым переездом.")}</> },
    'test-start': { title: tr("Тестовый запуск ВМ на цели?"), yes: tr("Запустить"), text: () => <>{tr("ВМ запустится")} <b>{tr("без сети")}</b>  {tr("(интерфейсы отключены — у неё те же адреса, что у работающего источника). Тома перед запуском сохраняются снимком; после теста возвращаются к нему. Синхронизации продолжаются, применение отложится до конца теста.")}</> },
    prepare: { title: tr("Подготовить источник к KVM?"), yes: tr("Подготовить"), text: () => <>{tr("На источнике: драйверы virtio в initramfs, имена сетевых интерфейсов закрепляются за MAC-адресами, fstab — по UUID, гостевой агент QEMU. Работающую систему это не нарушает; изменённые файлы сохраняются рядом с суффиксом")} <code>.hb-prep</code>.</> },
  }
  return (
    <div>
      <PageHeader title={tr("Переезд")} icon={<Truck size={22} />}
        subtitle={tr("Перенос Linux-машин на Proxmox VE и zVirt / oVirt / РОСА: блочная реплика дисков, синхронизации только изменений, тестовый запуск без сети, переключение с коротким простоем и откат.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Новый переезд")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<Truck size={28} />} title={tr("Переездов пока нет")}
          text={<>{tr("Нужны два агента: на переносимой машине (источник) и на узле Proxmox VE или рядом с Engine zVirt/oVirt (цель). Агент работает как обычная программа, без своих драйверов ядра.")}</>}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Новый переезд")}</Button>} /></Card>
      ) : (
        <div className="space-y-4">
          {data.map(m => {
            const src = agents?.find(a => a.id === m.source_agent), tgt = agents?.find(a => a.id === m.target_agent)
            const applied = m.state?.applied
            const fresh = m.last_sync && applied !== m.last_sync.id
            const pi = PHASES.findIndex(p => p.id === m.phase)
            const b = (op: string, label: string, icon: React.ReactNode, kind: 'primary' | 'secondary' | 'danger' = 'secondary', disabled = false) =>
              perms.edit && <Button key={op} kind={kind} className="!py-1.5 text-xs" icon={icon} loading={busy === m.id + op} disabled={disabled || !!m.busy}
                onClick={() => asks[op] ? setAsk({ m, op }) : step(m, op)}>{label}</Button>
            return (
              <Card key={m.id} title={<span className="flex items-center gap-2">{m.name}<Badge tone={m.phase === 'done' ? 'green' : m.phase === 'running' ? 'violet' : m.phase === 'testing' ? 'amber' : 'blue'}>{PHASES[pi]?.label || m.phase}</Badge></span>}
                actions={perms.edit && <>
                  {m.phase !== 'done' && <IconButton title={tr("Изменить")} onClick={() => setEdit(m)}><Pencil size={16} /></IconButton>}
                  <IconButton title={tr("Удалить")} danger onClick={() => setRemove(m)}><Trash2 size={16} /></IconButton>
                </>}>
                <ol className="flex flex-wrap items-center gap-1.5 text-xs mb-4">
                  {PHASES.map((p, i) => <li key={p.id} className="flex items-center gap-1.5">
                    <span className={cx('px-2 py-0.5 rounded-full border', i < pi ? 'border-emerald-200 bg-emerald-50 text-emerald-700' : i === pi ? 'border-blue-300 bg-blue-50 text-blue-800 font-semibold' : 'border-slate-200 text-slate-400')}>{p.label}</span>
                    {i < PHASES.length - 1 && <ArrowRight size={12} className="text-slate-300" />}
                  </li>)}
                </ol>
                <div className="grid md:grid-cols-3 gap-4 text-sm">
                  <div>
                    <div className="text-xs text-slate-500 mb-1">{tr("Источник")}</div>
                    <div className="flex items-center gap-2"><StatusDot tone={src?.online ? 'green' : 'red'} />{m.source_agent_name || m.source_agent}</div>
                    <div className="text-xs text-slate-500 mt-0.5">{m.source_config?.source === 'vsphere' ? <>{tr("vSphere без агента · ВМ «")}{m.source_config.vm}»</> : (m.source_config?.disks || []).length ? m.source_config.disks.join(', ') : tr("все диски")}{m.schedule?.full ? <>  {tr("· синхр.")} <span className="font-mono">{m.schedule.full}</span>{!m.schedule.enabled && tr(" (выкл.)")}</> : tr(" · синхронизации вручную")}</div>
                  </div>
                  <div>
                    <div className="text-xs text-slate-500 mb-1">{tr("Цель ·")} {PLATFORM[m.platform]}</div>
                    <div className="flex items-center gap-2"><StatusDot tone={tgt?.online ? 'green' : 'red'} />{m.target_agent_name || m.target_agent}</div>
                    <div className="text-xs text-slate-500 mt-0.5">{vmOf(m) || trf("ВМ создаётся при первом применении · {0}", m.params?.storage || '')}{m.state?.dirty && <span className="text-amber-700">  {tr("· тома будут переписаны")}</span>}</div>
                  </div>
                  <div>
                    <div className="text-xs text-slate-500 mb-1">{tr("Синхронизация")}</div>
                    <div>{m.last_sync ? <>{fmtTime(m.last_sync.time)}{m.last_sync.cutover && <Badge tone="violet">{tr("последняя")}</Badge>}</> : <span className="text-slate-400">{tr("ещё не было")}</span>}</div>
                    <div className="text-xs mt-0.5">{applied ? (fresh ? <span className="text-amber-700">{tr("на цели — предыдущая")}{m.pending_apply && tr(" (применение отложено)")}</span> : <span className="text-emerald-700">{tr("применена на цели")}</span>) : <span className="text-slate-400">{tr("на цель не применялась")}</span>}</div>
                  </div>
                </div>
                {m.busy && <Alert tone="info"><span className="flex items-center gap-2"><Spinner />{tr("Выполняется:")} {STEP[m.busy.step] || m.busy.step} · <button className="text-blue-700 hover:underline" onClick={() => setLog(m.busy!.job)}>{tr("журнал")}</button></span></Alert>}
                <div className="flex flex-wrap gap-2 mt-4">
                  {m.phase === 'syncing' && <>
                    {b('sync', tr("Синхронизировать"), <Repeat size={14} />)}
                    {b('apply', tr("Применить на цели"), <Play size={14} />, 'secondary', !fresh && !m.state?.dirty)}
                    {b('test-start', tr("Тестовый запуск"), <FlaskConical size={14} />, 'secondary', !applied)}
                    {b('cutover', tr("Переключить"), <Zap size={14} />, 'primary', !applied)}
                    {b('prepare', tr("Подготовка к KVM"), <Wrench size={14} />)}
                  </>}
                  {m.phase === 'testing' && b('test-stop', tr("Завершить тест"), <Square size={14} />, 'primary')}
                  {m.phase === 'running' && <>
                    {b('finish', tr("Завершить переезд"), <CheckCircle2 size={14} />, 'primary')}
                    {b('rollback', tr("Откат"), <RotateCcw size={14} />, 'danger')}
                  </>}
                </div>
                {m.events.length > 0 && <details className="mt-4">
                  <summary className="text-xs text-slate-500 cursor-pointer select-none">{tr("События (")}{m.events.length})</summary>
                  <ul className="mt-2 space-y-1 text-xs">
                    {m.events.slice(0, 15).map((e, i) => <li key={i} className="flex gap-2">
                      <span className="text-slate-400 whitespace-nowrap">{fmtTime(e.t)}</span>
                      <span className={e.ok ? 'text-slate-700' : 'text-red-700'}>{e.what}</span>
                      {e.job && <button className="text-blue-700 hover:underline whitespace-nowrap" onClick={() => setLog(e.job)}>{tr("журнал")}</button>}
                    </li>)}
                  </ul>
                </details>}
              </Card>
            )
          })}
        </div>
      )}
      {edit && <MigrationModal mig={edit === 'new' ? null : edit} agents={agents || []} sites={sites || []} onClose={() => setEdit(null)} onSaved={() => { setEdit(null); reload() }} />}
      <JobLogModal id={log} onClose={() => { setLog(null); reload() }} />
      <Confirm open={!!ask} danger={ask ? asks[ask.op].danger : false} yes={ask ? asks[ask.op].yes : ''} title={ask ? asks[ask.op].title : ''} onNo={() => setAsk(null)}
        onYes={() => { const a = ask!; setAsk(null); step(a.m, a.op) }} text={ask ? asks[ask.op].text(ask.m) : ''} />
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить переезд?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Переезд «")}<b>{remove?.name}</b>{tr("» и его синхронизации по расписанию будут удалены. ВМ на цели и копии в репозитории не удаляются.")}</>} />
    </div>
  )
}

const list = (s: string) => s.split(/[\s,]+/).map(x => x.trim()).filter(Boolean)

function MigrationModal({ mig, agents, sites, onClose, onSaved }: { mig: Migration | null; agents: Agent[]; sites: Site[]; onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const p0 = mig?.params || {}
  const [platform, setPlatform] = useState<'proxmox' | 'ovirt'>(mig?.platform || 'proxmox')
  const [f, setF] = useState({
    name: mig?.name || '', source_agent: mig?.source_agent || '', site_id: mig?.site_id || sites[0]?.id || '', target_agent: mig?.target_agent || '',
    disks: (mig?.source_config?.disks || []).join(', '), stop_services: (mig?.source_config?.stop_services || []).join(', '), sync_cron: mig?.schedule?.full || '0 * * * *',
    storage: p0.storage || '', network: p0.networks?.['*'] || '', vm_name: p0.name || '', cores: p0.cores ? String(p0.cores) : '', vmid: p0.vmid ? String(p0.vmid) : '', memory_mb: p0.memory_mb ? String(p0.memory_mb) : '',
    src_kind: (mig?.source_config?.source === 'vsphere' ? 'vsphere' : 'agent') as 'agent' | 'vsphere',
    vs_url: mig?.source_config?.url || '', vs_user: mig?.source_config?.user || '', vs_password: mig?.source_config?.password || '', vs_vm: mig?.source_config?.vm || '', vs_insecure: !!mig?.source_config?.insecure,
    guest_user: mig?.source_config?.guest_user || 'root', guest_password: mig?.source_config?.guest_password || '', hard_poweroff: !!mig?.source_config?.hard_poweroff,
    url: p0.url || '', user: p0.user || 'admin@internal', password: p0.password || '', cluster: p0.cluster || '', insecure: p0.insecure ?? false,
    auto_apply: mig?.auto_apply ?? true, shutdown_source: mig?.shutdown_source ?? true,
  })
  const set = (p: Partial<typeof f>) => setF({ ...f, ...p })
  const [busy, setBusy] = useState(false)
  const created = !!mig?.state
  const save = async () => {
    if (!f.name.trim()) return toast.err(tr("Укажите имя переезда"))
    if (!mig && !f.source_agent) return toast.err(tr("Выберите агента источника"))
    if (!f.target_agent) return toast.err(tr("Выберите агента цели"))
    if (!mig && !f.site_id) return toast.err(tr("Выберите площадку для реплики"))
    if (!f.storage.trim()) return toast.err(platform === 'proxmox' ? tr("Укажите хранилище Proxmox") : tr("Укажите домен хранения"))
    if (f.src_kind === 'vsphere' && (!f.vs_url.trim() || !f.vs_vm.trim())) return toast.err(tr("Укажите адрес vCenter и ВМ"))
    const source: Record<string, any> = f.src_kind === 'vsphere'
      ? { source: 'vsphere', url: f.vs_url.trim(), user: f.vs_user.trim(), password: f.vs_password || (mig ? SECRET_MASK : ''), vm: f.vs_vm.trim(), insecure: f.vs_insecure,
          guest_user: f.guest_user.trim(), guest_password: f.guest_password || (mig ? SECRET_MASK : ''), hard_poweroff: f.hard_poweroff }
      : { disks: list(f.disks), stop_services: list(f.stop_services) }
    // Прочие параметры (заданные через API) сохраняются; поля формы — поверх.
    const params: Record<string, any> = { ...p0, storage: f.storage.trim() }
    for (const k of ['networks', 'name', 'cores', 'memory_mb', 'vmid']) delete params[k]
    if (f.network.trim()) params.networks = { ...(p0.networks || {}), '*': f.network.trim() }
    if (f.vm_name.trim()) params.name = f.vm_name.trim()
    if (f.cores) params.cores = Number(f.cores)
    if (f.memory_mb) params.memory_mb = Number(f.memory_mb)
    if (platform === 'proxmox' && f.vmid) params.vmid = Number(f.vmid)
    if (platform === 'ovirt') Object.assign(params, { url: f.url.trim(), user: f.user.trim(), password: f.password || (mig ? SECRET_MASK : ''), cluster: f.cluster.trim(), insecure: f.insecure })
    setBusy(true)
    try {
      if (mig) await put(`/api/migrations/${encodeURIComponent(mig.id)}`, { name: f.name, params, auto_apply: f.auto_apply, shutdown_source: f.shutdown_source, target_agent: f.target_agent, source: f.src_kind === 'vsphere' ? source : undefined })
      else await post('/api/migrations', {
        name: f.name.trim(), source_agent: f.source_agent, site_id: f.site_id, target_agent: f.target_agent, platform, params, auto_apply: f.auto_apply, shutdown_source: f.shutdown_source,
        source, sync_cron: f.sync_cron.trim(),
      })
      toast.ok(mig ? tr("Переезд сохранён") : tr("Переезд создан — запустите первую синхронизацию")); onSaved()
    } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const agentOpts = <>{agents.map(a => <option key={a.id} value={a.id}>{a.name}{a.host ? ` · ${a.host}` : ''}{a.online ? '' : tr(" (не в сети)")}</option>)}</>
  return (
    <Modal open xwide onClose={onClose} title={mig ? trf("Переезд «{0}»", mig.name) : tr("Новый переезд")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{mig ? tr("Сохранить") : tr("Создать")}</Button></>}>
      <div className="space-y-5">
        <Field label={tr("Имя")}><Input value={f.name} onChange={e => set({ name: e.target.value })} placeholder="web01 → Proxmox" /></Field>
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-2">{tr("Источник")}</div>
          <div className="grid sm:grid-cols-2 gap-2 mb-3" role="radiogroup">
            <ChoiceCard selected={f.src_kind === 'agent'} disabled={!!mig && f.src_kind !== 'agent'} onClick={() => set({ src_kind: 'agent' })} title={tr("Linux-машина с агентом")} text={tr("Любая платформа (VMware, vCloud Director, Yandex Cloud, «железо»): агент читает диски, заморозка ФС при переключении")} />
            <ChoiceCard selected={f.src_kind === 'vsphere'} disabled={!!mig && f.src_kind !== 'vsphere'} onClick={() => set({ src_kind: 'vsphere' })} title={tr("ВМ VMware vSphere без агента")} text={tr("Снимок и экспорт через vCenter/ESXi; при переключении гостевая ОС выключается штатно")} />
          </div>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={f.src_kind === 'agent' ? tr("Агент источника") : tr("Агент-сборщик")} hint={f.src_kind === 'agent' ? tr("Установлен на переносимой машине") : tr("Любой агент с доступом к vCenter и ESXi (порт 443)")}><Select value={f.source_agent} disabled={!!mig} onChange={e => set({ source_agent: e.target.value })}><option value="">{tr("— выберите —")}</option>{agentOpts}</Select></Field>
            <Field label={tr("Площадка реплики")} hint={tr("Где хранится цепочка синхронизаций")}><Select value={f.site_id} disabled={!!mig} onChange={e => set({ site_id: e.target.value })}>{sites.map(s => <option key={s.id} value={s.id}>{s.name}</option>)}</Select></Field>
            {f.src_kind === 'agent' ? <>
              <Field label={tr("Диски")} hint={tr("Через запятую (/dev/sda, /dev/vdb); пусто — все диски машины")}><Input className="font-mono text-[13px]" disabled={!!mig} value={f.disks} onChange={e => set({ disks: e.target.value })} placeholder={tr("все")} /></Field>
              <Field label={tr("Остановить при переключении")} hint={tr("Службы приложений (СУБД и т. п.) — для согласованной последней синхронизации")}><Input className="font-mono text-[13px]" disabled={!!mig} value={f.stop_services} onChange={e => set({ stop_services: e.target.value })} placeholder="postgresql, nginx" /></Field>
            </> : <>
              <Field label="vCenter / ESXi"><Input className="font-mono text-[13px]" value={f.vs_url} onChange={e => set({ vs_url: e.target.value })} placeholder="https://vcenter.corp" /></Field>
              <Field label={tr("ВМ")} hint={tr("Имя в инвентаре vSphere")}><Input value={f.vs_vm} onChange={e => set({ vs_vm: e.target.value })} placeholder="app01" /></Field>
              <Field label={tr("Пользователь vSphere")}><Input value={f.vs_user} onChange={e => set({ vs_user: e.target.value })} placeholder="administrator@vsphere.local" /></Field>
              <Field label={tr("Пароль vSphere")} hint={mig?.source_config?.password ? tr("Задан — оставьте пустым, чтобы не менять") : undefined}><Input type="password" value={f.vs_password === SECRET_MASK ? '' : f.vs_password} onChange={e => set({ vs_password: e.target.value })} /></Field>
              <Field label={tr("Пользователь гостевой ОС")} hint={tr("Для подготовки к KVM через VMware Tools: root или с sudo без пароля")}><Input value={f.guest_user} onChange={e => set({ guest_user: e.target.value })} /></Field>
              <Field label={tr("Пароль гостевой ОС")} hint={mig?.source_config?.guest_password ? tr("Задан — оставьте пустым, чтобы не менять") : undefined}><Input type="password" value={f.guest_password === SECRET_MASK ? '' : f.guest_password} onChange={e => set({ guest_password: e.target.value })} /></Field>
            </>}
            <Field label={tr("Синхронизации по расписанию (cron)")} hint={tr("Пусто — только вручную")}><Input className="font-mono text-[13px]" disabled={!!mig} value={f.sync_cron} onChange={e => set({ sync_cron: e.target.value })} /></Field>
          </div>
          {f.src_kind === 'vsphere' && <div className="mt-3 space-y-2">
            <Toggle checked={f.vs_insecure} onChange={v => set({ vs_insecure: v })} label={tr("Не проверять сертификат vCenter/ESXi")} />
            <Toggle checked={f.hard_poweroff} onChange={v => set({ hard_poweroff: v })} label={tr("Жёсткое выключение, если гостевая ОС не выключилась сама")} hint={tr("Иначе переключение остановится с ошибкой — ВМ продолжит работать")} />
          </div>}
        </div>
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-2">{tr("Цель")}</div>
          <div className="grid sm:grid-cols-2 gap-2 mb-3" role="radiogroup">
            <ChoiceCard selected={platform === 'proxmox'} disabled={!!mig && mig.platform !== 'proxmox'} onClick={() => setPlatform('proxmox')} title="Proxmox VE" text={tr("Агент на узле Proxmox: тома в хранилище узла (LVM, LVM-thin, ZFS, каталог, Ceph RBD), ВМ через qm")} />
            <ChoiceCard selected={platform === 'ovirt'} disabled={!!mig && mig.platform !== 'ovirt'} onClick={() => setPlatform('ovirt')} title={tr("zVirt / oVirt / РОСА")} text={tr("Агент с доступом к Engine: ВМ и диски через API, запись — передачами imageio")} />
          </div>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Агент цели")} hint={platform === 'proxmox' ? tr("Установлен на узле Proxmox VE") : tr("Любой агент с доступом к Engine и узлам (imageio)")}><Select value={f.target_agent} disabled={created} onChange={e => set({ target_agent: e.target.value })}><option value="">{tr("— выберите —")}</option>{agentOpts}</Select></Field>
            <Field label={platform === 'proxmox' ? tr("Хранилище") : tr("Домен хранения")}><Input className="font-mono text-[13px]" value={f.storage} onChange={e => set({ storage: e.target.value })} placeholder={platform === 'proxmox' ? 'local-lvm' : 'data-san1'} /></Field>
            {platform === 'ovirt' && <>
              <Field label={tr("Адрес API Engine")}><Input className="font-mono text-[13px]" value={f.url} onChange={e => set({ url: e.target.value })} placeholder="https://engine.corp/ovirt-engine/api" /></Field>
              <Field label={tr("Кластер")}><Input value={f.cluster} onChange={e => set({ cluster: e.target.value })} placeholder="Default" /></Field>
              <Field label={tr("Пользователь")}><Input value={f.user} onChange={e => set({ user: e.target.value })} /></Field>
              <Field label={tr("Пароль")} hint={p0.password ? tr("Задан — оставьте пустым, чтобы не менять") : undefined}><Input type="password" value={f.password === SECRET_MASK ? '' : f.password} onChange={e => set({ password: e.target.value })} /></Field>
            </>}
            <Field label={platform === 'proxmox' ? tr("Мост сети") : tr("Профиль сети")} hint={tr("Для всех интерфейсов; MAC-адреса — как у источника")}><Input className="font-mono text-[13px]" value={f.network} onChange={e => set({ network: e.target.value })} placeholder={platform === 'proxmox' ? 'vmbr0' : 'ovirtmgmt'} /></Field>
            <Field label={tr("Имя ВМ")} hint={tr("По умолчанию — имя машины источника")}><Input value={f.vm_name} onChange={e => set({ vm_name: e.target.value })} /></Field>
            {platform === 'proxmox' && <Field label="VMID" hint={tr("Пусто — следующий свободный в кластере")}><Input type="number" disabled={created} value={f.vmid} onChange={e => set({ vmid: e.target.value })} /></Field>}
            <Field label="vCPU" hint={tr("Пусто — как у источника")}><Input type="number" value={f.cores} onChange={e => set({ cores: e.target.value })} /></Field>
            <Field label={tr("Память, МиБ")} hint={tr("Пусто — как у источника")}><Input type="number" value={f.memory_mb} onChange={e => set({ memory_mb: e.target.value })} /></Field>
          </div>
        </div>
        <div className="space-y-2">
          <Toggle checked={f.auto_apply} onChange={v => set({ auto_apply: v })} label={tr("Применять каждую синхронизацию на цели")} hint={tr("ВМ на цели всё время близка к источнику — последняя синхронизация при переключении короче")} />
          <Toggle checked={f.shutdown_source} onChange={v => set({ shutdown_source: v })} label={tr("Выключить источник при переключении")} hint={tr("ВМ на цели получает те же MAC и адреса — источник должен быть выключен до её запуска")} />
          {platform === 'ovirt' && <Toggle checked={!!f.insecure} onChange={v => set({ insecure: v })} label={tr("Не проверять сертификат Engine и imageio")} />}
        </div>
      </div>
    </Modal>
  )
}
