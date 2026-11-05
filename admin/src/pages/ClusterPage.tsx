import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { Copy, Crown, GitBranch, KeyRound, Link2, Plus, RefreshCw, ShieldAlert, Trash2 } from 'lucide-react'
import { auth, del, fmtTime, post, put } from '../api'
import type { Cluster, Connector, Site } from '../types'
import { Alert, Badge, Button, Card, Code, Confirm, Field, IconButton, Input, KV, Modal, PageHeader, Select, Spinner, StatusDot, Table, Td, Toggle, useApi, useToast, RefreshButton } from '../components/ui'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

export function ClusterPage() {
  const toast = useToast()
  const perms = usePerms()
  const nav = useNavigate()
  const { data: c, error, reload } = useApi<Cluster>('/api/cluster', [], 10000)
  const { data: sites, reload: reloadSites } = useApi<Site[]>('/api/sites')
  const [invite, setInvite] = useState(false)
  const [join, setJoin] = useState(false)
  const [offer, setOffer] = useState(false)
  const [adopt, setAdopt] = useState(false)
  const { data: conns } = useApi<Connector[]>('/api/connectors')
  const [take, setTake] = useState(false)
  const [remove, setRemove] = useState<string | null>(null)
  if (error && !c) return <Alert tone="error">{error}</Alert>
  if (!c) return <Spinner />
  const active = c.role === 'active'
  const alone = c.peers.filter(p => !p.self).length === 0
  const doTake = async () => {
    setTake(false)
    try { const r = await post<{ epoch: number }>('/api/cluster/takeover'); toast.ok(trf("Узел стал активным (эпоха {0}). Агенты переключатся сами в течение минуты.", r.epoch)); reload() } catch (e) { toast.err(e) }
  }
  const doRemove = async () => {
    const id = remove!; setRemove(null)
    try { await del(`/api/cluster/peers/${encodeURIComponent(id)}`); toast.ok(tr("Узел удалён из кластера")); reload() } catch (e) { toast.err(e) }
  }
  return (
    <div className="max-w-6xl">
      <PageHeader title={tr("Узлы управления")} icon={<GitBranch size={22} />}
        subtitle={tr("Активный узел управляет копиями; принимающие на других площадках получают от него каталог (площадки, экземпляры, цели, пользователей), а данные копий — репликацией площадок. При потере основной площадки примите управление на принимающем узле.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {active && perms.admin && <Button icon={<KeyRound size={16} />} onClick={() => setAdopt(true)}>{tr("Подключить по коду узла")}</Button>}
          {active && perms.admin && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setInvite(true)}>{tr("Пригласить принимающий узел")}</Button>}
        </>} />
      <div className="grid lg:grid-cols-2 gap-6 mb-6">
        <Card title={tr("Этот узел")} icon={active ? <Crown size={18} className="text-amber-500" /> : <GitBranch size={18} className="text-blue-600" />}>
          <KV k={tr("Роль")}>{active ? <Badge tone="green">{tr("активный")}</Badge> : <Badge tone="amber">{tr("принимающий")}</Badge>}<span className="text-slate-400 text-xs ml-2">{tr("эпоха")} {c.epoch}</span></KV>
          <KV k={tr("Имя")}>{c.name}<span className="text-slate-400 font-mono text-xs ml-2">{c.node_id}</span></KV>
          <KV k={tr("Адрес")} mono>{c.self_url || <span className="text-slate-400 font-sans">{tr("не задан (задаётся при приглашении или подключении)")}</span>}</KV>
          <KV k={tr("Отпечаток")} mono><span className="text-xs break-all">{c.fingerprint}</span></KV>
          {!active && <>
            <KV k={tr("Активный узел")} mono>{c.active_url}</KV>
            <KV k={tr("Последняя синхронизация")}>{c.last_sync ? fmtTime(c.last_sync) : '—'}</KV>
          </>}
        </Card>
        {!active ? (
          <Card title={tr("Потеря основной площадки")} icon={<ShieldAlert size={18} className="text-red-600" />}>
            <div className="text-sm text-slate-600 space-y-2">
              <p>{tr("Если активный узел недоступен и не вернётся в ближайшее время, примите управление здесь: узел станет активным, расписания и агенты перейдут на него.")}</p>
              <p>{tr("Прежний активный узел, если вернётся, увидит большую эпоху и сам станет принимающим — двух активных узлов не будет.")}</p>
              <p>{tr("Затем в «Площадках» переведите экземпляры потерянной площадки на принимающую — копии и журнал там уже есть.")}</p>
            </div>
            {perms.admin && <div className="mt-4"><Button kind="danger" icon={<Crown size={16} />} onClick={() => setTake(true)}>{tr("Принять управление")}</Button></div>}
          </Card>
        ) : alone ? (
          <Card title={tr("Подключить этот узел принимающим")} icon={<Link2 size={18} className="text-blue-600" />}>
            <div className="text-sm text-slate-600 space-y-2">
              <p>{tr("Этот узел пока единственный. Чтобы он стал принимающим для другого (активного) узла, есть два способа — каталог этого узла в обоих случаях заменится каталогом активного:")}</p>
              <ul className="list-disc ml-5 space-y-1">
                <li><b>{tr("Отсюда до активного узла есть доступ")}</b>  {tr("— получите на активном код приглашения и нажмите «Подключиться к активному узлу».")}</li>
                <li><b>{tr("Доступ только в обратную сторону")}</b>  {tr("(например, VPN поднимает активный узел) — получите здесь код подключения и введите его на активном: «Узлы управления» → «Подключить по коду узла».")}</li>
              </ul>
            </div>
            {perms.admin && <div className="mt-4 flex flex-wrap gap-2">
              <Button icon={<Link2 size={16} />} onClick={() => setJoin(true)}>{tr("Подключиться к активному узлу")}</Button>
              <Button icon={<KeyRound size={16} />} onClick={() => setOffer(true)}>{tr("Подготовить к подключению с активного")}</Button>
            </div>}
          </Card>
        ) : (
          <Card title={tr("Как это работает")} icon={<GitBranch size={18} className="text-blue-600" />}>
            <ul className="text-sm text-slate-600 list-disc ml-5 space-y-1.5">
              <li>{tr("Каталог передаётся принимающим каждые 30 секунд и сразу после изменений.")}</li>
              <li>{tr("Данные копий — репликацией площадок (у площадки — список принимающих).")}</li>
              <li>{tr("Агенты знают все узлы и переключаются на активный сами.")}</li>
              <li>{tr("На принимающем узле настройки не меняются — только просмотр, проверки и восстановление.")}</li>
            </ul>
          </Card>
        )}
      </div>
      <Card title={tr("Узлы кластера")} padded={false}>
        {c.peers.length === 0 ? <div className="p-5 text-sm text-slate-500">{tr("Кластер не настроен: узел работает один.")}</div> : (
          <Table head={[tr("Узел"), tr("Адрес"), tr("Версия"), tr("Синхронизация"), '']}>
            {c.peers.map(p => (
              <tr key={p.id} className="align-top">
                <Td className="font-medium text-slate-900">{p.name}{p.self && <Badge tone="blue">{tr("этот узел")}</Badge>}<div className="text-xs font-mono text-slate-400">{p.id}</div></Td>
                <Td className="font-mono text-xs">{p.url}<div className="text-slate-400 break-all">{p.fingerprint.slice(0, 23)}…</div>
                  {p.connector_id && <div className="font-sans text-slate-500">{tr("через коннектор «")}{conns?.find(x => x.id === p.connector_id)?.name || p.connector_id}»</div>}</Td>
                <Td className="text-xs">{p.version ? <Badge tone={p.version === c.version ? 'green' : 'amber'}>v{p.version}</Badge> : <span className="text-slate-400">—</span>}
                  <div className="text-slate-400 mt-1">{tr("с")} {fmtTime(p.joined)}</div></Td>
                <Td className="text-xs">{p.self ? '—' : !active ? <span className="text-slate-400">{tr("ведёт активный узел")}</span> : !p.sync ? <span className="text-slate-400">{tr("ещё не было")}</span> : (
                  <span className="flex flex-col"><span className="flex items-center gap-1.5"><StatusDot tone={p.sync.ok ? 'green' : 'red'} />{p.sync.ok ? tr("успешно") : tr("ошибка")} · {fmtTime(p.sync.time)}</span>{p.sync.error && <span className="text-red-700 break-words max-w-xs">{p.sync.error}</span>}</span>
                )}</Td>
                <Td className="text-right">{active && perms.admin && !p.self && <IconButton title={tr("Удалить из кластера")} danger onClick={() => setRemove(p.id)}><Trash2 size={16} /></IconButton>}</Td>
              </tr>
            ))}
          </Table>
        )}
      </Card>
      {sites && sites.length > 0 && <Overrides sites={sites} c={c} onSaved={() => { reload(); reloadSites() }} />}
      {invite && <InviteModal onClose={() => { setInvite(false); reload() }} />}
      {join && <JoinModal onClose={() => setJoin(false)} onJoined={() => { auth.clear(); nav('/login') }} />}
      {offer && <OfferModal onClose={() => { setOffer(false); reload() }} />}
      {adopt && <AdoptModal conns={conns || []} onClose={() => { setAdopt(false); reload() }} />}
      <Confirm open={take} danger yes={tr("Принять управление")} title={tr("Принять управление?")} onNo={() => setTake(false)} onYes={doTake}
        text={<>{tr("Этот узел станет активным (эпоха")} {c.epoch + 1}{tr("): запустятся расписания, агенты переключатся на него. Делайте это, только если активный узел")} <b>{c.active_url}</b>  {tr("действительно недоступен.")}</>} />
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить узел из кластера?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={tr("Узел перестанет получать каталог. Сам узел и его данные не затрагиваются.")} />
    </div>
  )
}

