import { useMemo, useState } from 'react'
import { ListChecks, RefreshCw } from 'lucide-react'
import { q } from '../api'
import type { Agent, Instance, Job } from '../types'
import { Alert, Button, Card, Empty, Input, PageHeader, Select, Spinner, useApi, RefreshButton } from '../components/ui'
import { JOB_STATUS, kindLabel } from '../components/common'
import { JobLogModal } from '../components/JobLog'
import { JobsTable } from '../components/JobsTable'
import { tr } from '../i18n'

export function JobsPage() {
  const [instance, setInstance] = useState('')
  const [status, setStatus] = useState('')
  const [kind, setKind] = useState('')
  const [text, setText] = useState('')
  const [limit, setLimit] = useState(200)
  const [open, setOpen] = useState<string | null>(null)
  const { data, error, reload, loading } = useApi<Job[]>(`/api/jobs${q({ instance_id: instance, limit })}`, [], 5000)
  const { data: insts } = useApi<Instance[]>('/api/instances', [], 30000)
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 30000)
  const instName = (id: string) => insts?.find(i => i.id === id)?.name || id
  const agentName = (id: string) => agents?.find(a => a.id === id)?.name || id
  const kinds = useMemo(() => [...new Set((data || []).map(j => j.kind))].sort(), [data])
  const rows = (data || []).filter(j => (!status || j.status === status) && (!kind || j.kind === kind) &&
    (!text || `${j.title} ${j.error || ''} ${j.id}`.toLowerCase().includes(text.toLowerCase())))
  return (
    <div>
      <PageHeader title={tr("Задания")} icon={<ListChecks size={22} />}
        subtitle={tr("Копии, обнаружения, восстановления и служебные задания агентов. Нажмите на строку, чтобы открыть журнал.")}
        actions={<RefreshButton onClick={reload} />} />
      <Card className="mb-4">
        <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
          <Select value={instance} onChange={e => setInstance(e.target.value)}>
            <option value="">{tr("Все экземпляры")}</option>
            {(insts || []).map(i => <option key={i.id} value={i.id}>{i.name}</option>)}
          </Select>
          <Select value={status} onChange={e => setStatus(e.target.value)}>
            <option value="">{tr("Любое состояние")}</option>
            {Object.entries(JOB_STATUS).map(([k, v]) => <option key={k} value={k}>{v.label}</option>)}
          </Select>
          <Select value={kind} onChange={e => setKind(e.target.value)}>
            <option value="">{tr("Все виды")}</option>
            {kinds.map(k => <option key={k} value={k}>{kindLabel(k)}</option>)}
          </Select>
          <Select value={limit} onChange={e => setLimit(+e.target.value)}>
            {[50, 200, 500, 2000].map(n => <option key={n} value={n}>{tr("последние")} {n}</option>)}
          </Select>
          <Input className="md:col-span-2" placeholder={tr("Название, ошибка, идентификатор…")} value={text} onChange={e => setText(e.target.value)} />
        </div>
      </Card>
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : (
        <Card padded={false}>
          {rows.length === 0 ? <Empty icon={<ListChecks size={28} />} title={data.length ? tr("Под фильтр ничего не попало") : tr("Заданий пока нет")}
            text={data.length ? tr("Измените условия фильтра.") : tr("Задания появятся, когда сработает расписание или вы запустите копию вручную.")} />
            : <JobsTable jobs={rows} onOpen={setOpen} instName={instName} agentName={agentName} />}
        </Card>
      )}
      <JobLogModal id={open} onClose={() => setOpen(null)} />
    </div>
  )
}
