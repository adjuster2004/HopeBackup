import { useEffect, useMemo, useState } from 'react'
import { Link, useSearchParams } from 'react-router-dom'
import { ArchiveRestore, ArrowLeft, ArrowRight, CheckCircle2, Plus, Play, Server, XCircle, AlertTriangle, Bot, Cloud } from 'lucide-react'
import { fmtBytes, fmtLocalTz, fmtTime, parseUserTime, post } from '../api'
import { VM_KINDS, type Agent, type Instance, type Points, type Site, type SnapshotHeader, type Target } from '../types'
import { vmWhere } from './TargetsPage'
import { Alert, Button, Card, ChoiceCard, cx, Empty, Field, Input, KV, PageHeader, Select, Spinner, Table, Td, Toggle, useApi, useToast } from '../components/ui'
import { kindLabel, useCatalog } from '../components/common'
import { SchemaForm, schemaClean, schemaInitial } from '../components/SchemaForm'
import { JobView, useJob } from '../components/JobLog'
import { NoAccess } from '../components/Layout'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

type Mode = 'pitr' | 'point'
type Params = {
  pgdata: string; bin_dir: string; os_user: string; port: number; start: boolean; mode: 'cluster' | 'dump'; database: string
  root: string; verify: boolean; reload: boolean; install: boolean
}
const DEFAULT_PARAMS: Params = { pgdata: '/var/lib/postgresql/16/restore', bin_dir: '', os_user: 'postgres', port: 5433, start: true, mode: 'cluster', database: '', root: '/', verify: true, reload: false, install: false }
/** Поправки к параметрам ВМ-цели на это восстановление (пусто — как в цели). */
type VmOver = { vm_name: string; cores: string; memory_mb: string; disk_gb: string; ip: string; destroy_on_failure: boolean }
const VM_OVER: VmOver = { vm_name: '', cores: '', memory_mb: '', disk_gb: '', ip: '', destroy_on_failure: true }

/** Базовая копия для PITR: последняя физическая копия не позже указанного момента. */
function baseFor(snaps: SnapshotHeader[], at: Date | null) {
  if (!at) return null
  return [...snaps].filter(s => s.kind !== 'dump' && new Date(s.time).getTime() <= at.getTime()).sort((a, b) => b.time.localeCompare(a.time))[0] || null
}

export function RestorePage() {
  const perms = usePerms()
  if (!perms.restore) return <NoAccess />
  return <Wizard />
}

