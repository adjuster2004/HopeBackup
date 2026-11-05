// «Обновления» (как в HopeDFS и HopeInventory): новые версии «Ковчега» — онлайн по манифесту или из архива
// выпуска для закрытого контура. Установку выполняет копия прежней программы вне службы: замена, перезапуск,
// проверка ответа новой версии, иначе — автоматический откат. Агенты обновляются отсюда же.
import { useCallback, useEffect, useRef, useState } from 'react'
import { AlertTriangle, ArrowUpCircle, Bot, CheckCircle2, ChevronRight, CircleDashed, Download, FileArchive, Globe, Loader2, PackageCheck, RefreshCw, RotateCcw, Save, Server, Trash2, Upload, XCircle } from 'lucide-react'
import { api, auth, fmtBytes, fmtTime, netError, post, put } from '../api'
import { Alert, Badge, Button, Card, Field, Input, Modal, PageHeader, Select, Spinner, StatusDot, Table, Td, Toggle, cx, useApi, useToast, RefreshButton } from '../components/ui'
import type { Cluster } from '../types'
import { JobLogModal } from '../components/JobLog'
import { LOCALE, tr, trf } from '../i18n'

const PHASES: Record<string, string> = {
  downloading: tr("Загрузка"), verifying: tr("Проверка пакета"), staging: tr("Подготовка"), handoff: tr("Передача установщику"), applying: tr("Замена программы"),
  restarting: tr("Перезапуск"), rolling_back: tr("Откат"), done: tr("Готово"), failed: tr("Ошибка"), rolled_back: tr("Откат выполнен"), rollback_failed: tr("Ошибка отката"),
}
const PCT: Record<string, number> = { verifying: 60, staging: 70, handoff: 75, applying: 82, restarting: 92, rolling_back: 95, done: 100 }
const v = (s?: string) => s ? (/^\d/.test(s) ? `v${s}` : s) : '—'
const cap = (s: string) => s ? s[0].toUpperCase() + s.slice(1) : s
type Msg = { kind: 'ok' | 'warn' | 'error'; text: string } | null
const TONE = { ok: 'success', warn: 'warn', error: 'error' } as const
const MIN_AGENT = '0.8.1'
const older = (a: string, b: string) => {
  const p = (x: string) => x.replace(/^v/, '').split(/[-+ ]/)[0].split('.').map(n => parseInt(n, 10) || 0)
  const [x, y] = [p(a), p(b)]
  for (let i = 0; i < 3; i++) { if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) < (y[i] || 0) }
  return false
}

