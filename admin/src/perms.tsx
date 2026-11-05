// Права текущего сеанса (me.perms: view, edit, restore, admin). Сервер проверяет каждый запрос сам —
// здесь только скрытие недоступных разделов и кнопок.
import { createContext, useContext } from 'react'
import { tr } from './i18n'

const KEY = 'hopebackup_perms'

export const ROLES: { id: string; label: string; hint: string }[] = [
  { id: 'admin', label: tr("Администратор"), hint: tr("всё, включая пользователей") },
  { id: 'operator', label: tr("Оператор"), hint: tr("просмотр и изменения, без пользователей") },
  { id: 'restore', label: tr("Восстановление"), hint: tr("просмотр и запуск восстановления") },
  { id: 'auditor', label: tr("Аудитор"), hint: tr("только просмотр") },
]
export const roleLabel = (r: string) => ROLES.find(x => x.id === r)?.label || r

/** Права по роли — пока не пришёл /api/auth/me. */
export const permsOfRole = (role: string): string[] => ({
  admin: ['view', 'edit', 'restore', 'admin'], operator: ['view', 'edit'], restore: ['view', 'restore'], auditor: ['view'],
} as Record<string, string[]>)[role] || ['view']

export const permStore = {
  get: (role: string): string[] => { try { const v = JSON.parse(localStorage.getItem(KEY) || 'null'); return Array.isArray(v) ? v : permsOfRole(role) } catch { return permsOfRole(role) } },
  set: (perms: string[]) => { try { localStorage.setItem(KEY, JSON.stringify(perms)) } catch { /* только на сеанс */ } },
}

export const PermsCtx = createContext<string[]>(['view'])
export function usePerms() {
  const perms = useContext(PermsCtx)
  const has = (p: string) => perms.includes(p) || perms.includes('admin')
  return { perms, edit: has('edit'), restore: has('restore'), admin: perms.includes('admin') }
}
