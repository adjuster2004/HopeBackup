import React, { createContext, useContext, useEffect, useState } from 'react'
import { Link, useLocation, useNavigate } from 'react-router-dom'
import {
  Activity, ArchiveRestore, ChevronDown, Database, Eye, LifeBuoy, ListChecks, Lock, LogOut, Network, PanelLeftClose,
  PanelLeftOpen, Plus, ScrollText, Server, ShieldCheck, Ship, Target, User, Users, Cable, Package, GitBranch, AlertTriangle, Settings, Monitor, BellRing, Truck, ArrowUpCircle,
} from 'lucide-react'
import { auth, fmtTime } from '../api'
import type { Cluster, Instance, Me } from '../types'
import { cx, useApi } from './ui'
import { PermsCtx, permStore, roleLabel } from '../perms'
import { INSTANCES_EVENT } from './common'
import { LangSwitch, tr, trf } from '../i18n'

// exact — подсвечивать только на самой странице; also — ещё и на этих разделах (страница экземпляра — часть «Продуктов»).
type Item = { path: string; icon: React.ReactNode; label: string; need?: 'restore' | 'admin'; exact?: boolean; also?: string[] }

const top: Item[] = [{ path: '/dashboard', icon: <Activity size={20} />, label: tr("Обзор") }]
const groups: { key: string; label: string; icon: React.ReactNode; items: Item[] }[] = [
  // Экземпляры в меню не перечисляются (их могут быть сотни) — список и состояние на странице «Продукты».
  { key: 'backup', label: tr("Резервное копирование"), icon: <Database size={15} />, items: [
    { path: '/products', icon: <Package size={20} />, label: tr("Продукты"), exact: true, also: ['/instances'] },
    { path: '/machines', icon: <Monitor size={20} />, label: tr("Серверы и ВМ") },
    { path: '/products/new', icon: <Plus size={20} />, label: tr("Добавить продукт") },
    { path: '/jobs', icon: <ListChecks size={20} />, label: tr("Задания") },
  ]},
  { key: 'sites', label: tr("Площадки"), icon: <Server size={15} />, items: [
    { path: '/sites', icon: <Server size={20} />, label: tr("Площадки") },
    { path: '/connectors', icon: <Cable size={20} />, label: tr("Коннекторы") },
    { path: '/agents', icon: <Network size={20} />, label: tr("Агенты") },
    { path: '/cluster', icon: <GitBranch size={20} />, label: tr("Узлы управления") },
  ]},
  { key: 'restore', label: tr("Восстановление"), icon: <LifeBuoy size={15} />, items: [
    { path: '/restore', icon: <ArchiveRestore size={20} />, label: tr("Мастер восстановления"), need: 'restore' },
    { path: '/targets', icon: <Target size={20} />, label: tr("Цели восстановления") },
  ]},
  { key: 'migrate', label: tr("Переезд"), icon: <Truck size={15} />, items: [
    { path: '/migrations', icon: <Truck size={20} />, label: tr("Проекты переезда") },
  ] },
  { key: 'sec', label: tr("Безопасность"), icon: <ShieldCheck size={15} />, items: [
    { path: '/users', icon: <Users size={20} />, label: tr("Пользователи"), need: 'admin' },
    { path: '/mail', icon: <BellRing size={20} />, label: tr("Почта и уведомления"), need: 'admin' },
    { path: '/audit', icon: <ScrollText size={20} />, label: tr("Журнал аудита") },
    { path: '/settings', icon: <Settings size={20} />, label: tr("Настройки") },
    { path: '/updates', icon: <ArrowUpCircle size={20} />, label: tr("Обновления"), need: 'admin' },
  ]},
]
const extraTitles: [string, string][] = [['/instances', tr("Продукт")]]

/** Экземпляры для меню и страниц (обновляются раз в 10 с и по событию INSTANCES_EVENT). */
export const InstancesCtx = createContext<{ list: Instance[] | null; reload: () => void }>({ list: null, reload: () => {} })
export const useInstances = () => useContext(InstancesCtx)
export const MeCtx = createContext<Me | null>(null)

