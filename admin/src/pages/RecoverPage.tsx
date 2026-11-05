// Восстановление доступа к панели (как в HopeDFS): ссылка сброса пароля по почте и аварийный токен вендора.
import { useEffect, useMemo, useState } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { AlertTriangle, ArrowLeft, CheckCircle2, Copy, Cpu, Eye, EyeOff, KeyRound, Lock, Mail, Send, ShieldAlert, Ship } from 'lucide-react'
import { api } from '../api'
import { Alert, cx } from '../components/ui'
import { LOCALE, tr, trf } from '../i18n'

function Shell({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="relative min-h-screen flex items-center justify-center bg-gradient-to-br from-slate-900 via-slate-800 to-blue-950 p-4">
      <div className="w-full max-w-md bg-white rounded-2xl shadow-2xl overflow-hidden">
        <div className="bg-slate-950 px-8 py-7 flex flex-col items-center border-b border-slate-800">
          <div className="p-3 bg-blue-600/20 border border-blue-500/30 rounded-2xl text-blue-400 mb-3"><Ship size={28} /></div>
          <h1 className="text-xl font-bold text-white">{title}</h1>
          <p className="text-xs text-slate-400 mt-1">{tr("HopeBackup — резервное копирование")}</p>
        </div>
        {children}
      </div>
    </div>
  )
}

const inputCls = 'w-full pl-10 pr-4 py-2.5 bg-slate-50 border border-slate-200 rounded-xl text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/40 focus:border-blue-500'

function Back() {
  const nav = useNavigate()
  return <button type="button" onClick={() => nav('/login')} className="mt-4 w-full text-slate-500 hover:text-slate-800 text-sm font-medium inline-flex items-center justify-center gap-2"><ArrowLeft size={16} />{tr("Назад ко входу")}</button>
}

const Spin = () => <span className="w-5 h-5 border-2 border-white/30 border-t-white rounded-full animate-spin" />

export function RecoverPage() {
  const [params, setParams] = useSearchParams()
  const tab = params.get('mode') === 'emergency' ? 'emergency' : 'email'
  return (
    <Shell title={tr("Восстановление доступа")}>
      <div className="grid grid-cols-2 border-b border-slate-200">
        {([['email', <Mail size={16} />, tr("По почте")], ['emergency', <ShieldAlert size={16} />, tr("Аварийное")]] as const).map(([id, icon, label]) => (
          <button key={id} onClick={() => setParams(id === 'email' ? {} : { mode: id })}
            className={cx('py-3.5 text-sm font-semibold flex items-center justify-center gap-2 border-b-2 -mb-px transition-colors',
              tab === id ? (id === 'email' ? 'border-blue-600 text-blue-700' : 'border-amber-500 text-amber-700') : 'border-transparent text-slate-400 hover:text-slate-600')}>
            {icon}{label}
          </button>
        ))}
      </div>
      <div className="p-8">{tab === 'email' ? <ByEmail /> : <Emergency />}</div>
    </Shell>
  )
}

function ByEmail() {
  const [id, setId] = useState('')
  const [busy, setBusy] = useState(false)
  const [done, setDone] = useState(false)
  const [err, setErr] = useState('')
  const submit = async (e: React.FormEvent) => {
    e.preventDefault(); setBusy(true); setErr('')
    try { await api('/api/auth/recover-request', { method: 'POST', body: { identifier: id.trim() } }); setDone(true) } catch (e: any) { setErr(e.message) } finally { setBusy(false) }
  }
  if (done) return (
    <div className="text-center">
      <div className="inline-flex p-3 rounded-full bg-emerald-50 text-emerald-500 mb-4"><CheckCircle2 size={40} /></div>
      <h2 className="text-lg font-bold text-slate-800 mb-2">{tr("Запрос принят")}</h2>
      <p className="text-sm text-slate-500 leading-relaxed">{tr("Если такая учётная запись есть, ссылка для сброса пароля отправлена на её адрес почты. Если почта не настроена или адрес у учётной записи не указан — ссылка записана на сервере управления в файл")} <span className="font-mono text-xs">/var/lib/hopebackup/server/password_reset_link.txt</span>  {tr("(доступен только root). Ссылка действует 30 минут.")}</p>
      <Back />
    </div>
  )
  return (
    <form onSubmit={submit}>
      <p className="text-sm text-slate-500 mb-5 leading-relaxed">{tr("Введите имя учётной записи панели или её адрес почты. Мы отправим ссылку для смены пароля.")}</p>
      {err && <div className="mb-4"><Alert tone="error">{err}</Alert></div>}
      <div className="relative">
        <Mail size={18} className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400" />
        <input value={id} onChange={e => setId(e.target.value)} required autoFocus placeholder={tr("Имя учётной записи или e-mail")} className={inputCls} />
      </div>
      <button type="submit" disabled={busy || !id.trim()} className="w-full mt-6 bg-blue-600 text-white font-semibold py-2.5 rounded-xl hover:bg-blue-700 flex items-center justify-center gap-2 shadow-lg disabled:opacity-60">
        {busy ? <Spin /> : <><Send size={16} />{tr("Отправить ссылку")}</>}
      </button>
      <p className="text-xs text-slate-400 mt-4 leading-relaxed">{tr("Адрес почты учётной записи задаёт администратор в разделе «Пользователи», сервер почты — в разделе «Почта и уведомления».")}</p>
      <Back />
    </form>
  )
}

