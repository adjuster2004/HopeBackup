// Общие элементы интерфейса (как в HopeDFS: Tailwind, lucide-react).
import React, { createContext, useCallback, useContext, useEffect, useRef, useState } from 'react'
import { AlertTriangle, Check as CheckIcon, CheckCircle2, ChevronDown, Info, Loader2, RefreshCw, X, XCircle } from 'lucide-react'
import { api } from '../api'
import { tr } from '../i18n'

export function cx(...c: (string | false | null | undefined)[]) { return c.filter(Boolean).join(' ') }

// ---------------------------------------------------------------- кнопки
type BtnKind = 'primary' | 'secondary' | 'danger' | 'ghost' | 'success'
export function Button({ kind = 'secondary', icon, children, loading, className, ...p }: React.ButtonHTMLAttributes<HTMLButtonElement> & { kind?: BtnKind; icon?: React.ReactNode; loading?: boolean }) {
  const k = {
    primary: 'bg-blue-600 hover:bg-blue-700 text-white shadow-sm border-blue-600',
    secondary: 'bg-white hover:bg-slate-50 text-slate-700 border-slate-300',
    danger: 'bg-red-600 hover:bg-red-700 text-white border-red-600',
    success: 'bg-emerald-600 hover:bg-emerald-700 text-white border-emerald-600',
    ghost: 'bg-transparent hover:bg-slate-100 text-slate-600 border-transparent',
  }[kind]
  return (
    <button type="button" {...p} disabled={p.disabled || loading}
      className={cx('inline-flex items-center justify-center gap-2 px-3.5 py-2 rounded-lg text-sm font-medium border transition-colors disabled:opacity-50 disabled:cursor-not-allowed whitespace-nowrap', k, className)}>
      {loading ? <Loader2 size={16} className="animate-spin" /> : icon}
      {children}
    </button>
  )
}

export function IconButton({ title, children, onClick, danger, disabled }: { title: string; children: React.ReactNode; onClick?: () => void; danger?: boolean; disabled?: boolean }) {
  return (
    <button type="button" title={title} onClick={onClick} disabled={disabled}
      className={cx('p-1.5 rounded-md transition-colors disabled:opacity-40', danger ? 'text-slate-400 hover:text-red-600 hover:bg-red-50' : 'text-slate-400 hover:text-blue-600 hover:bg-blue-50')}>
      {children}
    </button>
  )
}

// ---------------------------------------------------------------- карточки и заголовки
/** Карточка. `collapsible` — ключ: заголовок сворачивает и разворачивает содержимое, выбор запоминается в браузере. */
export function Card({ title, actions, children, className, padded = true, icon, collapsible, defaultOpen = true }: { title?: React.ReactNode; actions?: React.ReactNode; children: React.ReactNode; className?: string; padded?: boolean; icon?: React.ReactNode; collapsible?: string; defaultOpen?: boolean }) {
  const key = collapsible ? `hopebackup_card:${collapsible}` : ''
  const [open, setOpen] = useState<boolean>(() => {
    if (!key) return true
    try { const v = localStorage.getItem(key); return v === null ? defaultOpen : v === '1' } catch { return defaultOpen }
  })
  const toggle = () => { const n = !open; setOpen(n); try { localStorage.setItem(key, n ? '1' : '0') } catch { /* */ } }
  return (
    <div className={cx('bg-white rounded-xl border border-slate-200 shadow-sm', className)}>
      {(title || actions) && (
        <div className={cx('flex items-center justify-between gap-3 px-5 py-3.5', open && 'border-b border-slate-100')}>
          {collapsible
            ? <button type="button" onClick={toggle} aria-expanded={open} className="font-semibold text-slate-800 flex items-center gap-2 text-left hover:text-blue-700 -ml-1 px-1 rounded focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-500" title={open ? tr("Свернуть") : tr("Развернуть")}>
                <ChevronDown size={18} className={cx('text-slate-400 transition-transform shrink-0', !open && '-rotate-90')} />{icon}{title}
              </button>
            : <h3 className="font-semibold text-slate-800 flex items-center gap-2">{icon}{title}</h3>}
          {open && <div className="flex items-center gap-2">{actions}</div>}
        </div>
      )}
      {open && <div className={padded ? 'p-5' : ''}>{children}</div>}
    </div>
  )
}