export const Layout: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const loc = useLocation()
  const nav = useNavigate()
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({})
  const [narrow, setNarrow] = useState(() => localStorage.getItem('hopebackup_nav') === '1')
  const toggleNarrow = () => setNarrow(v => { localStorage.setItem('hopebackup_nav', v ? '0' : '1'); return !v })
  const me = useApi<Me>('/api/auth/me', [], 15000)
  const inst = useApi<Instance[]>('/api/instances', [], 10000)
  const cl = useApi<Cluster>('/api/cluster', [], 15000)
  const [perms, setPerms] = useState<string[]>(permStore.get(auth.role()))
  useEffect(() => {
    const r = me.data
    if (!r || !Array.isArray(r.perms)) return
    if (JSON.stringify(r.perms) !== JSON.stringify(perms)) setPerms(r.perms)
    permStore.set(r.perms)
  }, [me.data])
  useEffect(() => { const f = () => inst.reload(); window.addEventListener(INSTANCES_EVENT, f); return () => window.removeEventListener(INSTANCES_EVENT, f) }, [inst.reload])

  const has = (p: string) => perms.includes(p) || perms.includes('admin')
  const visible = (i: Item) => !i.need || has(i.need)
  const isActive = (p: string) => loc.pathname === p || loc.pathname.startsWith(p + '/')
  const itemActive = (i: Item) => (i.exact ? loc.pathname === i.path : isActive(i.path)) || (i.also || []).some(isActive)
  const all = [...top, ...groups.flatMap(g => g.items)]
  const current = all.find(i => itemActive(i) && !i.also) || all.find(itemActive)
  const instId = loc.pathname.startsWith('/instances/') ? decodeURIComponent(loc.pathname.split('/')[2] || '') : ''
  const curInst = instId ? inst.data?.find(i => i.id === instId) : undefined
  const title = curInst?.name || current?.label || extraTitles.find(([p]) => isActive(p))?.[1] || 'HopeBackup'
  const blocked = all.find(i => itemActive(i) && !visible(i))
  // Счётчик у «Продуктов»: сколько экземпляров с ошибкой или требуют внимания.
  const bad = (inst.data || []).filter(x => x.runtime?.status === 'error').length
  const warn = (inst.data || []).filter(x => x.runtime?.status === 'warn').length
  const readonly = !has('edit') && !(isActive('/restore') && has('restore'))
  const user = me.data?.user || auth.user()
  const role = me.data?.role || auth.role()

  const link = (i: Item) => {
    const active = itemActive(i)
    const count = i.path === '/products' ? (bad || warn) : 0
    const tone = bad ? 'bg-red-500' : 'bg-amber-500'
    const hint = i.path === '/products' && count ? ` — ${bad ? trf("с ошибкой: {0}", bad) : ''}${bad && warn ? ', ' : ''}${warn ? trf("требуют внимания: {0}", warn) : ''}` : ''
    return (
      <li key={i.path}>
        <Link to={i.path} title={narrow || hint ? i.label + hint : undefined}
          className={cx('flex items-center rounded-lg transition-all duration-150', narrow ? 'justify-center py-2.5 relative' : 'gap-3 px-3 py-2.5',
            active ? 'bg-blue-600 text-white shadow-md' : 'hover:bg-slate-800 hover:text-white')}>
          {i.icon}{!narrow && <span className="font-medium text-sm flex-1">{i.label}</span>}
          {count > 0 && (narrow
            ? <span className={cx('absolute top-1.5 right-3 w-2.5 h-2.5 rounded-full ring-2 ring-slate-900', tone)} />
            : <span role="button" title={tr("Показать экземпляры с проблемами")} onClick={e => { e.preventDefault(); e.stopPropagation(); nav('/products?status=problem') }}
                className={cx('min-w-[20px] h-5 px-1.5 rounded-full text-[11px] font-semibold text-white flex items-center justify-center hover:ring-2 hover:ring-white/40', tone)}>{count}</span>)}
        </Link>
      </li>
    )
  }

  return (
    <MeCtx.Provider value={me.data}>
    <InstancesCtx.Provider value={{ list: inst.data, reload: inst.reload }}>
    <div className="flex h-screen bg-slate-50">
      <aside className={cx('bg-slate-900 text-slate-300 flex flex-col shadow-2xl z-20 transition-[width] duration-200 shrink-0', narrow ? 'w-20' : 'w-64')}>
        <div className={cx('h-16 flex items-center border-b border-slate-800 bg-slate-950 shrink-0', narrow ? 'justify-center' : 'justify-between px-5')}>
          {!narrow && (
            <div className="flex items-center">
              <Ship className="text-blue-500 mr-2.5" size={24} />
              <div className="leading-tight">
                <div className="text-white font-bold text-lg tracking-wide">HopeBackup</div>
                <div className="text-[10px] text-slate-500 uppercase tracking-normal whitespace-nowrap">{tr("резервное копирование")}</div>
              </div>
            </div>
          )}
          <button onClick={toggleNarrow} className="p-2 text-slate-400 hover:text-white hover:bg-slate-800 rounded-lg" title={narrow ? tr("Развернуть меню") : tr("Свернуть меню")}>
            {narrow ? <PanelLeftOpen size={20} /> : <PanelLeftClose size={20} />}
          </button>
        </div>
        <nav className="flex-1 py-4 overflow-y-auto hide-scrollbar">
          <ul className="space-y-1 px-3">{top.map(link)}</ul>
          {groups.map(g => ({ ...g, items: g.items.filter(visible) })).filter(g => g.items.length > 0).map(g => {
            const open = !collapsed[g.key]
            const body = <ul className="space-y-1 px-3 mt-1">{g.items.map(link)}</ul>
            if (narrow) return (
              <div key={g.key} className="mt-4">
                <div className="flex justify-center py-1 text-slate-600" title={g.label}>{g.icon}</div>
                {body}
              </div>
            )
            return (
              <div key={g.key} className="mt-5">
                <button onClick={() => setCollapsed(c => ({ ...c, [g.key]: !c[g.key] }))}
                  className="w-full flex items-center justify-between px-4 mb-1 text-[11px] font-semibold uppercase tracking-wider text-slate-500 hover:text-slate-300">
                  <span className="flex items-center gap-2">{g.icon}{g.label}</span>
                  <ChevronDown size={14} className={cx('transition-transform', !open && '-rotate-90')} />
                </button>
                {open && body}
              </div>
            )
          })}
        </nav>
        <div className={cx('border-t border-slate-800 bg-slate-950/50 shrink-0', narrow ? 'p-3' : 'p-4')}>
          <div className={cx('flex items-center gap-3', narrow && 'flex-col')}>
            <div className="bg-slate-800 p-2 rounded-lg" title={user}><User size={16} className="text-blue-400" /></div>
            {!narrow && <div className="flex-1 min-w-0"><p className="text-sm font-bold text-white truncate">{user}</p><p className="text-xs text-slate-500 truncate">{roleLabel(role).toLowerCase()}</p></div>}
            <button onClick={() => { auth.clear(); nav('/login') }} className="p-2 text-slate-400 hover:text-red-400 hover:bg-slate-800 rounded-lg" title={tr("Выйти")}><LogOut size={16} /></button>
          </div>
        </div>
      </aside>
      <div className="flex-1 flex flex-col min-w-0">
        <header className="h-16 bg-white border-b border-slate-200 grid grid-cols-[1fr_auto_1fr] items-center gap-4 px-8 shrink-0">
          <h2 className="text-lg font-semibold text-slate-800 flex items-center gap-3 min-w-0">
            <span className="truncate">{title}</span>
            {readonly && !blocked && <span className="inline-flex items-center gap-1 bg-slate-100 text-slate-500 text-[11px] px-2 py-0.5 rounded-md border border-slate-200 font-medium uppercase tracking-wide shrink-0" title={tr("Нет прав на изменения — только просмотр")}><Eye size={12} />{tr("Только просмотр")}</span>}
          </h2>
          <ArkBrand />
          <div className="justify-self-end flex items-center gap-3">{me.data && <div className="text-xs text-slate-500 whitespace-nowrap">{me.data.hostname} · HopeBackup {me.data.version}</div>}<LangSwitch /></div>
        </header>
        {cl.data?.role === 'standby' && (
          <div className="bg-amber-50 border-b border-amber-200 text-amber-900 text-sm px-8 py-2 flex items-center gap-2 shrink-0">
            <AlertTriangle size={16} className="shrink-0" />
            <span>{tr("Принимающий узел: активный —")} <span className="font-mono">{cl.data.active_url}</span>{cl.data.last_sync ? trf(", синхронизация {0}", fmtTime(cl.data.last_sync)) : ''}{tr(". Настройки меняются на активном узле; здесь — просмотр и восстановление.")}</span>
            <Link to="/cluster" className="ml-auto font-medium hover:underline whitespace-nowrap">{tr("Узлы управления")}</Link>
          </div>
        )}
        <main className="flex-1 overflow-y-auto p-8">
          {/* Сервер обновили, а эта вкладка ещё на старой панели (загружена до обновления). */}
          {me.data?.version && __PANEL_VERSION__ !== 'dev' && me.data.version !== __PANEL_VERSION__ && (
            <div className="mb-4 flex flex-wrap items-center justify-between gap-3 rounded-xl border border-blue-200 bg-blue-50 px-4 py-2.5 text-sm text-blue-800">
              <span>{tr("Сервер обновлён до")} {me.data.version}{tr(", а эта страница — ещё панель")} {__PANEL_VERSION__}{tr(". Новые кнопки и разделы появятся после обновления страницы.")}</span>
              <button className="rounded-lg bg-blue-600 px-3 py-1.5 text-white font-medium hover:bg-blue-700" onClick={() => window.location.reload()}>{tr("Обновить страницу")}</button>
            </div>
          )}
          <PermsCtx.Provider value={perms}>
            {blocked ? <NoAccess /> : children}
          </PermsCtx.Provider>
        </main>
      </div>
    </div>
    </InstancesCtx.Provider>
    </MeCtx.Provider>
  )
}

