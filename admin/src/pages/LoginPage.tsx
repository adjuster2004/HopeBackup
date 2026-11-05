import { useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'
import { Lock, Ship, User } from 'lucide-react'
import { api, auth } from '../api'
import { Alert, Button } from '../components/ui'
import { permsOfRole, permStore } from '../perms'
import { LangSwitch, tr } from '../i18n'

export function LoginPage() {
  const nav = useNavigate()
  const [u, setU] = useState('Administrator')
  const [p, setP] = useState('')
  const [err, setErr] = useState('')
  const [busy, setBusy] = useState(false)
  const submit = async (e: React.FormEvent) => {
    e.preventDefault(); setErr(''); setBusy(true)
    try {
      const r = await api<{ token: string; user: string; role: string }>('/api/auth/login', { method: 'POST', body: { user: u, password: p } })
      auth.set(r.token, r.user, r.role)
      permStore.set(permsOfRole(r.role))
      nav('/dashboard')
    } catch (e: any) { setErr(e.message) } finally { setBusy(false) }
  }
  return (
    <div className="relative min-h-screen flex items-center justify-center bg-gradient-to-br from-slate-900 via-slate-800 to-blue-950 p-4">
      <LangSwitch className="absolute top-4 right-4" />
      <form onSubmit={submit} className="w-full max-w-sm bg-white rounded-2xl shadow-2xl p-8">
        <div className="flex flex-col items-center mb-6">
          <div className="p-3 bg-blue-600 rounded-2xl text-white mb-3 shadow-lg"><Ship size={30} /></div>
          <h1 className="text-2xl font-bold text-slate-900">HopeBackup</h1>
          <p className="text-sm text-slate-500">{tr("Резервное копирование и восстановление")}</p>
        </div>
        {err && <div className="mb-4"><Alert tone="error">{err}</Alert></div>}
        <label className="block mb-3">
          <span className="text-sm font-medium text-slate-700">{tr("Пользователь")}</span>
          <div className="mt-1 flex items-center gap-2 px-3 py-2 border border-slate-300 rounded-lg focus-within:ring-2 focus-within:ring-blue-500/30 focus-within:border-blue-500">
            <User size={16} className="text-slate-400" />
            <input className="flex-1 outline-none text-sm" value={u} onChange={e => setU(e.target.value)} autoComplete="username" autoFocus />
          </div>
        </label>
        <label className="block mb-6">
          <span className="text-sm font-medium text-slate-700">{tr("Пароль")}</span>
          <div className="mt-1 flex items-center gap-2 px-3 py-2 border border-slate-300 rounded-lg focus-within:ring-2 focus-within:ring-blue-500/30 focus-within:border-blue-500">
            <Lock size={16} className="text-slate-400" />
            <input type="password" className="flex-1 outline-none text-sm" value={p} onChange={e => setP(e.target.value)} autoComplete="current-password" />
          </div>
        </label>
        <Button kind="primary" className="w-full py-2.5" loading={busy} type="submit">{tr("Войти")}</Button>
        <div className="text-center mt-4"><Link to="/recover" className="text-sm text-slate-500 hover:text-blue-600 hover:underline">{tr("Забыли пароль?")}</Link></div>
        <p className="text-xs text-slate-400 mt-5 text-center leading-relaxed">{tr("Учётная запись панели HopeBackup. Пароль первого входа — на сервере управления в файле")} <span className="font-mono break-all">/var/lib/hopebackup/server/initial_admin_password.txt</span></p>
      </form>
    </div>
  )
}