function Overrides({ sites, c, onSaved }: { sites: Site[]; c: Cluster; onSaved: () => void }) {
  const toast = useToast()
  const [v, setV] = useState<Record<string, { url: string; direct: boolean }>>(() => Object.fromEntries(sites.map(s => [s.id, { url: c.site_overrides[s.id]?.url || '', direct: c.site_overrides[s.id]?.direct || false }])))
  const save = async (id: string) => {
    try { await put(`/api/cluster/overrides/${encodeURIComponent(id)}`, v[id]); toast.ok(tr("Доступ к площадке с этого узла сохранён")); onSaved() } catch (e) { toast.err(e) }
  }
  return (
    <Card title={tr("Доступ к площадкам с этого узла")} className="mt-6">
      <div className="text-sm text-slate-600 mb-3">{tr("Только для этого узла управления: например, принимающая площадка стоит рядом с принимающим узлом и доступна напрямую по внутреннему адресу. Пусто — как в каталоге.")}</div>
      <div className="space-y-3">
        {sites.filter(s => s.kind !== 's3').map(s => (
          <div key={s.id} className="grid sm:grid-cols-[200px_1fr_auto_auto] gap-3 items-center">
            <div className="text-sm font-medium text-slate-800 truncate">{s.name}</div>
            <Input className="font-mono text-[13px]" value={v[s.id]?.url || ''} placeholder={s.url} onChange={e => setV({ ...v, [s.id]: { ...v[s.id], url: e.target.value } })} />
            <Toggle checked={v[s.id]?.direct || false} onChange={d => setV({ ...v, [s.id]: { ...v[s.id], direct: d } })} label={tr("без коннекторов")} />
            <Button onClick={() => save(s.id)}>{tr("Сохранить")}</Button>
          </div>
        ))}
      </div>
    </Card>
  )
}

