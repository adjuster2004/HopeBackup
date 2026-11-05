import { useState } from 'react'
import { Cable, KeyRound, Package, Pencil, Plus, PlugZap, RefreshCw, Trash2 } from 'lucide-react'
import { del, fmtTime, get, post, put, q } from '../api'
import type { CatalogKind, Connector, ConnectorTest } from '../types'
import { Alert, Badge, Button, Card, ChoiceCard, Confirm, Empty, Field, IconButton, InDev, Input, Modal, PageHeader, Spinner, StatusDot, Table, Td, Textarea, useApi, useToast, RefreshButton } from '../components/ui'
import { useCatalog } from '../components/common'
import { SchemaForm, schemaInitial } from '../components/SchemaForm'
import { usePerms } from '../perms'
import { JobLogModal } from '../components/JobLog'
import { tr, trf } from '../i18n'

type Params = { host: string; port: number; user: string; private_key: string; remote_host: string; remote_port: number; jump: string }
const EMPTY: Params = { host: '', port: 22, user: 'root', private_key: '', remote_host: '127.0.0.1', remote_port: 8443, jump: '' }
const FALLBACK_KINDS: CatalogKind[] = [{ id: 'ssh_tunnel', title: tr("SSH-туннель (-L)"), available: true }, { id: 'direct', title: tr("Напрямую"), available: true }]
const VPN = ['openvpn', 'wireguard', 'l2tp_ipsec', 'ipsec']
/** Ошибка канала из-за отсутствующих пакетов VPN. */
const NO_PKGS = /нет strongSwan|нет xl2tpd|нет pppd|нет xl2tpd, ppp|не найдена программа (openvpn|wg|swanctl|xl2tpd|pppd)/

/** Куда и как подключается коннектор — одной строкой для таблицы. */
function summary(c: Connector): { main: string; sub?: string; warn?: string } {
  const p = c.params
  switch (c.kind) {
    case 'ssh_tunnel': return { main: `${p.user}@${p.host}:${p.port}`, sub: `→ ${p.remote_host}:${p.remote_port}${p.jump ? trf(" · через {0}", p.jump) : ''}`, warn: p.has_key ? undefined : tr("ключ не задан") }
    case 'openvpn': return { main: p.profile ? tr("профиль .ovpn") : '—', sub: p.username ? trf("логин {0}", p.username) : tr("по сертификату"), warn: p.profile ? undefined : tr("профиль не задан") }
    case 'wireguard': return { main: p.endpoint || '—', sub: p.address ? trf("адрес {0}", p.address) : undefined, warn: p.private_key ? undefined : tr("ключ не задан") }
    case 'l2tp_ipsec': return { main: p.server || '—', sub: p.username ? trf("логин {0}", p.username) : undefined }
    case 'ipsec': return { main: p.remote || '—', sub: p.auth === 'eap' ? `EAP · ${p.username || ''}` : 'PSK' }
    default: return { main: p.node_url || tr("URL площадки") }
  }
}

