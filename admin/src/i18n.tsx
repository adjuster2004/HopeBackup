// Локализация панели (RU / EN) — тот же подход, что в HopeDC.
// ---------------------------------------------------------------------
// «Русский текст как ключ»: строки обёрнуты в tr("...") (или trf("... {0} ...", x) для строк
// с подстановками), английский перевод — в словаре i18n_en.ts. Непереведённое показывается
// по-русски, поэтому новые строки ничего не ломают: их достаточно добавить в словарь.
// Тексты с сервера (каталог продуктов, поля форм) переводятся тем же словарём там, где их
// выводит панель; сообщения об ошибках и журналы заданий приходят с сервера по-русски.
//
// Выбор языка хранится в браузере (localStorage). При переключении страница перезагружается:
// часть строк — константы модулей (подписи состояний, пункты меню), они вычисляются один раз.
import { EN } from './i18n_en'

export type Lang = 'ru' | 'en'
const KEY = 'hopebackup_lang'

const read = (): Lang => {
  try {
    const v = localStorage.getItem(KEY)
    if (v === 'en' || v === 'ru') return v
    // Первый вход: язык браузера (русский — по умолчанию).
    return /^ru|^be|^uk|^kk/i.test(navigator.language || 'ru') ? 'ru' : 'en'
  } catch { return 'ru' }
}

export const lang: Lang = read()
/** Локаль для дат и чисел. en-GB — 24-часовое время и день перед месяцем. */
export const LOCALE = lang === 'en' ? 'en-GB' : 'ru-RU'

try { document.documentElement.lang = lang; document.title = lang === 'en' ? 'HopeBackup — backup' : 'HopeBackup — резервное копирование' } catch { /* */ }

const lookup = (s: string): string | undefined => {
  const v = EN[s]
  if (v !== undefined) return v
  // Ключ с пробелами по краям («больше ») — перевод хранится без них.
  const m = /^(\s*)([\s\S]*?)(\s*)$/.exec(s)
  if (m && (m[1] || m[3])) { const c = EN[m[2]]; if (c !== undefined) return m[1] + c + m[3] }
  return undefined
}

/** Перевести строку (русский текст — ключ). */
export function tr(s: string): string {
  if (lang !== 'en' || !s) return s
  return lookup(s) ?? s
}

/** Перевести строку с подстановками {0}, {1}… */
export function trf(s: string, ...args: unknown[]): string {
  const p = lang === 'en' ? (lookup(s) ?? s) : s
  return p.replace(/\{(\d+)\}/g, (all, i) => { const n = Number(i); return n < args.length ? String(args[n]) : all })
}

/** Перевести текст, пришедший с сервера (может быть пустым). */
export const trx = (s: string | null | undefined): string => (s ? tr(s) : (s ?? ''))

const TEXT_KEYS = new Set(['title', 'description', 'category', 'modes', 'hint'])
/** Перевести описательные поля (title, description, category, modes, hint) во всём дереве JSON — каталог и схемы форм с сервера. */
export function trTree<T>(v: T): T {
  if (lang !== 'en') return v
  const walk = (x: any): any => {
    if (Array.isArray(x)) return x.map(walk)
    if (x && typeof x === 'object') {
      const o: any = {}
      for (const [k, val] of Object.entries(x)) o[k] = typeof val === 'string' && TEXT_KEYS.has(k) ? tr(val) : walk(val)
      return o
    }
    return x
  }
  return walk(v)
}

export function setLang(l: Lang) {
  if (l === lang) return
  try { localStorage.setItem(KEY, l) } catch { /* */ }
  location.reload()
}

/** Для совместимости с HopeDC: { lang, setLang, t }. */
export const useI18n = () => ({ lang, setLang, t: tr })

/** Переключатель RU / EN в шапке. */
export function LangSwitch({ className }: { className?: string }) {
  const btn = (l: Lang) => (
    <button type="button" onClick={() => setLang(l)} aria-pressed={lang === l}
      className={`px-2.5 py-1.5 ${lang === l ? 'bg-blue-600 text-white' : 'bg-white text-slate-500 hover:bg-slate-50'}`}>{l.toUpperCase()}</button>
  )
  return (
    <div className={`inline-flex rounded-lg border border-slate-300 overflow-hidden text-xs font-semibold ${className || ''}`} title={lang === 'en' ? 'Language' : 'Язык'}>
      {btn('ru')}<span className="w-px bg-slate-300" />{btn('en')}
    </div>
  )
}