/** Раздел недоступен по роли. */
export function NoAccess() {
  return (
    <div className="max-w-xl mx-auto mt-16 bg-white border border-slate-200 rounded-2xl shadow-sm p-8 text-center">
      <div className="w-14 h-14 bg-amber-50 text-amber-600 rounded-full flex items-center justify-center mx-auto mb-4"><Lock size={26} /></div>
      <h3 className="text-lg font-semibold text-slate-800 mb-2">{tr("Нет доступа к разделу")}</h3>
      <p className="text-sm text-slate-500 mb-5">{tr("Вашей роли не разрешён этот раздел. Роль назначает администратор в разделе «Пользователи».")}</p>
      <Link to="/dashboard" className="text-sm text-blue-600 hover:underline font-medium">{tr("Перейти к обзору")}</Link>
    </div>
  )
}

/** Эмблема продукта в шапке: рисунок ковчега и название «Ковчег» — на всех страницах панели. */
function ArkBrand() {
  return (
    <a href="https://hopedc.ru" target="_blank" rel="noopener noreferrer" className="hidden lg:flex items-center gap-2.5 select-none rounded-lg px-2 py-1 -my-1 hover:bg-slate-50 transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-500" title={tr("HopeBackup «Ковчег» — hopedc.ru (откроется в новой вкладке)")}>
      <svg viewBox="0 0 64 40" width="52" height="33" aria-hidden="true">
        <path d="M0.5 35.5q4-3 8 0t8 0t8 0t8 0t8 0t8 0t8 0t8 0" fill="none" stroke="#93c5fd" strokeWidth="2" strokeLinecap="round" />
        <path d="M4 19h56l-6 12q-22 3.5-44 0z" fill="#b45309" />
        <path d="M7 23h50M9 27h46" stroke="#78350f" strokeWidth="1" opacity=".45" />
        <rect x="21" y="9" width="22" height="10" rx="1" fill="#d97706" />
        <path d="M18.5 9.5L32 2l13.5 7.5z" fill="#92400e" />
        <rect x="24.5" y="11.5" width="4" height="4" rx=".7" fill="#fef3c7" />
        <rect x="30" y="11.5" width="4" height="4" rx=".7" fill="#fef3c7" />
        <rect x="35.5" y="11.5" width="4" height="4" rx=".7" fill="#fef3c7" />
        <path d="M2.5 38.5q4-3 8 0t8 0t8 0t8 0t8 0t8 0t8 0" fill="none" stroke="#3b82f6" strokeWidth="2" strokeLinecap="round" />
      </svg>
      <span className="text-xl font-bold tracking-wide text-slate-800">{tr("Ковчег")}</span>
    </a>
  )
}
