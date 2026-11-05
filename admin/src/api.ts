import { LOCALE, tr, trf } from './i18n'
// Обёртка над REST API сервера управления HopeBackup: токен сеанса, ошибки, JSON, форматирование.

const TOKEN_KEY = 'hopebackup_token'
const USER_KEY = 'hopebackup_user'
const ROLE_KEY = 'hopebackup_role'
const PERMS_KEY = 'hopebackup_perms'

export const auth = {
  token: () => localStorage.getItem(TOKEN_KEY) || '',
  user: () => localStorage.getItem(USER_KEY) || '',
  role: () => localStorage.getItem(ROLE_KEY) || '',
  set: (token: string, user: string, role: string) => {
    localStorage.setItem(TOKEN_KEY, token); localStorage.setItem(USER_KEY, user); localStorage.setItem(ROLE_KEY, role)
  },
  clear: () => { [TOKEN_KEY, USER_KEY, ROLE_KEY, PERMS_KEY].forEach(k => localStorage.removeItem(k)) },
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) { super(message); this.status = status }
}

export async function api<T = any>(path: string, opts: { method?: string; body?: any; signal?: AbortSignal } = {}): Promise<T> {
  const headers: Record<string, string> = {}
  const t = auth.token()
  if (t) headers['Authorization'] = `Bearer ${t}`
  if (opts.body !== undefined) headers['Content-Type'] = 'application/json'
  let res: Response
  try { res = await fetch(path, { method: opts.method || 'GET', headers, body: opts.body !== undefined ? JSON.stringify(opts.body) : undefined, signal: opts.signal }) }
  catch (e: any) { throw netError(e) }
  const text = await res.text()
  let data: any = null
  try { data = text ? JSON.parse(text) : null } catch { data = text }
  if (res.status === 401 && !path.startsWith('/api/auth/login')) {
    auth.clear()
    if (!location.pathname.startsWith('/login')) location.href = '/login'
  }
  if (!res.ok) throw new ApiError(res.status, (data && data.error) || (typeof data === 'string' && data) || `HTTP ${res.status}`)
  return data as T
}

/** Сетевой сбой fetch («Failed to fetch», «NetworkError…», «Load failed») — понятным текстом. */
export function netError(e: any): Error {
  if (e?.name === 'AbortError') return e
  const m = String(e?.message || e || '')
  if (e instanceof TypeError || /failed to fetch|networkerror|load failed|network request failed/i.test(m)) {
    return new ApiError(0, tr("Нет связи с сервером управления: соединение прервалось или сервер недоступен (сеть, VPN, перезапуск службы). Проверьте связь и повторите."))
  }
  return e instanceof Error ? e : new Error(m)
}

export const get = <T = any>(p: string) => api<T>(p)
export const post = <T = any>(p: string, body?: any) => api<T>(p, { method: 'POST', body: body ?? {} })
export const put = <T = any>(p: string, body?: any) => api<T>(p, { method: 'PUT', body: body ?? {} })
export const del = <T = any>(p: string) => api<T>(p, { method: 'DELETE' })

export const q = (o: Record<string, any>) => {
  const s = Object.entries(o).filter(([, v]) => v !== undefined && v !== null && v !== '').map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(String(v))}`).join('&')
  return s ? '?' + s : ''
}

/** Секрет, заданный на сервере (в ответах вместо значения). */
export const SECRET_MASK = '********'

// ---------------------------------------------------------------- форматирование
export function fmtBytes(n: number | null | undefined): string {
  const u = [tr("Б"), tr("КБ"), tr("МБ"), tr("ГБ"), tr("ТБ"), tr("ПБ")]
  if (!n) return `0 ${u[0]}`
  let i = 0; let v = n
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++ }
  return `${i === 0 ? v : v.toFixed(1).replace('.', ',')} ${u[i]}`
}

const parse = (s: string | null | undefined) => { if (!s) return null; const d = new Date(s); return isNaN(d.getTime()) ? null : d }

/** Время RFC 3339 → местное «24.10.2026, 14:31». */
export function fmtTime(s: string | null | undefined, withSec = false): string {
  if (!s) return '—'
  const d = parse(s)
  if (!d) return s
  return d.toLocaleString(LOCALE, { day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit', second: withSec ? '2-digit' : undefined })
}

/** Только время суток «14:31:05». */
export function fmtClock(s: string | null | undefined): string {
  const d = parse(s)
  return d ? d.toLocaleTimeString(LOCALE) : (s || '')
}

/** Длительность в секундах → «3 мин», «2 ч 5 мин». */
export function fmtDuration(sec: number | null | undefined): string {
  if (sec === null || sec === undefined || isNaN(sec)) return '—'
  sec = Math.max(0, Math.round(sec))
  if (sec < 60) return trf("{0} с", sec)
  if (sec < 3600) return trf("{0} мин", Math.floor(sec / 60))
  if (sec < 86400) { const m = Math.floor((sec % 3600) / 60); return trf("{0} ч{1}", Math.floor(sec / 3600), m ? trf(" {0} мин", m) : '') }
  const h = Math.floor((sec % 86400) / 3600)
  return trf("{0} д{1}", Math.floor(sec / 86400), h ? trf(" {0} ч", h) : '')
}

/** «5 мин назад» / «через 2 ч». */
export function fmtAgo(s: string | null | undefined): string {
  const d = parse(s)
  if (!d) return '—'
  const diff = (Date.now() - d.getTime()) / 1000
  if (Math.abs(diff) < 30) return tr("только что")
  return diff > 0 ? trf("{0} назад", fmtDuration(diff)) : trf("через {0}", fmtDuration(-diff))
}

/** Секунды между двумя временами RFC 3339 (до сейчас, если конца нет). */
export function secondsBetween(a: string | null | undefined, b?: string | null): number | null {
  const x = parse(a)
  if (!x) return null
  const y = parse(b) || new Date()
  return (y.getTime() - x.getTime()) / 1000
}

/** Местное время с часовым поясом «2026-10-24 14:31:00+03» (формат target_time). */
export function fmtLocalTz(s: string | Date | null | undefined): string {
  const d = s instanceof Date ? s : parse(s || '')
  if (!d) return ''
  const p = (n: number) => String(Math.abs(n)).padStart(2, '0')
  const off = -d.getTimezoneOffset()
  const tz = `${off >= 0 ? '+' : '-'}${p(Math.trunc(off / 60))}${off % 60 ? ':' + p(off % 60) : ''}`
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}${tz}`
}

/** Разбор введённого времени PITR («2026-10-24 14:31:00+03», ISO, без пояса — местное). */
export function parseUserTime(s: string): Date | null {
  s = s.trim()
  if (!s) return null
  let iso = s.replace(' ', 'T')
  if (/[+-]\d{2}$/.test(iso)) iso += ':00'
  const d = new Date(iso)
  return isNaN(d.getTime()) ? null : d
}

export const copyText = async (s: string) => {
  try { await navigator.clipboard.writeText(s); return true } catch {
    const ta = document.createElement('textarea'); ta.value = s; document.body.appendChild(ta); ta.select()
    const ok = document.execCommand('copy'); ta.remove(); return ok
  }
}
