import { useEffect, useRef, useState } from 'react'
import { ArrowDown, ArrowRightLeft, ArrowUp, CheckCircle2, Cloud, Copy as CopyIcon, Database, ExternalLink, Fingerprint, HardDrive, KeyRound, Pencil, Plus, PlugZap, RefreshCw, Server, Trash2, X } from 'lucide-react'
import { del, fmtBytes, fmtTime, get, post, put, SECRET_MASK } from '../api'
import type { CloudSite, Connector, Instance, S3Site, Site, SiteTest } from '../types'
import { Alert, Badge, Button, Card, ChoiceCard, Code, Confirm, Empty, Field, IconButton, Input, KV, Modal, PageHeader, Select, Spinner, Toggle, useApi, useToast, RefreshButton } from '../components/ui'
import { JobLogModal } from '../components/JobLog'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

const COMPRESSION: [number, string][] = [[0, tr("без сжатия")], [1, tr("1 — быстрое")], [3, tr("3 — по умолчанию")], [6, '6'], [9, tr("9 — сильное")], [19, tr("19 — максимальное, медленно")]]

type Kind = 'node' | 'disk' | 'yandex' | 'aws' | 's3' | 'gdrive' | 'yadisk' | 'webdav'
const S3_KINDS: Kind[] = ['yandex', 'aws', 's3']
const CLOUD_KINDS: Kind[] = ['gdrive', 'yadisk', 'webdav']
const CLOUD_EMPTY: CloudSite = { provider: 'gdrive', url: '', username: '', password: '', folder: '', folder_id: '', client_id: '', client_secret: '', refresh_token: '', access_token: '', token_expires: 0, account: '', fingerprint: '', endpoint: '' }
const kindOf = (s: Site): Kind => s.kind === 's3' ? (s.s3?.provider || 's3') : s.kind === 'cloud' ? (s.cloud?.provider || 'webdav') : s.kind === 'disk' ? 'disk' : 'node'
type Form = { name: string; url: string; fingerprint: string; repo: string; token: string; password: string; prune_token: string; connector_ids: string[]; compression: number }
const EMPTY: Form = { name: '', url: 'https://', fingerprint: '', repo: 'main', token: '', password: '', prune_token: '', connector_ids: [], compression: 3 }
const S3_EMPTY: S3Site = { provider: 'yandex', endpoint: '', region: '', bucket: '', prefix: 'hopebackup', access_key: '', secret_key: '', admin_access_key: '', admin_secret_key: '', path_style: false, lock_mode: '', lock_days: 30, storage_class: '', fingerprint: '' }
const KINDS: { id: Kind; title: string; text: string }[] = [
  { id: 'node', title: tr("Узел HopeBackup"), text: tr("Сервер хранения на площадке (в том числе ВМ в облаке)") },
  { id: 'disk', title: tr("Диск сервера управления"), text: tr("Каталог на большом диске этого сервера (/mnt/data и т. п.)") },
  { id: 'yandex', title: 'Yandex Object Storage', text: tr("Бакет в Yandex Cloud, Object Lock") },
  { id: 'aws', title: 'AWS S3', text: tr("Бакет в AWS, Object Lock, классы хранения") },
  { id: 's3', title: tr("S3-совместимое"), text: tr("MinIO, Ceph RGW, другие облака") },
  { id: 'gdrive', title: tr("Google Диск"), text: tr("Папка на Google Диске, вход по коду (OAuth)") },
  { id: 'yadisk', title: tr("Яндекс Диск"), text: tr("Папка приложения на Яндекс Диске, вход по коду") },
  { id: 'webdav', title: 'Nextcloud / WebDAV', text: tr("Nextcloud, ownCloud, любой сервер WebDAV") },
]
const CLASSES: Record<string, [string, string][]> = {
  yandex: [['', tr("стандартное")], ['COLD', tr("холодное (COLD)")], ['ICE', tr("ледяное (ICE)")]],
  aws: [['', 'STANDARD'], ['STANDARD_IA', 'STANDARD_IA'], ['INTELLIGENT_TIERING', 'INTELLIGENT_TIERING'], ['GLACIER_IR', tr("GLACIER_IR (мгновенный доступ)")]],
  s3: [['', tr("по умолчанию")]],
}
const AGENT_POLICY = (bucket: string, prefix: string) => JSON.stringify({ Version: '2012-10-17', Statement: [
  { Sid: 'HopeBackupAgentList', Effect: 'Allow', Action: ['s3:ListBucket'], Resource: [`arn:aws:s3:::${bucket || 'BUCKET'}`] },
  { Sid: 'HopeBackupAgentReadWrite', Effect: 'Allow', Action: ['s3:GetObject', 's3:PutObject', 's3:PutObjectRetention'], Resource: [`arn:aws:s3:::${bucket || 'BUCKET'}/${prefix ? prefix + '/' : ''}*`] },
] }, null, 2)

/** Время набора образов 20261104T063129Z → 04.11.2026, 06:31 (UTC → местное). */
const setTime = (n: string) => { const m = n.match(/^(\d{4})(\d{2})(\d{2})T(\d{2})(\d{2})(\d{2})Z$/); return m ? fmtTime(`${m[1]}-${m[2]}-${m[3]}T${m[4]}:${m[5]}:${m[6]}Z`) : n }