function Wizard() {
  const [sp] = useSearchParams()
  const toast = useToast()
  const { data: instances } = useApi<Instance[]>('/api/instances')
  const { data: targets } = useApi<Target[]>('/api/targets')
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 15000)
  const { data: sites } = useApi<Site[]>('/api/sites')
  const [srcSite, setSrcSite] = useState('')
  const [step, setStep] = useState(1)
  const [instanceId, setInstanceId] = useState(sp.get('instance') || '')
  const [mode, setMode] = useState<Mode>(sp.get('snapshot') ? 'point' : 'pitr')
  const [snapshotId, setSnapshotId] = useState(sp.get('snapshot') || '')
  const [when, setWhen] = useState('')
  const [targetId, setTargetId] = useState('')
  const [via, setVia] = useState<'direct' | 'server'>('direct')
  const [params, setParams] = useState<Params>(DEFAULT_PARAMS)
  const [vm, setVm] = useState<VmOver>(VM_OVER)
  const { product } = useCatalog()
  const [pp, setPp] = useState<Record<string, any>>({})
  const [busy, setBusy] = useState(false)
  const [jobId, setJobId] = useState<string | null>(null)
  const { job } = useJob(jobId)

  useEffect(() => { if (!instanceId && instances?.length === 1) setInstanceId(instances[0].id) }, [instances])
  useEffect(() => { if (!targetId && targets?.length === 1) setTargetId(targets[0].id) }, [targets])
  const inst = instances?.find(i => i.id === instanceId) || null
  const isPg = inst?.plugin === 'postgresql'
  const rschema = inst ? product(inst.catalog_id)?.restore_schema : undefined
  const custom = !!rschema?.properties && Object.keys(rschema.properties).length > 0
  useEffect(() => { setPp(schemaInitial(rschema)) }, [inst?.id, !!rschema])
  // Площадки, где есть копии экземпляра: основная и связанные репликацией.
  const srcSites = useMemo(() => {
    if (!inst || !sites) return []
    const main = sites.find(x => x.id === inst.site_id)
    const rel = sites.filter(x => x.id !== inst.site_id && (main?.replicas.includes(x.id) || x.replicas.includes(inst.site_id)))
    return [...(main ? [main] : []), ...rel]
  }, [inst?.id, inst?.site_id, sites])
  useEffect(() => { setSrcSite('') }, [inst?.id])
  const pts = useApi<Points>(instanceId ? `/api/instances/${encodeURIComponent(instanceId)}/points${srcSite ? `?site=${encodeURIComponent(srcSite)}` : ''}` : null, [srcSite])
  const points = pts.data && instanceId && !pts.loading ? pts.data : null
  const snaps = useMemo(() => [...(points?.snapshots || [])].sort((a, b) => b.time.localeCompare(a.time)), [points])

  // Новый экземпляр — параметры по умолчанию, bin_dir из его настроек.
  useEffect(() => {
    if (!inst) return
    setParams(p => ({ ...p, bin_dir: typeof inst.config?.bin_dir === 'string' ? inst.config.bin_dir : '', port: 5433 }))
  }, [inst?.id])
  // Точки загружены: режим, момент и точка по умолчанию.
  useEffect(() => {
    if (!points) return
    if (!points.pitr && mode === 'pitr') setMode('point')
    if (points.pitr && !when) setWhen(fmtLocalTz(points.pitr.to))
    if (!snapshotId || !points.snapshots.some(s => s.id === snapshotId)) setSnapshotId(snaps[0]?.id || '')
  }, [points])

  const at = parseUserTime(when)
  const pitrBase = mode === 'pitr' ? baseFor(snaps, at) : null
  const snap = mode === 'pitr' ? pitrBase : snaps.find(s => s.id === snapshotId) || null
  const pitrRangeOk = !!(at && points?.pitr && at.getTime() >= new Date(points.pitr.from).getTime() && at.getTime() <= new Date(points.pitr.to).getTime())
  const target = targets?.find(t => t.id === targetId) || null
  const targetAgent = target?.kind === 'agent' ? agents?.find(a => a.id === target.agent_id) : null
  const isVm = !!target && VM_KINDS.includes(target.kind)
  // Новая ВМ — чистая машина: PostgreSQL ставится, кластер — на стандартный порт.
  useEffect(() => {
    if (!target) return
    const vmT = VM_KINDS.includes(target.kind)
    setParams(p => ({ ...p, install: vmT, port: vmT ? 5432 : (p.port === 5432 ? 5433 : p.port) }))
    setVm({ ...VM_OVER, destroy_on_failure: target.params?.destroy_on_failure ?? true })
  }, [target?.id])

  const pickSnap = (s: SnapshotHeader) => { setSnapshotId(s.id); if (isPg) setParams(p => ({ ...p, mode: s.kind === 'dump' ? 'dump' : 'cluster' })) }

  const valid: Record<number, string> = {
    1: !inst ? tr("Выберите экземпляр") : mode === 'pitr' ? (!at ? tr("Укажите момент времени в формате 2026-10-24 14:31:00+03") : !pitrBase ? tr("Нет копии раньше указанного момента") : '') : (!snap ? tr("Выберите точку восстановления") : ''),
    2: !target ? tr("Выберите цель восстановления") : '',
    3: isPg && params.mode === 'cluster' && !params.pgdata.trim() ? tr("Укажите каталог данных (pgdata)") : isPg && params.mode === 'dump' && !params.database.trim() ? tr("Укажите базу для восстановления дампа") : !isPg && !custom && !params.root.trim() ? tr("Укажите каталог восстановления") : '',
    4: '',
  }
  const firstInvalid = [1, 2, 3].find(n => valid[n]) || 5
  const canGo = (n: number) => !jobId && n <= firstInvalid
  const next = () => { if (valid[step]) return toast.err(valid[step]); setStep(step + 1) }

  const start = async () => {
    if (!inst || !snap || !target) return
    setBusy(true)
    try {
      const r = await post<{ job_id: string }>('/api/restore', {
        instance_id: inst.id, snapshot_id: snap.id, target_id: target.id, via, site_id: srcSite,
        params: {
          ...(custom ? schemaClean(rschema, pp) : { ...params, port: Number(params.port) || 0, target_time: mode === 'pitr' ? when.trim() : '' }),
          ...(isVm ? { vm: { vm_name: vm.vm_name.trim(), ip: vm.ip.trim(), destroy_on_failure: vm.destroy_on_failure, ...Object.fromEntries((['cores', 'memory_mb', 'disk_gb'] as const).filter(k => vm[k].trim()).map(k => [k, parseInt(vm[k], 10)])) } } : {}),
        },
      })
      setJobId(r.job_id)
      toast.ok(tr("Восстановление запущено"))
    } catch (e) { toast.err(e) } finally { setBusy(false) }
  }

  const whenText = mode === 'pitr' ? (when || '—') : snap ? `${fmtTime(snap.time, true)} — ${kindLabel(snap.kind)}` : '—'
  const steps = [
    { n: 1, label: tr("Точка"), sub: inst ? whenText : '' },
    { n: 2, label: tr("Цель"), sub: target?.name || '' },
    { n: 3, label: tr("Параметры"), sub: isPg ? (params.mode === 'dump' ? tr("одна база (дамп)") : tr("кластер целиком")) : custom ? (product(inst!.catalog_id)?.title || '') : inst ? trf("в {0}", params.root) : '' },
    { n: 4, label: tr("Проверка и запуск"), sub: job ? (job.status === 'ok' ? tr("завершено") : job.status === 'failed' ? tr("ошибка") : tr("выполняется")) : '' },
  ]

  if (!instances) return <Spinner />
  return (
    <div className="max-w-6xl">
      <PageHeader title={inst ? trf("Восстановление {0}", inst.name) : tr("Мастер восстановления")} icon={<ArchiveRestore size={22} />}
        subtitle={tr("Точка → цель → параметры → проверка. Ничего не меняется, пока вы не нажмёте «Запустить».")} />
      {instances.length === 0 ? <Card><Empty icon={<ArchiveRestore size={28} />} title={tr("Восстанавливать пока нечего")} text={tr("Добавьте продукт и дождитесь первой копии — затем вернитесь в мастер.")} action={<Link to="/products/new" className="text-sm text-blue-600 hover:underline">{tr("Добавить продукт")}</Link>} /></Card> : <>
      <ol className="flex flex-wrap gap-2 mb-6">
        {steps.map(s => {
          const done = s.n < step || (s.n === 4 && job?.status === 'ok'), cur = s.n === step
          return (
            <li key={s.n} className="flex-[1_1_180px]">
              <button type="button" disabled={!canGo(s.n)} onClick={() => setStep(s.n)}
                className={cx('w-full flex items-center gap-2.5 px-3 py-2.5 rounded-xl bg-white text-left disabled:cursor-not-allowed', cur ? 'border-2 border-blue-600' : 'border border-slate-200')}>
                <span className={cx('w-7 h-7 rounded-full inline-flex items-center justify-center text-[13px] font-bold shrink-0', done ? 'bg-emerald-100 text-emerald-800' : cur ? 'bg-blue-600 text-white' : 'bg-slate-100 text-slate-600')}>{done ? '✓' : s.n}</span>
                <span className="min-w-0"><span className="block text-sm font-semibold text-slate-900">{s.label}</span><span className="block text-xs text-slate-500 truncate">{s.sub}</span></span>
              </button>
            </li>
          )
        })}
      </ol>

      <Card>
        {step === 1 && (
          <div className="space-y-5">
            <Field label={tr("Экземпляр")} className="max-w-md">
              <Select value={instanceId} disabled={!!jobId} onChange={e => { setInstanceId(e.target.value); setSnapshotId(''); setWhen(''); setMode('pitr') }}>
                <option value="">{tr("— выберите экземпляр —")}</option>
                {instances.map(i => <option key={i.id} value={i.id}>{i.name}</option>)}
              </Select>
            </Field>
            {srcSites.length > 1 && (
              <Field label={tr("Площадка-источник")} className="max-w-md" hint={tr("Основная или принимающая площадка — копии там одинаковые; при потере основной выберите принимающую.")}>
                <Select value={srcSite || inst!.site_id} disabled={!!jobId} onChange={e => { setSrcSite(e.target.value === inst!.site_id ? '' : e.target.value); setSnapshotId('') }}>
                  {srcSites.map((x, i) => <option key={x.id} value={x.id}>{x.name}{i === 0 ? tr(" — основная") : tr(" — принимающая")}</option>)}
                </Select>
              </Field>
            )}
            {inst && (pts.error ? <Alert tone="error">{pts.error}</Alert> : !points ? <Spinner /> : snaps.length === 0 ? (
              <Alert tone="warn">{tr("У экземпляра ещё нет точек восстановления — сначала сделайте полную копию.")}</Alert>
            ) : <>
              <div role="radiogroup" aria-label={tr("Что восстанавливать")} className="flex flex-wrap gap-2.5">
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={mode === 'pitr'} disabled={!points.pitr} onClick={() => setMode('pitr')} title={tr("Момент времени (PITR)")}
                  text={points.pitr ? tr("ближайшая копия + журнал WAL до указанной секунды") : tr("недоступно: нет непрерывного архива WAL")} /></div>
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={mode === 'point'} onClick={() => setMode('point')} title={tr("Готовая точка")} text={tr("полная копия или дамп из списка")} /></div>
              </div>
              {mode === 'pitr' && points.pitr ? (
                <div className="space-y-3">
                  <Field label={tr("Восстановить на момент")} className="max-w-md" hint={<>{tr("Доступно с")} {fmtTime(points.pitr.from, true)}  {tr("по")} {fmtTime(points.pitr.to, true)}{tr(". Формат: 2026-10-24 14:31:00+03")}</>}>
                    <Input className="font-mono text-[13px]" value={when} onChange={e => setWhen(e.target.value)} />
                  </Field>
                  {at && !pitrRangeOk && <Alert tone="warn">{tr("Момент вне непрерывного диапазона архива WAL — восстановление может не дойти до указанной секунды.")}</Alert>}
                  {pitrBase && <div className="text-sm text-slate-600">{tr("Базовая копия:")} <b>{fmtTime(pitrBase.time, true)}</b> · {kindLabel(pitrBase.kind)} · {fmtBytes(pitrBase.size)} <span className="font-mono text-xs text-slate-400">{pitrBase.id.slice(0, 12)}</span>{tr(", далее — журнал WAL.")}</div>}
                  {at && !pitrBase && <Alert tone="error">{tr("Нет физической копии раньше указанного момента.")}</Alert>}
                </div>
              ) : (
                <div className="border border-slate-200 rounded-xl overflow-hidden">
                  <Table head={['', tr("Время"), tr("Вид"), tr("Хост"), tr("Размер"), tr("Снапшот")]}>
                    {snaps.map(s => (
                      <tr key={s.id} onClick={() => pickSnap(s)} className={cx('cursor-pointer', snapshotId === s.id ? 'bg-blue-50' : 'hover:bg-slate-50')}>
                        <Td className="w-8"><input type="radio" readOnly checked={snapshotId === s.id} className="text-blue-600" /></Td>
                        <Td className="whitespace-nowrap">{fmtTime(s.time, true)}</Td>
                        <Td>{kindLabel(s.kind)}</Td>
                        <Td className="text-slate-600">{s.host}</Td>
                        <Td>{fmtBytes(s.size)}</Td>
                        <Td className="font-mono text-xs text-slate-500">{s.id.slice(0, 12)}</Td>
                      </tr>
                    ))}
                  </Table>
                </div>
              )}
            </>)}
          </div>
        )}

        {step === 2 && (
          <div className="space-y-4">
            <div className="text-sm text-slate-600">{tr("Куда восстанавливать: сервер по SSH, машина с агентом или новая ВМ у гипервизора или в облаке.")}</div>
            {!targets ? <Spinner /> : targets.length === 0 ? (
              <Empty icon={<Server size={28} />} title={tr("Целей восстановления пока нет")} text={tr("Добавьте сервер по SSH или машину с агентом в разделе «Цели восстановления».")}
                action={<Link to="/targets" className="inline-flex items-center gap-1.5 text-sm text-blue-600 hover:underline"><Plus size={14} />{tr("Добавить цель")}</Link>} />
            ) : <>
              <div className="grid gap-2.5" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(260px, 1fr))' }}>
                {targets.map(t => {
                  const a = t.kind === 'agent' ? agents?.find(x => x.id === t.agent_id) : null
                  const v = VM_KINDS.includes(t.kind)
                  return <ChoiceCard key={t.id} selected={targetId === t.id} onClick={() => setTargetId(t.id)}
                    icon={<span className={cx('p-2 rounded-lg', v ? 'bg-violet-50 text-violet-600' : 'bg-blue-50 text-blue-600')}>{t.kind === 'agent' ? <Bot size={18} /> : v ? <Cloud size={18} /> : <Server size={18} />}</span>}
                    title={t.name}
                    text={t.kind === 'agent' ? <>{tr("агент")} {a?.name || t.agent_id} · {a ? (a.online ? tr("в сети") : tr("не в сети")) : '—'}</> : v ? <>{tr("новая ВМ ·")} {vmWhere(t)}</> : <>SSH {t.user}@{t.host}:{t.port}</>} />
                })}
              </div>
              <Link to="/targets" className="inline-flex items-center gap-1.5 text-sm text-blue-600 hover:underline"><Plus size={14} />{tr("Добавить цель")}</Link>
            </>}
          </div>
        )}

        {step === 3 && inst && (
          <div className="space-y-5">
            {isPg ? <>
              <div role="radiogroup" className="flex flex-wrap gap-2.5">
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={params.mode === 'cluster'} onClick={() => setParams({ ...params, mode: 'cluster' })} title={tr("Кластер целиком")} text={tr("физическая копия в новый каталог данных")} /></div>
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={params.mode === 'dump'} disabled={mode === 'pitr'} onClick={() => setParams({ ...params, mode: 'dump' })} title={tr("Одна база (логический дамп)")} text={mode === 'pitr' ? tr("недоступно для PITR") : tr("pg_restore в существующий кластер")} /></div>
              </div>
              <div className="grid sm:grid-cols-2 gap-4">
                {params.mode === 'cluster' && <Field label={tr("Каталог данных (pgdata)")} hint={tr("Должен быть пустым или отсутствовать.")}><Input className="font-mono text-[13px]" value={params.pgdata} onChange={e => setParams({ ...params, pgdata: e.target.value })} /></Field>}
                {params.mode === 'dump' && <Field label={tr("База данных")} hint={tr("Имя базы, в которую восстановить дамп.")}><Input value={params.database} onChange={e => setParams({ ...params, database: e.target.value })} /></Field>}
                <Field label={tr("Каталог бинарников (bin_dir)")} hint={tr("Пусто — найти на цели автоматически.")}><Input className="font-mono text-[13px]" value={params.bin_dir} onChange={e => setParams({ ...params, bin_dir: e.target.value })} /></Field>
                <Field label={tr("Пользователь ОС")} hint={tr("Владелец каталога данных.")}><Input value={params.os_user} onChange={e => setParams({ ...params, os_user: e.target.value })} /></Field>
                <Field label={tr("Порт")} hint={tr("Порт восстановленного экземпляра (не занятый продуктивным).")}><Input type="number" value={params.port} onChange={e => setParams({ ...params, port: parseInt(e.target.value, 10) || 0 })} /></Field>
              </div>
              {params.mode === 'cluster' && <Toggle checked={params.start} onChange={v => setParams({ ...params, start: v })} label={tr("Запустить PostgreSQL после восстановления")} hint={tr("Иначе каталог данных останется подготовленным, но сервер не стартует.")} />}
              {inst.catalog_id !== 'qdatabase' && inst.catalog_id !== 'qbase' && <Toggle checked={params.install} onChange={v => setParams({ ...params, install: v })} label={tr("Установить PostgreSQL той же версии, если его нет")} hint={tr("Из репозитория дистрибутива или PGDG (нужен доступ цели в интернет или к зеркалу). Для qdatabase — только заранее установленная сборка.")} />}
            </> : custom ? <SchemaForm schema={rschema} value={pp} onChange={setPp} columns={2} /> : <>
              <Field label={tr("Куда восстанавливать (root)")} className="max-w-md" hint={tr("«/» — на исходные пути; другой каталог — рядом, для сверки.")}><Input className="font-mono text-[13px]" value={params.root} onChange={e => setParams({ ...params, root: e.target.value })} /></Field>
              <Toggle checked={params.verify} onChange={v => setParams({ ...params, verify: v })} label={tr("Проверить контрольные суммы")} hint={tr("Сверить восстановленные файлы с манифестом точки.")} />
              <Toggle checked={params.reload} onChange={v => setParams({ ...params, reload: v })} label={tr("Перезагрузить службу после восстановления")} hint={tr("Например, nginx -s reload — по правилам плагина.")} />
            </>}
            {isVm && (
              <div className="pt-4 border-t border-slate-100 space-y-3">
                <div className="text-sm font-medium text-slate-700">{tr("Новая ВМ")} <span className="font-normal text-slate-500">{tr("— пусто: как в цели «")}{target!.name}»</span></div>
                <div className="grid sm:grid-cols-3 gap-4">
                  <Field label={tr("Имя ВМ")} className="sm:col-span-3" hint={tr("Пусто — hb-<экземпляр>-<дата>")}><Input className="font-mono text-[13px]" value={vm.vm_name} onChange={e => setVm({ ...vm, vm_name: e.target.value })} /></Field>
                  <Field label="vCPU"><Input type="number" value={vm.cores} placeholder={String(target!.params?.cores ?? 2)} onChange={e => setVm({ ...vm, cores: e.target.value })} /></Field>
                  <Field label={tr("Память, МиБ")}><Input type="number" value={vm.memory_mb} placeholder={String(target!.params?.memory_mb ?? 4096)} onChange={e => setVm({ ...vm, memory_mb: e.target.value })} /></Field>
                  <Field label={tr("Диск, ГиБ")}><Input type="number" value={vm.disk_gb} placeholder={target!.params?.disk_gb ? String(target!.params.disk_gb) : tr("как в шаблоне")} onChange={e => setVm({ ...vm, disk_gb: e.target.value })} /></Field>
                  {(target!.kind === 'proxmox' || target!.kind === 'vmware') && <Field label={tr("Адрес ВМ")} className="sm:col-span-3" hint={tr("dhcp или 10.0.0.50/24 (шлюз и DNS — из цели)")}><Input className="font-mono text-[13px]" value={vm.ip} placeholder={target!.params?.ip || 'dhcp'} onChange={e => setVm({ ...vm, ip: e.target.value })} /></Field>}
                </div>
                <Toggle checked={vm.destroy_on_failure} onChange={v => setVm({ ...vm, destroy_on_failure: v })} label={tr("Удалить ВМ, если восстановление не удалось")} hint={tr("Выключите, чтобы оставить машину для разбора.")} />
              </div>
            )}
            <div className="pt-4 border-t border-slate-100">
              <div className="text-sm font-medium text-slate-700 mb-2">{tr("Откуда цель получает данные")}</div>
              <div role="radiogroup" className="flex flex-wrap gap-2.5">
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={via === 'direct'} onClick={() => setVia('direct')} title={tr("Напрямую с площадки")} text={tr("цель сама подключается к узлу хранения — быстрее")} /></div>
                <div className="flex-[1_1_260px] flex"><ChoiceCard selected={via === 'server'} onClick={() => setVia('server')} title={tr("Через сервер управления")} text={tr("обратный SSH-туннель: цели не нужен доступ к площадке")} /></div>
              </div>
            </div>
          </div>
        )}

        {step === 4 && inst && (
          <div className="space-y-5">
            <div className="grid lg:grid-cols-2 gap-6">
              <div>
                <KV k={tr("Экземпляр")}>{inst.name}</KV>
                <KV k={mode === 'pitr' ? tr("Момент") : tr("Точка")}><span className={cx(mode === 'pitr' && 'font-mono text-xs')}>{whenText}</span></KV>
                {snap && <KV k={tr("Копия")} mono>{snap.id} · {fmtBytes(snap.size)}</KV>}
                <KV k={tr("Цель")}>{target ? `${target.name} · ${target.kind === 'agent' ? trf("агент {0}", targetAgent?.name || target.agent_id) : isVm ? trf("новая ВМ: {0}", vmWhere(target)) : `${target.user}@${target.host}:${target.port}`}` : '—'}</KV>
                {isVm && <KV k={tr("ВМ")}>{vm.vm_name || tr("имя по умолчанию")} · {vm.cores || target!.params?.cores || 2} vCPU · {vm.memory_mb || target!.params?.memory_mb || 4096}  {tr("МиБ")}{vm.disk_gb ? trf(" · {0} ГиБ", vm.disk_gb) : ''} · {vm.destroy_on_failure ? tr("удалить при сбое") : tr("оставить при сбое")}</KV>}
                <KV k={tr("Данные")}>{srcSite ? trf("с площадки «{0}» · ", sites?.find(x => x.id === srcSite)?.name || srcSite) : ''}{via === 'direct' ? tr("напрямую с площадки") : tr("через сервер управления")}</KV>
                {isPg ? <>
                  <KV k={tr("Режим")}>{params.mode === 'dump' ? trf("одна база: {0}", params.database) : tr("кластер целиком")}</KV>
                  {params.mode === 'cluster' && <KV k="pgdata" mono>{params.pgdata}</KV>}
                  <KV k="bin_dir" mono>{params.bin_dir || tr("автоматически")}</KV>
                  <KV k={tr("Пользователь · порт")}>{params.os_user} · {params.port}{params.mode === 'cluster' ? (params.start ? tr(" · запустить") : tr(" · не запускать")) : ''}</KV>
                </> : custom ? <>
                  {Object.entries(rschema!.properties!).filter(([k]) => pp[k] !== undefined && pp[k] !== '' && rschema!.properties![k].format !== 'secret').map(([k, p]) => <KV key={k} k={p.title || k}>{typeof pp[k] === 'boolean' ? (pp[k] ? tr("да") : tr("нет")) : String(pp[k])}</KV>)}
                </> : <>
                  <KV k={tr("Каталог")} mono>{params.root}</KV>
                  <KV k={tr("Проверка · перезагрузка")}>{params.verify ? tr("проверить") : tr("без проверки")} · {params.reload ? tr("перезагрузить") : tr("без перезагрузки")}</KV>
                </>}
              </div>
              <div>
                <div className="text-sm font-semibold text-slate-800 mb-2">{tr("Предварительные проверки")}</div>
                <ul className="space-y-1.5 text-sm">
                  <Check ok={!!snap} text={snap ? trf("Копия найдена ({0})", fmtTime(snap.time)) : tr("Копия не выбрана")} />
                  {mode === 'pitr' && <Check ok={pitrRangeOk} warn text={pitrRangeOk ? tr("Момент в непрерывном архиве WAL") : tr("Момент вне непрерывного архива WAL")} />}
                  <Check ok={!!target} text={target ? trf("Цель «{0}» выбрана", target.name) : tr("Цель не выбрана")} />
                  {target?.kind === 'agent' && <Check ok={!!targetAgent?.online} text={targetAgent?.online ? tr("Агент цели в сети") : tr("Агент цели не в сети — задание дождётся его")} warn />}
                  {target?.kind === 'ssh' && !target.has_key && <Check ok={false} warn text={tr("У цели не задан SSH-ключ")} />}
                  {isVm && isPg && <Check ok={params.install || !!params.bin_dir} warn text={params.install ? tr("PostgreSQL будет установлен на новую ВМ") : tr("На новой ВМ должен быть PostgreSQL той же версии (в шаблоне)")} />}
                  {isPg && !isVm && params.mode === 'cluster' && inst.config?.port === params.port && <Check ok={false} warn text={tr("Порт совпадает с портом продуктива")} />}
                </ul>
              </div>
            </div>
            {!jobId && (isVm
              ? <Alert tone="info">{tr("После запуска сервер создаст новую ВМ (")}{target!.name}{tr("), дождётся её загрузки и восстановит данные. Продуктивные машины не затрагиваются.")}</Alert>
              : <Alert tone="warn">{tr("После запуска агент цели начнёт запись в указанные каталоги. Убедитесь, что это не продуктивный сервер, или что каталог не используется.")}</Alert>)}
            {jobId && (!job ? <Spinner label={tr("Ожидание задания…")} /> : <div className="pt-2 border-t border-slate-100"><div className="text-sm font-semibold text-slate-800 mb-2 mt-2">{tr("Ход восстановления")}</div><JobView job={job} /></div>)}
          </div>
        )}

        {step < 4 && valid[step] && <div className="mt-4 text-xs text-slate-500">{valid[step]}</div>}
        <div className="flex justify-between gap-2 mt-6 pt-4 border-t border-slate-100">
          <Button icon={<ArrowLeft size={16} />} disabled={step === 1 || !!jobId} onClick={() => setStep(step - 1)}>{tr("Назад")}</Button>
          {step < 4 ? <Button kind="primary" onClick={next}>{tr("Далее")} <ArrowRight size={16} /></Button>
            : jobId ? <Link to="/jobs" className="text-sm text-blue-600 hover:underline self-center">{tr("Все задания")}</Link>
            : <Button kind="primary" icon={<Play size={16} />} loading={busy} disabled={!!valid[1] || !!valid[2] || !!valid[3]} onClick={start}>{tr("Запустить")}</Button>}
        </div>
      </Card>
      </>}
    </div>
  )
}

function Check({ ok, text, warn }: { ok: boolean; text: string; warn?: boolean }) {
  return (
    <li className="flex items-start gap-2">
      {ok ? <CheckCircle2 size={16} className="text-emerald-600 mt-0.5 shrink-0" /> : warn ? <AlertTriangle size={16} className="text-amber-500 mt-0.5 shrink-0" /> : <XCircle size={16} className="text-red-600 mt-0.5 shrink-0" />}
      <span className={ok ? 'text-slate-700' : warn ? 'text-amber-800' : 'text-red-700'}>{text}</span>
    </li>
  )
}