export function PageHeader({ title, subtitle, actions, icon }: { title: string; subtitle?: React.ReactNode; actions?: React.ReactNode; icon?: React.ReactNode }) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-4 mb-6">
      <div className="flex items-start gap-3">
        {icon && <div className="p-2.5 bg-blue-50 text-blue-600 rounded-xl">{icon}</div>}
        <div>
          <h1 className="text-2xl font-bold text-slate-900">{title}</h1>
          {subtitle && <p className="text-sm text-slate-500 mt-1">{subtitle}</p>}
        </div>
      </div>
      <div className="flex items-center gap-2 flex-wrap">{actions}</div>
    </div>
  )
}

export function Stat({ label, value, sub, icon, tone = 'blue' }: { label: string; value: React.ReactNode; sub?: React.ReactNode; icon?: React.ReactNode; tone?: 'blue' | 'green' | 'amber' | 'red' | 'slate' | 'violet' }) {
  const t = { blue: 'bg-blue-50 text-blue-600', green: 'bg-emerald-50 text-emerald-600', amber: 'bg-amber-50 text-amber-600', red: 'bg-red-50 text-red-600', slate: 'bg-slate-100 text-slate-600', violet: 'bg-violet-50 text-violet-600' }[tone]
  return (
    <div className="bg-white rounded-xl border border-slate-200 shadow-sm p-4 flex items-center gap-4">
      {icon && <div className={cx('p-3 rounded-xl', t)}>{icon}</div>}
      <div className="min-w-0">
        <div className="text-xs font-medium text-slate-500 uppercase tracking-wide">{label}</div>
        <div className="text-2xl font-bold text-slate-900 leading-tight">{value}</div>
        {sub && <div className="text-xs text-slate-500 truncate">{sub}</div>}
      </div>
    </div>
  )
}

export function Badge({ tone = 'slate', children }: { tone?: 'slate' | 'green' | 'red' | 'amber' | 'blue' | 'violet'; children: React.ReactNode }) {
  const t = { slate: 'bg-slate-100 text-slate-700', green: 'bg-emerald-100 text-emerald-800', red: 'bg-red-100 text-red-700', amber: 'bg-amber-100 text-amber-800', blue: 'bg-blue-100 text-blue-700', violet: 'bg-violet-100 text-violet-700' }[tone]
  return <span className={cx('inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium whitespace-nowrap', t)}>{children}</span>
}

export function Dot({ ok }: { ok: boolean }) { return <span className={cx('inline-block w-2 h-2 rounded-full', ok ? 'bg-emerald-500' : 'bg-red-500')} /> }

// ---------------------------------------------------------------- поля ввода
export function Field({ label, hint, children, className }: { label: React.ReactNode; hint?: React.ReactNode; children: React.ReactNode; className?: string }) {
  return (
    <label className={cx('block', className)}>
      <span className="block text-sm font-medium text-slate-700 mb-1">{label}</span>
      {children}
      {hint && <span className="block text-xs text-slate-500 mt-1">{hint}</span>}
    </label>
  )
}

export const inputCls = 'w-full px-3 py-2 rounded-lg border border-slate-300 bg-white text-sm focus:outline-none focus:ring-2 focus:ring-blue-500/30 focus:border-blue-500 disabled:bg-slate-50 disabled:text-slate-500'