export function UpdatesPage() {
  const [d, setD] = useState<any>(null)
  const [err, setErr] = useState('')
  const [lost, setLost] = useState(false)   // служба перезапускается
  const [msg, setMsg] = useState<Msg>(null)
  const [busy, setBusy] = useState('')
  const [ask, setAsk] = useState(false)
  const [log, setLog] = useState<string | null>(null)
  const [tab, setTab] = useState<'offline' | 'online'>(() => { try { return localStorage.getItem('hopebackup_upd_src') === 'online' ? 'online' : 'offline' } catch { return 'offline' } })
  const [cfg, setCfg] = useState({ channel: 'stable', source_url: '', auto_check: false })
  const [file, setFile] = useState<File | null>(null)
  const [ver, setVer] = useState('')
  const fileRef = useRef<HTMLInputElement>(null)
  const [up, setUp] = useState<{ sent: number; size: number; stage: string; retry?: number } | null>(null)
  const [paused, setPaused] = useState(false)
  const runningRef = useRef(false)
  const startedVer = useRef<string>('')
  useEffect(() => { try { localStorage.setItem('hopebackup_upd_src', tab) } catch { /* */ } }, [tab])

  const load = useCallback(async (quiet = false) => {
    try {
      const r = await api<any>('/api/updates')
      setD(r); setErr(''); setLost(false)
      if (!startedVer.current) startedVer.current = r.current_version
      if (!quiet) setCfg({ channel: r.channel || 'stable', source_url: r.source_url || '', auto_check: !!r.auto_check })
    } catch (e: any) {
      if (runningRef.current || quiet) setLost(true)
      else setErr(e.message)
    }
  }, [])
  useEffect(() => { load() }, [load])
  const running = !!d?.job?.running || lost
  const agentsBusy = !!d?.agents?.some((a: any) => a.last_job && (a.last_job.status === 'running' || a.last_job.status === 'queued') || a.update?.running)
  useEffect(() => { runningRef.current = running }, [running])
  useEffect(() => {
    const id = setInterval(() => load(true), running ? 1500 : agentsBusy ? 3000 : 20000)
    return () => clearInterval(id)
  }, [running, agentsBusy, load])

  const act = async (what: string, fn: () => Promise<any>) => {
    setBusy(what); setMsg(null)
    try { await fn() } catch (e: any) { setMsg({ kind: 'error', text: netError(e).message }) } finally { setBusy('') }
  }
  const check = () => act('check', async () => {
    const r = await post<any>('/api/updates/check')
    if (r.kind === 'error' || (r.kind === 'warn' && !r.latest_version)) setMsg({ kind: r.kind, text: r.note })
    else if (r.update_available) setMsg({ kind: 'ok', text: trf("Доступна новая версия {0}", v(r.latest_version)) })
    else setMsg({ kind: 'ok', text: tr("Установлена последняя версия.") })
    load(true)
  })
  const saveCfg = () => act('cfg', async () => {
    await put('/api/updates/config', { channel: cfg.channel, source_url: cfg.source_url.trim(), auto_check: cfg.auto_check })
    setMsg({ kind: 'ok', text: tr("Настройки источника сохранены. Нажмите «Проверить обновления».") }); load(true)
  })
  // Загрузка частями по 4 МБ: каждая часть повторяется при обрыве, прерванную загрузку можно продолжить —
  // уже переданное остаётся на сервере. В конце сервер сверяет SHA-256 всего файла.
  const upload = () => act('upload', async () => {
    if (!file) throw new Error(tr("Выберите файл пакета"))
    setPaused(false)
    setUp({ sent: 0, size: file.size, stage: tr("Подсчёт контрольной суммы…") })
    let sha = ''
    try { if (crypto?.subtle) sha = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', await file.arrayBuffer()))).map(b => b.toString(16).padStart(2, '0')).join('') } catch { /* без суммы — сервер проверит сборки внутри архива */ }
    const key = `hopebackup_upload:${file.name}:${file.size}:${file.lastModified}`
    let resume = ''
    try { resume = localStorage.getItem(key) || '' } catch { /* */ }
    const s = await post<any>('/api/updates/upload/start', { filename: file.name, size: file.size, sha256: sha, version: ver.trim(), resume })
    try { localStorage.setItem(key, s.id) } catch { /* */ }
    let sent: number = s.received
    const chunk: number = s.chunk || 4 * 1024 * 1024
    while (sent < file.size) {
      setUp({ sent, size: file.size, stage: s.resumed && sent > 0 ? tr("Продолжение загрузки") : tr("Загрузка") })
      const part = file.slice(sent, Math.min(sent + chunk, file.size))
      let attempt = 0
      for (;;) {
        try {
          const res = await fetch(`/api/updates/upload/${encodeURIComponent(s.id)}?offset=${sent}`, { method: 'PUT', headers: { 'Content-Type': 'application/octet-stream', Authorization: `Bearer ${auth.token()}` }, body: part })
          const r = await res.json().catch(() => ({}))
          if (!res.ok) throw Object.assign(new Error(r.error || `HTTP ${res.status}`), { fatal: res.status !== 502 && res.status !== 503 && res.status !== 504 })
          sent = r.received
          break
        } catch (e: any) {
          if (e.fatal) throw e
          attempt++
          if (attempt > 6) { setPaused(true); throw new Error(trf("Связь с сервером прервалась на {0}% ({1} из {2}). Уже переданное сохранено на сервере — нажмите «Продолжить загрузку».", Math.floor(sent * 100 / file.size), fmtBytes(sent), fmtBytes(file.size))) }
          const wait = Math.min(30, 2 ** attempt)
          setUp({ sent, size: file.size, stage: trf("Связь прервалась — повтор через {0} с (попытка {1} из 6)", wait, attempt), retry: attempt })
          await new Promise(res => setTimeout(res, wait * 1000))
        }
      }
    }
    setUp({ sent, size: file.size, stage: tr("Проверка пакета на сервере…") })
    let r: any
    try { r = await post<any>(`/api/updates/upload/${encodeURIComponent(s.id)}/finish`) } catch (e) { throw netError(e) }
    try { localStorage.removeItem(key) } catch { /* */ }
    const text = trf("Пакет {0} ({1}) проверен. Версия {2} готова к установке{3}.", r.filename, fmtBytes(r.size), v(r.version), r.kit?.length ? trf("; сборки для агентов: {0}", r.kit.join(', ')) : '')
    setMsg({ kind: r.note ? 'warn' : 'ok', text: r.note ? `${text} ${r.note}` : text })
    setFile(null); setVer(''); setUp(null); if (fileRef.current) fileRef.current.value = ''
    load(true)
  }).finally(() => setUp(u => u && u.stage.startsWith('Проверка') ? null : u))
  const install = () => { setAsk(false); act('apply', async () => { await post('/api/updates/apply'); runningRef.current = true; load(true) }) }
  const discard = () => act('discard', async () => { await post('/api/updates/discard'); load(true) })
  const dismiss = () => act('dismiss', async () => { await post('/api/updates/dismiss'); load(true) })
  const updAgents = (ids: string[]) => act('agents' + ids.join(','), async () => {
    const r = await post<any>('/api/updates/agents', { ids })
    const errs = (r.jobs || []).filter((x: any) => x.error)
    const ok = (r.jobs || []).filter((x: any) => x.job_id)
    if (ok.length === 1 && !errs.length) setLog(ok[0].job_id)
    setMsg(errs.length ? { kind: ok.length ? 'warn' : 'error', text: errs.map((x: any) => `${x.name}: ${x.error}`).join('; ') } : { kind: 'ok', text: trf("Обновление запущено: {0}. Агент скачает свою сборку с сервера, проверит её и перезапустится; версия обновится, когда он снова выйдет на связь.", ok.map((x: any) => x.name).join(', ')) })
    load(true)
  })

  const head = (
    <PageHeader title={tr("Обновления")} icon={<ArrowUpCircle size={22} />}
      subtitle={tr("Новые версии «Ковчега»: онлайн по манифесту или из архива выпуска для закрытого контура. Сервер управления обновляется здесь, агенты — отсюда же, из комплекта сборок сервера.")}
      actions={<RefreshButton onClick={() => load()} />} />
  )
  if (err && !d) return <div>{head}<Alert tone="error">{err}</Alert></div>
  if (!d) return <div>{head}<Spinner /></div>

  const job = d.job
  const canInstall = !!d.target_version && d.updater_installed && !running
  const pct = (j: any) => j.phase === 'downloading' ? Math.max(3, j.progress) : PCT[j.phase] ?? 100
  const notesTone = d.notes_kind === 'error' ? 'text-red-600' : d.notes_kind === 'warn' ? 'text-amber-700' : 'text-slate-500'
  const ph = job?.phase || ''
  const done = !!job && job.finished && !job.failed
  const failed = !!job && job.finished && job.failed
  const rolled = failed && ph === 'rolled_back'
  const reloadUi = done && startedVer.current && startedVer.current !== d.current_version
  const agents: any[] = d.agents || []
  // Загружен пакет новее сервера: агенты обновятся только после сервера.
  const serverBehind = !!d.target_version && !d.target_is_same && !d.target_is_downgrade && agents.length > 0
  const outdated = agents.filter(a => a.outdated && a.online && !older(a.version, MIN_AGENT))

  return (
    <div className="space-y-6">
      {head}
      {lost && <div className="p-3 rounded-xl text-sm border flex items-center gap-2 bg-blue-50 border-blue-200 text-blue-700"><Loader2 size={16} className="animate-spin shrink-0" />{tr("Служба перезапускается — ждём, пока она снова ответит…")}</div>}
      {reloadUi && <Alert tone="success">{tr("Сервер работает на версии")} {v(d.current_version)}{tr(". Обновите страницу (Ctrl+F5), чтобы загрузилась новая версия панели.")}</Alert>}
      {msg && <Alert tone={TONE[msg.kind]}>{msg.text}</Alert>}
      {d.role && d.role !== 'active' && <Alert tone="info">{tr("Это принимающий узел управления. Его обновляют с активного узла («Обновления» → «Узлы управления») или здесь, загрузкой архива выпуска; агентов обновляет активный узел.")}</Alert>}

      <Card>
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="flex flex-wrap items-center gap-10">
            <div>
              <div className="text-xs text-slate-400 uppercase font-semibold mb-1">{tr("Текущая версия")}</div>
              <div className="text-xl font-bold text-slate-800 font-mono">{v(d.current_version)}</div>
              <div className="text-[11px] text-slate-400 font-mono">{tr("сборка")} {d.build} · {d.target}</div>
            </div>
            <div>
              <div className="text-xs text-slate-400 uppercase font-semibold mb-1">{tr("Доступная версия")}</div>
              <div className={cx('text-xl font-bold font-mono', d.update_available ? 'text-emerald-600' : 'text-slate-500')}>{d.latest_version ? v(d.latest_version) : '—'}</div>
            </div>
            <div>
              <div className="text-xs text-slate-400 uppercase font-semibold mb-1">{tr("Статус")}</div>
              {d.update_available
                ? <span className="inline-flex items-center gap-1.5 text-sm font-semibold text-emerald-600"><ArrowUpCircle size={16} />{tr("Доступно обновление")}</span>
                : d.latest_version
                  ? <span className="inline-flex items-center gap-1.5 text-sm font-medium text-slate-500"><CheckCircle2 size={16} />{tr("Актуальная версия")}</span>
                  : <span className="inline-flex items-center gap-1.5 text-sm font-medium text-slate-400"><CircleDashed size={16} />{tr("Не проверялось")}</span>}
            </div>
          </div>
          <Button icon={<RefreshCw size={16} />} loading={busy === 'check'} disabled={running} onClick={check}>{tr("Проверить обновления")}</Button>
        </div>
        {d.notes && !(msg && msg.text === d.notes) && (
          <p className={cx('text-xs mt-4 pt-4 border-t border-slate-100 whitespace-pre-wrap flex items-start gap-1.5', notesTone)}>
            {d.notes_kind !== 'info' && <AlertTriangle size={14} className="shrink-0 mt-px" />}{d.notes}
          </p>
        )}
        {d.last_checked > 0 && <p className="text-[11px] text-slate-400 mt-2">{tr("Последняя проверка:")} {new Date(d.last_checked * 1000).toLocaleString(LOCALE)}</p>}
      </Card>

      <Card title={<span className="flex items-center gap-2"><PackageCheck size={18} className="text-blue-600" />{tr("Обновление сервера управления")}</span>}>
        <div className="space-y-4">
          {!d.updater_installed && <Alert tone="warn">{cap(d.updater_note)}</Alert>}
          <div className="rounded-xl border border-slate-200 p-4 flex items-center gap-4">
            <div className="w-9 h-9 rounded-lg flex items-center justify-center border bg-blue-50 border-blue-200 text-blue-600"><Server size={16} /></div>
            <div className="flex-1 min-w-0">
              <div className="font-medium text-slate-800">{tr("Сервер")} {d.node}</div>
              <div className="text-xs text-slate-500">{tr("Сохраняется текущая программа и комплект сборок для агентов, ставится новая версия, служба перезапускается. Если она не поднимется или панель не ответит новой версией за 90 секунд — автоматический откат.")}</div>
              {job && <div className="mt-2 h-1.5 bg-slate-100 rounded-full overflow-hidden">
                <div className={cx('h-full rounded-full transition-all', done ? 'bg-emerald-500' : rolled ? 'bg-amber-400' : failed ? 'bg-red-400' : 'bg-blue-500')} style={{ width: `${pct(job)}%` }} /></div>}
            </div>
            {job && <span className={cx('shrink-0 inline-flex items-center gap-1.5 text-xs font-medium', done ? 'text-emerald-600' : rolled ? 'text-amber-700' : failed ? 'text-red-600' : 'text-blue-600')}>
              {done ? <CheckCircle2 size={14} /> : failed ? (rolled ? <RotateCcw size={14} /> : <XCircle size={14} />) : <Loader2 size={14} className="animate-spin" />}
              {PHASES[ph] || ph}{ph === 'downloading' && job.running ? ` ${job.progress}%` : ''}</span>}
          </div>

          {job && job.finished && (
            <div className={cx('flex items-center justify-between gap-3 p-3 border rounded-xl text-sm',
              job.failed ? (job.phase === 'rolled_back' ? 'bg-amber-50 border-amber-200 text-amber-800' : 'bg-red-50 border-red-200 text-red-700') : 'bg-emerald-50 border-emerald-200 text-emerald-700')}>
              <span className="flex items-center gap-2">{job.failed ? <AlertTriangle size={16} className="shrink-0" /> : <CheckCircle2 size={16} className="shrink-0" />}
                {job.message ? cap(job.message) : job.failed ? tr("Установка не выполнена.") : trf("Обновление до {0} завершено.", v(job.target_version))}</span>
              <button onClick={dismiss} className="text-sm font-medium hover:underline shrink-0" title={tr("Убрать итог прошлой установки")}>{tr("Скрыть")}</button>
            </div>
          )}
          {/* Итог прошлой установки не прячет кнопку: загруженный новый пакет ставится сразу. */}
          {(!job || !job.finished || (!!d.target_version && !d.target_is_same)) && (
            <div className="flex flex-wrap items-center justify-between gap-3">
              <p className="text-xs text-slate-500 max-w-3xl">
                {running ? tr("Идёт установка. Во время перезапуска панель будет недоступна несколько секунд.")
                  : d.target_version
                    ? d.target_is_downgrade ? <span className="text-amber-700">{tr("Будет установлена версия")} {v(d.target_version)}  {tr("— это откат на более старую версию.")}</span>
                      : d.target_is_same ? trf("Будет переустановлена текущая версия {0}.", v(d.target_version))
                        : trf("Будет установлена версия {0}.", v(d.target_version))
                    : tr("Нет версии для установки: проверьте обновления или загрузите архив выпуска.")}
                {d.running_jobs > 0 && !running && <span className="block text-amber-700 mt-1">{tr("Сейчас выполняется заданий:")} {d.running_jobs}{tr(". Задания на агентах продолжатся, их итог дойдёт после перезапуска; задания самого сервера (восстановление по SSH, опись) прервутся — лучше дождаться их окончания.")}</span>}
              </p>
              <Button kind="primary" icon={<Download size={16} />} loading={busy === 'apply' || running} disabled={!canInstall} onClick={() => setAsk(true)}>
                
                {tr("Установить")}{d.target_version ? ` ${v(d.target_version)}` : ''}</Button>
            </div>
          )}
        </div>
      </Card>

      <Card padded={false}>
        <div className="flex gap-1 px-4 pt-3 border-b border-slate-100">
          {([['offline', <FileArchive size={16} />, tr("Из архива (закрытый контур)")], ['online', <Globe size={16} />, tr("Онлайн-источник")]] as const).map(([id, icon, label]) => (
            <button key={id} type="button" onClick={() => setTab(id)}
              className={cx('px-4 py-2.5 text-sm font-semibold rounded-t-lg border-b-2 -mb-px transition-colors flex items-center gap-2', tab === id ? 'border-blue-600 text-blue-700' : 'border-transparent text-slate-400 hover:text-slate-600')}>
              {icon}{label}</button>
          ))}
        </div>
        <div className="p-6 space-y-4">
          {tab === 'offline' && <>
            <p className="text-xs text-slate-500">{tr("Для серверов без доступа в интернет: загрузите архив выпуска")} <span className="font-mono">{tr("hopebackup-<версия>-linux.tar.gz")}</span>  {tr("из комплекта поставки (папка")} <span className="font-mono">bin/release</span>{tr("). Из него берётся сборка для этого сервера, контрольные суммы всех сборок сверяются, комплект для агентов обновляется вместе с сервером. Можно загрузить и одну программу")} <span className="font-mono">hopebackup.gz</span>  {tr("— тогда агенты останутся на прежней версии.")}</p>
            {d.offline_package && (
              <div className="flex items-center gap-3 p-3 bg-indigo-50 border border-indigo-100 rounded-xl text-sm">
                <PackageCheck size={18} className="text-indigo-600 shrink-0" />
                <div className="flex-1 min-w-0">
                  <div className="font-medium text-slate-800">{tr("Готов к установке:")} <span className="font-mono">{d.offline_package.filename}</span> · {v(d.offline_package.version)}</div>
                  <div className="text-xs text-slate-500">{fmtBytes(d.offline_package.size)} · {d.offline_package.source === 'online' ? tr("скачан с сервера обновлений") : tr("загружен вручную")} · {new Date(d.offline_package.uploaded_at * 1000).toLocaleString(LOCALE)}
                    {d.offline_package.kit?.length ? trf(" · сборки: {0}", d.offline_package.kit.join(', ')) : tr(" · только программа сервера")}</div>
                  <div className="text-[11px] text-slate-500 font-mono break-all mt-0.5" title={tr("Сверьте с контрольной суммой от поставщика (SHA256SUMS)")}>SHA-256: {d.offline_package.file_sha256}</div>
                </div>
                <button onClick={discard} disabled={running} title={tr("Удалить пакет")} className="p-2 text-slate-400 hover:text-red-600 hover:bg-white rounded-lg disabled:opacity-40"><Trash2 size={16} /></button>
              </div>
            )}
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
              <Field label={tr("Файл пакета")} className="md:col-span-2">
                <input ref={fileRef} type="file" accept=".gz,.tgz,application/gzip,application/octet-stream" onChange={e => setFile(e.target.files?.[0] || null)}
                  className="w-full text-sm border border-slate-300 rounded-lg px-3 py-2 file:mr-3 file:py-1 file:px-3 file:rounded-md file:border-0 file:bg-slate-100 file:text-slate-600 file:text-sm" />
              </Field>
              <Field label={tr("Версия (необязательно)")}><Input value={ver} onChange={e => setVer(e.target.value)} spellCheck={false} className="font-mono" placeholder={tr("определится сама")} /></Field>
            </div>
            {up && <div className="rounded-lg border border-slate-200 p-3">
              <div className="flex justify-between text-xs text-slate-600 mb-1.5"><span className={up.retry ? 'text-amber-700' : ''}>{up.stage}</span><span className="font-mono">{fmtBytes(up.sent)} / {fmtBytes(up.size)} · {Math.floor(up.sent * 100 / Math.max(1, up.size))}%</span></div>
              <div className="h-2 bg-slate-100 rounded-full overflow-hidden"><div className={cx('h-full rounded-full transition-all', up.retry ? 'bg-amber-500' : 'bg-blue-600')} style={{ width: `${Math.floor(up.sent * 100 / Math.max(1, up.size))}%` }} /></div>
            </div>}
            <div className="flex justify-end"><Button icon={<Upload size={16} />} loading={busy === 'upload'} disabled={!file || running} onClick={upload}>{paused ? tr("Продолжить загрузку") : tr("Загрузить и проверить")}</Button></div>
          </>}
          {tab === 'online' && <>
            <p className="text-xs text-slate-500">{tr("Сервер обновлений публикует манифест — JSON с последней версией, ссылкой на архив выпуска и его контрольной суммой SHA-256. Пакет без контрольной суммы установлен не будет. Подойдёт и внутренний веб-сервер организации.")}</p>
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
              <Field label={tr("Канал")}><Select value={cfg.channel} onChange={e => setCfg({ ...cfg, channel: e.target.value })}>
                <option value="stable">{tr("Стабильный (stable)")}</option><option value="beta">{tr("Бета (beta)")}</option></Select></Field>
              <Field label={tr("Адрес манифеста")} className="md:col-span-2"
                hint={cfg.source_url.trim().toLowerCase().startsWith('http://') ? <span className="text-amber-700">{tr("Без HTTPS манифест и пакет можно подменить по пути. Используйте http только во внутренней доверенной сети.")}</span> : undefined}>
                <Input value={cfg.source_url} onChange={e => setCfg({ ...cfg, source_url: e.target.value })} spellCheck={false} className="font-mono" placeholder="https://updates.example.ru/hopebackup/manifest.json" />
              </Field>
            </div>
            <Toggle checked={cfg.auto_check} onChange={x => setCfg({ ...cfg, auto_check: x })} label={tr("Проверять обновления автоматически")} hint={tr("Раз в сутки; о новой версии — запись в журнале службы и письмо (событие «Обновления» в разделе «Почта и уведомления»).")} />
            <div className="flex justify-end"><Button icon={<Save size={16} />} loading={busy === 'cfg'} onClick={saveCfg}>{tr("Сохранить")}</Button></div>
            <details className="text-[11px] text-slate-500">
              <summary className="cursor-pointer select-none flex items-center gap-1 w-fit"><ChevronRight size={12} />{tr("Формат манифеста")}</summary>
              <pre className="mt-2 p-3 bg-slate-50 border border-slate-200 rounded-lg font-mono text-[11px] text-slate-600 overflow-x-auto">{tr("{\n  \"stable\": { \"version\": \"0.9.0\", \"url\": \"hopebackup-0.9.0-linux.tar.gz\",\n              \"sha256\": \"<sha256 архива>\", \"notes\": \"Что нового\" },\n  \"beta\":   { \"version\": \"0.10.0-beta\", \"url\": \"...\", \"sha256\": \"...\", \"notes\": \"...\" }\n}")}</pre>
              <p className="mt-1">{tr("Можно и без каналов — один объект")} {'{ "version", "url", "sha256", "notes" }'}{tr(". Относительная ссылка считается от адреса манифеста. Контрольная сумма:")} <span className="font-mono">sha256sum hopebackup-0.9.0-linux.tar.gz</span>  {tr("(она же — в файле SHA256SUMS выпуска).")}</p>
            </details>
          </>}
        </div>
      </Card>

      {d.role === 'active' && <PeersCard current={d.current_version} serverRunning={running} />}

      <Card padded={false} title={<span className="flex items-center gap-2"><Bot size={18} className="text-blue-600" />{tr("Агенты")}</span>}
        actions={outdated.length > 0 && <Button kind="primary" className="!py-1.5 text-xs" icon={<Download size={14} />} loading={busy.startsWith('agents') && busy === 'agents'} disabled={running} onClick={() => updAgents([])}>{tr("Обновить устаревшие (")}{outdated.length})</Button>}>
        {serverBehind && <div className="mx-4 mt-3"><Alert tone="info">{tr("Агенты обновляются до версии сервера управления (")}{v(d.current_version)}{tr("). Сначала установите")} {v(d.target_version)}  {tr("на сервер (кнопка «Установить» выше) — после этого у агентов здесь появится «Обновить».")}</Alert></div>}
        <div className="px-4 pt-3 pb-1 text-xs text-slate-500">{tr("Агент скачивает сборку своей архитектуры с сервера управления (комплект:")} {(d.kit || []).join(', ') || tr("нет")}{tr("), сверяет SHA-256, проверяет запуском и перезапускается. Если новая версия не выйдет на связь с сервером за 2 минуты — агент сам вернётся на прежнюю. Агенты до версии")} {MIN_AGENT}  {tr("обновляются скриптом установки или кнопкой «Установить агента» в разделе «Серверы и ВМ».")}</div>
        {agents.length === 0 ? <div className="p-6 text-sm text-slate-400">{tr("Агентов нет.")}</div> : (
          <Table head={[tr("Агент"), tr("Сборка"), tr("Версия"), tr("Обновление"), '']}>
            {agents.map(a => {
              const u = a.update || {}
              const lj = a.last_job
              const legacy = !a.version || older(a.version, MIN_AGENT)
              const jobRunning = lj && (lj.status === 'running' || lj.status === 'queued')
              return (
                <tr key={a.id} className="hover:bg-slate-50">
                  <Td><div className="flex items-center gap-2 font-medium text-slate-800"><StatusDot tone={a.online ? 'green' : 'red'} />{a.name}</div><div className="text-[11px] text-slate-400">{a.host}{a.os ? ` · ${a.os}` : ''}</div></Td>
                  <Td className="font-mono text-xs">{a.build}{!a.kit_has && <span className="block text-amber-700 font-sans">{tr("нет в комплекте")}</span>}</Td>
                  <Td>{a.version ? <Badge tone={a.outdated ? 'amber' : a.newer ? 'violet' : 'green'}>{v(a.version)}</Badge> : '—'}</Td>
                  <Td className="text-xs">
                    {jobRunning ? <span className="inline-flex items-center gap-1 text-blue-700"><Loader2 size={12} className="animate-spin" />{tr("загрузка и проверка")}</span>
                      : u.running ? <span className="inline-flex items-center gap-1 text-blue-700"><Loader2 size={12} className="animate-spin" />{PHASES[u.phase] || u.phase} → {v(u.target_version)}</span>
                      : u.finished ? <span className={u.failed ? (u.phase === 'rolled_back' ? 'text-amber-700' : 'text-red-700') : 'text-emerald-700'}>{u.failed ? cap(u.message || tr("не установлено")) : trf("{0} → {1} установлено", v(u.from_version), v(u.target_version))}</span>
                      : lj && lj.status === 'failed' ? <span className="text-red-700">{cap(lj.error || tr("ошибка"))}</span>
                      : <span className="text-slate-400">—</span>}
                    {lj && <button className="block text-blue-700 hover:underline" onClick={() => setLog(lj.id)}>{tr("журнал ·")} {fmtTime(lj.created)}</button>}
                  </Td>
                  <Td className="text-right whitespace-nowrap">
                    {a.outdated && !legacy && <Button className="!py-1 !px-2.5 text-xs" icon={<Download size={14} />} disabled={!a.online || running || jobRunning || !a.kit_has} loading={busy === 'agents' + a.id}
                      onClick={() => updAgents([a.id])}>{tr("До")} {v(d.current_version)}</Button>}
                    {a.outdated && legacy && <span className="text-[11px] text-amber-700">{tr("обновите скриптом")}</span>}
                  </Td>
                </tr>
              )
            })}
          </Table>
        )}
      </Card>

      <Modal open={ask} onClose={() => setAsk(false)} title={trf("Установить {0}?", v(d.target_version))}
        footer={<><Button onClick={() => setAsk(false)}>{tr("Отмена")}</Button><Button kind="primary" icon={<Download size={16} />} onClick={install}>{tr("Установить")}</Button></>}>
        <ul className="text-sm text-slate-600 space-y-2 list-disc pl-5">
          <li>{tr("Служба сервера управления будет перезапущена: панель и приём отчётов агентов прервутся на несколько секунд. Задания на агентах продолжатся — агенты сохранят итоги и отправят их после перезапуска.")}</li>
          <li>{tr("Текущая программа и комплект сборок сохраняются; если новая версия не запустится или панель не ответит, произойдёт автоматический откат.")}</li>
          <li>{tr("Настройки, площадки, пароли репозиториев, экземпляры и история заданий остаются на месте. Копии на площадках не затрагиваются.")}</li>
          <li>{tr("После установки обновите агентов в блоке «Агенты» ниже — по одному или все устаревшие сразу.")}</li>
        </ul>
      </Modal>
      <JobLogModal id={log} onClose={() => { setLog(null); load(true) }} />
    </div>
  )
}

