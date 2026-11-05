import { useEffect, useMemo, useState } from 'react'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom'
import { ArchiveRestore, CalendarClock, Check, CheckCircle2, Clock, Copy, Database, Gauge, HardDriveDownload, History, ListChecks, Play, Radar, RefreshCw, Save, Trash2, Upload, Wand2, XCircle } from 'lucide-react'
import { del, fmtAgo, fmtBytes, fmtDuration, fmtTime, get, post, put, q, secondsBetween } from '../api'
import type { Agent, CatalogProduct, Instance, InstanceBody, Job, Points, RetryPolicy, Settings, Site } from '../types'
import { RetryFields, plural } from '../components/RetryFields'
import { Alert, Badge, Button, Card, Code, Confirm, Empty, Field, Input, KV, PageHeader, Spinner, Stat, Table, Tabs, Td, useApi, useToast, RefreshButton } from '../components/ui'
import { InstanceBadge, JobBadge, jobActive, kindLabel, notifyInstances, pretty, RpoValue, Tile, useCatalog } from '../components/common'
import { JobLogModal, useJob } from '../components/JobLog'
import { JobsTable } from '../components/JobsTable'
import { SchemaForm, schemaClean, schemaInitial } from '../components/SchemaForm'
import { AgentSelect, FOLLOWS_SOURCE, SERVER_ONLY, SERVER_RUN, ScheduleFields, ServerOnlyNote, SiteFromSource, SiteSelect, agentLabel, type ScheduleValue } from '../components/InstanceFields'
import { usePerms } from '../perms'
import { HostInventory } from '../components/HostInventory'
import { ImageReplNotice, ReplicationTab } from '../components/ReplicationTab'
import { tr, trf } from '../i18n'

type Tab = 'overview' | 'points' | 'schedule' | 'replication' | 'params' | 'log'
const TABS: { id: Tab; label: string }[] = [
  { id: 'overview', label: tr("Обзор") }, { id: 'points', label: tr("Точки восстановления") }, { id: 'schedule', label: tr("Расписание и хранение") }, { id: 'replication', label: tr("Репликация") },
  { id: 'params', label: tr("Параметры") }, { id: 'log', label: tr("Журнал") },
]

export const bodyOf = (i: Instance, over: Partial<InstanceBody> = {}): InstanceBody => ({
  name: i.name, catalog_id: i.catalog_id, agent_id: i.agent_id, site_id: i.site_id, config: i.config,
  schedule: i.schedule, retention: i.retention, rpo_target_min: i.rpo_target_min,
  retry_mode: i.retry ? 'custom' : 'inherit', retry: i.retry ?? null, ...over,
})