/** Классы поля: своя ширина страницы (`w-64`, `flex-1`…) заменяет «во всю ширину», а не спорит с ней. */
const fieldCls = (extra?: string) => {
  const own = !!extra && /(^|\s)(w-|flex-1|flex-auto|basis-)/.test(extra)
  return cx(own ? inputCls.replace(/^w-full /, '') : inputCls, extra)
}
export function Input(p: React.InputHTMLAttributes<HTMLInputElement>) { return <input {...p} className={fieldCls(p.className)} /> }
export function Textarea(p: React.TextareaHTMLAttributes<HTMLTextAreaElement>) { return <textarea {...p} className={fieldCls(cx('font-mono', p.className))} /> }
export function Select({ children, ...p }: React.SelectHTMLAttributes<HTMLSelectElement>) { return <select {...p} className={fieldCls(p.className)}>{children}</select> }

export function Toggle({ checked, onChange, label, hint, disabled }: { checked: boolean; onChange: (v: boolean) => void; label: React.ReactNode; hint?: React.ReactNode; disabled?: boolean }) {
  return (
    <label className={cx('flex items-start gap-3 py-1.5 select-none', disabled ? 'opacity-50' : 'cursor-pointer')}>
      <button type="button" disabled={disabled} onClick={() => onChange(!checked)}
        className={cx('relative mt-0.5 w-9 h-5 shrink-0 rounded-full transition-colors', checked ? 'bg-blue-600' : 'bg-slate-300')}>
        <span className={cx('absolute top-0.5 left-0.5 w-4 h-4 bg-white rounded-full shadow transition-transform', checked && 'translate-x-4')} />
      </button>
      <span>
        <span className="block text-sm text-slate-800">{label}</span>
        {hint && <span className="block text-xs text-slate-500">{hint}</span>}
      </span>
    </label>
  )
}

export function Check({ checked, onChange, label, disabled, indeterminate }: { checked: boolean; onChange: (v: boolean) => void; label: React.ReactNode; disabled?: boolean; indeterminate?: boolean }) {
  const ref = useRef<HTMLInputElement>(null)
  useEffect(() => { if (ref.current) ref.current.indeterminate = !!indeterminate }, [indeterminate])
  return (
    <label className={cx('flex items-center gap-2 text-sm py-0.5', disabled ? 'text-slate-400' : 'text-slate-700 cursor-pointer')}>
      <input ref={ref} type="checkbox" className="w-4 h-4 rounded border-slate-300 text-blue-600 focus:ring-blue-500" checked={checked} disabled={disabled} onChange={e => onChange(e.target.checked)} />
      {label}
    </label>
  )
}

// ---------------------------------------------------------------- вкладки
export function Tabs<T extends string>({ tabs, value, onChange }: { tabs: { id: T; label: React.ReactNode }[]; value: T; onChange: (v: T) => void }) {
  return (
    <div className="flex gap-1 border-b border-slate-200 mb-4 overflow-x-auto hide-scrollbar">
      {tabs.map(t => (
        <button type="button" key={t.id} onClick={() => onChange(t.id)}
          className={cx('px-4 py-2 text-sm font-medium border-b-2 -mb-px whitespace-nowrap transition-colors', value === t.id ? 'border-blue-600 text-blue-700' : 'border-transparent text-slate-500 hover:text-slate-800')}>
          {t.label}
        </button>
      ))}
    </div>
  )
}

// ---------------------------------------------------------------- таблица
export function Table({ head, children, empty, className }: { head: React.ReactNode[]; children: React.ReactNode; empty?: React.ReactNode; className?: string }) {
  const rows = React.Children.toArray(children).filter(Boolean)
  return (
    <div className={cx('overflow-x-auto', className)}>
      <table className="w-full text-sm">
        <thead>
          <tr className="text-left text-xs uppercase tracking-wide text-slate-500 border-b border-slate-200 bg-slate-50/60">
            {head.map((h, i) => <th key={i} className="px-4 py-2.5 font-semibold whitespace-nowrap">{h}</th>)}
          </tr>
        </thead>
        <tbody className="divide-y divide-slate-100">
          {rows.length ? rows : <tr><td colSpan={head.length} className="px-4 py-10 text-center text-slate-400">{empty || tr("Нет данных")}</td></tr>}
        </tbody>
      </table>
    </div>
  )
}
export const Td = ({ children, className, title }: { children?: React.ReactNode; className?: string; title?: string }) => <td title={title} className={cx('px-4 py-2.5 align-middle', className)}>{children}</td>