/** Принимающие узлы управления: версия и обновление до версии этого (активного) узла. */
function PeersCard({ current, serverRunning }: { current: string; serverRunning: boolean }) {
  const toast = useToast()
  const { data: c, reload } = useApi<Cluster>('/api/cluster', [], 5000)
  const [busy, setBusy] = useState('')
  const peers = (c?.peers || []).filter(p => !p.self)
  if (!c || peers.length === 0) return null
  const upd = async (id: string, name: string) => {
    setBusy(id)
    try { await post(`/api/cluster/peers/${encodeURIComponent(id)}/update`); toast.ok(trf("«{0}»: пакет передаётся, узел обновится и перезапустится", name)); reload() } catch (e) { toast.err(e) } finally { setBusy('') }
  }
  return (
    <Card padded={false} title={<span className="flex items-center gap-2"><Server size={18} className="text-blue-600" />{tr("Узлы управления")}</span>}>
      <div className="px-4 pt-3 pb-1 text-xs text-slate-500">{tr("Принимающий узел обновляется до версии этого сервера (")}{v(current)}{tr("): сервер собирает архив выпуска из своей программы и комплекта сборок и передаёт его узлу (через коннектор узла, если он задан), подписав ключом кластера. Узел проверяет пакет как обычную загрузку, ставит его и перезапускается; при сбое — откат.")}</div>
      <Table head={[tr("Узел"), tr("Версия"), tr("Обновление"), '']}>
        {peers.map(p => {
          const u = p.update
          const behind = !p.version || older(p.version, current)
          return (
            <tr key={p.id} className="hover:bg-slate-50">
              <Td><div className="flex items-center gap-2 font-medium text-slate-800"><StatusDot tone={p.sync?.ok ? 'green' : 'red'} />{p.name}</div><div className="text-[11px] text-slate-400 font-mono">{p.url}</div></Td>
              <Td>{p.version ? <Badge tone={behind ? 'amber' : 'green'}>{v(p.version)}</Badge> : <span className="text-xs text-slate-400">{tr("неизвестна")}</span>}</Td>
              <Td className="text-xs">
                {u?.running ? <span className="inline-flex items-center gap-1 text-blue-700"><Loader2 size={12} className="animate-spin" />{tr("передача и установка")} {v(u.target)}</span>
                  : u?.ok === false ? <span className="text-red-700">{cap(u.error || tr("ошибка"))} · {fmtTime(u.time)}</span>
                  : u?.ok ? <span className="text-emerald-700">{tr("пакет")} {v(u.target)}  {tr("принят ·")} {fmtTime(u.time)}</span>
                  : <span className="text-slate-400">—</span>}
              </Td>
              <Td className="text-right whitespace-nowrap">
                {behind && <Button className="!py-1 !px-2.5 text-xs" icon={<Download size={14} />} disabled={serverRunning || !!u?.running} loading={busy === p.id} onClick={() => upd(p.id, p.name)}>{tr("До")} {v(current)}</Button>}
              </Td>
            </tr>
          )
        })}
      </Table>
    </Card>
  )
}