export function InstancePage() {
  const { id = '' } = useParams()
  const [sp, setSp] = useSearchParams()
  const tab = (TABS.find(t => t.id === sp.get('tab'))?.id || 'overview') as Tab
  const setTab = (t: Tab) => setSp(t === 'overview' ? {} : { tab: t }, { replace: true })
  const nav = useNavigate()
  const toast = useToast()
  const perms = usePerms()
  const path = `/api/instances/${encodeURIComponent(id)}`
  const { data: inst, error, reload, setData } = useApi<Instance>(path, [], 5000)
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 15000)
  const { data: sites } = useApi<Site[]>('/api/sites')
  const { product } = useCatalog()
  const [job, setJob] = useState<string | null>(null)
  const [busy, setBusy] = useState('')

  if (error && !inst) return <Alert tone="error">{error}</Alert>
  if (!inst) return <Spinner />
  const p = product(inst.catalog_id)
  const agent = agents?.find(a => a.id === inst.agent_id)
  const site = sites?.find(s => s.id === inst.site_id)
  const isPg = inst.plugin === 'postgresql'

  const backup = async (kind: string) => {
    setBusy(kind)
    try { const r = await post<{ job_id: string }>(`${path}/backup`, { kind }); setJob(r.job_id); toast.ok(tr("Задание копирования поставлено в очередь")) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const save = async (over: Partial<InstanceBody>) => {
    const r = await put<Instance>(path, bodyOf(inst, over))
    setData(r); notifyInstances(); toast.ok(tr("Сохранено"))
  }

  return (
    <div>
      <PageHeader title={inst.name} icon={<Tile product={p} fallback={inst.plugin} size={28} className="-m-0.5" />}
        subtitle={<span className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span>{p?.title || inst.plugin}</span>
          <span>· {inst.agent_id === 'server' ? tr("выполняет сервер управления") : <>{tr("агент")} {agent ? <><span className={`inline-block w-1.5 h-1.5 rounded-full align-middle ${agent.online ? 'bg-emerald-500' : 'bg-red-500'}`} /> {agent.name}</> : agentLabel(inst.agent_id, agents)}</>}</span>
          <span>{tr("· площадка")} {site?.name || inst.site_id}</span>
          <InstanceBadge status={inst.runtime?.status} />
        </span>}
        actions={<>
          {perms.edit && <Button kind="primary" icon={<Play size={16} />} loading={busy === 'full' || busy === 'files'} onClick={() => backup(inst.plugin === 'files' ? 'files' : 'full')}>{inst.plugin === 'qcow2images' ? tr("Собрать образы") : tr("Полная копия")}</Button>}
          {perms.edit && inst.plugin === 'qcow2images' && (inst.replicate || []).some(t => t.enabled) && <Button icon={<Upload size={16} />} loading={busy === 'ship'} title={tr("Отправить последний собранный набор qcow2 на выбранные площадки (только изменения)")}
            onClick={async () => { setBusy('ship'); try { const r = await post<{ job_id: string }>(`${path}/ship`, {}); setJob(r.job_id); toast.ok(tr("Отправка на площадки начата")) } catch (e) { toast.err(e) } finally { setBusy('') } }}>{tr("Отправить на площадки")}</Button>}
          {perms.edit && isPg && <Button icon={<HardDriveDownload size={16} />} loading={busy === 'dump'} onClick={() => backup('dump')}>{tr("Дамп")}</Button>}
          {perms.edit && (p?.kinds || []).map(k => <Button key={k.id} icon={<HardDriveDownload size={16} />} loading={busy === k.id} onClick={() => backup(k.id)} title={k.hint}>{k.title}</Button>)}
          {perms.restore && <Button icon={<ArchiveRestore size={16} />} onClick={() => nav(`/restore${q({ instance: inst.id })}`)}>{tr("Восстановить")}</Button>}
        </>} />
      <ImageReplNotice inst={inst} sites={sites} open={() => setTab('replication')} />
      <Tabs tabs={inst.plugin === 'replica' ? TABS.filter(t => t.id !== 'replication') : TABS} value={tab} onChange={setTab} />
      {tab === 'overview' && <OverviewTab inst={inst} product={p} openJob={setJob} openParams={perms.edit ? () => setTab('params') : undefined} />}
      {tab === 'points' && <PointsTab inst={inst} />}
      {tab === 'schedule' && <ScheduleTab inst={inst} save={save} product={p} />}
      {tab === 'replication' && <ReplicationTab inst={inst} openJob={setJob} />}
      {tab === 'params' && <ParamsTab inst={inst} product={p} agents={agents} sites={sites} save={save} onDeleted={() => { notifyInstances(); nav('/products') }} />}
      {tab === 'log' && <LogTab inst={inst} openJob={setJob} />}
      <JobLogModal id={job} onClose={() => setJob(null)} onDone={() => reload()} />
    </div>
  )
}

// ---------------------------------------------------------------- Обзор
function OverviewTab({ inst, product, openJob, openParams }: { inst: Instance; product: CatalogProduct | null; openJob: (id: string) => void; openParams?: () => void }) {
  const rt = inst.runtime
  const lb = rt?.last_backup
  const lj = rt?.last_job
  const isPg = inst.plugin === 'postgresql'
  const target = inst.rpo_target_min * 60
  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-4 gap-4">
        <Stat label={tr("Последняя копия")} icon={<History size={22} />} value={lb ? fmtAgo(lb.time) : '—'} sub={lb ? `${kindLabel(lb.kind)} · ${fmtBytes(lb.size)}` : tr("копий ещё не было")} />
        <Stat label={tr("Текущий RPO")} icon={<Gauge size={22} />} tone={rt?.rpo_seconds != null && target && rt.rpo_seconds > target ? 'red' : 'green'}
          value={rt?.rpo_seconds != null ? fmtDuration(rt.rpo_seconds) : '—'} sub={trf("цель {0}{1}", fmtDuration(target), rt?.wal ? tr(" · с учётом WAL") : '')} />
        <Stat label={tr("Следующий запуск")} icon={<CalendarClock size={22} />} tone="violet"
          value={!inst.schedule.enabled ? tr("выкл.") : rt?.next_run ? fmtAgo(rt.next_run) : '—'} sub={inst.schedule.enabled ? (rt?.next_run ? fmtTime(rt.next_run) : `cron ${inst.schedule.full}`) : tr("расписание выключено")} />
        <Stat label={tr("Последнее задание")} icon={lj?.status === 'failed' ? <XCircle size={22} /> : <CheckCircle2 size={22} />} tone={lj?.status === 'failed' ? 'red' : lj ? 'green' : 'slate'}
          value={lj ? <span className="text-lg"><JobBadge status={lj.status} /></span> : '—'} sub={lj ? (lj.finished ? fmtTime(lj.finished) : tr("выполняется")) : tr("заданий не было")} />
      </div>
      {lj?.status === 'ok' && (rt?.warnings?.length || 0) > 0 && (
        <Alert tone="warn" title={tr("Последняя копия создана, но есть предупреждения")}>
          <ul className="list-disc pl-5 space-y-1">{rt!.warnings!.map((w, i) => <li key={i} className="break-words">{w}</li>)}</ul>
          <button className="underline font-medium mt-1" onClick={() => openJob(lj.id)}>{tr("Открыть журнал")}</button>
        </Alert>
      )}
      {lj?.status === 'failed' && lj.error && (
        <Alert tone="error" title={tr("Последнее задание завершилось с ошибкой")}><span className="whitespace-pre-line">{lj.error}</span> <button className="underline font-medium ml-1" onClick={() => openJob(lj.id)}>{tr("Открыть журнал")}</button>
          {openParams && /Где поправить: страница экземпляра/.test(lj.error) && <button className="underline font-medium ml-3" onClick={openParams}>{tr("Открыть параметры")}</button>}
          {isPg && /архивация WAL/.test(lj.error) && <div className="mt-3"><WalSetup inst={inst} openJob={openJob} /></div>}
        </Alert>
      )}
      <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
        <Card title={tr("Последняя копия")} icon={<Database size={18} className="text-blue-600" />}>
          {!lb ? <Empty title={tr("Копий ещё не было")} text={inst.schedule.enabled ? trf("Первая полная копия — по расписанию ({0}) или по кнопке «Полная копия».", inst.schedule.full) : tr("Расписание выключено — запустите копию вручную.")} /> : <>
            <KV k={tr("Время")}>{fmtTime(lb.time, true)}</KV>
            <KV k={tr("Вид")}>{kindLabel(lb.kind)}</KV>
            <KV k={tr("Размер данных")}>{fmtBytes(lb.size)}</KV>
            <KV k={tr("Новых данных")}>{fmtBytes(lb.new_bytes)}</KV>
            <KV k={tr("Передано на площадку")}>{fmtBytes(lb.uploaded)}{lb.uploaded > 0 && lb.size > 0 && <span className="text-slate-400">  {tr("· экономия ×")}{(lb.size / lb.uploaded).toFixed(1).replace('.', ',')}</span>}</KV>
            <KV k={tr("Снапшот")} mono>{lb.snapshot_id}</KV>
          </>}
        </Card>
        {(isPg || rt?.wal) && <WalCard wal={rt?.wal || null} inst={inst} openJob={openJob} />}
      </div>
      <DiscoverCard inst={inst} product={product} openJob={openJob} />
    </div>
  )
}