// ---------------------------------------------------------------- модальные окна
export function Modal({ open, onClose, title, children, footer, wide, xwide }: { open: boolean; onClose: () => void; title: React.ReactNode; children: React.ReactNode; footer?: React.ReactNode; wide?: boolean; xwide?: boolean }) {
  useEffect(() => {
    if (!open) return
    const h = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', h)
    return () => window.removeEventListener('keydown', h)
  }, [open, onClose])
  if (!open) return null
  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center p-4 pt-[6vh] bg-slate-900/50 backdrop-blur-[2px] overflow-y-auto" onMouseDown={e => { if (e.target === e.currentTarget) onClose() }}>
      <div className={cx('bg-white rounded-2xl shadow-2xl w-full flex flex-col max-h-[88vh]', xwide ? 'max-w-5xl' : wide ? 'max-w-3xl' : 'max-w-lg')}>
        <div className="flex items-center justify-between px-6 py-4 border-b border-slate-100">
          <h2 className="text-lg font-semibold text-slate-900">{title}</h2>
          <button type="button" onClick={onClose} className="p-1.5 rounded-lg text-slate-400 hover:text-slate-700 hover:bg-slate-100"><X size={18} /></button>
        </div>
        <div className="px-6 py-5 overflow-y-auto">{children}</div>
        {footer && <div className="px-6 py-4 border-t border-slate-100 bg-slate-50/60 rounded-b-2xl flex justify-end gap-2">{footer}</div>}
      </div>
    </div>
  )
}

export function Alert({ tone = 'info', children, title }: { tone?: 'info' | 'warn' | 'error' | 'success'; children: React.ReactNode; title?: string }) {
  const t = {
    info: ['bg-blue-50 border-blue-200 text-blue-900', <Info key="i" size={18} className="text-blue-600 shrink-0 mt-0.5" />],
    warn: ['bg-amber-50 border-amber-200 text-amber-900', <AlertTriangle key="i" size={18} className="text-amber-600 shrink-0 mt-0.5" />],
    error: ['bg-red-50 border-red-200 text-red-900', <XCircle key="i" size={18} className="text-red-600 shrink-0 mt-0.5" />],
    success: ['bg-emerald-50 border-emerald-200 text-emerald-900', <CheckCircle2 key="i" size={18} className="text-emerald-600 shrink-0 mt-0.5" />],
  }[tone] as [string, React.ReactNode]
  return (
    <div className={cx('flex gap-3 p-3.5 rounded-lg border text-sm', t[0])}>
      {t[1]}
      <div>{title && <div className="font-semibold mb-0.5">{title}</div>}{children}</div>
    </div>
  )
}

export function Spinner({ label }: { label?: string }) {
  return <div className="flex items-center justify-center gap-2 py-12 text-slate-400"><Loader2 className="animate-spin" size={20} />{label || tr("Загрузка…")}</div>
}

// ---------------------------------------------------------------- уведомления
type ToastT = { id: number; kind: 'ok' | 'err' | 'info'; text: string }
const ToastCtx = createContext<{ ok: (t: string) => void; err: (t: any) => void; info: (t: string) => void }>({ ok: () => {}, err: () => {}, info: () => {} })
export const useToast = () => useContext(ToastCtx)

