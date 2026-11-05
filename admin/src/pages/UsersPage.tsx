import { useState } from 'react'
import { KeyRound, Mail, Pencil, Plus, Trash2, Users } from 'lucide-react'
import { auth, del, fmtTime, post, put } from '../api'
import type { User } from '../types'
import { Alert, Badge, Button, Card, Confirm, Empty, Field, IconButton, Input, Modal, PageHeader, Select, Spinner, Table, Td, useApi, useToast } from '../components/ui'
import { NoAccess } from '../components/Layout'
import { ROLES, roleLabel, usePerms } from '../perms'
import { tr, trf } from '../i18n'

const ROLE_TONE: Record<string, 'violet' | 'blue' | 'green' | 'slate'> = { admin: 'violet', operator: 'blue', restore: 'green', auditor: 'slate' }

export function UsersPage() {
  const perms = usePerms()
  if (!perms.admin) return <NoAccess />
  return <Users_ />
}

function Users_() {
  const toast = useToast()
  const { data, error, reload } = useApi<User[]>('/api/users')
  const [add, setAdd] = useState(false)
  const [pw, setPw] = useState<User | null>(null)
  const [edit, setEdit] = useState<User | null>(null)
  const [remove, setRemove] = useState<User | null>(null)
  const doRemove = async () => {
    const u = remove!; setRemove(null)
    try { await del(`/api/users/${encodeURIComponent(u.name)}`); toast.ok(trf("Пользователь «{0}» удалён", u.name)); reload() } catch (e) { toast.err(e) }
  }
  return (
    <div>
      <PageHeader title={tr("Пользователи")} icon={<Users size={22} />}
        subtitle={tr("Учётные записи панели и их роли. Сервер проверяет права роли при каждом запросе.")}
        actions={<Button kind="primary" icon={<Plus size={16} />} onClick={() => setAdd(true)}>{tr("Добавить пользователя")}</Button>} />
      <div className="grid grid-cols-1 xl:grid-cols-3 gap-6">
        <Card padded={false} className="xl:col-span-2">
          {error && !data ? <div className="p-5"><Alert tone="error">{error}</Alert></div> : !data ? <Spinner /> : data.length === 0 ? <Empty icon={<Users size={28} />} title={tr("Пользователей нет")} /> : (
            <Table head={[tr("Имя"), tr("Роль"), tr("Почта"), tr("Создан"), '']}>
              {data.map(u => (
                <tr key={u.name} className="hover:bg-slate-50">
                  <Td className="font-medium text-slate-900">{u.name}{u.name === auth.user() && <span className="ml-2 text-xs text-slate-400">{tr("это вы")}</span>}</Td>
                  <Td><Badge tone={ROLE_TONE[u.role] || 'slate'}>{roleLabel(u.role)}</Badge></Td>
                  <Td className="text-xs text-slate-600">{u.email ? <span className="inline-flex items-center gap-1"><Mail size={12} className="text-slate-400" />{u.email}</span> : <span className="text-slate-400">{tr("не указана")}</span>}</Td>
                  <Td className="text-xs text-slate-500">{fmtTime(u.created)}</Td>
                  <Td className="text-right whitespace-nowrap">
                    <IconButton title={tr("Роль и почта")} onClick={() => setEdit(u)}><Pencil size={16} /></IconButton>
                    <IconButton title={tr("Сменить пароль")} onClick={() => setPw(u)}><KeyRound size={16} /></IconButton>
                    <IconButton title={tr("Удалить")} danger disabled={u.name === auth.user()} onClick={() => setRemove(u)}><Trash2 size={16} /></IconButton>
                  </Td>
                </tr>
              ))}
            </Table>
          )}
        </Card>
        <Card title={tr("Роли")}>
          <ul className="space-y-3 text-sm">
            {ROLES.map(r => <li key={r.id}><Badge tone={ROLE_TONE[r.id]}>{r.label}</Badge><div className="text-slate-500 mt-1">{r.hint}</div></li>)}
          </ul>
          <div className="text-xs text-slate-400 mt-4 leading-relaxed">{tr("Право «Восстановление» отделено от «Изменения»: роль может поднимать копии, не трогая расписания и площадки.")}</div>
          <div className="text-xs text-slate-400 mt-3 leading-relaxed">{tr("На адрес почты пользователя приходит ссылка «Забыли пароль?» со страницы входа. Без адреса (или без настроенной почты) ссылка записывается на сервере управления в файл, доступный только root.")}</div>
        </Card>
      </div>
      {add && <AddModal onClose={() => setAdd(false)} onSaved={() => { setAdd(false); reload() }} />}
      {pw && <PasswordModal user={pw} onClose={() => setPw(null)} />}
      {edit && <EditModal user={edit} onClose={() => setEdit(null)} onSaved={() => { setEdit(null); reload() }} />}
      <Confirm open={!!remove} danger yes={tr("Удалить")} title={tr("Удалить пользователя?")} onNo={() => setRemove(null)} onYes={doRemove}
        text={<>{tr("Пользователь «")}<b>{remove?.name}</b>{tr("» потеряет доступ к панели; его открытые сеансы будут закрыты.")}</>} />
    </div>
  )
}