export function ConnectorsPage() {
  const toast = useToast()
  const perms = usePerms()
  const { data, error, reload } = useApi<Connector[]>('/api/connectors', [], 10000)
  const { catalog } = useCatalog()
  const kinds = catalog?.connector_kinds?.length ? catalog.connector_kinds : FALLBACK_KINDS
  const kindTitle = (k: string) => kinds.find(x => x.id === k)?.title || k
  const [edit, setEdit] = useState<Connector | 'new' | null>(null)
  const [busy, setBusy] = useState('')
  const [remove, setRemove] = useState<Connector | null>(null)
  // Окно «пакеты для коннектора»: после «Сохранить» (если чего-то нет) или по кнопке «Пакеты».
  const [pkgs, setPkgs] = useState<{ id: string; kind: string; name: string; auto: boolean } | null>(null)
  const afterSave = async (c: { id: string; kind: string; name: string }) => {
    setEdit(null); reload()
    if (!VPN.includes(c.kind)) return
    try {
      const r = await get<PkgReport>(`/api/connectors/requirements${q({ kind: c.kind })}`)
      if (r.missing.length) setPkgs({ ...c, auto: true })
    } catch { /* проверка необязательна */ }
  }
  const test = async (c: Connector) => {
    setBusy(c.id)
    try {
      const r = await post<ConnectorTest>(`/api/connectors/${encodeURIComponent(c.id)}/test`)
      const chan = r.local_port ? trf(", локальный порт {0}", r.local_port) : r.iface ? `, ${r.iface}${r.address ? ` ${r.address}` : ''}` : ''
      if (r.node && !r.node.ok) toast.err(trf("«{0}»: канал поднят{1}, но узел площадки «{2}» не отвечает: {3}", c.name, chan, r.node.site, r.node.error))
      else toast.ok(trf("«{0}»: соединение установлено{1}{2}", c.name, chan, r.node ? trf(", узел «{0}» отвечает", r.node.site) : ''))
      reload()
    }
    catch (e) {
      toast.err(e)
      // Нет пакетов VPN — сразу окно установки.
      if (NO_PKGS.test(String((e as any)?.message || e))) setPkgs({ id: c.id, kind: c.kind, name: c.name, auto: true })
      reload()
    } finally { setBusy('') }
  }
  const doRemove = async () => {
    const c = remove!; setRemove(null)
    try { await del(`/api/connectors/${encodeURIComponent(c.id)}`); toast.ok(trf("Коннектор «{0}» удалён", c.name)); reload() } catch (e) { toast.err(e) }
  }
  return (
    <div>
      <PageHeader title={tr("Коннекторы")} icon={<Cable size={22} />}
        subtitle={tr("Каналы до площадок: SSH-туннель, OpenVPN, WireGuard, L2TP/IPsec, IPsec IKEv2 или напрямую. Через VPN идёт только трафик до узла площадки; площадка использует коннекторы по порядку, переключаясь при сбое.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить коннектор")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<Cable size={28} />} title={tr("Коннекторов пока нет")}
          text={tr("Если площадка недоступна напрямую, добавьте SSH-туннель до неё. Без коннекторов сервер подключается к площадке по её URL.")}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить коннектор")}</Button>} /></Card>
      ) : (
        <Card padded={false}>
          <Table head={[tr("Коннектор"), tr("Тип"), tr("Подключение"), tr("Площадки"), tr("Состояние"), '']}>
            {data.map(c => {
              const sm = summary(c)
              return (
              <tr key={c.id} className="hover:bg-slate-50 align-top">
                <Td className="font-medium text-slate-900">{c.name}<div className="text-xs font-normal text-slate-400">{fmtTime(c.created)}</div></Td>
                <Td><Badge tone={VPN.includes(c.kind) ? 'violet' : 'blue'}>{kindTitle(c.kind)}</Badge></Td>
                <Td className="font-mono text-xs">{sm.main}{sm.sub && <div className="text-slate-400">{sm.sub}</div>}{sm.warn && <div className="text-amber-700 font-sans">{sm.warn}</div>}</Td>
                <Td className="text-xs">{c.sites?.length ? c.sites.join(', ') : <span className="text-slate-400">{tr("не используется")}</span>}</Td>
                <Td>
                  {!c.status ? <span className="text-slate-400 text-xs">{tr("не проверялся")}</span> : (
                    <div className="text-xs">
                      <span className="flex items-center gap-1.5 text-sm"><StatusDot tone={c.status.up ? 'green' : 'red'} />{c.status.up ? (c.status.shared ? tr("Подключён (общий канал)") : tr("Подключён")) : tr("Нет связи")}</span>
                      {c.status.local_port ? <div className="text-slate-500">{tr("локальный порт")} {c.status.local_port}</div> : null}
                      {c.status.iface && <div className="text-slate-500 font-mono">{c.status.iface}{c.status.address ? ` · ${c.status.address}` : ''}{c.status.implementation ? ` · ${c.status.implementation}` : ''}</div>}
                      {c.status.routes?.length ? <div className="text-slate-500 font-mono">→ {c.status.routes.join(', ')}</div> : null}
                      {c.status.error && <div className="text-red-700 max-w-xs break-words">{c.status.error}</div>}
                      {c.status.error && NO_PKGS.test(c.status.error) && <Button kind="primary" className="!py-1 !px-2.5 text-xs mt-1.5" icon={<Package size={14} />} onClick={() => setPkgs({ id: c.id, kind: c.kind, name: c.name, auto: true })}>{tr("Установить пакеты")}</Button>}
                    </div>
                  )}
                </Td>
                <Td className="text-right whitespace-nowrap">
                  <Button className="!py-1 !px-2.5 text-xs mr-1" icon={<PlugZap size={14} />} loading={busy === c.id} onClick={() => test(c)}>{tr("Проверить")}</Button>
                  {VPN.includes(c.kind) && <Button className="!py-1 !px-2.5 text-xs mr-1" icon={<Package size={14} />} title={tr("Пакеты VPN на сервере и агентах")} onClick={() => setPkgs({ id: c.id, kind: c.kind, name: c.name, auto: false })}>{tr("Пакеты")}</Button>}
                  {perms.edit && <IconButton title={tr("Изменить")} onClick={() => setEdit(c)}><Pencil size={16} /></IconButton>}
                  {perms.edit && <IconButton title={tr("Удалить")} danger onClick={() => setRemove(c)}><Trash2 size={16} /></IconButton>}
                </Td>
              </tr>
              )
            })}
          </Table>
        </Card>
      )}
      {edit && <ConnectorModal conn={edit === 'new' ? null : edit} kinds={kinds} onClose={() => setEdit(null)} onSaved={afterSave} />}
      {pkgs && <PackagesModal {...pkgs} kindTitle={kindTitle(pkgs.kind)} conn={data?.find(x => x.id === pkgs.id)} onClose={() => setPkgs(null)} />}
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить коннектор?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Коннектор «")}<b>{remove?.name}</b>{tr("» будет удалён. Площадки, где он указан, будут использовать оставшиеся коннекторы.")}</>} />
    </div>
  )
}