export function ToastProvider({ children }: { children: React.ReactNode }) {
  const [list, setList] = useState<ToastT[]>([])
  const push = useCallback((kind: ToastT['kind'], text: string) => {
    const id = Date.now() + Math.random()
    setList(l => [...l, { id, kind, text }])
    setTimeout(() => setList(l => l.filter(x => x.id !== id)), kind === 'err' ? 8000 : 4000)
  }, [])
  const v = { ok: (t: string) => push('ok', t), info: (t: string) => push('info', t), err: (e: any) => push('err', e?.message || String(e)) }
  return (
    <ToastCtx.Provider value={v}>
      {children}
      <div className="fixed bottom-4 right-4 z-[60] flex flex-col gap-2 w-96 max-w-[calc(100vw-2rem)]">
        {list.map(t => (
          <div key={t.id} className={cx('flex items-start gap-3 px-4 py-3 rounded-xl shadow-lg border text-sm bg-white',
            t.kind === 'ok' ? 'border-emerald-200' : t.kind === 'err' ? 'border-red-200' : 'border-blue-200')}>
            {t.kind === 'ok' ? <CheckCircle2 size={18} className="text-emerald-600 shrink-0" /> : t.kind === 'err' ? <XCircle size={18} className="text-red-600 shrink-0" /> : <Info size={18} className="text-blue-600 shrink-0" />}
            <span className="text-slate-800">{t.text}</span>
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  )
}

// ---------------------------------------------------------------- загрузка данных
export function useApi<T = any>(path: string | null, deps: any[] = [], interval?: number) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [tick, setTick] = useState(0)
  // reload() возвращает обещание, которое выполнится, когда данные придут (для индикации на кнопке «Обновить»).
  const waiters = useRef<(() => void)[]>([])
  const settle = () => { const w = waiters.current; waiters.current = []; w.forEach(f => f()) }
  const reload = useCallback(() => new Promise<void>(res => { waiters.current.push(res); setTick(t => t + 1) }), [])
  useEffect(() => {
    if (!path) { setLoading(false); settle(); return }
    let alive = true
    const ctl = new AbortController()
    const run = (first: boolean) => {
      if (first) setLoading(true)
      api<T>(path, { signal: ctl.signal }).then(d => { if (alive) { setData(d); setError(null) } }).catch(e => { if (alive && e.name !== 'AbortError') setError(e.message) }).finally(() => { if (alive) setLoading(false); if (first) settle() })
    }
    run(true)
    const id = interval ? setInterval(() => run(false), interval) : undefined
    return () => { alive = false; ctl.abort(); if (id) clearInterval(id) }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, tick, ...deps])
  return { data, error, loading, reload, setData }
}

export function Confirm({ open, title, text, onYes, onNo, danger, yes }: { open: boolean; title: string; text: React.ReactNode; onYes: () => void; onNo: () => void; danger?: boolean; yes?: string }) {
  return (
    <Modal open={open} onClose={onNo} title={title} footer={<><Button onClick={onNo}>{tr("Отмена")}</Button><Button kind={danger ? 'danger' : 'primary'} onClick={onYes}>{yes ?? tr("Да")}</Button></>}>
      <div className="text-sm text-slate-700">{text}</div>
    </Modal>
  )
}

export function Empty({ icon, title, text, action }: { icon?: React.ReactNode; title: string; text?: React.ReactNode; action?: React.ReactNode }) {
  return (
    <div className="flex flex-col items-center text-center py-14 px-6">
      {icon && <div className="p-4 rounded-2xl bg-slate-100 text-slate-400 mb-4">{icon}</div>}
      <div className="font-semibold text-slate-700">{title}</div>
      {text && <div className="text-sm text-slate-500 mt-1 max-w-md">{text}</div>}
      {action && <div className="mt-4">{action}</div>}
    </div>
  )
}

// ---------------------------------------------------------------- дополнения HopeBackup
export type Tone = 'slate' | 'green' | 'red' | 'amber' | 'blue' | 'violet'

/** Цветная точка состояния (зелёная / жёлтая / красная / серая). */
export function StatusDot({ tone, className }: { tone: Tone; className?: string }) {
  const c = { green: 'bg-emerald-500', amber: 'bg-amber-500', red: 'bg-red-500', slate: 'bg-slate-400', blue: 'bg-blue-500', violet: 'bg-violet-500' }[tone]
  return <span className={cx('inline-block w-2 h-2 rounded-full shrink-0', c, className)} />
}

/** Блок моноширинного текста (команды, JSON, вывод). */
export function Code({ children, className }: { children: React.ReactNode; className?: string }) {
  return <pre className={cx('bg-slate-900 text-slate-100 rounded-lg p-3 text-xs font-mono overflow-x-auto whitespace-pre-wrap break-all', className)}>{children}</pre>
}

/** Карточка выбора (радио-плитка), как в макетах: выбранная — синяя рамка. */
export function ChoiceCard({ selected, disabled, onClick, title, text, badge, icon }: { selected?: boolean; disabled?: boolean; onClick?: () => void; title: React.ReactNode; text?: React.ReactNode; badge?: React.ReactNode; icon?: React.ReactNode }) {
  return (
    <button type="button" role="radio" aria-checked={!!selected} disabled={disabled} onClick={onClick}
      className={cx('text-left flex gap-3 items-start p-3 rounded-xl bg-white transition-shadow',
        selected ? 'border-2 border-blue-600 ring-4 ring-blue-600/10' : 'border border-slate-200 hover:border-slate-300',
        disabled && 'opacity-50 cursor-not-allowed bg-slate-50 hover:border-slate-200')}>
      {icon}
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2 text-sm font-semibold text-slate-900">{title}{badge}</span>
        {text && <span className="block text-xs text-slate-500 mt-0.5">{text}</span>}
      </span>
    </button>
  )
}

/** Ярлык «в разработке» для недоступных плагинов, коннекторов и целей. */
export const InDev = () => <span className="px-1.5 py-0.5 rounded bg-slate-200 text-slate-500 text-[10px] font-semibold uppercase tracking-wide">{tr("в разработке")}</span>

/** Строка «ключ — значение» в карточках сведений. */
export function KV({ k, children, mono }: { k: React.ReactNode; children: React.ReactNode; mono?: boolean }) {
  return (
    <div className="flex gap-4 py-1.5 text-sm border-b border-slate-100 last:border-0">
      <div className="w-44 shrink-0 text-slate-500">{k}</div>
      <div className={cx('min-w-0 break-all text-slate-800', mono && 'font-mono text-xs leading-5')}>{children}</div>
    </div>
  )
}

/** Кнопка «Обновить»: значок крутится, пока идёт запрос, затем коротко — «Обновлено» с галочкой. */
export function RefreshButton({ onClick, label = tr("Обновить") }: { onClick: () => unknown; label?: string }) {
  const [st, setSt] = useState<'idle' | 'spin' | 'done'>('idle')
  const timer = useRef<ReturnType<typeof setTimeout>>()
  useEffect(() => () => clearTimeout(timer.current), [])
  const go = async () => {
    if (st === 'spin') return
    clearTimeout(timer.current)
    setSt('spin')
    try {
      await Promise.all([Promise.resolve(onClick()), new Promise(r => setTimeout(r, 450))])
      setSt('done')
      timer.current = setTimeout(() => setSt('idle'), 1500)
    } catch { setSt('idle') }
  }
  return (
    <Button onClick={go} aria-live="polite" className="min-w-[8.5rem] justify-center"
      icon={st === 'done' ? <CheckIcon size={16} className="text-emerald-600" /> : <RefreshCw size={16} className={st === 'spin' ? 'animate-spin' : ''} />}>
      <span className={st === 'done' ? 'text-emerald-700' : ''}>{st === 'done' ? tr("Обновлено") : label}</span>
    </Button>
  )
}