function AddModal({ onClose, onSaved }: { onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const [f, setF] = useState({ name: '', password: '', password2: '', role: 'operator', email: '' })
  const [busy, setBusy] = useState(false)
  const save = async () => {
    if (!f.name.trim()) return toast.err(tr("Укажите имя"))
    if (f.password.length < 8) return toast.err(tr("Пароль — не короче 8 символов"))
    if (f.password !== f.password2) return toast.err(tr("Пароли не совпадают"))
    setBusy(true)
    try { await post('/api/users', { name: f.name.trim(), password: f.password, role: f.role, email: f.email.trim() }); toast.ok(trf("Пользователь «{0}» добавлен", f.name.trim())); onSaved() }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open onClose={onClose} title={tr("Новый пользователь")}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{tr("Добавить")}</Button></>}>
      <div className="space-y-4">
        <Field label={tr("Имя")}><Input value={f.name} onChange={e => setF({ ...f, name: e.target.value })} autoFocus autoComplete="off" /></Field>
        <Field label={tr("Роль")} hint={ROLES.find(r => r.id === f.role)?.hint}>
          <Select value={f.role} onChange={e => setF({ ...f, role: e.target.value })}>
            {ROLES.map(r => <option key={r.id} value={r.id}>{r.label}</option>)}
          </Select>
        </Field>
        <Field label={tr("Почта")} hint={tr("Для ссылки сброса забытого пароля (необязательно)")}><Input type="email" value={f.email} onChange={e => setF({ ...f, email: e.target.value })} autoComplete="off" placeholder="ivanov@company.ru" /></Field>
        <Field label={tr("Пароль")}><Input type="password" autoComplete="new-password" value={f.password} onChange={e => setF({ ...f, password: e.target.value })} /></Field>
        <Field label={tr("Пароль ещё раз")}><Input type="password" autoComplete="new-password" value={f.password2} onChange={e => setF({ ...f, password2: e.target.value })} /></Field>
      </div>
    </Modal>
  )
}

function PasswordModal({ user, onClose }: { user: User; onClose: () => void }) {
  const toast = useToast()
  const [p, setP] = useState(['', ''])
  const [busy, setBusy] = useState(false)
  const save = async () => {
    if (p[0].length < 8) return toast.err(tr("Пароль — не короче 8 символов"))
    if (p[0] !== p[1]) return toast.err(tr("Пароли не совпадают"))
    setBusy(true)
    try {
      const r = await put<{ token?: string }>(`/api/users/${encodeURIComponent(user.name)}/password`, { password: p[0] })
      // Свой пароль: старые сеансы закрыты, этот продолжает работать с новым токеном.
      if (r.token) auth.set(r.token, auth.user(), auth.role())
      toast.ok(trf("Пароль «{0}» изменён, его открытые сеансы закрыты", user.name)); onClose()
    }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open onClose={onClose} title={trf("Сменить пароль «{0}»", user.name)}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{tr("Сменить")}</Button></>}>
      <div className="space-y-4">
        <Field label={tr("Новый пароль")}><Input type="password" autoComplete="new-password" value={p[0]} onChange={e => setP([e.target.value, p[1]])} autoFocus /></Field>
        <Field label={tr("Ещё раз")}><Input type="password" autoComplete="new-password" value={p[1]} onChange={e => setP([p[0], e.target.value])} /></Field>
      </div>
    </Modal>
  )
}

function EditModal({ user, onClose, onSaved }: { user: User; onClose: () => void; onSaved: () => void }) {
  const toast = useToast()
  const [f, setF] = useState({ role: user.role, email: user.email || '' })
  const [busy, setBusy] = useState(false)
  const save = async () => {
    setBusy(true)
    try { await put(`/api/users/${encodeURIComponent(user.name)}`, { role: f.role, email: f.email.trim() }); toast.ok(trf("Пользователь «{0}» изменён", user.name)); onSaved() }
    catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Modal open onClose={onClose} title={trf("Пользователь «{0}»", user.name)}
      footer={<><Button onClick={onClose}>{tr("Отмена")}</Button><Button kind="primary" loading={busy} onClick={save}>{tr("Сохранить")}</Button></>}>
      <div className="space-y-4">
        <Field label={tr("Роль")} hint={ROLES.find(r => r.id === f.role)?.hint}>
          <Select value={f.role} onChange={e => setF({ ...f, role: e.target.value })} disabled={user.name === auth.user()}>
            {ROLES.map(r => <option key={r.id} value={r.id}>{r.label}</option>)}
          </Select>
        </Field>
        <Field label={tr("Почта")} hint={tr("Сюда придёт ссылка сброса пароля; пусто — ссылка запишется в файл на сервере управления")}>
          <Input type="email" value={f.email} onChange={e => setF({ ...f, email: e.target.value })} autoComplete="off" placeholder="ivanov@company.ru" autoFocus />
        </Field>
      </div>
    </Modal>
  )
}
