import { useState } from 'react'
import { Bot, Cloud, Pencil, Plus, PlugZap, RefreshCw, Server, Target as TargetIcon, Trash2 } from 'lucide-react'
import { del, fmtTime, post, put } from '../api'
import { VM_KINDS, type Agent, type CatalogKind, type Target } from '../types'
import { SchemaForm, schemaClean, schemaInitial } from '../components/SchemaForm'
import { Alert, Badge, Button, Card, ChoiceCard, Code, Confirm, Empty, Field, IconButton, InDev, Input, Modal, PageHeader, Select, Spinner, StatusDot, Table, Td, Textarea, useApi, useToast, RefreshButton } from '../components/ui'
import { useCatalog } from '../components/common'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

const FALLBACK_KINDS: CatalogKind[] = [{ id: 'ssh', title: tr("Сервер по SSH"), available: true }, { id: 'agent', title: tr("Машина с агентом"), available: true }]

/** Кратко: куда цель создаёт ВМ. */
export function vmWhere(t: Target): string {
  const p = t.params || {}
  switch (t.kind) {
    case 'proxmox': return trf("{0} · узел {1} · шаблон {2}", p.url || '?', p.node || 'pve', p.template || '?')
    case 'vmware': return `${p.url || '?'} · ${p.source_type === 'library' ? tr("шаблон") : tr("эталон")} «${p.source || '?'}»`
    case 'yandex_cloud': return trf("каталог {0} · {1}", p.folder_id || '?', p.zone || 'ru-central1-a')
    case 'aws': return `${p.region || '?'} · ${p.subnet_id || '?'} · ${p.instance_type || 't3.large'}`
    default: return ''
  }
}

export function TargetsPage() {
  const toast = useToast()
  const perms = usePerms()
  const { data, error, reload } = useApi<Target[]>('/api/targets', [], 15000)
  const { data: agents } = useApi<Agent[]>('/api/agents', [], 15000)
  const { catalog } = useCatalog()
  const kinds = catalog?.target_kinds?.length ? catalog.target_kinds : FALLBACK_KINDS
  const kindTitle = (k: string) => kinds.find(x => x.id === k)?.title || k
  const [add, setAdd] = useState<Target | 'new' | null>(null)
  const [busy, setBusy] = useState('')
  const [out, setOut] = useState<{ t: Target; ok: boolean; output: string } | null>(null)
  const [remove, setRemove] = useState<Target | null>(null)
  const test = async (t: Target) => {
    setBusy(t.id)
    try { const r = await post<{ ok: boolean; output: string }>(`/api/targets/${encodeURIComponent(t.id)}/test`); setOut({ t, ...r }) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doRemove = async () => {
    const t = remove!; setRemove(null)
    try { await del(`/api/targets/${encodeURIComponent(t.id)}`); toast.ok(trf("Цель «{0}» удалена", t.name)); reload() } catch (e) { toast.err(e) }
  }
  return (
    <div>
      <PageHeader title={tr("Цели восстановления")} icon={<TargetIcon size={22} />}
        subtitle={tr("Куда мастер восстановления может поднять копию: готовый сервер по SSH, машина с агентом или новая ВМ в Proxmox VE, VMware vSphere, Yandex Cloud, AWS.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setAdd('new')}>{tr("Добавить цель")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<TargetIcon size={28} />} title={tr("Целей восстановления пока нет")}
          text={tr("Добавьте сервер, на который можно восстановить данные: по SSH-ключу или через агент, уже подключённый к серверу управления.")}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setAdd('new')}>{tr("Добавить цель")}</Button>} /></Card>
      ) : (
        <Card padded={false}>
          <Table head={[tr("Цель"), tr("Тип"), tr("Подключение"), tr("Создана"), '']}>
            {data.map(t => {
              const a = t.kind === 'agent' ? agents?.find(x => x.id === t.agent_id) : null
              return (
                <tr key={t.id} className="hover:bg-slate-50">
                  <Td className="font-medium text-slate-900"><span className="flex items-center gap-2">{t.kind === 'agent' ? <Bot size={16} className="text-blue-600" /> : VM_KINDS.includes(t.kind) ? <Cloud size={16} className="text-violet-600" /> : <Server size={16} className="text-blue-600" />}{t.name}</span></Td>
                  <Td><Badge tone={VM_KINDS.includes(t.kind) ? 'violet' : 'blue'}>{kindTitle(t.kind)}</Badge></Td>
                  <Td>{t.kind === 'agent'
                    ? <span className="flex items-center gap-2"><StatusDot tone={a?.online ? 'green' : 'red'} />{a?.name || t.agent_id}<span className="text-xs text-slate-400">{a ? (a.online ? tr("в сети") : tr("не в сети")) : tr("агент не найден")}</span></span>
                    : VM_KINDS.includes(t.kind) ? <span className="text-xs"><span className="text-slate-500">{tr("новая ВМ:")} </span><span className="font-mono">{vmWhere(t)}</span></span>
                    : <span className="font-mono text-xs">{t.user}@{t.host}:{t.port}{!t.has_key && !t.has_password && <span className="text-amber-700 font-sans ml-2">{tr("ключ не задан")}</span>}{t.has_password && !t.has_key && <span className="text-slate-500 font-sans ml-2">{tr("по паролю")}</span>}</span>}</Td>
                  <Td className="text-xs text-slate-500">{fmtTime(t.created)}</Td>
                  <Td className="text-right whitespace-nowrap">
                    <Button className="!py-1 !px-2.5 text-xs mr-1" icon={<PlugZap size={14} />} loading={busy === t.id} onClick={() => test(t)}>{tr("Проверить")}</Button>
                    {perms.edit && t.kind !== 'agent' && <IconButton title={tr("Изменить")} onClick={() => setAdd(t)}><Pencil size={16} /></IconButton>}
                    {perms.edit && <IconButton title={tr("Удалить")} danger onClick={() => setRemove(t)}><Trash2 size={16} /></IconButton>}
                  </Td>
                </tr>
              )
            })}
          </Table>
        </Card>
      )}
      {add && <TargetModal target={add === 'new' ? null : add} kinds={kinds} agents={agents || []} onClose={() => setAdd(null)} onSaved={() => { setAdd(null); reload() }} />}
      <Modal open={!!out} onClose={() => setOut(null)} wide title={out ? trf("Проверка «{0}»", out.t.name) : ''} footer={<Button onClick={() => setOut(null)}>{tr("Закрыть")}</Button>}>
        {out && <div className="space-y-3">
          <Alert tone={out.ok ? 'success' : 'error'}>{out.ok ? (VM_KINDS.includes(out.t.kind) ? tr("API доступен, шаблон найден") : tr("Цель доступна")) : tr("Цель недоступна")}</Alert>
          {out.output && <Code className="max-h-96 overflow-y-auto">{out.output}</Code>}
        </div>}
      </Modal>
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить цель?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Цель «")}<b>{remove?.name}</b>{tr("» исчезнет из мастера восстановления. Сам сервер не затрагивается.")}</>} />
    </div>
  )
}