function InviteModal({ onClose }: { onClose: () => void }) {
  const toast = useToast()
  const [url, setUrl] = useState(window.location.origin)
  const [name, setName] = useState('')
  const [code, setCode] = useState('')
  const [busy, setBusy] = useState(false)
  const go = async () => {
    setBusy(true)
    try { const r = await post<{ code: string }>('/api/cluster/invite', { self_url: url.trim(), name: name.trim() }); setCode(r.code) } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={tr("Пригласить принимающий узел")}
      footer={code ? <Button kind="primary" onClick={onClose}>{tr("Готово")}</Button> : <><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={go}>{tr("Получить код")}</Button></>}>
      {!code ? <div className="space-y-4">
        <Field label={tr("Адрес этого (активного) узла")} hint={tr("Как его видят принимающие узлы и агенты: https://сервер:4443")}><Input className="font-mono text-[13px]" value={url} onChange={e => setUrl(e.target.value)} /></Field>
        <Field label={tr("Имя этого узла (необязательно)")}><Input value={name} onChange={e => setName(e.target.value)} placeholder={tr("ЦОД-1 · активный")} /></Field>
      </div> : <div className="space-y-3">
        <Alert tone="success">{tr("Код действует 24 часа и подходит для одного подключения. На принимающем узле откройте «Узлы управления» → «Подключиться к активному узлу» и вставьте код.")}</Alert>
        <Code className="break-all whitespace-pre-wrap text-xs">{code}</Code>
        <Button icon={<Copy size={16} />} onClick={() => { navigator.clipboard?.writeText(code); toast.ok(tr("Код скопирован")) }}>{tr("Скопировать")}</Button>
      </div>}
    </Modal>
  )
}

