// Журнал задания: опрос /api/jobs/:id каждые 2 с, пока задание в очереди или выполняется.
import { useEffect, useRef, useState } from 'react'
import { api, fmtClock, fmtDuration, fmtTime, secondsBetween } from '../api'
import type { Job } from '../types'
import { JobBadge, jobActive, kindLabel, pretty } from './common'
import { Alert, Button, Code, Modal, Spinner } from './ui'
import { tr } from '../i18n'

/** Задание с живым обновлением. onDone вызывается один раз, когда задание завершилось. */
export function useJob(id: string | null, onDone?: (j: Job) => void) {
  const [job, setJob] = useState<Job | null>(null)
  const [error, setError] = useState<string | null>(null)
  const done = useRef(onDone)
  done.current = onDone
  useEffect(() => {
    setJob(null); setError(null)
    if (!id) return
    let alive = true
    let timer: number | undefined
    let fired = false
    const tick = async () => {
      try {
        const j = await api<Job>(`/api/jobs/${encodeURIComponent(id)}`)
        if (!alive) return
        setJob(j); setError(null)
        if (jobActive(j.status)) timer = window.setTimeout(tick, 2000)
        else if (!fired) { fired = true; done.current?.(j) }
      } catch (e: any) {
        if (!alive) return
        setError(e.message)
        timer = window.setTimeout(tick, 4000)
      }
    }
    tick()
    return () => { alive = false; if (timer) clearTimeout(timer) }
  }, [id])
  return { job, error }
}

/** Строки журнала с автопрокруткой вниз. */
export function JobLogLines({ job, height = 'max-h-96' }: { job: Job; height?: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const stick = useRef(true)
  const n = job.log?.length || 0
  useEffect(() => { const el = ref.current; if (el && stick.current) el.scrollTop = el.scrollHeight }, [n])
  return (
    <div ref={ref} onScroll={e => { const el = e.currentTarget; stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40 }}
      className={`bg-slate-900 text-slate-100 rounded-lg p-3 font-mono text-xs overflow-y-auto ${height}`}>
      {n === 0 ? <div className="text-slate-500">{jobActive(job.status) ? tr("Ожидание вывода агента…") : tr("Журнал пуст")}</div> :
        job.log!.map((l, i) => (
          <div key={i} className="flex gap-3 leading-5">
            <span className="text-slate-500 shrink-0">{fmtClock(l.t)}</span>
            <span className="whitespace-pre-wrap break-all">{l.m}</span>
          </div>
        ))}
      {jobActive(job.status) && <div className="text-blue-400 animate-pulse mt-1">▍</div>}
    </div>
  )
}

/** Сводка и журнал задания (без окна) — для мастера восстановления и страниц. */
export function JobView({ job, showResult = true }: { job: Job; showResult?: boolean }) {
  const dur = job.started ? secondsBetween(job.started, job.finished) : null
  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-x-5 gap-y-1 text-sm">
        <JobBadge status={job.status} />
        <span className="text-slate-500">{tr("вид:")} <b className="text-slate-700 font-medium">{kindLabel(job.kind)}</b></span>
        <span className="text-slate-500">{tr("создано:")} {fmtTime(job.created, true)}</span>
        {job.started && <span className="text-slate-500">{tr("длительность:")} {fmtDuration(dur)}</span>}
        <span className="text-slate-400 font-mono text-xs">{job.id}</span>
      </div>
      {job.error && <Alert tone="error" title={tr("Задание завершилось с ошибкой")}><span className="whitespace-pre-line">{job.error}</span></Alert>}
      <JobLogLines job={job} />
      {showResult && job.result !== null && job.result !== undefined && !jobActive(job.status) && (
        <div>
          <div className="text-xs font-semibold uppercase tracking-wide text-slate-500 mb-1">{tr("Результат")}</div>
          <Code className="max-h-64 overflow-y-auto">{pretty(job.result)}</Code>
        </div>
      )}
    </div>
  )
}

export function JobLogModal({ id, onClose, onDone }: { id: string | null; onClose: () => void; onDone?: (j: Job) => void }) {
  const { job, error } = useJob(id, onDone)
  return (
    <Modal open={!!id} onClose={onClose} xwide title={job ? job.title || kindLabel(job.kind) : tr("Задание")} footer={<Button onClick={onClose}>{tr("Закрыть")}</Button>}>
      {error && !job ? <Alert tone="error">{error}</Alert> : !job ? <Spinner /> : <JobView job={job} />}
    </Modal>
  )
}
