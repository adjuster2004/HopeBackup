import { useState } from 'react'
import { Copy, Network, Plus, RefreshCw, Trash2 } from 'lucide-react'
import { copyText, del, fmtAgo, fmtTime, post } from '../api'
import type { Agent, EnrollToken, Instance } from '../types'
import { Alert, Badge, Button, Card, Code, Confirm, Empty, Field, IconButton, Input, KV, Modal, PageHeader, Spinner, StatusDot, Table, Td, useApi, useToast, RefreshButton } from '../components/ui'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

export function AgentsPage() {
  const toast = useToast()
  const perms = usePerms()
  const { data, error, reload } = useApi<Agent[]>('/api/agents', [], 10000)
  const { data: insts } = useApi<Instance[]>('/api/instances', [], 30000)
  const [enroll, setEnroll] = useState(false)
  const [remove, setRemove] = useState<Agent | null>(null)
  const instName = (id: string) => insts?.find(i => i.id === id)?.name || id
  const doRemove = async () => {
    const a = remove!; setRemove(null)
    try { await del(`/api/agents/${encodeURIComponent(a.id)}`); toast.ok(trf("Агент «{0}» отключён", a.name)); reload() } catch (e) { toast.err(e) }
  }
  return (
    <div>
      <PageHeader title={tr("Агенты")} icon={<Network size={22} />}
        subtitle={tr("Агенты работают на серверах с продуктами: делают копии, отправляют архив WAL и выполняют восстановление. Связь — исходящая от агента к серверу управления.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEnroll(true)}>{tr("Подключить агент")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<Network size={28} />} title={tr("Агенты ещё не подключены")}
          text={tr("Нажмите «Подключить агент» и выполните полученную команду на сервере, где работает продукт.")}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEnroll(true)}>{tr("Подключить агент")}</Button>} /></Card>
      ) : (
        <Card padded={false}>
          <Table head={[tr("Агент"), tr("Хост"), tr("ОС"), tr("Версия"), tr("Последняя связь"), tr("Очереди WAL"), tr("Подключён"), '']}>
            {data.map(a => (
              <tr key={a.id} className="hover:bg-slate-50 align-top">
                <Td><span className="flex items-center gap-2 font-medium text-slate-900"><StatusDot tone={a.online ? 'green' : 'red'} />{a.name}</span><span className="text-xs text-slate-400 font-mono">{a.id}</span></Td>
                <Td className="font-mono text-xs">{a.host}</Td>
                <Td className="text-slate-600">{a.os || '—'}</Td>
                <Td><Badge>{a.version || '—'}</Badge></Td>
                <Td><span title={fmtTime(a.last_seen, true)} className={a.online ? 'text-slate-700' : 'text-red-700'}>{a.online ? tr("в сети") : tr("не в сети")} · {fmtAgo(a.last_seen)}</span></Td>
                <Td>
                  {!a.wal?.length ? <span className="text-slate-400">—</span> : (
                    <div className="space-y-1 text-xs">
                      {a.wal.map(w => (
                        <div key={w.instance_id}>
                          <span className="font-medium text-slate-700">{instName(w.instance_id)}</span>: <span className={w.queued > 20 ? 'text-amber-700 font-semibold' : ''}>{tr("в очереди")} {w.queued}</span>{tr(", отправлено")} {w.shipped}
                          {w.last_shipped && <div className="font-mono text-slate-400">{w.last_shipped} · {fmtAgo(w.last_time)}</div>}
                          {w.last_error && <div className="text-red-700 break-words max-w-xs">{w.last_error}</div>}
                        </div>
                      ))}
                    </div>
                  )}
                </Td>
                <Td className="text-xs text-slate-500 whitespace-nowrap">{fmtTime(a.enrolled)}</Td>
                <Td className="text-right">{perms.edit && <IconButton title={tr("Отключить агент")} danger onClick={() => setRemove(a)}><Trash2 size={16} /></IconButton>}</Td>
              </tr>
            ))}
          </Table>
        </Card>
      )}
      {enroll && <EnrollModal onClose={() => { setEnroll(false); reload() }} />}
      <Confirm open={!!remove} danger yes={tr("Отключить")} title={tr("Отключить агент?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Агент «")}<b>{remove?.name}</b>{tr("» потеряет доступ к серверу управления. Экземпляры на нём перестанут делать копии, пока агент не будет подключён заново.")}</>} />
    </div>
  )
}

function EnrollModal({ onClose }: { onClose: () => void }) {
  const toast = useToast()
  const [note, setNote] = useState('')
  const [busy, setBusy] = useState(false)
  const [res, setRes] = useState<EnrollToken | null>(null)
  const issue = async () => {
    setBusy(true)
    try { setRes(await post<EnrollToken>('/api/agents/enroll-token', { note: note.trim() })) } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const copy = async () => { if (res && await copyText(res.command)) toast.ok(tr("Команда скопирована")) }
  return (
    <Modal open wide onClose={onClose} title={tr("Подключить агент")}
      footer={res ? <Button kind="primary" onClick={onClose}>{tr("Готово")}</Button> : <><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={issue}>{tr("Получить команду")}</Button></>}>
      {!res ? (
        <div className="space-y-4">
          <div className="text-sm text-slate-600">{tr("Сервер выдаст одноразовый токен подключения. Команду нужно выполнить от root на сервере с продуктом — агент установится и подключится сам.")}</div>
          <Field label={tr("Примечание (необязательно)")} hint={tr("Для журнала аудита: для какого сервера выдан токен.")}><Input value={note} onChange={e => setNote(e.target.value)} placeholder="db-prod-02" autoFocus /></Field>
        </div>
      ) : (
        <div className="space-y-4">
          <div className="text-sm text-slate-700">{tr("Выполните на сервере с продуктом:")}</div>
          <div className="relative">
            <Code className="pr-12 text-[12.5px] leading-relaxed">{res.command}</Code>
            <button type="button" onClick={copy} title={tr("Скопировать")} className="absolute top-2 right-2 p-1.5 rounded-md bg-slate-700 text-slate-200 hover:bg-slate-600"><Copy size={14} /></button>
          </div>
          <Button icon={<Copy size={16} />} onClick={copy}>{tr("Скопировать команду")}</Button>
          <div>
            <KV k={tr("Действует до")}>{fmtTime(res.expires)} ({fmtAgo(res.expires)})</KV>
            <KV k={tr("Отпечаток сервера")} mono>{res.server_fingerprint}</KV>
          </div>
          <Alert tone="info">{tr("Агент проверит сертификат сервера по отпечатку. Токен одноразовый; после подключения агент появится в списке в течение нескольких секунд.")}</Alert>
        </div>
      )}
    </Modal>
  )
}