type PkgReport = { host: string; missing: string[]; command: string; manager: string | null; root: boolean; can_install: boolean; requirements: { what: string; present: boolean }[] }

/** Пакеты для коннектора: что нужно на сервере управления, установить по подтверждению; то же — на агентах. */
function PackagesModal({ id, kind, name, auto, kindTitle, conn, onClose }: { id: string; kind: string; name: string; auto: boolean; kindTitle: string; conn?: Connector; onClose: () => void }) {
  const toast = useToast()
  const perms = usePerms()
  const { data: r, reload } = useApi<PkgReport>(`/api/connectors/requirements${q({ kind })}`)
  const [job, setJob] = useState<string | null>(null)
  const [busy, setBusy] = useState('')
  const agents = conn?.agents || []
  const install = async (on: string[]) => {
    setBusy(on.length ? 'agents' : 'server')
    try {
      const res = await post<{ jobs: string[] }>('/api/connectors/install', { kind, agents: on })
      if (res.jobs.length === 1) setJob(res.jobs[0]); else toast.ok(trf("Заданий установки: {0} — ход в разделе «Задания»", res.jobs.length))
    } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  void id
  return (
    <>
      <Modal open onClose={onClose} title={trf("Пакеты для «{0}»", name)}
        footer={<Button onClick={onClose}>{r && r.missing.length && auto ? tr("Не сейчас") : tr("Закрыть")}</Button>}>
        {!r ? <Spinner /> : (
          <div className="space-y-4 text-sm">
            <div>
              <div className="font-medium text-slate-900 mb-1">{tr("Сервер управления")} {r.host}</div>
              <ul className="space-y-0.5">{r.requirements.map(x => <li key={x.what} className="flex items-center gap-2"><StatusDot tone={x.present ? 'green' : 'red'} />{x.what}{x.present ? '' : tr(" — нет")}</li>)}</ul>
            </div>
            {r.missing.length === 0 ? <Alert tone="success">{tr("Всё нужное для коннектора «")}{kindTitle}{tr("» на сервере установлено.")}</Alert> : <>
              <Alert tone="warn" title={trf("Для коннектора «{0}» на сервере нужно установить пакеты", kindTitle)}>
                <div className="font-mono text-xs break-words">{r.missing.join(' ')}</div>
                <div className="mt-2">{tr("Сервер установит их из репозиториев дистрибутива (")}{r.manager || tr("менеджер пакетов не найден")}){kind !== 'openvpn' && kind !== 'wireguard' ? tr(" и запустит службу strongswan") : ''}{kind === 'l2tp_ipsec' ? tr("; системная служба xl2tpd будет выключена — Ковчег запускает свой экземпляр") : ''}{tr(". Вручную:")} <span className="font-mono text-xs">{r.command}</span></div>
              </Alert>
              {perms.admin ? (r.can_install
                ? <Button kind="primary" icon={<Package size={16} />} loading={busy === 'server'} onClick={() => install([])}>{tr("Установить на сервере")}</Button>
                : <div className="text-amber-700 text-xs">{!r.root ? tr("Сервер работает не от root — установите пакеты вручную командой выше.") : tr("Менеджер пакетов (apt-get, dnf, yum) не найден — установите пакеты вручную.")}</div>)
                : <div className="text-slate-500 text-xs">{tr("Установить пакеты может администратор.")}</div>}
            </>}
            {agents.length > 0 && (
              <div className="border-t border-slate-100 pt-3">
                <div className="font-medium text-slate-900 mb-1">{tr("Агенты, которые пишут через этот коннектор")}</div>
                <div className="text-xs text-slate-600 mb-2">{tr("Им нужны те же пакеты. Агент проверит, чего не хватает, и поставит только это.")}</div>
                <div className="text-xs mb-2">{agents.map(a => a.name + (a.can_install ? '' : tr(" (старая версия — обновите)"))).join(', ')}</div>
                {perms.admin && <Button icon={<Package size={16} />} disabled={!agents.some(a => a.can_install)} loading={busy === 'agents'} onClick={() => install(agents.filter(a => a.can_install).map(a => a.id))}>{tr("Установить на агентах (")}{agents.filter(a => a.can_install).length})</Button>}
              </div>
            )}
          </div>
        )}
      </Modal>
      <JobLogModal id={job} onClose={() => setJob(null)} onDone={() => reload()} />
    </>
  )
}

