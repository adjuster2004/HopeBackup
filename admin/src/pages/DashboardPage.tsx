import { useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { Activity, AlertTriangle, ArchiveRestore, CalendarClock, CheckCircle2, Clock, EyeOff, Gauge, Layers, Network, Plus, Server, ShieldCheck, Target, Cable, X } from 'lucide-react'
import { fmtAgo, fmtBytes, fmtDuration, fmtTime, post } from '../api'
import type { Instance, Status } from '../types'
import { Alert, Button, Card, Empty, PageHeader, Spinner, Stat, Table, Td, useApi, useToast } from '../components/ui'
import { InstanceBadge, kindLabel, RpoValue, Tile, useCatalog } from '../components/common'
import { JobLogModal } from '../components/JobLog'
import { Suggestions } from '../components/Suggestions'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

export function DashboardPage() {
  const nav = useNavigate()
  const perms = usePerms()
  const { data: s, error, reload } = useApi<Status>('/api/status', [], 5000)
  const toast = useToast()
  const [hiding, setHiding] = useState(false)
  // Скрыть ошибки с «Обзора»: задания и журналы остаются в разделе «Задания».
  const hide = async (ids: string[]) => {
    setHiding(true)
    try { const r = await post<{ hidden: number }>('/api/jobs/ack', { ids }); toast.ok(r.hidden > 1 ? trf("Скрыто ошибок: {0}", r.hidden) : tr("Ошибка скрыта")); reload() }
    catch (e) { toast.err(e) } finally { setHiding(false) }
  }
  const { data: list } = useApi<Instance[]>('/api/instances', [], 5000)
  const { data: sites } = useApi<{ id: string; name: string }[]>('/api/sites', [], 30000)
  const { product } = useCatalog()
  const [job, setJob] = useState<string | null>(null)
  if (error && !s) return <Alert tone="error">{error}</Alert>
  if (!s) return <Spinner />
  const siteName = (id: string) => sites?.find(x => x.id === id)?.name || id || '—'
  const jobsDone = s.jobs24.ok + s.jobs24.failed
  const ratio = s.transfer24.uploaded > 0 ? s.transfer24.read / s.transfer24.uploaded : 0
  const w = s.worst_rpo
  const wBad = !!w && w.target_seconds > 0 && w.seconds > w.target_seconds
  return (
    <div className="space-y-6">
      <PageHeader title={trf("Сервер управления {0}", s.hostname)} icon={<Server size={22} />}
        subtitle={<>HopeBackup {s.version}  {tr("· работает")} {fmtDuration(s.uptime)}  {tr("· агентов в сети")} {s.counts.agents_online}  {tr("из")} {s.counts.agents}</>}
        actions={<>
          {perms.edit && <Button icon={<Plus size={16} />} onClick={() => nav('/products/new')}>{tr("Добавить продукт")}</Button>}
          {perms.restore && <Button kind="primary" icon={<ArchiveRestore size={16} />} onClick={() => nav('/restore')}>{tr("Восстановить")}</Button>}
        </>} />

      {s.recent_failures.length > 0 && (
        <div className="flex items-start gap-3 rounded-xl border border-red-300 bg-red-50 px-4 py-3 text-sm text-red-800 shadow-sm">
          <AlertTriangle size={20} className="shrink-0 mt-0.5 text-red-600" />
          <div className="space-y-1 min-w-0 flex-1">
            {s.recent_failures.slice(0, 3).map(f => (
              <div key={f.job_id} className="flex items-start gap-2">
                <div className="flex-1 min-w-0">
                  {/* Подсказка «Что это значит…» и длинный текст — в журнале; здесь первая строка. */}
                  <div className="line-clamp-2 break-words" title={f.error}><b>{tr("Задание завершилось с ошибкой:")}</b> {f.title} — {(f.error || '').split('\n')[0]}</div>
                  <span className="text-red-600/70">{(f.count || 1) > 1 ? trf("×{0}, последняя ", f.count) : ''}{fmtAgo(f.finished)}</span>{' '}
                  <button className="underline font-medium" onClick={() => setJob(f.job_id)}>{tr("Открыть журнал")}</button>
                </div>
                {perms.edit && <button title={tr("Скрыть эту ошибку с обзора (задание и журнал останутся в «Заданиях»)")} disabled={hiding}
                  className="shrink-0 p-1 -m-1 rounded-md text-red-500 hover:text-red-800 hover:bg-red-100" onClick={() => hide(f.ids?.length ? f.ids : [f.job_id])}><X size={16} /></button>}
              </div>
            ))}
            {(() => { const total = s.recent_failures_total ?? s.recent_failures.length; return (
              <div className="flex flex-wrap items-center gap-x-4 gap-y-1 pt-1">
                {total > 3 && <Link to="/jobs" className="underline font-medium">{tr("Ещё")} {total - 3}  {tr("— все задания")}</Link>}
                {perms.edit && total > 1 && <button disabled={hiding} className="inline-flex items-center gap-1 underline font-medium" onClick={() => hide(s.recent_failures.flatMap(f => f.ids?.length ? f.ids : [f.job_id]))}>
                  <EyeOff size={14} />{tr("Скрыть все")}{total > s.recent_failures.length ? ` (${s.recent_failures.length})` : ''}</button>}
              </div>) })()}
            <div className="text-xs text-red-700/70">{tr("Ошибка пропадает сама, когда такое же задание этого экземпляра потом проходит успешно.")}</div>
          </div>
        </div>
      )}

      <Suggestions compact />

      <div className="grid grid-cols-1 sm:grid-cols-2 xl:grid-cols-4 gap-4">
        <Stat label={tr("Защищено")} value={s.counts.instances} sub={trf("экземпляров · площадок {0}", s.counts.sites)} icon={<ShieldCheck size={22} />} />
        <Stat label={tr("За 24 часа")} value={jobsDone ? trf("{0} из {1}", s.jobs24.ok, jobsDone) : '—'} tone={s.jobs24.failed ? 'red' : 'green'} icon={<CheckCircle2 size={22} />}
          sub={jobsDone || s.jobs24.running ? trf("успешно{0}{1}", s.jobs24.failed ? trf(", {0} с ошибкой", s.jobs24.failed) : '', s.jobs24.running ? trf(", выполняется {0}", s.jobs24.running) : '') : tr("заданий не было")} />
        <Stat label={tr("Худший RPO")} value={w ? fmtDuration(w.seconds) : '—'} tone={wBad ? 'red' : 'amber'} icon={<Gauge size={22} />}
          sub={w ? `${w.name}${w.target_seconds ? trf(", цель {0}", fmtDuration(w.target_seconds)) : ''}` : tr("нет данных")} />
        <Stat label={tr("Сжатие + дедупликация")} value={ratio ? `×${ratio.toFixed(1).replace('.', ',')}` : '—'} tone="violet" icon={<Layers size={22} />}
          sub={trf("за сутки передано {0} из {1}", fmtBytes(s.transfer24.uploaded), fmtBytes(s.transfer24.read))} />
      </div>

      <div className="grid grid-cols-1 xl:grid-cols-3 gap-6">
        <Card title={tr("Продукты")} icon={<ShieldCheck size={18} className="text-blue-600" />} className="xl:col-span-2" padded={false}
          actions={<>{perms.edit && <Link to="/products/new" className="text-sm text-blue-600 hover:underline">{tr("Добавить продукт")}</Link>}<Link to="/products" className="text-sm text-blue-600 hover:underline ml-3">{tr("Все")}</Link></>}>
          {list && list.length === 0 ? (
            <Empty icon={<ShieldCheck size={28} />} title={tr("Продуктов пока нет")}
              text={tr("Добавьте первый экземпляр из каталога: PostgreSQL, файлы и другие. Для этого нужны подключённый агент и площадка хранения.")}
              action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => nav('/products/new')}>{tr("Добавить продукт")}</Button>} />
          ) : !list ? <Spinner /> : (
            <Table head={[tr("Экземпляр"), tr("Последняя копия"), 'RPO', tr("Площадка"), tr("Состояние")]}>
              {list.map(i => {
                const p = product(i.catalog_id)
                const lb = i.runtime?.last_backup
                return (
                  <tr key={i.id} className="hover:bg-slate-50 cursor-pointer" onClick={() => nav(`/instances/${encodeURIComponent(i.id)}`)}>
                    <Td>
                      <div className="flex items-center gap-3">
                        <Tile product={p} fallback={i.plugin} />
                        <div className="min-w-0"><div className="font-medium text-slate-900 truncate">{i.name}</div><div className="text-xs text-slate-500 truncate">{p?.title || i.plugin}</div></div>
                      </div>
                    </Td>
                    <Td>{lb ? <><div>{fmtTime(lb.time)}</div><div className="text-xs text-slate-500">{kindLabel(lb.kind)} · {fmtBytes(lb.size)}</div></> : <span className="text-slate-400">{tr("ещё не было")}</span>}</Td>
                    <Td><RpoValue inst={i} /></Td>
                    <Td className="text-slate-600">{siteName(i.site_id)}</Td>
                    <Td><InstanceBadge status={i.runtime?.status} /></Td>
                  </tr>
                )
              })}
            </Table>
          )}
        </Card>

        <div className="space-y-6">
          <Card title={tr("Ближайшие задания")} icon={<CalendarClock size={18} className="text-blue-600" />}>
            {s.next_runs.length === 0 ? <div className="text-sm text-slate-500">{tr("Запланированных заданий нет — включите расписание у продукта.")}</div> : (
              <ul className="space-y-2 text-sm">
                {s.next_runs.slice(0, 8).map((r, i) => (
                  <li key={i} className="flex items-start gap-3">
                    <span className="font-mono text-xs text-slate-500 w-28 shrink-0 pt-0.5" title={fmtTime(r.at)}>{fmtTime(r.at).replace(/\.\d{4},?/, '')}</span>
                    <Link to={`/instances/${encodeURIComponent(r.instance_id)}`} className="text-slate-800 hover:text-blue-600">{r.name} <span className="text-slate-500">— {kindLabel(r.kind)}</span></Link>
                  </li>
                ))}
              </ul>
            )}
          </Card>
          <Card title={tr("Инфраструктура")} icon={<Network size={18} className="text-blue-600" />}>
            <div className="grid grid-cols-2 gap-3 text-sm">
              <InfraLink to="/sites" icon={<Server size={16} />} label={tr("Площадки")} value={s.counts.sites} />
              <InfraLink to="/connectors" icon={<Cable size={16} />} label={tr("Коннекторы")} value={s.counts.connectors} />
              <InfraLink to="/agents" icon={<Network size={16} />} label={tr("Агенты")} value={`${s.counts.agents_online}/${s.counts.agents}`} />
              <InfraLink to="/targets" icon={<Target size={16} />} label={tr("Цели")} value={s.counts.targets} />
            </div>
            {s.counts.sites === 0 && <div className="mt-3"><Alert tone="warn">{tr("Площадок пока нет — добавьте узел хранения в разделе")} <Link to="/sites" className="underline">{tr("«Площадки»")}</Link>.</Alert></div>}
            {s.counts.agents === 0 && <div className="mt-3"><Alert tone="info">{tr("Агенты не подключены —")} <Link to="/agents" className="underline">{tr("подключите агент")}</Link>  {tr("на сервере с продуктом.")}</Alert></div>}
          </Card>
          <Card title={tr("Передача за сутки")} icon={<Clock size={18} className="text-blue-600" />}>
            <div className="space-y-1.5 text-sm">
              <Row k={tr("Прочитано на агентах")} v={fmtBytes(s.transfer24.read)} />
              <Row k={tr("Новые данные")} v={fmtBytes(s.transfer24.new)} />
              <Row k={tr("Отправлено на площадки")} v={fmtBytes(s.transfer24.uploaded)} />
            </div>
          </Card>
        </div>
      </div>
      <div className="text-xs text-slate-400 flex items-center gap-1"><Activity size={12} />{tr("Данные обновляются каждые 5 секунд")}</div>
      <JobLogModal id={job} onClose={() => setJob(null)} />
    </div>
  )
}

const Row = ({ k, v }: { k: string; v: string }) => <div className="flex justify-between"><span className="text-slate-500">{k}</span><span className="font-medium text-slate-800">{v}</span></div>

function InfraLink({ to, icon, label, value }: { to: string; icon: React.ReactNode; label: string; value: React.ReactNode }) {
  return (
    <Link to={to} className="flex items-center gap-2.5 p-2.5 rounded-lg border border-slate-200 hover:border-blue-300 hover:bg-blue-50/40">
      <span className="text-blue-600">{icon}</span>
      <span className="flex-1 text-slate-600">{label}</span>
      <b className="text-slate-900">{value}</b>
    </Link>
  )
}
