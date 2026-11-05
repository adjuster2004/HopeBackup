import { Fragment, useState } from 'react'
import { Link, useNavigate, useParams, useSearchParams } from 'react-router-dom'
import { ArrowLeft, Download, FileSearch, FilterX, HardDrive, LayoutGrid, List, Search, KeyRound, Monitor, Pencil, Plus, PlugZap, Radar, RefreshCw, RotateCcw, Server, Trash2, Wand2 } from 'lucide-react'
import { del, fmtAgo, fmtBytes, fmtTime, post, put, SECRET_MASK } from '../api'
import type { Agent, Site } from '../types'
import { Alert, Badge, Button, Card, Confirm, cx, Empty, Field, IconButton, Input, KV, Modal, PageHeader, Select, Spinner, Table, Tabs, Td, Textarea, useApi, useToast, RefreshButton } from '../components/ui'
import { JobLogModal } from '../components/JobLog'
import { AnalysisView, SystemView, type AnalysisT } from '../components/Analysis'
import { HostInventory } from '../components/HostInventory'
import { SiteSelect } from '../components/InstanceFields'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

export type Machine = {
  id: string; name: string; host: string; port: number; user: string; auth: 'password' | 'key'
  password: string; private_key: string; key_passphrase: string; sudo_password: string; host_key: string
  agent_id: string; agent: { id: string; name: string; last_seen: string | null; version: string } | null; notes: string; created: string
  last: { time: string; ok: boolean; error: string; os: string; arch: string; method: string; sudo: string; recommendations: number; estimate: number; prepare_todo?: number } | null
}
type TestRes = { ok: boolean; host_key: string; sudo: string; sudo_note?: string; root: boolean; uname: string; os: string; libc: string; program?: string }

const VIEW_KEY = 'hopebackup_machines_view'
const online = (a: Machine['agent']) => !!a?.last_seen && (Date.now() - new Date(a.last_seen).getTime()) < 120_000