function JoinModal({ onClose, onJoined }: { onClose: () => void; onJoined: () => void }) {
  const toast = useToast()
  const [code, setCode] = useState('')
  const [url, setUrl] = useState(window.location.origin)
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const go = async () => {
    if (!code.trim()) return toast.err(tr("Вставьте код приглашения"))
    setBusy(true)
    try { await post('/api/cluster/join', { code: code.trim(), self_url: url.trim(), name: name.trim() }); toast.ok(tr("Узел подключён принимающим. Войдите учётной записью активного узла.")); onJoined() }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={tr("Подключиться к активному узлу")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={go}>{tr("Подключить")}</Button></>}>
      <div className="space-y-4">
        <Alert tone="warn" title={tr("Каталог этого узла будет заменён")}>{tr("Площадки, экземпляры, цели и пользователи этого узла заменятся каталогом активного узла. После подключения войдите учётной записью активного узла.")}</Alert>
        <Field label={tr("Код приглашения")}><textarea className="w-full rounded-lg border border-slate-300 px-3 py-2 font-mono text-xs" rows={4} value={code} onChange={e => setCode(e.target.value)} /></Field>
        <Field label={tr("Адрес этого узла")} hint={tr("Как его видят активный узел и агенты")}><Input className="font-mono text-[13px]" value={url} onChange={e => setUrl(e.target.value)} /></Field>
        <Field label={tr("Имя этого узла (необязательно)")}><Input value={name} onChange={e => setName(e.target.value)} placeholder={tr("ЦОД-2 · принимающий")} /></Field>
      </div>
    </Modal>
  )
}