function decode(tok: string): any | null {
  try {
    const p = tok.replace(/\s+/g, '').split('.')
    if (p.length !== 3) return null
    const b = p[1].replace(/-/g, '+').replace(/_/g, '/')
    const json = decodeURIComponent(atob(b + '='.repeat((4 - b.length % 4) % 4)).split('').map(c => '%' + c.charCodeAt(0).toString(16).padStart(2, '0')).join(''))
    return JSON.parse(json)
  } catch { return null }
}

function Emergency() {
  const nav = useNavigate()
  const [info, setInfo] = useState<{ fingerprint: string; req: string; node: string } | null>(null)
  const [tok, setTok] = useState('')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  const [copied, setCopied] = useState(false)
  const [show, setShow] = useState(false)
  const [done, setDone] = useState<string | null>(null)
  useEffect(() => { api<any>('/api/auth/recovery-request').then(setInfo).catch(e => setErr(e.message)) }, [])
  const claims = useMemo(() => decode(tok), [tok])
  const now = Date.now() / 1000
  const warn = claims && info ? (
    claims.purpose !== 'admin-recovery' ? trf("Это не токен восстановления доступа (назначение «{0}»).", claims.purpose || '—')
    : String(claims.sub || '').toUpperCase() !== info.fingerprint.toUpperCase() ? trf("Токен выпущен для другого сервера: отпечаток {0}, у этого сервера {1}.", claims.sub, info.fingerprint)
    : claims.exp && claims.exp < now ? tr("Срок действия токена истёк — запросите новый у вендора.") : ''
  ) : ''
  const copy = async () => { try { await navigator.clipboard.writeText(info?.req || ''); setCopied(true); setTimeout(() => setCopied(false), 1500) } catch { /* буфер обмена недоступен */ } }
  const submit = async (e: React.FormEvent) => {
    e.preventDefault(); setBusy(true); setErr('')
    try { const r = await api<any>('/api/auth/recover-emergency', { method: 'POST', body: { token: tok.trim() } }); setDone(r.user || 'Administrator') } catch (e: any) { setErr(e.message) } finally { setBusy(false) }
  }
  if (done) return (
    <div className="text-center">
      <div className="inline-flex p-3 rounded-full bg-emerald-50 text-emerald-500 mb-4"><CheckCircle2 size={40} /></div>
      <h2 className="text-lg font-bold text-slate-800 mb-2">{tr("Доступ восстановлен")}</h2>
      <p className="text-sm text-slate-500 leading-relaxed">{tr("Пароль учётной записи")} <b>{done}</b>  {tr("задан из токена. Войдите с ним и сразу смените пароль в разделе «Пользователи».")}</p>
      <button onClick={() => nav('/login')} className="w-full mt-6 bg-blue-600 text-white font-semibold py-2.5 rounded-xl hover:bg-blue-700 flex items-center justify-center gap-2"><Lock size={16} />{tr("Ко входу")}</button>
    </div>
  )
  return (
    <form onSubmit={submit}>
      <p className="text-sm text-slate-500 mb-5 leading-relaxed">{tr("Для закрытого контура или если почты нет. Передайте код запроса вендору любым каналом и вставьте полученный токен — пароль учётной записи будет задан из токена.")}</p>
      <div className="text-sm font-semibold text-slate-700 mb-1.5 flex items-center gap-1.5"><Cpu size={15} />{tr("Код запроса (отправьте вендору)")}</div>
      <div className="relative">
        <pre className="bg-slate-900 text-emerald-400 text-xs font-mono rounded-xl p-4 pr-12 overflow-x-auto whitespace-pre">{info?.req || '…'}</pre>
        <button type="button" onClick={copy} title={tr("Копировать")} className="absolute top-2 right-2 p-1.5 rounded-lg bg-slate-800 text-slate-300 hover:text-white">{copied ? <CheckCircle2 size={15} className="text-emerald-400" /> : <Copy size={15} />}</button>
      </div>
      <p className="text-xs text-slate-400 mt-1.5">{tr("Отпечаток сервера:")} <span className="font-mono text-slate-600">{info?.fingerprint}</span></p>
      <div className="text-sm font-semibold text-slate-700 mt-5 mb-1.5 flex items-center gap-1.5"><KeyRound size={15} />{tr("Токен восстановления от вендора")}</div>
      <textarea value={tok} onChange={e => setTok(e.target.value)} rows={4} placeholder={tr("Вставьте сюда подписанный токен (JWT)…")}
        className="w-full px-3 py-2.5 bg-slate-50 border border-slate-200 rounded-xl text-xs font-mono focus:outline-none focus:ring-2 focus:ring-amber-500/40 focus:border-amber-500" />
      {claims && (
        <div className={cx('mt-3 rounded-xl border p-3 text-xs space-y-1', warn ? 'border-amber-300 bg-amber-50' : 'border-slate-200 bg-slate-50')}>
          <div>{tr("Сервер:")} <span className="font-mono">{claims.sub}</span></div>
          <div>{tr("Учётная запись:")} <b>{claims.user || 'Administrator'}</b></div>
          <div className="flex items-center gap-1">{tr("Новый пароль:")} <span className="font-mono font-bold">{claims.new_password ? (show ? claims.new_password : '••••••••') : '—'}</span>
            {claims.new_password && <button type="button" onClick={() => setShow(!show)} className="text-slate-400 hover:text-slate-700 ml-1">{show ? <EyeOff size={13} /> : <Eye size={13} />}</button>}</div>
          {claims.exp && <div>{tr("Действует до:")} {new Date(claims.exp * 1000).toLocaleString(LOCALE)}</div>}
          {warn && <div className="text-amber-700 flex items-start gap-1 pt-1"><AlertTriangle size={13} className="shrink-0 mt-0.5" />{warn}</div>}
        </div>
      )}
      {err && <div className="mt-4"><Alert tone="error">{err}</Alert></div>}
      <button type="submit" disabled={busy || !tok.trim()} className="w-full mt-6 bg-amber-500 text-white font-semibold py-2.5 rounded-xl hover:bg-amber-600 flex items-center justify-center gap-2 shadow-lg disabled:opacity-60">
        {busy ? <Spin /> : <><ShieldAlert size={16} />{tr("Восстановить доступ")}</>}
      </button>
      <Back />
    </form>
  )
}