export function MachinesPage() {
  const toast = useToast()
  const perms = usePerms()
  const nav = useNavigate()
  const { data, error, reload } = useApi<Machine[]>('/api/machines', [], 10000)
  const [edit, setEdit] = useState<Machine | 'new' | null>(null)
  const [busy, setBusy] = useState('')
  const [job, setJob] = useState<string | null>(null)
  const [tests, setTests] = useState<Record<string, TestRes | { error: string }>>({})
  const [remove, setRemove] = useState<Machine | null>(null)
  const test = async (m: Machine) => {
    setBusy('test:' + m.id)
    try { const r = await post<TestRes>(`/api/machines/${m.id}/test`); setTests(t => ({ ...t, [m.id]: r })); toast.ok(trf("«{0}»: вход выполнен, {1}", m.name, r.sudo)); reload() }
    catch (e: any) { setTests(t => ({ ...t, [m.id]: { error: e.message } })); toast.err(e) } finally { setBusy('') }
  }
  const inventory = async (m: Machine) => {
    setBusy('inv:' + m.id)
    try { const r = await post<{ job_id: string }>(`/api/machines/${m.id}/inventory`); setJob(r.job_id) } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doRemove = async () => {
    const m = remove!; setRemove(null)
    try { await del(`/api/machines/${m.id}`); toast.ok(trf("«{0}» удалена", m.name)); reload() } catch (e) { toast.err(e) }
  }
  // Фильтры — в адресе страницы (?q=&agent=&inv=), вид (карточки или список) запоминается в браузере.
  const [sp, setSp] = useSearchParams()
  const f = { q: sp.get('q') || '', agent: sp.get('agent') || '', inv: sp.get('inv') || '' }
  const setF = (k: keyof typeof f, v: string) => { const n = new URLSearchParams(sp); if (v) n.set(k, v); else n.delete(k); setSp(n, { replace: true }) }
  const [view, setViewState] = useState<'cards' | 'list'>(() => { try { return localStorage.getItem(VIEW_KEY) === 'list' ? 'list' : 'cards' } catch { return 'cards' } })
  const setView = (v: 'cards' | 'list') => { setViewState(v); try { localStorage.setItem(VIEW_KEY, v) } catch { /* */ } }
  const agentOk = (m: Machine) => !f.agent || (f.agent === 'none' ? !m.agent : f.agent === 'online' ? online(m.agent) : !!m.agent && !online(m.agent))
  const invOk = (m: Machine) => !f.inv || (f.inv === 'none' ? !m.last : f.inv === 'ok' ? !!m.last?.ok : f.inv === 'error' ? !!m.last && !m.last.ok : !!m.last?.prepare_todo)
  const shown = (data || []).filter(m => agentOk(m) && invOk(m)
    && (!f.q || `${m.name} ${m.host} ${m.user} ${m.last?.os || ''} ${m.notes} ${m.agent?.name || ''}`.toLowerCase().includes(f.q.toLowerCase())))
  const filtered = !!(f.q || f.agent || f.inv)
  const imageOf = (m: Machine) => nav(`/products/new?catalog=machine&machine=${encodeURIComponent(m.id)}`)
  const agentBadge = (m: Machine) => m.agent ? <Badge tone={online(m.agent) ? 'green' : 'amber'}>{online(m.agent) ? tr("агент в сети") : tr("агент не в сети")}</Badge> : <Badge>{tr("без агента")}</Badge>
  const loginBadge = (m: Machine) => m.auth === 'key' ? <Badge tone="violet"><KeyRound size={11} />{tr("ключ SSH")}</Badge> : <Badge tone="blue">{tr("пароль")}</Badge>
  const lastInv = (m: Machine, short: boolean) => !m.last ? <span className="text-slate-500">{tr("ещё не было")}</span> : m.last.ok
    ? <span>{fmtTime(m.last.time)} · {m.last.os}{short ? null : <>  {tr("· рекомендаций")} {m.last.recommendations}</>} · ≈{fmtBytes(m.last.estimate)}{m.last.method === 'shell' ? <Badge tone="amber">{tr("сокращённая")}</Badge> : null}{m.last.prepare_todo ? <Badge tone="amber">{tr("донастроить:")} {m.last.prepare_todo}</Badge> : null}</span>
    : <span className="text-red-700">{fmtTime(m.last.time)}: {m.last.error}</span>
  const testView = (t: TestRes | { error: string }) => 'error' in t ? <Alert tone="error">{t.error}</Alert> : <div className="mt-2 text-sm text-slate-600 bg-slate-50 rounded-lg px-3 py-2">{t.os} · <span className="font-mono text-xs">{t.uname}</span> · {t.sudo}{t.sudo_note ? <span className="text-amber-700"> — {t.sudo_note}</span> : null}{t.libc ? <span className="text-xs text-slate-400"> · {t.libc}</span> : null}{t.program ? <div className="text-xs text-slate-500 mt-1">{tr("Программа описи и агента:")} {t.program}</div> : null}</div>
  return (
    <div>
      <PageHeader title={tr("Серверы и ВМ")} icon={<Monitor size={22} />}
        subtitle={tr("Машины, которые Ковчег осматривает по SSH до начала копирования: ОС, ядро, загрузка, диски, программы, задания, пользователи — и рекомендации, что копировать.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.admin && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить сервер")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<Server size={28} />} title={tr("Серверов пока нет")} text={tr("Добавьте сервер или ВМ: адрес, пользователь и пароль (или ключ SSH, если пароли запрещены). Ковчег снимет опись и подскажет, что копировать.")}
          action={perms.admin && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить сервер")}</Button>} /></Card>
      ) : (<>
        <div className="flex flex-wrap items-center gap-2 mb-4 bg-white border border-slate-200 rounded-xl px-4 py-3">
          <div className="relative w-full sm:w-64">
            <Search size={15} className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400 pointer-events-none" />
            <Input className="!pl-9" placeholder={tr("Имя, адрес, ОС")} value={f.q} onChange={e => setF('q', e.target.value)} />
          </div>
          <Select className="w-full sm:w-44" value={f.agent} onChange={e => setF('agent', e.target.value)} aria-label={tr("Агент")}>
            <option value="">{tr("Агент: любой")}</option>
            <option value="online">{tr("Агент в сети")}</option>
            <option value="offline">{tr("Агент не в сети")}</option>
            <option value="none">{tr("Без агента")}</option>
          </Select>
          <Select className="w-full sm:w-48" value={f.inv} onChange={e => setF('inv', e.target.value)} aria-label={tr("Опись")}>
            <option value="">{tr("Опись: любая")}</option>
            <option value="ok">{tr("Опись снята")}</option>
            <option value="error">{tr("Опись с ошибкой")}</option>
            <option value="none">{tr("Описи не было")}</option>
            <option value="todo">{tr("Нужно донастроить")}</option>
          </Select>
          {filtered && <span className="flex items-center gap-3 whitespace-nowrap ml-1">
            <span className="text-xs text-slate-500">{tr("показано")} {shown.length}  {tr("из")} {data.length}</span>
            <button type="button" className="inline-flex items-center gap-1 text-xs text-blue-600 hover:underline" onClick={() => setSp(new URLSearchParams(), { replace: true })}><FilterX size={13} />{tr("Сбросить")}</button>
          </span>}
          <div className="ml-auto inline-flex rounded-lg border border-slate-300 overflow-hidden text-sm" role="group" aria-label={tr("Вид")}>
            <button type="button" title={tr("Карточки")} onClick={() => setView('cards')} className={cx('px-2.5 py-1.5 inline-flex items-center gap-1.5', view === 'cards' ? 'bg-blue-600 text-white' : 'bg-white text-slate-600 hover:bg-slate-50')}><LayoutGrid size={15} />{tr("Карточки")}</button>
            <button type="button" title={tr("Список")} onClick={() => setView('list')} className={cx('px-2.5 py-1.5 inline-flex items-center gap-1.5 border-l border-slate-300', view === 'list' ? 'bg-blue-600 text-white' : 'bg-white text-slate-600 hover:bg-slate-50')}><List size={15} />{tr("Список")}</button>
          </div>
        </div>
        {shown.length === 0 ? <Card><div className="py-6 text-center text-sm text-slate-500">{tr("Под фильтр ничего не подходит.")} <button type="button" className="text-blue-600 hover:underline" onClick={() => setSp(new URLSearchParams(), { replace: true })}>{tr("Сбросить фильтры")}</button></div></Card>
        : view === 'list' ? (
        <Card padded={false}>
          <Table head={[tr("Сервер"), tr("Адрес"), tr("Вход"), tr("Агент"), tr("Последняя опись"), '']}>
            {shown.map(m => {
              const t = tests[m.id]
              return (<Fragment key={m.id}>
                <tr className="hover:bg-slate-50 align-top">
                  <Td><Link to={`/machines/${m.id}`} className="font-medium text-slate-900 hover:underline">{m.name}</Link>{m.notes ? <div className="text-xs text-slate-500 max-w-xs truncate" title={m.notes}>{m.notes}</div> : null}</Td>
                  <Td className="font-mono text-xs whitespace-nowrap">{m.user}@{m.host}:{m.port}</Td>
                  <Td>{loginBadge(m)}</Td>
                  <Td>{agentBadge(m)}</Td>
                  <Td className="text-sm">{lastInv(m, true)}</Td>
                  <Td className="whitespace-nowrap text-right">
                    <span className="inline-flex items-center gap-0.5">
                      <IconButton title={tr("Проверить вход")} onClick={() => test(m)} disabled={busy === 'test:' + m.id}><PlugZap size={16} /></IconButton>
                      {perms.edit && <IconButton title={tr("Опись и анализ")} onClick={() => inventory(m)} disabled={busy === 'inv:' + m.id}><Radar size={16} /></IconButton>}
                      {m.last?.ok && <IconButton title={tr("Открыть анализ")} onClick={() => nav(`/machines/${m.id}`)}><FileSearch size={16} /></IconButton>}
                      {perms.edit && <IconButton title={tr("Образ дисков")} onClick={() => imageOf(m)}><HardDrive size={16} /></IconButton>}
                      {perms.admin && <IconButton title={tr("Изменить")} onClick={() => setEdit(m)}><Pencil size={16} /></IconButton>}
                      {perms.admin && <IconButton title={tr("Удалить")} danger onClick={() => setRemove(m)}><Trash2 size={16} /></IconButton>}
                    </span>
                  </Td>
                </tr>
                {t && <tr><td colSpan={6} className="px-4 pb-3">{testView(t)}</td></tr>}
              </Fragment>)
            })}
          </Table>
        </Card>
        ) : (
        <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
          {shown.map(m => {
            const t = tests[m.id]
            return (
              <Card key={m.id} title={<Link to={`/machines/${m.id}`} className="hover:underline">{m.name}</Link>} icon={<Server size={18} className="text-blue-600" />}
                actions={<>
                  {agentBadge(m)}
                  {perms.admin && <IconButton title={tr("Изменить")} onClick={() => setEdit(m)}><Pencil size={16} /></IconButton>}
                  {perms.admin && <IconButton title={tr("Удалить")} danger onClick={() => setRemove(m)}><Trash2 size={16} /></IconButton>}
                </>}>
                <KV k={tr("Адрес")} mono>{m.user}@{m.host}:{m.port}</KV>
                <KV k={tr("Вход")}>{loginBadge(m)}{m.sudo_password ? <span className="text-xs text-slate-500 ml-2">{tr("отдельный пароль sudo")}</span> : null}</KV>
                <KV k={tr("Ключ хоста")} mono>{m.host_key || <span className="text-slate-400 font-sans">{tr("запомнится при первом подключении")}</span>}</KV>
                <KV k={tr("Последняя опись")}>{lastInv(m, false)}</KV>
                {t && testView(t)}
                <div className="flex flex-wrap gap-2 mt-4">
                  <Button icon={<PlugZap size={16} />} loading={busy === 'test:' + m.id} onClick={() => test(m)}>{tr("Проверить вход")}</Button>
                  {perms.edit && <Button kind="primary" icon={<Radar size={16} />} loading={busy === 'inv:' + m.id} onClick={() => inventory(m)}>{tr("Опись и анализ")}</Button>}
                  {m.last?.ok && <Button onClick={() => nav(`/machines/${m.id}`)}>{tr("Открыть анализ")}</Button>}
                  {perms.edit && <Button icon={<HardDrive size={16} />} title={tr("Копия дисков машины целиком по SSH, без агента: развернуть ВМ на гипервизоре или выгрузить .qcow2")} onClick={() => imageOf(m)}>{tr("Образ дисков")}</Button>}
                </div>
              </Card>
            )
          })}
        </div>
        )}
      </>)}
      {edit && <MachineModal m={edit === 'new' ? null : edit} onClose={() => setEdit(null)} onSaved={() => { setEdit(null); reload() }} />}
      <JobLogModal id={job} onClose={() => { setJob(null); reload() }} />
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить сервер?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>«<b>{remove?.name}</b>{tr("» и его опись будут удалены из Ковчега. На самой машине ничего не меняется; агент (если установлен) продолжит работу.")}</>} />
    </div>
  )
}