function OfferModal({ onClose }: { onClose: () => void }) {
  const toast = useToast()
  const [url, setUrl] = useState(window.location.origin)
  const [name, setName] = useState('')
  const [code, setCode] = useState('')
  const [busy, setBusy] = useState(false)
  const go = async () => {
    setBusy(true)
    try { const r = await post<{ code: string }>('/api/cluster/offer', { self_url: url.trim(), name: name.trim() }); setCode(r.code) } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={tr("Подготовить к подключению с активного узла")}
      footer={code ? <Button kind="primary" onClick={() => { auth.clear(); window.location.href = '/login' }}>{tr("Готово — войти заново")}</Button> : <><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={go}>{tr("Получить код")}</Button></>}>
      {!code ? <div className="space-y-4">
        <div className="text-sm text-slate-600">{tr("Активный узел сам зайдёт на этот узел по адресу ниже (через коннектор, если до этой площадки он доходит по VPN) и передаст каталог. Отсюда до активного узла доступ не нужен.")}</div>
        <Field label={tr("Адрес этого узла")} hint={tr("Как его видит активный узел: https://сервер:4443")}><Input className="font-mono text-[13px]" value={url} onChange={e => setUrl(e.target.value)} /></Field>
        <Field label={tr("Имя этого узла (необязательно)")}><Input value={name} onChange={e => setName(e.target.value)} placeholder={tr("ЦОД-2 · принимающий")} /></Field>
      </div> : <div className="space-y-3">
        <Alert tone="success">{tr("Код действует 24 часа и подходит для одного подключения. На")} <b>{tr("активном")}</b>  {tr("узле откройте «Узлы управления» → «Подключить по коду узла», вставьте код и выберите коннектор до этой площадки.")}</Alert>
        <Code className="break-all whitespace-pre-wrap text-xs">{code}</Code>
        <Button icon={<Copy size={16} />} onClick={() => { navigator.clipboard?.writeText(code); toast.ok(tr("Код скопирован")) }}>{tr("Скопировать")}</Button>
        <Alert tone="info">{tr("После подключения пользователи этого узла заменятся пользователями активного — входите их учётными записями.")}</Alert>
      </div>}
    </Modal>
  )
}

const VPN_KINDS = ['l2tp_ipsec', 'ipsec', 'wireguard', 'openvpn']

function AdoptModal({ conns, onClose }: { conns: Connector[]; onClose: () => void }) {
  const toast = useToast()
  const [code, setCode] = useState('')
  const [url, setUrl] = useState(window.location.origin)
  const [name, setName] = useState('')
  const [conn, setConn] = useState('')
  const [busy, setBusy] = useState(false)
  const vpn = conns.filter(x => VPN_KINDS.includes(x.kind))
  const go = async () => {
    if (!code.trim()) return toast.err(tr("Вставьте код подключения с принимающего узла"))
    setBusy(true)
    try { const r = await post<{ name: string }>('/api/cluster/adopt', { code: code.trim(), self_url: url.trim(), name: name.trim(), connector_id: conn }); toast.ok(trf("Узел «{0}» подключён принимающим — каталог передан", r.name)); onClose() }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={tr("Подключить принимающий узел по его коду")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={go}>{tr("Подключить")}</Button></>}>
      <div className="space-y-4">
        <div className="text-sm text-slate-600">{tr("Код получают на принимающем узле: «Узлы управления» → «Подготовить к подключению с активного». Этот узел сам зайдёт на него и передаст каталог; дальше каждые 30 секунд — синхронизация тем же путём.")}</div>
        <Field label={tr("Код подключения")}><textarea className="w-full rounded-lg border border-slate-300 px-3 py-2 font-mono text-xs" rows={4} value={code} onChange={e => setCode(e.target.value)} /></Field>
        <Field label={tr("Коннектор до принимающего узла")} hint={tr("Если до его площадки этот сервер доходит только по VPN. Канал будет подниматься перед каждой синхронизацией.")}>
          <Select value={conn} onChange={e => setConn(e.target.value)}>
            <option value="">{tr("напрямую")}</option>
            {vpn.map(x => <option key={x.id} value={x.id}>{x.name}</option>)}
          </Select>
        </Field>
        <Field label={tr("Адрес этого (активного) узла")} hint={tr("Как его видят принимающие узлы и агенты: https://сервер:4443")}><Input className="font-mono text-[13px]" value={url} onChange={e => setUrl(e.target.value)} /></Field>
        <Field label={tr("Имя этого узла (необязательно)")}><Input value={name} onChange={e => setName(e.target.value)} placeholder={tr("ЦОД-1 · активный")} /></Field>
      </div>
    </Modal>
  )
}