function ConnectorModal({ conn, kinds, onClose, onSaved }: { conn: Connector | null; kinds: CatalogKind[]; onClose: () => void; onSaved: (c: { id: string; kind: string; name: string }) => void }) {
  const toast = useToast()
  const [name, setName] = useState(conn?.name || '')
  const [kind, setKind] = useState(conn?.kind || kinds.find(k => k.available)?.id || 'ssh_tunnel')
  const [p, setP] = useState<Params>(conn ? { ...EMPTY, ...conn.params, private_key: '' } : EMPTY)
  const kdef = kinds.find(k => k.id === kind)
  const generic = kind !== 'ssh_tunnel' && !!kdef?.schema?.properties
  const [pub, setPub] = useState<string>(conn?.params.public_key || '')
  const genKeys = async () => {
    try { const k = await post<{ private_key: string; public_key: string }>('/api/connectors/wg-keygen'); setExtra({ ...extra, private_key: k.private_key }); setPub(k.public_key); toast.ok(tr("Ключи созданы — добавьте открытый ключ пиром на сервере площадки")) }
    catch (e) { toast.err(e) }
  }
  const [extra, setExtra] = useState<Record<string, any>>(() => schemaInitial(kdef?.schema, undefined, conn?.params as any))
  const [busy, setBusy] = useState(false)
  const set = (x: Partial<Params>) => setP({ ...p, ...x })
  const save = async () => {
    if (!name.trim()) return toast.err(tr("Укажите имя коннектора"))
    if (kind === 'ssh_tunnel' && (!p.host.trim() || !p.user.trim())) return toast.err(tr("Укажите хост и пользователя SSH"))
    if (kind === 'ssh_tunnel' && !conn && !p.private_key.trim()) return toast.err(tr("Вставьте закрытый ключ SSH"))
    const params = generic ? extra : { ...p, port: Number(p.port) || 22, remote_port: Number(p.remote_port) || 8443 }
    setBusy(true)
    try {
      const body = { name: name.trim(), kind, params }
      const saved = conn ? await put<{ id: string }>(`/api/connectors/${encodeURIComponent(conn.id)}`, body) : await post<{ id: string }>('/api/connectors', body)
      toast.ok(conn ? tr("Коннектор сохранён") : tr("Коннектор добавлен")); onSaved({ id: saved?.id || conn?.id || '', kind, name: name.trim() })
    } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open wide onClose={onClose} title={conn ? trf("Коннектор «{0}»", conn.name) : tr("Новый коннектор")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{conn ? tr("Сохранить") : tr("Добавить")}</Button></>}>
      <div className="space-y-4">
        <Field label={tr("Имя")}><Input value={name} onChange={e => setName(e.target.value)} placeholder={tr("ЦОД-2 · SSH-туннель")} autoFocus /></Field>
        <div>
          <div className="text-sm font-medium text-slate-700 mb-1.5">{tr("Тип")}</div>
          <div className="grid sm:grid-cols-2 gap-2" role="radiogroup">
            {kinds.map(k => (
              <ChoiceCard key={k.id} selected={kind === k.id} disabled={!k.available || (!!conn && conn.kind !== k.id)} onClick={() => setKind(k.id)}
                title={k.title} text={k.description} badge={!k.available && <InDev />} />
            ))}
          </div>
        </div>
        {kind === 'ssh_tunnel' && <>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label={tr("Хост SSH")} className="sm:col-span-2"><Input className="font-mono text-[13px]" value={p.host} onChange={e => set({ host: e.target.value })} placeholder="bastion.dc2.example" /></Field>
            <Field label={tr("Порт")}><Input type="number" value={p.port} onChange={e => set({ port: parseInt(e.target.value, 10) || 0 })} /></Field>
          </div>
          <Field label={tr("Пользователь")}><Input value={p.user} onChange={e => set({ user: e.target.value })} /></Field>
          <Field label={tr("Закрытый ключ")} hint={conn?.params.has_key ? tr("Ключ задан — оставьте пустым, чтобы не менять.") : tr("OpenSSH или PEM; хранится в секретах сервера.")}>
            <Textarea rows={5} value={p.private_key} onChange={e => set({ private_key: e.target.value })} placeholder={conn?.params.has_key ? tr("•••••••• задан") : '-----BEGIN OPENSSH PRIVATE KEY-----'} />
          </Field>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label={tr("Узел хранения за туннелем")} className="sm:col-span-2" hint={tr("Адрес площадки, как его видит SSH-сервер.")}><Input className="font-mono text-[13px]" value={p.remote_host} onChange={e => set({ remote_host: e.target.value })} /></Field>
            <Field label={tr("Порт")}><Input type="number" value={p.remote_port} onChange={e => set({ remote_port: parseInt(e.target.value, 10) || 0 })} /></Field>
          </div>
          <Field label={tr("Промежуточный хост (jump, необязательно)")} hint={tr("user@host:port — как ssh -J.")}><Input className="font-mono text-[13px]" value={p.jump} onChange={e => set({ jump: e.target.value })} /></Field>
        </>}
        {kind === 'direct' && <Alert tone="info">{tr("Прямое подключение: сервер обращается к площадке по её URL без туннеля. Удобно, чтобы явно указать прямой путь как резервный после туннеля.")}</Alert>}
        {VPN.includes(kind) && <Alert tone="info">{tr("Агент поднимает канал от root на время работы и направляет в него только трафик до узла площадки (или до перечисленных сетей). Маршрут по умолчанию, DNS и скрипты, которые присылает сервер VPN, не принимаются.")}{kind === 'l2tp_ipsec' || kind === 'ipsec' ? tr(" Нужны strongSwan (strongswan-swanctl, charon-systemd)") + (kind === 'l2tp_ipsec' ? tr(", xl2tpd и ppp") : '') + tr(" — если их нет, агент или сервер ставит их сам из репозиториев дистрибутива (apt/dnf).") : kind === 'openvpn' ? tr(" Нужен пакет openvpn.") : tr(" Нужен пакет wireguard-tools; без модуля ядра — wireguard-go.")}{kind === 'ipsec' ? tr(" Подключение Windows «L2TP/IPsec с общим ключом» — это тип «L2TP / IPsec», а не IKEv2.") : ''}</Alert>}
        {kind === 'wireguard' && (
          <div className="flex flex-wrap items-center gap-3 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2.5">
            <Button icon={<KeyRound size={16} />} onClick={genKeys}>{tr("Создать ключи")}</Button>
            {pub ? <div className="text-sm min-w-0"><div className="text-slate-500">{tr("Открытый ключ этого хоста (добавьте пиром на сервере площадки):")}</div><div className="font-mono text-[13px] break-all select-all text-slate-900">{pub}</div></div>
              : <div className="text-sm text-slate-500">{tr("Или вставьте свой закрытый ключ ниже (wg genkey).")}</div>}
          </div>
        )}
        {generic && <SchemaForm schema={kdef!.schema} value={extra} onChange={setExtra} />}
      </div>
    </Modal>
  )
}