export function ResetPasswordPage() {
  const nav = useNavigate()
  const [params] = useSearchParams()
  const token = params.get('token') || ''
  const [check, setCheck] = useState<{ user?: string; error?: string } | null>(null)
  const [p1, setP1] = useState('')
  const [p2, setP2] = useState('')
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState('')
  const [done, setDone] = useState(false)
  useEffect(() => {
    if (!token) { setCheck({ error: tr("В ссылке нет токена сброса.") }); return }
    api<any>(`/api/auth/recover-check?token=${encodeURIComponent(token)}`).then(r => setCheck({ user: r.user })).catch(e => setCheck({ error: e.message }))
  }, [token])
  const short = p1.length > 0 && p1.length < 8
  const mismatch = p2.length > 0 && p1 !== p2
  const submit = async (e: React.FormEvent) => {
    e.preventDefault(); setBusy(true); setErr('')
    try { await api('/api/auth/recover-reset', { method: 'POST', body: { token, password: p1 } }); setDone(true) } catch (e: any) { setErr(e.message) } finally { setBusy(false) }
  }
  return (
    <Shell title={tr("Новый пароль")}>
      <div className="p-8">
        {!check ? <div className="text-center text-sm text-slate-400">…</div>
        : check.error ? <><Alert tone="error">{check.error}</Alert><button onClick={() => nav('/recover')} className="w-full mt-5 text-blue-600 hover:underline text-sm font-medium">{tr("Запросить новую ссылку")}</button><Back /></>
        : done ? (
          <div className="text-center">
            <div className="inline-flex p-3 rounded-full bg-emerald-50 text-emerald-500 mb-4"><CheckCircle2 size={40} /></div>
            <h2 className="text-lg font-bold text-slate-800 mb-2">{tr("Пароль изменён")}</h2>
            <p className="text-sm text-slate-500">{tr("Войдите с новым паролем. Открытые сеансы этой учётной записи закрыты.")}</p>
            <button onClick={() => nav('/login')} className="w-full mt-6 bg-blue-600 text-white font-semibold py-2.5 rounded-xl hover:bg-blue-700 flex items-center justify-center gap-2"><Lock size={16} />{tr("Ко входу")}</button>
          </div>
        ) : (
          <form onSubmit={submit}>
            <p className="text-sm text-slate-500 mb-5">{tr("Новый пароль для учётной записи")} <b className="text-slate-800">{check.user}</b></p>
            {err && <div className="mb-4"><Alert tone="error">{err}</Alert></div>}
            <div className="relative mb-3">
              <Lock size={18} className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400" />
              <input type="password" value={p1} onChange={e => setP1(e.target.value)} autoFocus placeholder={tr("Новый пароль")} className={inputCls} autoComplete="new-password" />
            </div>
            <div className="relative">
              <Lock size={18} className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-400" />
              <input type="password" value={p2} onChange={e => setP2(e.target.value)} placeholder={tr("Повторите пароль")} className={inputCls} autoComplete="new-password" />
            </div>
            <p className={cx('text-xs mt-2', short || mismatch ? 'text-red-600' : 'text-slate-400')}>{mismatch ? tr("Пароли не совпадают") : tr("Не короче 8 символов")}</p>
            <button type="submit" disabled={busy || p1.length < 8 || p1 !== p2} className="w-full mt-5 bg-blue-600 text-white font-semibold py-2.5 rounded-xl hover:bg-blue-700 flex items-center justify-center gap-2 shadow-lg disabled:opacity-60">
              {busy ? <Spin /> : <><KeyRound size={16} />{tr("Сменить пароль")}</>}
            </button>
            <Back />
          </form>
        )}
      </div>
    </Shell>
  )
}
