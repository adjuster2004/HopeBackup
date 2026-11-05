// Таблица заданий (журнал экземпляра и раздел «Задания»).
import { fmtDuration, fmtTime, secondsBetween } from '../api'
import type { Job } from '../types'
import { JobBadge, kindLabel } from './common'
import { Badge, Table, Td } from './ui'
import { LOCALE, tr } from '../i18n'

export function JobsTable({ jobs, onOpen, instName, agentName }: { jobs: Job[]; onOpen: (id: string) => void; instName?: (id: string) => string; agentName?: (id: string) => string }) {
  return (
    <Table head={[tr("Создано"), tr("Задание"), tr("Вид"), ...(instName ? [tr("Экземпляр")] : []), ...(agentName ? [tr("Агент")] : []), tr("Состояние"), tr("Длительность"), tr("Ошибка")]}>
      {jobs.map(j => (
        <tr key={j.id} className="hover:bg-slate-50 cursor-pointer align-top" onClick={() => onOpen(j.id)}>
          <Td className="whitespace-nowrap text-xs">{fmtTime(j.created, true)}</Td>
          <Td className="font-medium text-slate-900">{j.title || kindLabel(j.kind)}<JobMarks j={j} /></Td>
          <Td><Badge>{kindLabel(j.kind)}</Badge></Td>
          {instName && <Td className="text-slate-600">{j.instance_id ? instName(j.instance_id) : '—'}</Td>}
          {agentName && <Td className="text-slate-600">{j.agent_id ? agentName(j.agent_id) : '—'}</Td>}
          <Td><JobBadge status={j.status} />{j.status === 'running' && j.await_until ? <div className="text-[11px] text-amber-700 mt-0.5">{tr("ждём итог от агента")}</div> : null}
            {j.ack ? <div className="text-[11px] text-slate-400 mt-0.5" title={`${j.ack.user}, ${fmtTime(j.ack.at, true)}`}>{tr("скрыта с обзора")}</div> : null}</Td>
          <Td className="text-slate-600 whitespace-nowrap">{j.started ? fmtDuration(secondsBetween(j.started, j.finished)) : '—'}</Td>
          <Td className="text-xs text-red-700 max-w-md break-words">{(j.error || '').split('\n\nЧто это значит')[0]}</Td>
        </tr>
      ))}
    </Table>
  )
}

/** Происхождение и повторы задания: «по расписанию», «повтор 2», «догон», «итог не дошёл», «повтор в 02:00». */
export function JobMarks({ j }: { j: Job }) {
  const m: React.ReactNode[] = []
  if (j.origin === 'schedule') m.push(<Badge key="o" tone="slate">{tr("по расписанию")}</Badge>)
  if (j.origin === 'manual_retry') m.push(<Badge key="o" tone="slate">{tr("вручную, с повторами")}</Badge>)
  if (j.origin === 'catchup') m.push(<Badge key="o" tone="violet">{tr("догон пропуска")}</Badge>)
  if (j.origin === 'retry') m.push(<Badge key="o" tone="amber">{tr("повтор, попытка")} {j.attempt}</Badge>)
  if (j.interrupted) m.push(<Badge key="i" tone="amber">{tr("итог не дошёл")}</Badge>)
  if (j.status === 'failed' && j.retry_at && !j.retry_job) m.push(<Badge key="r" tone="blue">{tr("повтор в")} {new Date(j.retry_at).toLocaleTimeString(LOCALE, { hour: '2-digit', minute: '2-digit' })}</Badge>)
  if (j.retry_job && j.retry_job !== 'skipped') m.push(<Badge key="r" tone="blue">{tr("повторено")}</Badge>)
  if (j.status === 'ok' && j.result?.reconciled) m.push(<Badge key="c" tone="green">{tr("снапшот найден на площадке")}</Badge>)
  return m.length ? <div className="flex flex-wrap gap-1 mt-1 font-normal">{m}</div> : null
}