/** Команда с кнопкой «Скопировать». */
function CmdLine({ cmd }: { cmd: string }) {
  const [done, setDone] = useState(false)
  const copy = async () => { try { await navigator.clipboard.writeText(cmd.replace(/\s+#\s.*$/, '')); setDone(true); setTimeout(() => setDone(false), 1500) } catch { /* буфер обмена недоступен */ } }
  return (
    <div className="mt-1 flex items-center gap-2 bg-slate-900 rounded-lg pl-3 pr-1.5 py-1.5">
      <code className="flex-1 text-xs font-mono text-emerald-300 overflow-x-auto whitespace-pre">{cmd}</code>
      <button type="button" onClick={copy} className="shrink-0 inline-flex items-center gap-1 px-2 py-1 rounded-md text-[11px] bg-slate-700 text-slate-100 hover:bg-slate-600">{done ? <Check size={12} /> : <Copy size={12} />}{done ? tr("Скопировано") : tr("Скопировать")}</button>
    </div>
  )
}

/** Настройка архива WAL по шагам: кнопка (агент меняет настройки PostgreSQL), перезапуск СУБД агентом, первая копия. */
function WalSetup({ inst, openJob }: { inst: Instance; openJob: (id: string) => void }) {
  const toast = useToast()
  const perms = usePerms()
  const { data: jobs, reload } = useApi<Job[]>(`/api/jobs${q({ instance_id: inst.id, limit: 50 })}`)
  const [ids, setIds] = useState<Record<string, string>>({})
  const latest = (mode: string) => ids[mode] || jobs?.find(j => j.kind === 'action' && j.mode === mode)?.id || null
  const { job } = useJob(latest('setup-wal'), () => reload())
  const { job: rjob } = useJob(latest('restart'), () => reload())
  const { data: agents } = useApi<Agent[]>('/api/agents')
  const agent = agents?.find(a => a.id === inst.agent_id)
  // Старый агент действия restart не знает — кнопку не предлагаем, подсказываем обновить агент.
  const oldAgent = !!agent && !(agent.caps || []).includes('pg-restart')
  const [busy, setBusy] = useState('')
  const [ask, setAsk] = useState(false)
  const action = async (name: string, msg: string) => {
    setBusy(name)
    try { const r = await post<{ job_id: string }>(`/api/instances/${encodeURIComponent(inst.id)}/action`, { name }); setIds(x => ({ ...x, [name]: r.job_id })); openJob(r.job_id); toast.ok(msg) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const backup = async () => {
    setBusy('full')
    try { const r = await post<{ job_id: string }>(`/api/instances/${encodeURIComponent(inst.id)}/backup`, { kind: 'full' }); openJob(r.job_id); toast.ok(tr("Полная копия запущена")) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const res: any = job && job.status === 'ok' ? job.result : null
  const rres: any = rjob && rjob.status === 'ok' ? rjob.result : null
  // Перезапуск засчитывается, только если он был после настройки.
  const restarted = !!rres && !!job && (rjob!.created || '') >= (job.created || '')
  const noRestart = !!res && res.restart_required === false
  const restartCmd: string = res?.restart_command || 'systemctl restart postgresql'
  const step2done = noRestart || restarted
  const n = (i: number, done: boolean) => <span className={`shrink-0 w-6 h-6 rounded-full text-xs font-semibold flex items-center justify-center ${done ? 'bg-emerald-600 text-white' : 'bg-blue-600 text-white'}`}>{done ? <Check size={13} /> : i}</span>
  const logLink = (j: Job | null | undefined) => j && <button className="text-xs text-blue-700 hover:underline" onClick={() => openJob(j.id)}>{tr("журнал")}{j.status === 'failed' ? tr(" — ошибка") : ''}</button>
  const noRole = <span className="text-amber-700 text-xs">{tr("Нужна роль оператора или администратора.")}</span>
  return (
    <div className="rounded-xl border border-blue-200 bg-white p-3 text-sm text-slate-700">
      <div className="font-semibold text-slate-900 mb-2">{tr("Как включить архив WAL — 3 шага")}</div>
      <ol className="space-y-3">
        <li className="flex gap-3">{n(1, !!res)}<div className="flex-1 min-w-0">
          <div className="font-medium text-slate-900">{tr("Настройте PostgreSQL")}</div>
          <div className="text-slate-600">{tr("Агент сам включит")} <span className="font-mono">archive_mode</span>{tr(", пропишет")} <span className="font-mono">archive_command</span>  {tr("(сегменты WAL складываются в спул агента и уходят на площадку) и")} <span className="font-mono">archive_timeout = 60s</span>.</div>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            {perms.edit ? <Button kind={res ? 'secondary' : 'primary'} className="!py-1.5 text-xs" icon={<Wand2 size={14} />} loading={busy === 'setup-wal' || (!!job && jobActive(job.status))} onClick={() => action('setup-wal', tr("Настройка архива WAL запущена"))}>{res ? tr("Настроить ещё раз") : tr("Настроить архив WAL")}</Button> : noRole}
            {logLink(job)}
          </div>
          {job?.status === 'failed' && <div className="mt-1 text-xs text-red-700 whitespace-pre-line">{job.error}</div>}
        </div></li>
        <li className="flex gap-3">{n(2, step2done)}<div className="flex-1 min-w-0">
          <div className="font-medium text-slate-900">{tr("Перезапустите PostgreSQL")}{noRestart ? tr(" — не нужно") : restarted ? tr(" — выполнено") : ''}</div>
          {noRestart ? <div className="text-slate-600">{tr("Архивация уже включена — перезапуск не требуется.")}</div> : <>
            <div className="text-slate-600"><span className="font-mono">archive_mode</span>  {tr("применяется только при перезапуске СУБД. База будет недоступна несколько секунд (подключения прервутся, приложения переподключатся) — выберите удобное время.")}</div>
            <div className="mt-2 flex flex-wrap items-center gap-2">
              {perms.edit ? <Button kind={res && !restarted && !oldAgent ? 'primary' : 'secondary'} className="!py-1.5 text-xs" icon={<RefreshCw size={14} />} disabled={!res || oldAgent}
                loading={busy === 'restart' || (!!rjob && jobActive(rjob.status))} onClick={() => setAsk(true)}>{tr("Перезапустить PostgreSQL")}</Button> : noRole}
              {logLink(rjob)}
              {!res && <span className="text-xs text-slate-500">{tr("сначала шаг 1")}</span>}
            </div>
            {oldAgent && <div className="mt-1.5 text-xs text-amber-800 bg-amber-50 border border-amber-200 rounded-lg px-2.5 py-1.5">{tr("Агент «")}{agent!.name}{tr("» версии")} {agent!.version || '?'}  {tr("ещё не умеет перезапускать PostgreSQL. Обновите его:")} {perms.admin ? <Link className="underline font-medium" to="/updates">{tr("«Обновления»")}</Link> : tr("«Обновления»")}  {tr("→ таблица агентов → «Обновить» — или перезапустите СУБД вручную командой ниже.")}</div>}
            {rjob?.status === 'failed' && !oldAgent && <div className="mt-1 text-xs text-red-700 whitespace-pre-line">{rjob.error}</div>}
            {restarted && <div className="mt-1 text-xs text-emerald-700">{tr("Перезапущено за")} {rres.seconds}  {tr("с (")}{rres.restarted}); archive_mode = {rres.archive_mode}{rres.archive_ok ? tr(" — архив WAL работает") : ''}.</div>}
            {!restarted && res && <details open={oldAgent} className="mt-1.5 text-xs text-slate-500"><summary className="cursor-pointer select-none">{tr("или вручную на сервере (от root)")}</summary><CmdLine cmd={restartCmd} /></details>}
          </>}
        </div></li>
        <li className="flex gap-3">{n(3, false)}<div className="flex-1 min-w-0">
          <div className="font-medium text-slate-900">{tr("Запустите копию")}</div>
          <div className="text-slate-600">{tr("В блоке «Архив WAL» появятся отправленные сегменты; восстановление на любой момент времени станет доступно.")}</div>
          {perms.edit && <div className="mt-2"><Button kind={step2done ? 'primary' : 'secondary'} className="!py-1.5 text-xs" icon={<Play size={14} />} disabled={!step2done} loading={busy === 'full'} onClick={backup}>{tr("Полная копия")}</Button></div>}
        </div></li>
      </ol>
      <Confirm open={ask} danger yes={tr("Перезапустить")} title={tr("Перезапустить PostgreSQL?")} onNo={() => setAsk(false)}
        onYes={() => { setAsk(false); action('restart', tr("Перезапуск PostgreSQL запущен")) }}
        text={<>{tr("Агент перезапустит кластер")} <b>{inst.name}</b>  {tr("на своём сервере командой")} <span className="font-mono text-xs">{restartCmd}</span>{tr(". Все подключения к базе прервутся, СУБД будет недоступна несколько секунд — приложения увидят короткий обрыв и переподключатся. Агент дождётся, пока СУБД снова примет подключения, и проверит, что архив WAL включился.")}</>} />
    </div>
  )
}

function WalCard({ wal, inst, openJob }: { wal: Instance['runtime']['wal']; inst: Instance; openJob: (id: string) => void }) {
  const toast = useToast()
  const perms = usePerms()
  const [busy, setBusy] = useState(false)
  const backup = async () => {
    setBusy(true)
    try { const r = await post<{ job_id: string }>(`/api/instances/${encodeURIComponent(inst.id)}/backup`, { kind: 'full' }); openJob(r.job_id); toast.ok(tr("Полная копия запущена")) }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const firstCopy = inst.plugin === 'postgresql' && !!wal && !wal.last_error && !inst.runtime?.last_backup && !(inst.runtime?.last_job && jobActive(inst.runtime.last_job.status))
  return (
    <Card title={tr("Архив WAL")} icon={<RefreshCw size={18} className="text-blue-600" />}
      actions={wal ? (wal.last_error ? <Badge tone="red">{tr("ошибка")}</Badge> : wal.queued > 20 ? <Badge tone="amber">{tr("очередь")} {wal.queued}</Badge> : <Badge tone="green">{tr("отправляется")}</Badge>) : <Badge>{tr("не настроен")}</Badge>}>
      {!wal ? (
        <div className="space-y-3">
          <div className="text-sm text-slate-600 leading-relaxed">{tr("Агент ещё не отправлял сегменты WAL. Без архива WAL полная копия не создаётся: восстановить её без журнала нельзя.")}</div>
          {inst.plugin === 'postgresql' && <WalSetup inst={inst} openJob={openJob} />}
        </div>
      ) : <>
        {wal.last_error && <div className="mb-3"><Alert tone="error" title={tr("Ошибка отправки WAL")}>{wal.last_error}</Alert></div>}
        {firstCopy && <div className="mb-3"><Alert tone="info" title={tr("Архив WAL включён — осталось сделать первую копию")}>
          <div className="mb-2">{tr("Полная копия станет точкой отсчёта: после неё можно восстановиться на любой момент времени по журналу.")}</div>
          {perms.edit && <Button kind="primary" className="!py-1.5 text-xs" icon={<Play size={14} />} loading={busy} onClick={backup}>{tr("Полная копия")}</Button>}
        </Alert></div>}
        <KV k={tr("Последний сегмент")} mono>{wal.last_shipped || '—'}</KV>
        <KV k={tr("Отправлен")}>{wal.last_time ? <>{fmtTime(wal.last_time, true)} <span className="text-slate-400">({fmtAgo(wal.last_time)})</span></> : '—'}</KV>
        <KV k={tr("В очереди на агенте")}>{wal.queued}</KV>
        <KV k={tr("Отправлено сегментов")}>{wal.shipped}</KV>
      </>}
    </Card>
  )
}

/** Обнаружение на агенте: последний результат и запуск нового. Для postgresql — разбор полей. */
function DiscoverCard({ inst, product, openJob }: { inst: Instance; product: CatalogProduct | null; openJob: (id: string) => void }) {
  const toast = useToast()
  const perms = usePerms()
  const { data: jobs } = useApi<Job[]>(`/api/jobs${q({ instance_id: inst.id, limit: 50 })}`)
  const [runId, setRunId] = useState<string | null>(null)
  const lastId = runId || jobs?.find(j => j.kind === 'discover')?.id || null
  const { job } = useJob(lastId)
  const [busy, setBusy] = useState('')
  const run = async () => {
    setBusy('discover')
    try { const r = await post<{ job_id: string }>(`/api/instances/${encodeURIComponent(inst.id)}/discover`); setRunId(r.job_id) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const res = job && !jobActive(job.status) ? job.result : null
  const saveConfig = async (config: Record<string, any>) => {
    try { await put(`/api/instances/${encodeURIComponent(inst.id)}`, bodyOf(inst, { config })); notifyInstances(); toast.ok(tr("Параметры экземпляра сохранены")) } catch (e) { toast.err(e) }
  }
  return (
    <Card title={inst.plugin === 'host' ? tr("Опись сервера") : tr("Обнаружение на агенте")} icon={<Radar size={18} className="text-blue-600" />}
      actions={<>
        {job && <button className="text-sm text-blue-600 hover:underline" onClick={() => openJob(job.id)}>{tr("Журнал")}</button>}
        {perms.edit && <Button icon={<Radar size={16} />} loading={busy === 'discover' || (!!job && jobActive(job.status) && job.id === runId)} onClick={run}>{tr("Обнаружить")}</Button>}
      </>}>
      {!lastId ? (
        <div className="text-sm text-slate-500">{inst.plugin === 'host'
          ? tr("Агент составит опись сервера: запущенные процессы и скрипты, задания cron и таймеры, свои службы, диски и сетевые каталоги, контейнеры, пользователей и ключи SSH. Нажмите «Обнаружить».")
          : <>{tr("Плагин")} {product?.title || inst.plugin}  {tr("проверит продукт на агенте: найдёт бинарники, версию и настройки архивации. Нажмите «Обнаружить».")}</>}</div>
      ) : !job ? <Spinner /> : jobActive(job.status) ? (
        <div className="flex items-center gap-3 text-sm text-slate-600"><JobBadge status={job.status} />  {tr("Агент выполняет обнаружение…")}</div>
      ) : job.status === 'failed' ? (
        <Alert tone="error" title={tr("Обнаружение не удалось")}><span className="whitespace-pre-line">{job.error || tr("см. журнал задания")}</span></Alert>
      ) : (
        <div className="space-y-3">
          <div className="text-xs text-slate-500">{tr("Результат от")} {fmtTime(job.finished, true)}</div>
          {inst.plugin === 'postgresql' && res && typeof res === 'object' ? <PgDiscover res={res} />
            : inst.plugin === 'host' && res && typeof res === 'object' ? <HostInventory res={res} config={inst.config || {}} canEdit={perms.edit} onConfig={saveConfig} />
            : <Code>{pretty(res)}</Code>}
          {inst.plugin === 'postgresql' && res?.archive_ok === false && perms.edit && (
            <Alert tone="warn" title={tr("Архив WAL не настроен")}>
              <div className="mb-2">{tr("Без архива WAL полная копия не создаётся, а восстановление на момент времени недоступно.")}{!inst.runtime?.wal && <>  {tr("Настройте его по шагам в блоке «Архив WAL» на этой странице — агент сам поменяет настройки и перезапустит PostgreSQL.")}</>}</div>
              {inst.runtime?.wal && <WalSetup inst={inst} openJob={openJob} />}
            </Alert>
          )}
        </div>
      )}
    </Card>
  )
}

function PgDiscover({ res }: { res: Record<string, any> }) {
  const known = ['bin_dir', 'version', 'flavor', 'archive_ok', 'recommended_archive_command', 'pending_restart']
  const rest = Object.fromEntries(Object.entries(res).filter(([k]) => !known.includes(k)))
  return (
    <div>
      <KV k={tr("Каталог бинарников")} mono>{res.bin_dir || '—'}</KV>
      <KV k={tr("Версия")}>{res.version || '—'}</KV>
      <KV k={tr("Сборка")}>{res.flavor || '—'}</KV>
      <KV k={tr("Архив WAL")}>{res.archive_ok ? <Badge tone="green"><CheckCircle2 size={12} />{tr("настроен")}</Badge> : <Badge tone="amber"><XCircle size={12} />{tr("не настроен")}</Badge>}</KV>
      {res.pending_restart && <KV k={tr("Перезапуск СУБД")}><Badge tone="amber">{tr("нужен — часть настроек применится только после него")}</Badge></KV>}
      {res.recommended_archive_command && <KV k={tr("Рекомендуемый archive_command")}><Code className="mt-0.5">{res.recommended_archive_command}</Code></KV>}
      {Object.keys(rest).length > 0 && <details className="mt-2"><summary className="text-xs text-slate-500 cursor-pointer">{tr("Остальные поля")}</summary><Code className="mt-2">{pretty(rest)}</Code></details>}
    </div>
  )
}

/** Скачать образы дисков снапшота готовыми файлами .qcow2 (admin): размер показывается до скачивания,
 *  ссылки одноразовые, каждое скачивание пишется в журнал аудита сервером. */
async function downloadImages(instId: string, snapId: string, toast: ReturnType<typeof useToast>) {
  try {
    const d = await get<{ disks: { name: string; size: number }[] }>(`/api/image/disks${q({ instance_id: instId, snapshot_id: snapId })}`)
    const disks = d.disks || []
    if (!disks.length) { toast.err(tr("В этом снапшоте нет образов дисков")); return }
    const prepared: { url: string; name: string; size: number }[] = []
    for (const disk of disks) prepared.push(await post('/api/image/download', { instance_id: instId, snapshot_id: snapId, disk: disk.name, format: 'qcow2' }))
    const total = prepared.reduce((a, p) => a + (p.size || 0), 0)
    if (!window.confirm(trf("Будет собрано и скачано файлов: {0}, примерно {1}.\nФайлы собираются на сервере — для крупных дисков это небыстро. Продолжить?", prepared.length, fmtBytes(total)))) return
    for (const p of prepared) { const a = document.createElement('a'); a.href = p.url; a.download = p.name; document.body.appendChild(a); a.click(); a.remove(); await new Promise(r => setTimeout(r, 400)) }
    toast.ok(tr("Скачивание начато (сборка образа может занять время)"))
  } catch (e: any) { toast.err(e?.message || tr("Не удалось подготовить скачивание")) }
}

// ---------------------------------------------------------------- Точки восстановления
function PointsTab({ inst }: { inst: Instance }) {
  const nav = useNavigate()
  const perms = usePerms()
  const toast = useToast()
  const { data, error, reload, loading } = useApi<Points>(`/api/instances/${encodeURIComponent(inst.id)}/points`, [], 30000)
  if (error && !data) return <Alert tone="error">{error}</Alert>
  if (!data) return <Spinner />
  const snaps = [...data.snapshots].sort((a, b) => b.time.localeCompare(a.time))
  return (
    <div className="space-y-4">
      {data.pitr ? (
        <Alert tone="info" title={tr("Восстановление на любой момент (PITR)")}>
          
          {tr("Архив WAL непрерывен: доступен любой момент с")} <b>{fmtTime(data.pitr.from, true)}</b>  {tr("по")} <b>{fmtTime(data.pitr.to, true)}</b>.
          {perms.restore && <> <Link to={`/restore${q({ instance: inst.id })}`} className="underline font-medium">{tr("Восстановить на момент")}</Link></>}
        </Alert>
      ) : inst.plugin === 'postgresql' && <Alert tone="warn">{tr("Архив WAL пуст или прерывается — восстановление возможно только на моменты копий.")}</Alert>}
      {data.wal && (
        <div className="grid grid-cols-2 lg:grid-cols-4 gap-4">
          <Stat label={tr("Сегментов WAL")} value={data.wal.count} icon={<RefreshCw size={20} />} tone="violet" />
          <Stat label={tr("Объём архива")} value={fmtBytes(data.wal.bytes)} icon={<Database size={20} />} tone="violet" />
          <Stat label={tr("Первый / последний")} value={<span className="text-sm font-mono">{data.wal.first}</span>} sub={data.wal.last} />
          <Stat label={tr("Последний сегмент")} value={<span className="text-base">{fmtAgo(data.wal.last_time)}</span>} sub={fmtTime(data.wal.last_time, true)} icon={<Clock size={20} />} />
        </div>
      )}
      <Card padded={false} title={trf("Точки восстановления · {0}", snaps.length)} actions={<RefreshButton onClick={reload} />}>
        {snaps.length === 0 ? <Empty icon={<History size={28} />} title={tr("Точек восстановления пока нет")} text={tr("Запустите полную копию — первая точка появится здесь.")} /> : (
          <Table head={[tr("Время"), tr("Вид"), tr("Хост"), tr("Размер"), tr("Новых"), tr("Передано"), tr("Блоков"), tr("Снапшот"), '']}>
            {snaps.map(s => (
              <tr key={s.id} className="hover:bg-slate-50">
                <Td className="whitespace-nowrap">{fmtTime(s.time, true)}</Td>
                <Td><Badge tone={s.kind === 'full' ? 'blue' : s.kind === 'dump' ? 'violet' : 'slate'}>{kindLabel(s.kind)}</Badge></Td>
                <Td className="text-slate-600">{s.host}</Td>
                <Td>{fmtBytes(s.size)}</Td>
                <Td>{fmtBytes(s.new_bytes)}</Td>
                <Td>{fmtBytes(s.uploaded)}</Td>
                <Td className="text-slate-500">{s.chunks}</Td>
                <Td className="font-mono text-xs text-slate-500" title={s.id}>{s.id.slice(0, 12)}</Td>
                <Td className="text-right whitespace-nowrap">
                  {inst.plugin === 'machine' && perms.admin && <Button className="!py-1 !px-2.5 text-xs mr-2" icon={<HardDriveDownload size={14} />} onClick={() => downloadImages(inst.id, s.id, toast)}>{tr("Скачать .qcow2")}</Button>}
                  {perms.restore && <Button className="!py-1 !px-2.5 text-xs" icon={<ArchiveRestore size={14} />} onClick={() => nav(`/restore${q({ instance: inst.id, snapshot: s.id })}`)}>{inst.plugin === 'machine' ? tr("Развернуть") : tr("Восстановить")}</Button>}
                </Td>
              </tr>
            ))}
          </Table>
        )}
      </Card>
    </div>
  )
}

// ---------------------------------------------------------------- Расписание и хранение
function ScheduleTab({ inst, save, product }: { inst: Instance; save: (o: Partial<InstanceBody>) => Promise<void>; product: CatalogProduct | null }) {
  const toast = useToast()
  const perms = usePerms()
  const [v, setV] = useState<ScheduleValue>({ schedule: inst.schedule, retention: inst.retention, rpo_target_min: inst.rpo_target_min })
  const initRetry = inst.retry ?? null
  const [retry, setRetry] = useState<RetryPolicy | null>(initRetry)
  const { data: settings } = useApi<Settings>('/api/settings')
  const global = settings?.settings.retry
  const [busy, setBusy] = useState(false)
  const dirty = JSON.stringify(v) !== JSON.stringify({ schedule: inst.schedule, retention: inst.retention, rpo_target_min: inst.rpo_target_min }) || JSON.stringify(retry) !== JSON.stringify(initRetry)
  const submit = async () => {
    if (!v.schedule.full.trim()) return toast.err(tr("Укажите расписание полной копии"))
    if (retry && (retry.attempts < 0 || retry.attempts > 24 || retry.interval_min < 1 || retry.interval_min > 1440)) return toast.err(tr("Повторов — 0–24, пауза — 1–1440 минут"))
    setBusy(true)
    try { await save({ ...v, retry_mode: retry ? 'custom' : 'inherit', retry }) } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Card title={tr("Расписание и хранение")} icon={<CalendarClock size={18} className="text-blue-600" />} className="max-w-3xl"
      actions={perms.edit && <Button kind="primary" icon={<Save size={16} />} loading={busy} disabled={!dirty} onClick={submit}>{tr("Сохранить")}</Button>}>
      <ScheduleFields value={v} onChange={setV} disabled={!perms.edit} product={product} />
      {inst.runtime?.next_run && <div className="text-xs text-slate-500 mt-4">{tr("Следующий запуск:")} {fmtTime(inst.runtime.next_run)} ({fmtAgo(inst.runtime.next_run)})</div>}
      <div className="mt-6 pt-5 border-t border-slate-100">
        <div className="text-sm font-semibold text-slate-800 mb-1">{tr("Повторы при ошибке")}</div>
        <p className="text-xs text-slate-500 mb-3">{tr("Если копия по расписанию не удалась (агент недоступен, обрыв связи с площадкой, ошибка), Ковчег повторит её. Письмо об ошибке — только когда повторов больше не будет.")}</p>
        <div className="space-y-2">
          <label className="flex items-center gap-2 text-sm"><input type="radio" checked={!retry} disabled={!perms.edit} onChange={() => setRetry(null)} />
            
            {tr("Как в настройках сервера")}{global ? <span className="text-slate-500">— {global.enabled ? trf("{0} {1} через {2} мин", global.attempts, plural(global.attempts, tr("повтор"), tr("повтора"), tr("повторов")), global.interval_min) : tr("повторы выключены")}</span> : null}</label>
          <label className="flex items-center gap-2 text-sm"><input type="radio" checked={!!retry} disabled={!perms.edit} onChange={() => setRetry(retry ?? { enabled: true, attempts: global?.attempts ?? 3, interval_min: global?.interval_min ?? 60 })} />{tr("Свои для этого экземпляра")}</label>
        </div>
        {retry && <div className="mt-3 ml-6"><RetryFields value={retry} onChange={setRetry} disabled={!perms.edit} /></div>}
      </div>
    </Card>
  )
}

// ---------------------------------------------------------------- Параметры
function ParamsTab({ inst, product, agents, sites, save, onDeleted }: {
  inst: Instance; product: CatalogProduct | null; agents: Agent[] | null; sites: Site[] | null
  save: (o: Partial<InstanceBody>) => Promise<void>; onDeleted: () => void
}) {
  const toast = useToast()
  const perms = usePerms()
  const init = useMemo(() => ({ name: inst.name, agent_id: inst.agent_id, site_id: inst.site_id, config: schemaInitial(product?.schema, undefined, inst.config) }), [inst, product])
  const [v, setV] = useState(init)
  const [busy, setBusy] = useState(false)
  const [confirm, setConfirm] = useState(false)
  useEffect(() => { setV(init) }, [inst.id, product?.id])
  const submit = async () => {
    if (!v.name.trim()) return toast.err(tr("Укажите имя экземпляра"))
    setBusy(true)
    try { await save({ name: v.name.trim(), agent_id: SERVER_ONLY.includes(inst.plugin) ? 'server' : v.agent_id, site_id: v.site_id, config: schemaClean(product?.schema, v.config) }) }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const remove = async () => {
    setConfirm(false)
    try { await del(`/api/instances/${encodeURIComponent(inst.id)}`); toast.ok(trf("Экземпляр «{0}» удалён", inst.name)); onDeleted() } catch (e) { toast.err(e) }
  }
  const ro = !perms.edit
  return (
    <div className="space-y-6 max-w-3xl">
      <Card title={tr("Экземпляр")} actions={!ro && <Button kind="primary" icon={<Save size={16} />} loading={busy} onClick={submit}>{tr("Сохранить")}</Button>}>
        <div className="space-y-4">
          <Field label={tr("Имя")}><Input value={v.name} disabled={ro} onChange={e => setV({ ...v, name: e.target.value })} /></Field>
          <div className="grid sm:grid-cols-2 gap-4">
            {SERVER_ONLY.includes(inst.plugin) ? <ServerOnlyNote /> : <AgentSelect agents={agents} value={v.agent_id} disabled={ro} onChange={x => setV({ ...v, agent_id: x })} allowServer={SERVER_RUN.includes(inst.plugin)} />}
            {FOLLOWS_SOURCE.includes(inst.plugin) ? <SiteFromSource source={v.config?.source} sites={sites} /> : <SiteSelect sites={sites} value={v.site_id} disabled={ro} onChange={x => setV({ ...v, site_id: x })} />}
          </div>
          <KV k={tr("Идентификатор")} mono>{inst.id}</KV>
          <KV k={tr("Плагин")} mono>{inst.plugin} · {inst.catalog_id}</KV>
          <KV k={tr("Создан")}>{fmtTime(inst.created)}</KV>
        </div>
      </Card>
      <Card title={tr("Параметры плагина")} actions={<span className="text-xs text-slate-400">{tr("Форма построена по схеме плагина")} {inst.plugin}</span>}>
        {!product ? <Spinner /> : <SchemaForm schema={product.schema} value={v.config} disabled={ro} columns={2} onChange={c => setV({ ...v, config: c })} />}
        {!ro && <div className="flex justify-end mt-5"><Button kind="primary" icon={<Save size={16} />} loading={busy} onClick={submit}>{tr("Сохранить")}</Button></div>}
      </Card>
      {!ro && (
        <Card title={tr("Удаление")}>
          <div className="flex items-center justify-between gap-4">
            <div className="text-sm text-slate-600">{tr("Экземпляр исчезнет из меню, расписание остановится. Точки восстановления на площадке не удаляются.")}</div>
            <Button kind="danger" icon={<Trash2 size={16} />} onClick={() => setConfirm(true)}>{tr("Удалить")}</Button>
          </div>
        </Card>
      )}
      <Confirm open={confirm} danger title={tr("Удалить экземпляр?")} yes={tr("Удалить")} onNo={() => setConfirm(false)} onYes={remove}
        text={<>{tr("Удалить «")}<b>{inst.name}</b>{tr("»? Копии на площадке останутся, но расписание и параметры будут удалены.")}</>} />
    </div>
  )
}

// ---------------------------------------------------------------- Журнал
function LogTab({ inst, openJob }: { inst: Instance; openJob: (id: string) => void }) {
  const { data, error } = useApi<Job[]>(`/api/jobs${q({ instance_id: inst.id, limit: 200 })}`, [], 5000)
  if (error && !data) return <Alert tone="error">{error}</Alert>
  if (!data) return <Spinner />
  return (
    <Card padded={false}>
      {data.length === 0 ? <Empty icon={<ListChecks size={28} />} title={tr("Заданий ещё не было")} text={tr("Здесь появятся копии, обнаружения и восстановления этого экземпляра.")} /> : (
        <JobsTable jobs={data} onOpen={openJob} />
      )}
    </Card>
  )
}