function MachineModal({ m, onClose, onSaved }: { m: Machine | null; onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const { data: agents } = useApi<Agent[]>('/api/agents')
  const [f, setF] = useState({ name: m?.name || '', host: m?.host || '', port: m?.port || 22, user: m?.user || 'root', auth: m?.auth || 'password', password: '', private_key: '', key_passphrase: '', sudo_password: '', notes: m?.notes || '', agent_id: m?.agent_id || '' })
  const [busy, setBusy] = useState(false)
  const set = (p: Partial<typeof f>) => setF({ ...f, ...p })
  const keep = (v: string, had: string | undefined) => v || (m && had ? SECRET_MASK : '')
  const save = async () => {
    if (!f.name.trim() || !f.host.trim() || !f.user.trim()) return toast.err(tr("Укажите имя, адрес и пользователя"))
    if (!m && f.auth === 'password' && !f.password) return toast.err(tr("Укажите пароль"))
    if (!m && f.auth === 'key' && !f.private_key.trim()) return toast.err(tr("Вставьте закрытый ключ"))
    const body = { ...f, port: Number(f.port) || 22, password: keep(f.password, m?.password), private_key: keep(f.private_key, m?.private_key), key_passphrase: keep(f.key_passphrase, m?.key_passphrase), sudo_password: keep(f.sudo_password, m?.sudo_password) }
    setBusy(true)
    try { if (m) await put(`/api/machines/${m.id}`, body); else await post('/api/machines', body); toast.ok(m ? tr("Сохранено") : tr("Сервер добавлен — нажмите «Опись и анализ»")); onSaved() } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const kept = m ? tr("задан — оставьте пустым, чтобы не менять") : ''
  return (
    <Modal open wide onClose={onClose} title={m ? trf("Сервер «{0}»", m.name) : tr("Новый сервер или ВМ")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{m ? tr("Сохранить") : tr("Добавить")}</Button></>}>
      <div className="space-y-4">
        <div className="grid sm:grid-cols-[1fr_1fr_110px] gap-4">
          <Field label={tr("Имя")}><Input value={f.name} onChange={e => set({ name: e.target.value })} placeholder="db-prod-01" autoFocus /></Field>
          <Field label={tr("Адрес")} hint={tr("Имя хоста или IP")}><Input className="font-mono text-[13px]" value={f.host} onChange={e => set({ host: e.target.value })} placeholder="10.0.0.21" /></Field>
          <Field label={tr("Порт SSH")}><Input type="number" value={f.port} onChange={e => set({ port: parseInt(e.target.value, 10) || 22 })} /></Field>
        </div>
        <div className="grid sm:grid-cols-2 gap-4">
          <Field label={tr("Пользователь")} hint={tr("root или пользователь с sudo")}><Input className="font-mono text-[13px]" value={f.user} onChange={e => set({ user: e.target.value })} /></Field>
          <Field label={tr("Вход")}>
            <div className="flex gap-2">
              {(['password', 'key'] as const).map(a => (
                <button key={a} type="button" onClick={() => set({ auth: a })} className={`flex-1 px-3 py-2 rounded-lg border text-sm ${f.auth === a ? 'border-blue-500 bg-blue-50 text-blue-800' : 'border-slate-200 hover:bg-slate-50'}`}>{a === 'password' ? tr("по паролю") : tr("по ключу SSH")}</button>
              ))}
            </div>
          </Field>
        </div>
        {f.auth === 'password' ? (
          <Field label={tr("Пароль")} hint={tr("Подходит и для входа «keyboard-interactive» (PAM), и для sudo")}><Input type="password" autoComplete="new-password" value={f.password} onChange={e => set({ password: e.target.value })} placeholder={m?.password ? kept : ''} /></Field>
        ) : <>
          <Field label={tr("Закрытый ключ")} hint={tr("OpenSSH (-----BEGIN OPENSSH PRIVATE KEY-----) или PEM: ed25519, ECDSA, RSA. Открытый ключ должен быть в ~/.ssh/authorized_keys пользователя.")}>
            <Textarea rows={6} className="text-xs" value={f.private_key} onChange={e => set({ private_key: e.target.value })} placeholder={m?.private_key ? kept : '-----BEGIN OPENSSH PRIVATE KEY-----'} spellCheck={false} />
          </Field>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Парольная фраза ключа")}><Input type="password" autoComplete="new-password" value={f.key_passphrase} onChange={e => set({ key_passphrase: e.target.value })} placeholder={m?.key_passphrase ? kept : tr("если ключ зашифрован")} /></Field>
            <Field label={tr("Пароль sudo")} hint={tr("Если sudo для этого пользователя спрашивает пароль")}><Input type="password" autoComplete="new-password" value={f.sudo_password} onChange={e => set({ sudo_password: e.target.value })} placeholder={m?.sudo_password ? kept : tr("не нужен")} /></Field>
          </div>
        </>}
        {m && <Field label={tr("Агент на этой машине")} hint={tr("Привязывается сам при установке из Ковчега; если агент уже стоял — выберите его")}>
          <select className="w-full rounded-lg border border-slate-300 px-3 py-2 text-sm" value={f.agent_id} onChange={e => set({ agent_id: e.target.value })}>
            <option value="">{tr("— нет —")}</option>
            {(agents || []).map(a => <option key={a.id} value={a.id}>{a.name} ({a.host})</option>)}
          </select>
        </Field>}
        <Field label={tr("Заметка")}><Input value={f.notes} onChange={e => set({ notes: e.target.value })} placeholder={tr("назначение, ответственный")} /></Field>
        <Alert tone="info">{tr("Пароль и ключ хранятся на сервере управления (файл состояния 0600) и в панель не возвращаются. Ключ хоста запоминается при первом подключении: если он сменится, Ковчег откажется подключаться, пока ключ не сброшен.")}</Alert>
      </div>
    </Modal>
  )
}

type InvDoc = { time: string; method: string; inventory: Record<string, any>; analysis: AnalysisT }
type Tab = 'analysis' | 'system' | 'inventory'

export function MachinePage() {
  const { id = '' } = useParams()
  const toast = useToast()
  const perms = usePerms()
  const { data: list, reload: reloadList } = useApi<Machine[]>('/api/machines', [], 10000)
  const { data: inv, error: invErr, reload } = useApi<InvDoc>(`/api/machines/${id}/inventory`)
  const { data: sites } = useApi<Site[]>('/api/sites')
  const [tab, setTab] = useState<Tab>('analysis')
  const [job, setJob] = useState<string | null>(null)
  const [busy, setBusy] = useState('')
  const [sel, setSel] = useState<string[] | null>(null)
  const [siteId, setSiteId] = useState('')
  const [install, setInstall] = useState(false)
  const [serverUrl, setServerUrl] = useState(window.location.origin)
  const m = list?.find(x => x.id === id)
  const recs = inv?.analysis?.recommendations || []
  const picked = sel ?? recs.filter(r => r.priority === 'high' && (r.action === 'instance') && r.available).map(r => r.id)
  const run = async () => {
    setBusy('inv')
    try { const r = await post<{ job_id: string }>(`/api/machines/${id}/inventory`); setJob(r.job_id) } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doInstall = async () => {
    setInstall(false); setBusy('agent')
    try { const r = await post<{ job_id: string }>(`/api/machines/${id}/install-agent`, { server_url: serverUrl.trim() }); setJob(r.job_id) } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const apply = async () => {
    if (!siteId) return toast.err(tr("Выберите площадку"))
    setBusy('apply')
    try {
      type Ref = { id: string; name: string }
      const r = await post<{ created: Ref[]; updated: Ref[]; skipped: { id: string; reason: string }[] }>(`/api/machines/${id}/apply`, { recommendations: picked, site_id: siteId })
      const parts: string[] = []
      if (r.created.length) parts.push(trf("создано экземпляров: {0} — {1}", r.created.length, r.created.map(c => c.name).join(', ')))
      if (r.updated?.length) parts.push(trf("дополнено: {0}", r.updated.map(c => c.name).join(', ')))
      if (r.skipped?.length) parts.push(trf("пропущено: {0}", r.skipped.map(x => `${x.id} (${x.reason})`).join('; ')))
      toast.ok(parts.length ? parts.join(' · ') : tr("Изменений нет"))
    } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  if (!list) return <Spinner />
  if (!m) return <Alert tone="error">{tr("Сервер не найден.")} <Link to="/machines" className="underline">{tr("К списку")}</Link></Alert>
  const agentRec = recs.find(r => r.id === 'agent')
  return (
    <div>
      <div className="mb-3"><Link to="/machines" className="text-sm text-blue-600 hover:underline inline-flex items-center gap-1"><ArrowLeft size={14} />{tr("Серверы и ВМ")}</Link></div>
      <PageHeader title={m.name} icon={<Server size={22} />}
        subtitle={<span className="font-mono text-sm">{m.user}@{m.host}:{m.port}{inv ? <span className="font-sans text-slate-500">  {tr("· опись")} {fmtTime(inv.time)} ({fmtAgo(inv.time)})</span> : null}</span>}
        actions={<>
          {perms.edit && <Button kind="primary" icon={<Radar size={16} />} loading={busy === 'inv'} onClick={run}>{inv ? tr("Повторить опись") : tr("Опись и анализ")}</Button>}
          {perms.admin && !m.agent && <Button icon={<Download size={16} />} loading={busy === 'agent'} onClick={() => setInstall(true)}>{tr("Установить агента")}</Button>}
        </>} />
      {m.last && !m.last.ok && <div className="mb-4"><Alert tone="error" title={tr("Последняя опись не удалась")}>{m.last.error}{m.last.error.includes('ключ хоста') && perms.admin && <Button className="ml-3 !py-1" icon={<RotateCcw size={14} />} onClick={async () => { await post(`/api/machines/${id}/reset-key`); toast.ok(tr("Ключ хоста сброшен — повторите опись")); reloadList() }}>{tr("Сбросить ключ хоста")}</Button>}</Alert></div>}
      {!inv ? (invErr ? <Card><Empty icon={<Radar size={28} />} title={tr("Опись ещё не снималась")} text={tr("Ковчег подключится по SSH, скопирует на время свою программу описи, соберёт сведения от root и удалит её. На машине ничего не устанавливается.")} action={perms.edit && <Button kind="primary" icon={<Radar size={16} />} onClick={run}>{tr("Опись и анализ")}</Button>} /></Card> : <Spinner />) : <>
        <Tabs tabs={[{ id: 'analysis', label: tr("Анализ и рекомендации") }, { id: 'system', label: tr("Система") }, { id: 'inventory', label: tr("Опись") }]} value={tab} onChange={setTab} />
        {tab === 'analysis' && (
          <Card>
            <AnalysisView a={inv.analysis} selectable={!!m.agent && perms.edit} selected={picked} onToggle={rid => setSel(picked.includes(rid) ? picked.filter(x => x !== rid) : [...picked, rid])} />
            <div className="mt-6 border-t border-slate-100 pt-4">
              {m.agent ? (
                <div className="grid md:grid-cols-[1fr_auto] gap-4 items-end">
                  <SiteSelect sites={sites} value={siteId} onChange={setSiteId} />
                  <Button kind="primary" icon={<Wand2 size={16} />} loading={busy === 'apply'} disabled={!picked.length} onClick={apply}>{tr("Создать экземпляры по отмеченным (")}{picked.length})</Button>
                </div>
              ) : (
                <Alert tone="info" title={tr("Чтобы начать копирование, установите агента")}>
                  
                  {tr("Агент снимает копии на самой машине и отправляет их прямо на площадку.")} {agentRec?.details?.join(' ')}
                  {perms.admin && agentRec?.available !== false && <div className="mt-2"><Button kind="primary" icon={<Download size={16} />} onClick={() => setInstall(true)}>{tr("Установить агента по SSH")}</Button></div>}
                </Alert>
              )}
            </div>
          </Card>
        )}
        {tab === 'system' && <SystemView sys={inv.inventory.system} method={inv.method} asRoot={inv.inventory.as_root} />}
        {tab === 'inventory' && <Card><HostInventory res={inv.inventory} config={{}} canEdit={false} onConfig={async () => {}} /></Card>}
      </>}
      <JobLogModal id={job} onClose={() => { setJob(null); reload(); reloadList() }} />
      <Modal open={install} onClose={() => setInstall(false)} title={tr("Установить агента по SSH")}
        footer={<><Button onClick={() => setInstall(false)}>{tr("Отмена")}</Button><Button kind="primary" icon={<Download size={16} />} onClick={doInstall}>{tr("Установить")}</Button></>}>
        <div className="space-y-3 text-sm">
          <p>{tr("Ковчег скопирует программу в")} <span className="font-mono">/opt/hopebackup</span>{tr(", подключит агента к этому серверу управления одноразовым токеном и запустит службу")} <span className="font-mono">hopebackup-agent</span>  {tr("(systemd или OpenRC).")}</p>
          <Field label={tr("Адрес сервера управления для агента")} hint={tr("Как машина видит этот сервер (порт панели 4443)")}><Input className="font-mono text-[13px]" value={serverUrl} onChange={e => setServerUrl(e.target.value)} /></Field>
        </div>
      </Modal>
    </div>
  )
}