export function SitesPage() {
  const toast = useToast()
  const perms = usePerms()
  const { data, error, reload } = useApi<Site[]>('/api/sites', [], 15000)
  const { data: conns } = useApi<Connector[]>('/api/connectors')
  const [edit, setEdit] = useState<Site | 'new' | null>(null)
  const [tests, setTests] = useState<Record<string, SiteTest | { error: string }>>({})
  const [busy, setBusy] = useState('')
  const [job, setJob] = useState<string | null>(null)
  const [remove, setRemove] = useState<Site | null>(null)
  const [fo, setFo] = useState<{ from: Site; to: Site } | null>(null)
  const connName = (id: string) => conns?.find(c => c.id === id)?.name || id
  const siteName = (id: string) => data?.find(x => x.id === id)?.name || id
  const { data: insts } = useApi<Instance[]>('/api/instances')
  const instName = (id: string) => insts?.find(x => x.id === id)?.name || id
  const sync = async (s: Site) => {
    setBusy('sync:' + s.id)
    try { const r = await post<{ job_id: string }>(`/api/sites/${encodeURIComponent(s.id)}/replicate`); setJob(r.job_id) } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doFailover = async () => {
    const x = fo!; setFo(null)
    try { const r = await post<{ instances: number }>(`/api/sites/${encodeURIComponent(x.from.id)}/failover`, { to: x.to.id }); toast.ok(trf("Экземпляров переведено на «{0}»: {1}", x.to.name, r.instances)); reload() } catch (e) { toast.err(e) }
  }

  const test = async (s: Site) => {
    setBusy('test:' + s.id)
    try { const r = await post<SiteTest>(`/api/sites/${encodeURIComponent(s.id)}/test`); setTests(t => ({ ...t, [s.id]: r })); reload(); r.ok ? toast.ok(trf("«{0}»: площадка доступна", s.name)) : toast.err(trf("«{0}»: площадка недоступна", s.name)) }
    catch (e: any) { setTests(t => ({ ...t, [s.id]: { error: e.message } })); toast.err(e) } finally { setBusy('') }
  }
  // Площадки, которые реплицируются на эту (она для них принимающая).
  const sourcesOf = (s: Site) => (data || []).filter(x => x.replicas.includes(s.id))
  const init = async (s: Site) => {
    setBusy('init:' + s.id)
    try { const r = await post<{ job_id: string }>(`/api/sites/${encodeURIComponent(s.id)}/init`); setJob(r.job_id) } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const doRemove = async () => {
    const s = remove!; setRemove(null)
    try { await del(`/api/sites/${encodeURIComponent(s.id)}`); toast.ok(trf("Площадка «{0}» удалена", s.name)); reload() } catch (e) { toast.err(e) }
  }

  return (
    <div>
      <PageHeader title={tr("Площадки")} icon={<Server size={22} />}
        subtitle={tr("Где хранятся копии: узлы HopeBackup или бакеты Yandex Object Storage, AWS S3, MinIO. Данные шифруются паролем репозитория до отправки; площадка видит только шифротекст.")}
        actions={<>
          <RefreshButton onClick={reload} />
          {perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить площадку")}</Button>}
        </>} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : data.length === 0 ? (
        <Card><Empty icon={<Server size={28} />} title={tr("Площадок пока нет — добавьте узел хранения или бакет")}
          text={tr("Площадка — узел хранения HopeBackup или бакет S3 (Yandex Cloud, AWS, MinIO). Понадобятся адрес и ключи доступа, а также пароль шифрования репозитория.")}
          action={perms.edit && <Button kind="primary" icon={<Plus size={16} />} onClick={() => setEdit('new')}>{tr("Добавить площадку")}</Button>} /></Card>
      ) : (
        <div className="grid grid-cols-1 xl:grid-cols-2 gap-6">
          {data.map(s => {
            const t = tests[s.id]
            return (
              <Card key={s.id} title={s.name} icon={s.kind === 's3' || s.kind === 'cloud' ? <Cloud size={18} className="text-violet-600" /> : s.kind === 'disk' ? <HardDrive size={18} className="text-emerald-600" /> : <Server size={18} className="text-blue-600" />}
                actions={<>
                  {t && ('error' in t ? <Badge tone="red">{tr("недоступна")}</Badge> : t.ok ? <Badge tone="green">{tr("доступна")}{t.via ? ` · ${t.via}` : ''}</Badge> : <Badge tone="red">{tr("недоступна")}</Badge>)}
                  {perms.edit && <IconButton title={tr("Изменить")} onClick={() => setEdit(s)}><Pencil size={16} /></IconButton>}
                  {perms.edit && <IconButton title={tr("Удалить")} danger onClick={() => setRemove(s)}><Trash2 size={16} /></IconButton>}
                </>}>
                {s.kind === 'disk' && s.disk ? <>
                  <KV k={tr("Каталог на сервере")} mono>{s.disk.path}</KV>
                  <KV k={tr("Место на диске")}><DiskBar total={s.disk.total} used={s.disk.used} /></KV>
                  <KV k={tr("Адрес для агентов")} mono>{s.url}</KV>
                </> : s.kind === 'cloud' && s.cloud ? <>
                  <KV k={tr("Облако")}>{KINDS.find(k => k.id === s.cloud!.provider)?.title || s.cloud.provider}{s.cloud.account ? <span className="text-slate-500 ml-2">· {s.cloud.account}</span> : null}</KV>
                  <KV k={tr("Папка")} mono>{s.url}</KV>
                  <KV k={tr("Доступ")}>{s.cloud.connected ? <Badge tone="green">{tr("подключено")}</Badge> : <Badge tone="red">{tr("не подключено")}</Badge>}
                    {s.cloud.provider === 'yadisk' && s.cloud.token_expires > 0 && <span className="text-xs text-slate-500 ml-2">{tr("токен до")} {fmtTime(new Date(s.cloud.token_expires * 1000).toISOString())}{s.cloud.refresh_token ? tr(", продлевается автоматически") : ''}</span>}</KV>
                  {t && !('error' in t) && t.cloud && t.cloud.total ? <KV k={tr("Место в облаке")}><DiskBar total={t.cloud.total || 0} used={t.cloud.used || 0} /></KV> : null}
                </> : s.kind === 's3' && s.s3 ? <>
                  <KV k={tr("Хранилище")}>{KINDS.find(k => k.id === s.s3!.provider)?.title || 'S3'}<span className="text-slate-400 font-mono text-xs ml-2">{s.s3.endpoint} · {s.s3.region}</span></KV>
                  <KV k={tr("Бакет")} mono>{s.url}</KV>
                  <KV k="Object Lock">{s.s3.lock_mode ? trf("{0} · {1} дн.", s.s3.lock_mode, s.s3.lock_days) : <span className="text-slate-500">{tr("не используется")}</span>}{s.s3.storage_class ? <span className="text-slate-400 ml-2">{tr("· класс")} {s.s3.storage_class}</span> : null}</KV>
                </> : <>
                  <KV k="URL" mono>{s.url}</KV>
                  <KV k={tr("Место на диске")}>{s.node_disk ? <><DiskBar total={s.node_disk.total} used={s.node_disk.used} /><div className="text-[11px] text-slate-400 mt-0.5">{tr("на")} {fmtTime(s.node_disk.time)}  {tr("— обновляется при проверке и репликации")}</div></>
                    : <span className="text-slate-500 text-sm">{tr("неизвестно — нажмите «Проверить»")}{' '}<span className="text-slate-400">{tr("(узлу нужна версия 0.8.20+)")}</span></span>}</KV>
                  <KV k={tr("Отпечаток сертификата")} mono>{s.fingerprint || <span className="text-amber-700 font-sans">{tr("не задан")}</span>}</KV>
                  {s.node_disk?.images?.sets?.length ? <KV k={tr("Образы qcow2")}>
                    <div className="space-y-0.5">
                      {s.node_disk.images.sets.filter(x => x.complete).map(x => <div key={x.instance + x.set} className="text-sm">
                        <span className="font-medium">{instName(x.instance)}</span> · {setTime(x.set)} · {x.files.map(f => `${f.name} (${fmtBytes(f.used)})`).join(', ')}
                      </div>)}
                      {s.node_disk.images.sets.some(x => !x.complete) && <div className="text-xs text-amber-700">{tr("идёт приём набора:")} {s.node_disk.images.sets.filter(x => !x.complete).map(x => `${instName(x.instance)} · ${setTime(x.set)}`).join(', ')}</div>}
                      <div className="text-[11px] text-slate-400 font-mono break-all">{tr("на площадке:")} {s.node_disk.images.root}</div>
                    </div>
                  </KV> : null}
                </>}
                <KV k={tr("Репозиторий")} mono>{s.repo}</KV>
                <KV k={tr("Коннекторы")}>{s.connector_ids.length ? <ol className="list-decimal ml-4">{s.connector_ids.map((c, i) => <li key={c}>{connName(c)}{i === 0 && s.connector_ids.length > 1 ? <span className="text-slate-400">  {tr("— основной")}</span> : ''}</li>)}</ol> : <span className="text-slate-500">{tr("напрямую по URL")}</span>}</KV>
                <KV k={tr("Сжатие")}>{COMPRESSION.find(([v]) => v === s.compression)?.[1] || String(s.compression)}</KV>
                {sourcesOf(s).length > 0 && <KV k={tr("Принимает копии с")}>{sourcesOf(s).map(x => x.name).join(', ')}<div className="text-xs text-slate-500">{tr("репозиторий создаёт и пополняет репликация")}</div></KV>}
                <KV k={tr("Принимающие площадки")}>{s.replicas.length === 0 ? <span className="text-slate-500">{tr("нет")}</span> : (
                  <ul className="space-y-1">{s.replication.map(r => (
                    <li key={r.site} className="text-sm">
                      <span className="inline-flex items-center gap-1.5"><CopyIcon size={13} className="text-slate-400" />{siteName(r.site)}</span>
                      <span className="text-xs ml-2">{r.state?.running ? <span className="text-blue-700">{tr("синхронизируется с")} {fmtTime(r.state.started)}{r.state.progress ? ` · ${r.state.progress}` : ''}</span>
                        : !r.state || r.state.ok === null ? <span className="text-slate-400">{tr("ещё не синхронизировалась")}</span> : r.state.ok
                        ? <span className="text-emerald-700">{tr("синхронизирована")} {fmtTime(r.state.finished)}</span>
                        : <span className="text-red-700">{tr("ошибка")} {fmtTime(r.state.finished)}: {r.state.error}</span>}</span>
                      {perms.edit && <button className="ml-2 text-xs text-blue-600 hover:underline inline-flex items-center gap-1" onClick={() => setFo({ from: s, to: data.find(x => x.id === r.site)! })}><ArrowRightLeft size={12} />{tr("перевести экземпляры сюда")}</button>}
                    </li>
                  ))}<li className="text-xs text-slate-400">{tr("догоняет каждые")} {s.replication_interval_min}  {tr("мин и после каждой копии")}</li></ul>
                )}</KV>
                <KV k={tr("Секреты")}>
                  <span className="flex flex-wrap gap-1.5">
                    <Badge tone={s.has_token ? 'green' : 'red'}>{s.kind === 's3' ? tr("ключ агентов") : s.kind === 'cloud' ? tr("доступ") : tr("токен")} {s.has_token ? tr("задан") : tr("нет")}</Badge>
                    <Badge tone={s.has_password ? 'green' : 'red'}>{tr("пароль")} {s.has_password ? tr("задан") : tr("нет")}</Badge>
                    {s.kind !== 'cloud' && <Badge tone={s.has_prune_token ? 'blue' : 'slate'}>{s.kind === 's3' ? tr("ключ очистки") : tr("токен очистки")} {s.has_prune_token ? tr("задан") : tr("нет")}</Badge>}
                  </span>
                </KV>
                <KV k={tr("Добавлена")}>{fmtTime(s.created)}</KV>
                {t && 'error' in t && <div className="mt-3"><Alert tone="error">{t.error}</Alert></div>}
                {t && !('error' in t) && t.ok && !t.repo_exists && <div className="mt-3">{sourcesOf(s).length
                  ? <Alert tone="info">{tr("Репозиторий «")}{s.repo}{tr("» создаст первая репликация с")} {sourcesOf(s).map(x => `«${x.name}»`).join(', ')}  {tr("— нажмите у источника «Синхронизировать сейчас». Вручную создавать его не нужно: у копии должен быть тот же ключ, что у исходного.")}</Alert>
                  : <Alert tone="warn">{tr("Репозиторий «")}{s.repo}{tr("» на площадке ещё не создан — нажмите «Создать репозиторий».")}</Alert>}</div>}
                {t && !('error' in t) && t.stats && <RepoStatsView stats={t.stats} />}
                <div className="flex flex-wrap gap-2 mt-4">
                  <Button icon={<PlugZap size={16} />} loading={busy === 'test:' + s.id} onClick={() => test(s)}>{tr("Проверить")}</Button>
                  {perms.edit && sourcesOf(s).length === 0 && <Button icon={<Database size={16} />} loading={busy === 'init:' + s.id} onClick={() => init(s)}>{tr("Создать репозиторий")}</Button>}
                  {perms.edit && s.replicas.length > 0 && <Button icon={<RefreshCw size={16} />} loading={busy === 'sync:' + s.id} onClick={() => sync(s)}>{tr("Синхронизировать сейчас")}</Button>}
                </div>
              </Card>
            )
          })}
        </div>
      )}
      {edit && <SiteModal site={edit === 'new' ? null : edit} sites={data || []} conns={conns || []} onClose={() => setEdit(null)} onSaved={() => { setEdit(null); reload() }} />}
      <Confirm open={!!fo} danger yes={tr("Перевести")} title={tr("Перевести экземпляры на принимающую площадку?")} onNo={() => setFo(null)} onYes={doFailover}
        text={<>{tr("Все экземпляры площадки «")}<b>{fo?.from.name}</b>{tr("» будут писать копии на «")}<b>{fo?.to.name}</b>{tr("» — там уже есть реплицированные копии и журнал. «")}{fo?.from.name}{tr("» станет принимающей для «")}{fo?.to.name}{tr("» и догонит изменения, когда вернётся.")}</>} />
      <JobLogModal id={job} onClose={() => setJob(null)} />
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить площадку?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Площадка «")}<b>{remove?.name}</b>{tr("» будет удалена из сервера управления. Данные на узле хранения не удаляются, но экземпляры, которые на неё пишут, перестанут делать копии.")}</>} />
    </div>
  )
}

function DiskBar({ total, used }: { total: number; used: number }) {
  if (!total) return <span className="text-slate-500">{tr("нет данных")}</span>
  const pct = Math.min(100, Math.round(used * 100 / total))
  return (
    <div className="w-full max-w-xs">
      <div className="h-2 rounded-full bg-slate-100 overflow-hidden"><div className={`h-full ${pct > 90 ? 'bg-red-500' : pct > 75 ? 'bg-amber-500' : 'bg-emerald-500'}`} style={{ width: `${pct}%` }} /></div>
      <div className="text-xs text-slate-500 mt-1">{tr("свободно")} {fmtBytes(total - used)}  {tr("из")} {fmtBytes(total)}  {tr("· занято")} {pct}%</div>
    </div>
  )
}

/** Вход в облако по коду устройства: сервер получает код, администратор подтверждает его на странице Google/Яндекса. */
function OAuthConnect({ provider, clientId, clientSecret, siteId, endpoint, onDone, connected }: { provider: 'gdrive' | 'yadisk'; clientId: string; clientSecret: string; siteId?: string; endpoint: string; onDone: (flow: string) => void; connected: boolean }) {
  const toast = useToast()
  const [st, setSt] = useState<{ flow: string; user_code: string; verification_url: string } | null>(null)
  const [status, setStatus] = useState<'idle' | 'pending' | 'done' | 'error'>('idle')
  const [err, setErr] = useState('')
  const timer = useRef<number | null>(null)
  useEffect(() => () => { if (timer.current) window.clearInterval(timer.current) }, [])
  const start = async () => {
    if (!clientId.trim()) return toast.err(tr("Укажите Client ID приложения OAuth"))
    try {
      const r = await post<{ flow: string; user_code: string; verification_url: string }>('/api/sites/oauth/start', { provider, client_id: clientId.trim(), client_secret: clientSecret, endpoint, site_id: siteId || '' })
      setSt(r); setStatus('pending'); setErr('')
      if (timer.current) window.clearInterval(timer.current)
      timer.current = window.setInterval(async () => {
        try {
          const x = await get<{ status: string; error: string }>(`/api/sites/oauth/${encodeURIComponent(r.flow)}`)
          if (x.status === 'done') { window.clearInterval(timer.current!); setStatus('done'); onDone(r.flow); toast.ok(tr("Доступ разрешён — сохраните площадку")) }
          if (x.status === 'error') { window.clearInterval(timer.current!); setStatus('error'); setErr(x.error) }
        } catch (e: any) { window.clearInterval(timer.current!); setStatus('error'); setErr(e.message) }
      }, 2000)
    } catch (e) { toast.err(e) }
  }
  return (
    <div className="rounded-xl border border-slate-200 p-3 space-y-2">
      <div className="flex items-center gap-3">
        <Button icon={<KeyRound size={16} />} onClick={start}>{connected || status === 'done' ? tr("Подключить заново") : tr("Подключить")}</Button>
        {status === 'done' ? <span className="text-sm text-emerald-700 inline-flex items-center gap-1"><CheckCircle2 size={16} />{tr("доступ получен")}</span>
          : connected ? <span className="text-sm text-emerald-700 inline-flex items-center gap-1"><CheckCircle2 size={16} />{tr("подключено")}</span>
          : <span className="text-sm text-slate-500">{tr("ещё не подключено")}</span>}
      </div>
      {st && status === 'pending' && (
        <div className="text-sm text-slate-700">
          
          {tr("Откройте")} <a className="text-blue-600 hover:underline inline-flex items-center gap-1" href={st.verification_url} target="_blank" rel="noreferrer">{st.verification_url}<ExternalLink size={12} /></a>{tr(", войдите в учётную запись")} {provider === 'gdrive' ? 'Google' : tr("Яндекса")}  {tr("и введите код:")}
          <div className="font-mono text-2xl tracking-widest font-semibold text-slate-900 my-2">{st.user_code}</div>
          <span className="text-xs text-slate-500 inline-flex items-center gap-2"><Spinner />{tr("ждём подтверждения…")}</span>
        </div>
      )}
      {status === 'error' && <Alert tone="error">{err}</Alert>}
    </div>
  )
}

function RepoStatsView({ stats }: { stats: NonNullable<SiteTest['stats']> }) {
  const cell = (k: string, v: React.ReactNode) => <div className="bg-slate-50 rounded-lg px-3 py-2"><div className="text-[11px] uppercase tracking-wide text-slate-500">{k}</div><div className="font-semibold text-slate-900">{v}</div></div>
  return (
    <div className="grid grid-cols-3 gap-2 mt-3">
      {cell(tr("Снапшотов"), stats.snapshots)}
      {cell(tr("Pack-файлов"), `${stats.packs} · ${fmtBytes(stats.pack_bytes)}`)}
      {cell(tr("Блоков"), stats.chunks)}
      {cell(tr("Объектов"), `${stats.objects} · ${fmtBytes(stats.object_bytes)}`)}
    </div>
  )
}

function SiteModal({ site, sites, conns, onClose, onSaved }: { site: Site | null; sites: Site[]; conns: Connector[]; onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const [kind, setKind] = useState<Kind>(site ? kindOf(site) : 'node')
  const [f, setF] = useState<Form>(site ? { ...EMPTY, name: site.name, url: site.kind === 's3' ? 'https://' : site.url, fingerprint: site.fingerprint, repo: site.repo, connector_ids: [...site.connector_ids], compression: site.compression } : EMPTY)
  const [cl, setCl] = useState<CloudSite>(site?.cloud ? { ...CLOUD_EMPTY, ...site.cloud, password: '', client_secret: '', refresh_token: '', access_token: '' } : CLOUD_EMPTY)
  const [disk, setDisk] = useState({ path: site?.disk?.path || '/mnt/data/hopebackup', public_url: site?.kind === 'disk' ? site.url.replace(/\/node\/[^/]+$/, '') : window.location.origin })
  const setC = (p: Partial<CloudSite>) => setCl({ ...cl, ...p })
  const [b, setB] = useState<S3Site>(site?.s3 ? { ...S3_EMPTY, ...site.s3, secret_key: '', admin_secret_key: '' } : S3_EMPTY)
  const [busy, setBusy] = useState('')
  const [add, setAdd] = useState('')
  const [replicas, setReplicas] = useState<string[]>(site?.replicas || [])
  const [interval, setIntervalMin] = useState<number>(site?.replication_interval_min || 10)
  const set = (p: Partial<Form>) => setF({ ...f, ...p })
  const setS = (p: Partial<S3Site>) => setB({ ...b, ...p })
  const isS3 = S3_KINDS.includes(kind)
  const isCloud = CLOUD_KINDS.includes(kind)
  const isNode = kind === 'node'
  const pickKind = (k: Kind) => { setKind(k); if (CLOUD_KINDS.includes(k)) setCl({ ...cl, provider: k as CloudSite['provider'], folder: k === 'yadisk' ? 'app:/HopeBackup' : 'HopeBackup' }); if (S3_KINDS.includes(k)) setB({ ...b, provider: k as S3Site['provider'], endpoint: k === 's3' ? b.endpoint : '', region: k === 'yandex' ? 'ru-central1' : k === 'aws' ? (b.region && b.region !== 'ru-central1' ? b.region : 'eu-central-1') : b.region, path_style: k === 's3', storage_class: '' }) }
  const probe = async () => {
    if (!/^https:\/\/.+/.test(f.url.trim())) return toast.err(tr("Укажите адрес вида https://узел:порт"))
    setBusy('probe')
    try { const r = await post<{ fingerprint: string }>('/api/sites/probe', { url: f.url.trim() }); set({ fingerprint: r.fingerprint }); toast.info(tr("Отпечаток получен — сверьте его с отпечатком на узле хранения")) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const move = (i: number, d: number) => { const l = [...f.connector_ids]; const j = i + d; if (j < 0 || j >= l.length) return; [l[i], l[j]] = [l[j], l[i]]; set({ connector_ids: l }) }
  const save = async () => {
    if (!f.name.trim()) return toast.err(tr("Укажите имя площадки"))
    if (isNode && (!f.url.trim() || f.url.trim() === 'https://')) return toast.err(tr("Укажите URL узла хранения"))
    if (isNode && !site && !f.token) return toast.err(tr("Укажите токен доступа"))
    if (kind === 'disk' && !site && !disk.path.startsWith('/')) return toast.err(tr("Каталог площадки — абсолютный путь, например /mnt/data/hopebackup"))
    if (kind === 'webdav' && (!/^https?:\/\/.+/.test(cl.url.trim()) || !cl.username.trim())) return toast.err(tr("Укажите адрес WebDAV и логин"))
    if (kind === 'webdav' && !site && !cl.password) return toast.err(tr("Укажите пароль приложения"))
    if (kind === 'gdrive' && !site?.cloud?.connected && !cl.oauth_flow) return toast.err(tr("Нажмите «Подключить» и разрешите доступ к Google Диску"))
    if (kind === 'yadisk' && !site?.cloud?.connected && !cl.oauth_flow && !cl.access_token) return toast.err(tr("Подключите Яндекс Диск или вставьте токен OAuth"))
    if (isS3 && !b.bucket.trim()) return toast.err(tr("Укажите бакет"))
    if (isS3 && kind === 's3' && !/^https?:\/\/.+/.test(b.endpoint.trim())) return toast.err(tr("Укажите адрес S3: https://s3.example:9000"))
    if (isS3 && (!b.access_key.trim() || (!site && !b.secret_key))) return toast.err(tr("Укажите ключи агентов (access key и secret key)"))
    if (!site && !f.password) return toast.err(tr("Укажите пароль репозитория"))
    const body: any = { name: f.name.trim(), kind: isS3 ? 's3' : isCloud ? 'cloud' : kind === 'disk' ? 'disk' : 'node', url: isNode ? f.url.trim() : '', fingerprint: f.fingerprint.trim(), repo: f.repo.trim() || 'main', token: f.token, password: f.password, connector_ids: f.connector_ids, compression: f.compression, replicas, replication_interval_min: Number(interval) || 10 }
    if (isS3) body.s3 = { ...b, provider: kind, secret_key: b.secret_key || (site ? SECRET_MASK : ''), admin_secret_key: b.admin_secret_key || (site?.s3?.admin_secret_key ? SECRET_MASK : ''), lock_days: Number(b.lock_days) || 0 }
    if (kind === 'disk') { body.disk_path = disk.path.trim(); body.public_url = disk.public_url.trim() }
    if (isCloud) {
      const keepS = (v: string, had: boolean) => v || (site && had ? SECRET_MASK : '')
      body.cloud = { ...cl, provider: kind, password: keepS(cl.password, !!site?.cloud?.password), client_secret: keepS(cl.client_secret, !!site?.cloud?.client_secret),
        refresh_token: keepS(cl.refresh_token, !!site?.cloud?.refresh_token), access_token: keepS(cl.access_token, !!site?.cloud?.access_token) }
    }
    // prune_token необязателен при создании; при изменении пустой — «не менять».
    if (isNode && (f.prune_token || site)) body.prune_token = f.prune_token
    setBusy('save')
    try {
      if (site) await put(`/api/sites/${encodeURIComponent(site.id)}`, body); else await post('/api/sites', body)
      toast.ok(site ? tr("Площадка сохранена") : tr("Площадка добавлена — проверьте связь и создайте репозиторий")); onSaved()
    } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const keep = site ? tr("задан — оставьте пустым, чтобы не менять") : ''
  const free = conns.filter(c => !f.connector_ids.includes(c.id) && !((isS3 || isCloud) && c.kind === 'ssh_tunnel'))
  return (
    <Modal open wide onClose={onClose} title={site ? trf("Площадка «{0}»", site.name) : tr("Новая площадка")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy === 'save'} onClick={save}>{site ? tr("Сохранить") : tr("Добавить")}</Button></>}>
      <div className="space-y-4">
        <div className="grid sm:grid-cols-2 gap-2" role="radiogroup">
          {KINDS.map(k => <ChoiceCard key={k.id} selected={kind === k.id} disabled={!!site && (S3_KINDS.includes(kindOf(site)) ? !S3_KINDS.includes(k.id) : k.id !== kindOf(site))} onClick={() => pickKind(k.id)} title={k.title} text={k.text} />)}
        </div>
        <Field label={tr("Имя")}><Input value={f.name} onChange={e => set({ name: e.target.value })} placeholder={isS3 ? 'Yandex Cloud · ru-central1' : kind === 'disk' ? tr("Большой диск сервера") : isCloud ? KINDS.find(k => k.id === kind)?.title : tr("ЦОД-2")} autoFocus /></Field>
        {kind === 'disk' ? <>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Каталог на сервере управления")} hint={tr("Например, большой диск, который вы примонтировали: /mnt/data/hopebackup. Каталог создаётся, если его нет.")}>
              <Input className="font-mono text-[13px]" value={disk.path} disabled={!!site} onChange={e => setDisk({ ...disk, path: e.target.value })} />
            </Field>
            <Field label={tr("Адрес сервера для агентов")} hint={tr("Как агенты обращаются к этому серверу (порт панели, тот же сертификат)")}>
              <Input className="font-mono text-[13px]" value={disk.public_url} onChange={e => setDisk({ ...disk, public_url: e.target.value })} placeholder="https://10.0.0.5:4443" />
            </Field>
          </div>
          <Field label={tr("Имя репозитория")}><Input className="font-mono text-[13px]" value={f.repo} onChange={e => set({ repo: e.target.value })} placeholder="main" /></Field>
          <Alert tone="info" title={tr("Как это работает")}>{tr("Сервер управления сам становится узлом хранения для этого каталога: агенты пишут копии через его порт по HTTPS, токены и отпечаток подставляются автоматически. Копии на том же сервере, что и управление, не защищают от потери сервера — добавьте принимающую площадку (облако или узел на другой площадке).")}</Alert>
        </> : isCloud ? <>
          {kind === 'webdav' ? <>
            <Field label={tr("Адрес WebDAV")} hint={tr("Nextcloud: https://облако/remote.php/dav/files/ЛОГИН · ownCloud: https://облако/remote.php/webdav · Яндекс Диск: https://webdav.yandex.ru")}>
              <Input className="font-mono text-[13px]" value={cl.url} onChange={e => setC({ url: e.target.value })} placeholder="https://cloud.example.ru/remote.php/dav/files/backup" />
            </Field>
            <div className="grid sm:grid-cols-3 gap-4">
              <Field label={tr("Логин")}><Input value={cl.username} onChange={e => setC({ username: e.target.value })} /></Field>
              <Field label={tr("Пароль приложения")} hint={tr("Nextcloud: Настройки → Безопасность → «Создать пароль приложения»")}><Input type="password" autoComplete="new-password" value={cl.password} onChange={e => setC({ password: e.target.value })} placeholder={site?.cloud?.password ? keep : ''} /></Field>
              <Field label={tr("Папка")}><Input className="font-mono text-[13px]" value={cl.folder} onChange={e => setC({ folder: e.target.value })} placeholder="HopeBackup" /></Field>
            </div>
            <Field label={tr("Отпечаток сертификата (необязательно)")} hint={tr("Для сервера с самоподписанным сертификатом; пусто — проверка по системным CA")}><Input className="font-mono text-xs" value={cl.fingerprint} onChange={e => setC({ fingerprint: e.target.value })} /></Field>
          </> : <>
            <div className="grid sm:grid-cols-3 gap-4">
              <Field label="Client ID" hint={kind === 'gdrive' ? tr("Google Cloud → APIs & Services → Credentials → OAuth client ID, тип «TVs and Limited Input devices»; включите Google Drive API") : tr("oauth.yandex.ru → «Создать приложение», права «Яндекс Диск REST API: доступ к папке приложения»")}>
                <Input className="font-mono text-[13px]" value={cl.client_id} onChange={e => setC({ client_id: e.target.value })} />
              </Field>
              <Field label="Client secret"><Input type="password" autoComplete="new-password" value={cl.client_secret} onChange={e => setC({ client_secret: e.target.value })} placeholder={site?.cloud?.client_secret ? keep : ''} /></Field>
              <Field label={tr("Папка")} hint={kind === 'yadisk' ? tr("app:/… — папка приложения; disk:/… — любая (нужен доступ ко всему диску)") : tr("Создаётся в корне диска")}>
                <Input className="font-mono text-[13px]" value={cl.folder} onChange={e => setC({ folder: e.target.value })} placeholder={kind === 'yadisk' ? 'app:/HopeBackup' : 'HopeBackup'} />
              </Field>
            </div>
            <OAuthConnect provider={kind as 'gdrive' | 'yadisk'} clientId={cl.client_id} clientSecret={cl.client_secret} siteId={site?.id} endpoint={cl.endpoint} connected={!!site?.cloud?.connected} onDone={flow => setC({ oauth_flow: flow })} />
            {kind === 'yadisk' && <Field label={tr("…или токен OAuth")} hint={tr("Если приложение уже выдало токен (например, отладочный на oauth.yandex.ru): токен живёт год, без токена обновления его нужно будет заменить вручную")}>
              <Input type="password" autoComplete="new-password" value={cl.access_token} onChange={e => setC({ access_token: e.target.value })} placeholder={site?.cloud?.access_token ? keep : ''} />
            </Field>}
          </>}
          <Field label={tr("Имя репозитория")}><Input className="font-mono text-[13px]" value={f.repo} onChange={e => set({ repo: e.target.value })} placeholder="main" /></Field>
          <Alert tone="warn" title={tr("Защита копий в облачном диске")}>{tr("У облачных дисков нет блокировки от удаления (Object Lock): доступ агентов позволяет и удалять файлы в папке площадки. Данные зашифрованы паролем репозитория; для защиты от удаления держите копии ещё и на площадке с Object Lock или узле HopeBackup (принимающая площадка).")}</Alert>
        </> : isNode ? <>
          <Field label={tr("URL узла хранения")} hint={tr("Адрес API узла хранения HopeBackup.")}>
            <div className="flex gap-2">
              <Input className="font-mono text-[13px]" value={f.url} onChange={e => set({ url: e.target.value })} placeholder="https://storage-01:8443" />
              <Button icon={<Fingerprint size={16} />} loading={busy === 'probe'} onClick={probe}>{tr("Получить отпечаток")}</Button>
            </div>
          </Field>
          <Field label={tr("Отпечаток сертификата (SHA-256)")} hint={tr("Сервер доверяет только этому сертификату узла. Сверьте с выводом на самом узле хранения.")}>
            <Input className="font-mono text-xs" value={f.fingerprint} onChange={e => set({ fingerprint: e.target.value })} placeholder={tr("нажмите «Получить отпечаток»")} />
          </Field>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Токен доступа")}><Input type="password" autoComplete="new-password" value={f.token} onChange={e => set({ token: e.target.value })} placeholder={site?.has_token ? keep : ''} /></Field>
            <Field label={tr("Имя репозитория")}><Input className="font-mono text-[13px]" value={f.repo} onChange={e => set({ repo: e.target.value })} placeholder="main" /></Field>
          </div>
        </> : <>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label={tr("Бакет")}><Input className="font-mono text-[13px]" value={b.bucket} onChange={e => setS({ bucket: e.target.value })} placeholder="company-backups" /></Field>
            <Field label={tr("Префикс")} hint={tr("Каталог внутри бакета")}><Input className="font-mono text-[13px]" value={b.prefix} onChange={e => setS({ prefix: e.target.value })} /></Field>
            <Field label={tr("Репозиторий")}><Input className="font-mono text-[13px]" value={f.repo} onChange={e => set({ repo: e.target.value })} placeholder="main" /></Field>
          </div>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label={tr("Адрес S3")} className="sm:col-span-2" hint={kind === 'yandex' ? tr("По умолчанию https://storage.yandexcloud.net") : kind === 'aws' ? tr("По умолчанию https://s3.<регион>.amazonaws.com") : tr("Например, https://minio.dc2.example:9000")}>
              <Input className="font-mono text-[13px]" value={b.endpoint} onChange={e => setS({ endpoint: e.target.value })} placeholder={kind === 'yandex' ? 'https://storage.yandexcloud.net' : kind === 'aws' ? `https://s3.${b.region || 'eu-central-1'}.amazonaws.com` : 'https://'} />
            </Field>
            <Field label={tr("Регион")}><Input className="font-mono text-[13px]" value={b.region} onChange={e => setS({ region: e.target.value })} placeholder={kind === 'yandex' ? 'ru-central1' : 'eu-central-1'} /></Field>
          </div>
          <div className="grid sm:grid-cols-2 gap-4">
            <Field label={tr("Ключ агентов: access key")} hint={tr("Чтение и запись, без права удаления")}><Input className="font-mono text-[13px]" value={b.access_key} onChange={e => setS({ access_key: e.target.value })} /></Field>
            <Field label={tr("Ключ агентов: secret key")}><Input type="password" autoComplete="new-password" value={b.secret_key} onChange={e => setS({ secret_key: e.target.value })} placeholder={site?.has_token ? keep : ''} /></Field>
            <Field label={tr("Ключ очистки: access key")} hint={tr("Для сервера: удаление по политике хранения (необязательно)")}><Input className="font-mono text-[13px]" value={b.admin_access_key} onChange={e => setS({ admin_access_key: e.target.value })} /></Field>
            <Field label={tr("Ключ очистки: secret key")}><Input type="password" autoComplete="new-password" value={b.admin_secret_key} onChange={e => setS({ admin_secret_key: e.target.value })} placeholder={site?.has_prune_token ? keep : tr("не задан")} /></Field>
          </div>
          <div className="grid sm:grid-cols-3 gap-4">
            <Field label="Object Lock" hint={tr("WORM: объекты нельзя удалить до срока — даже ключом администратора (COMPLIANCE). Бакет создаётся с включённой блокировкой.")}>
              <Select value={b.lock_mode} onChange={e => setS({ lock_mode: e.target.value })}>
                <option value="">{tr("не использовать")}</option><option value="GOVERNANCE">GOVERNANCE</option><option value="COMPLIANCE">COMPLIANCE</option>
              </Select>
            </Field>
            <Field label={tr("Срок хранения, дней")}><Input type="number" disabled={!b.lock_mode} value={b.lock_days} onChange={e => setS({ lock_days: parseInt(e.target.value, 10) || 0 })} /></Field>
            <Field label={tr("Класс хранения")}>
              <Select value={b.storage_class} onChange={e => setS({ storage_class: e.target.value })}>
                {(CLASSES[kind] || CLASSES.s3).map(([v, l]) => <option key={v} value={v}>{l}</option>)}
              </Select>
            </Field>
          </div>
          {kind === 's3' && <div className="grid sm:grid-cols-2 gap-4 items-end">
            <Field label={tr("Отпечаток сертификата (необязательно)")} hint={tr("Для MinIO с самоподписанным сертификатом; пусто — проверка по системным CA")}><Input className="font-mono text-xs" value={b.fingerprint} onChange={e => setS({ fingerprint: e.target.value })} /></Field>
            <Toggle checked={b.path_style} onChange={v => setS({ path_style: v })} label={tr("Адресация по пути")} hint={tr("https://адрес/бакет/… — для MinIO и Ceph")} />
          </div>}
          <Alert tone="info" title={tr("Права ключей")}>
            {kind === 'yandex'
              ? <>{tr("Агентам — сервисный аккаунт с ролью")} <b>storage.uploader</b>  {tr("на бакет (чтение и запись без удаления), серверу для очистки —")} <b>storage.editor</b>{tr(". Object Lock включается в настройках бакета («Блокировка версий объектов»).")}</>
              : <>{tr("Агентам — только чтение, запись и список (без s3:DeleteObject), серверу для очистки — дополнительно s3:DeleteObject. Политика для ключа агентов:")}</>}
            {kind !== 'yandex' && <Code className="mt-2 max-h-48 overflow-y-auto text-xs">{AGENT_POLICY(b.bucket, b.prefix)}</Code>}
          </Alert>
        </>}
        <Field label={tr("Пароль репозитория")} hint={tr("Ключ шифрования копий. На площадку он не передаётся.")}>
          <Input type="password" autoComplete="new-password" value={f.password} onChange={e => set({ password: e.target.value })} placeholder={site?.has_password ? keep : ''} />
        </Field>
        <Alert tone="warn" title={tr("Сохраните пароль в надёжном месте")}>{tr("Без пароля репозитория копии невозможно расшифровать: потеря пароля означает потерю всех копий на этой площадке. Восстановить его нельзя.")}</Alert>
        {isNode && <Field label={tr("Токен очистки (необязательно)")} hint={tr("Отдельный токен с правом удаления старых точек. Без него очистка по хранению не выполняется.")}>
          <Input type="password" autoComplete="new-password" value={f.prune_token} onChange={e => set({ prune_token: e.target.value })} placeholder={site?.has_prune_token ? keep : tr("не задан")} />
        </Field>}
        <Field label={tr("Коннекторы по порядку")} hint={isS3 || isCloud ? tr("Пусто — по интернету напрямую. Для частного адреса хранилища — VPN-коннектор.") : tr("Первый — основной, следующие — если предыдущий недоступен. Пусто — подключаться напрямую по URL.")}>
          <div className="space-y-1.5">
            {f.connector_ids.map((id, i) => (
              <div key={id} className="flex items-center gap-2 px-3 py-1.5 rounded-lg border border-slate-200 bg-slate-50 text-sm">
                <span className="w-5 text-slate-400 font-mono">{i + 1}.</span>
                <span className="flex-1">{conns.find(c => c.id === id)?.name || id}</span>
                <IconButton title={tr("Выше")} disabled={i === 0} onClick={() => move(i, -1)}><ArrowUp size={14} /></IconButton>
                <IconButton title={tr("Ниже")} disabled={i === f.connector_ids.length - 1} onClick={() => move(i, 1)}><ArrowDown size={14} /></IconButton>
                <IconButton title={tr("Убрать")} danger onClick={() => set({ connector_ids: f.connector_ids.filter(x => x !== id) })}><X size={14} /></IconButton>
              </div>
            ))}
            {free.length > 0 ? (
              <div className="flex gap-2">
                <Select value={add} onChange={e => setAdd(e.target.value)}>
                  <option value="">{tr("— добавить коннектор —")}</option>
                  {free.map(c => <option key={c.id} value={c.id}>{c.name} ({c.kind})</option>)}
                </Select>
                <Button icon={<Plus size={16} />} disabled={!add} onClick={() => { set({ connector_ids: [...f.connector_ids, add] }); setAdd('') }}>{tr("Добавить")}</Button>
              </div>
            ) : conns.length === 0 && <div className="text-xs text-slate-500">{tr("Коннекторов нет — площадка доступна напрямую. Туннели и VPN настраиваются в разделе «Коннекторы».")}</div>}
          </div>
        </Field>
        <Field label={tr("Уровень сжатия (zstd)")}>
          <Select value={f.compression} onChange={e => set({ compression: parseInt(e.target.value, 10) })}>
            {COMPRESSION.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
            {!COMPRESSION.some(([v]) => v === f.compression) && <option value={f.compression}>{f.compression}</option>}
          </Select>
        </Field>
        <Field label={tr("Принимающие площадки")} hint={tr("Всё, что попадает на эту площадку, копируется на них как есть (уже зашифровано): после каждой копии и периодически — для журнала WAL. Удаления не повторяются: у принимающей площадки своя очистка.")}>
          <div className="space-y-2">
            {sites.filter(x => x.id !== site?.id).length === 0 ? <div className="text-xs text-slate-500">{tr("Других площадок нет — добавьте принимающую площадку (узел или бакет) и вернитесь сюда.")}</div> : (
              <div className="flex flex-wrap gap-2">{sites.filter(x => x.id !== site?.id).map(x => (
                <label key={x.id} className="inline-flex items-center gap-2 px-3 py-1.5 rounded-lg border border-slate-200 text-sm cursor-pointer hover:bg-slate-50">
                  <input type="checkbox" checked={replicas.includes(x.id)} onChange={e => setReplicas(e.target.checked ? [...replicas, x.id] : replicas.filter(r => r !== x.id))} />{x.name}
                </label>
              ))}</div>
            )}
            {replicas.length > 0 && <div className="flex items-center gap-2 text-sm text-slate-600">{tr("догонять каждые")} <Input type="number" className="!w-20" value={interval} onChange={e => setIntervalMin(parseInt(e.target.value, 10) || 1)} />  {tr("мин")}</div>}
          </div>
        </Field>
      </div>
    </Modal>
  )
}