function TargetModal({ target, kinds, agents, onClose, onSaved }: { target: Target | null; kinds: CatalogKind[]; agents: Agent[]; onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const [kind, setKind] = useState(target?.kind || kinds.find(k => k.available)?.id || 'ssh')
  const [f, setF] = useState({ name: target?.name || '', host: target?.host || '', port: target?.port || 22, user: target?.user || 'root', private_key: '', password: '', auth: target && target.has_password && !target.has_key ? 'password' : 'key', agent_id: target?.agent_id || '' })
  const kdef = kinds.find(k => k.id === kind)
  const isVm = VM_KINDS.includes(kind)
  const [vp, setVp] = useState<Record<string, any>>(() => schemaInitial(kdef?.schema, undefined, target?.params))
  const [busy, setBusy] = useState(false)
  const set = (p: Partial<typeof f>) => setF({ ...f, ...p })
  const pick = (k: string) => { setKind(k); setVp(schemaInitial(kinds.find(x => x.id === k)?.schema)) }
  const save = async () => {
    if (!f.name.trim()) return toast.err(tr("Укажите имя цели"))
    if (kind === 'ssh' && (!f.host.trim() || !f.user.trim())) return toast.err(tr("Укажите хост и пользователя"))
    if (kind === 'ssh' && !target && f.auth === 'key' && !f.private_key.trim()) return toast.err(tr("Вставьте закрытый ключ SSH"))
    if (kind === 'ssh' && !target && f.auth === 'password' && !f.password) return toast.err(tr("Укажите пароль"))
    if (kind === 'agent' && !f.agent_id) return toast.err(tr("Выберите агент"))
    const miss = isVm ? (kdef?.schema?.required || []).filter(k => vp[k] === undefined || vp[k] === '' || vp[k] === null) : []
    if (miss.length && !target) return toast.err(trf("Заполните: {0}", miss.map(k => kdef?.schema?.properties?.[k]?.title || k).join(', ')))
    setBusy(true)
    try {
      const body = kind === 'ssh'
        ? { name: f.name.trim(), kind, host: f.host.trim(), port: Number(f.port) || 22, user: f.user.trim(), private_key: f.auth === 'key' ? f.private_key : '', password: f.auth === 'password' ? f.password : '', agent_id: '' }
        : kind === 'agent' ? { name: f.name.trim(), kind, host: '', port: 0, user: '', private_key: '', agent_id: f.agent_id }
        : { name: f.name.trim(), kind, params: schemaClean(kdef?.schema, vp) }
      if (target) await put(`/api/targets/${encodeURIComponent(target.id)}`, body); else await post('/api/targets', body)
      toast.ok(target ? tr("Цель сохранена") : tr("Цель добавлена")); onSaved()
    } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={target ? trf("Цель «{0}»", target.name) : tr("Новая цель восстановления")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{target ? tr("Сохранить") : tr("Добавить")}</Button></>}>
      <div className="space-y-4">
        <div>
          <div className="text-sm font-medium text-slate-700 mb-1.5">{tr("Тип цели")}</div>
          <div className="grid sm:grid-cols-2 lg:grid-cols-3 gap-2" role="radiogroup">
            {kinds.map(k => <ChoiceCard key={k.id} selected={kind === k.id} disabled={!k.available || (!!target && target.kind !== k.id)} onClick={() => pick(k.id)} title={k.title} text={k.description} badge={!k.available && <InDev />} />)}
          </div>
        </div>
        <Field label={tr("Имя")}><Input value={f.name} onChange={e => set({ name: e.target.value })} placeholder={kind === 'agent' ? 'db-dr-01' : isVm ? tr("Кластер Proxmox ЦОД-2") : tr("Резервный сервер ЦОД-2")} /></Field>
        {kind === 'ssh' && <>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label={tr("Хост")} className="sm:col-span-2"><Input className="font-mono text-[13px]" value={f.host} onChange={e => set({ host: e.target.value })} placeholder="10.20.5.17" /></Field>
            <Field label={tr("Порт")}><Input type="number" value={f.port} onChange={e => set({ port: parseInt(e.target.value, 10) || 0 })} /></Field>
          </div>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Пользователь")} hint={tr("root или пользователь с sudo")}><Input value={f.user} onChange={e => set({ user: e.target.value })} /></Field>
            <Field label={tr("Вход")}>
              <div className="flex gap-2">
                {(['key', 'password'] as const).map(a => (
                  <button key={a} type="button" onClick={() => set({ auth: a })} className={`flex-1 px-3 py-2 rounded-lg border text-sm ${f.auth === a ? 'border-blue-500 bg-blue-50 text-blue-800' : 'border-slate-200 hover:bg-slate-50'}`}>{a === 'key' ? tr("по ключу SSH") : tr("по паролю")}</button>
                ))}
              </div>
            </Field>
          </div>
          {f.auth === 'password' ? (
            <Field label={tr("Пароль")} hint={target?.has_password ? tr("Пароль задан — оставьте пустым, чтобы не менять. Он же используется для sudo.") : tr("Для входа и для sudo (если пользователь не root). Где пароли запрещены — используйте ключ.")}>
              <Input type="password" autoComplete="new-password" value={f.password} onChange={e => set({ password: e.target.value })} placeholder={target?.has_password ? tr("•••••••• задан") : ''} />
            </Field>
          ) : <Field label={tr("Закрытый ключ")} hint={target?.has_key ? tr("Ключ задан — оставьте пустым, чтобы не менять.") : tr("Хранится в секретах сервера; при восстановлении сервер ставит на цель временного агента.")}>
            <Textarea rows={5} value={f.private_key} onChange={e => set({ private_key: e.target.value })} placeholder={target?.has_key ? tr("•••••••• задан") : '-----BEGIN OPENSSH PRIVATE KEY-----'} />
          </Field>}
        </>}
        {kind === 'agent' && (
          agents.length === 0 ? <Alert tone="warn">{tr("Нет подключённых агентов — сначала подключите агент на целевой машине в разделе «Агенты».")}</Alert> : (
            <Field label={tr("Агент")}>
              <Select value={f.agent_id} onChange={e => { const a = agents.find(x => x.id === e.target.value); set({ agent_id: e.target.value, name: f.name || a?.name || '' }) }}>
                <option value="">{tr("— выберите агент —")}</option>
                {agents.map(a => <option key={a.id} value={a.id}>{a.name} ({a.host}) · {a.online ? tr("в сети") : tr("не в сети")}</option>)}
              </Select>
            </Field>
          )
        )}
        {isVm && <>
          <Alert tone="info">{tr("При восстановлении сервер создаёт новую ВМ, передаёт ей одноразовый ключ SSH через cloud-init, дожидается загрузки и восстанавливает данные по SSH. Если восстановление не удалось, ВМ удаляется (настраивается). После успеха ВМ появится среди целей как сервер по SSH.")}</Alert>
          <SchemaForm schema={kdef?.schema} value={vp} onChange={setVp} columns={2} />
        </>}
      </div>
    </Modal>
  )
}
