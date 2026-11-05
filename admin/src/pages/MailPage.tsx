// Почта и уведомления: сервер исходящей почты (SMTP), получатели и события уведомлений, журнал отправки.
import { useEffect, useMemo, useState } from 'react'
import { BellRing, CheckCircle2, Mail, RefreshCw, Save, Send, XCircle } from 'lucide-react'
import { fmtTime, post, put } from '../api'
import { Alert, Badge, Button, Card, Check, Field, Input, PageHeader, Select, Spinner, Table, Td, Textarea, Toggle, useApi, useToast, RefreshButton } from '../components/ui'
import { NoAccess } from '../components/Layout'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

type Smtp = { host: string; port: number; security: string; username: string; has_password: boolean; from: string; from_name: string; panel_url: string; panel_url_default: string; ca_pem: string; insecure_tls: boolean }
type Ev = { id: string; group: string; title: string; text: string }
type LogEntry = { at: string; event: string; subject: string; to: string[]; ok: boolean; error: string }
type MailData = { mail: Smtp; configured: boolean; notify: { enabled: boolean; recipients: string[]; events: string[] }; catalog: Ev[]; log: LogEntry[] }

const PORTS: Record<string, number> = { starttls: 587, tls: 465, none: 25 }

export function MailPage() {
  const perms = usePerms()
  if (!perms.admin) return <NoAccess />
  return <Mail_ />
}

function Mail_() {
  const { data, error, reload } = useApi<MailData>('/api/mail')
  return (
    <div>
      <PageHeader title={tr("Почта и уведомления")} icon={<Mail size={22} />}
        subtitle={tr("Сервер исходящей почты организации: через него Ковчег присылает администраторам уведомления о копиях, агентах и площадках, а пользователям — ссылки для сброса забытого пароля.")}
        actions={<RefreshButton onClick={reload} />} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : (
        <div className="space-y-6">
          <SmtpCard smtp={data.mail} onSaved={reload} />
          <NotifyCard data={data} onSaved={reload} />
          <LogCard log={data.log || []} catalog={data.catalog || []} />
        </div>
      )}
    </div>
  )
}

function SmtpCard({ smtp, onSaved }: { smtp: Smtp; onSaved: () => void }) {
  const toast = useToast()
  const [f, setF] = useState({ ...smtp, password: '', clear_password: false })
  const [busy, setBusy] = useState(false)
  const [to, setTo] = useState('')
  const [testing, setTesting] = useState(false)
  const [res, setRes] = useState<{ ok: boolean; text: string } | null>(null)
  useEffect(() => { setF({ ...smtp, password: '', clear_password: false }) }, [smtp])
  const set = (k: string, v: any) => setF(x => ({ ...x, [k]: v }))
  const body = () => ({ host: f.host, port: Number(f.port) || 0, security: f.security, username: f.username, password: f.password, clear_password: f.clear_password,
    from: f.from, from_name: f.from_name, panel_url: f.panel_url, ca_pem: f.ca_pem, insecure_tls: f.insecure_tls })
  const save = async () => {
    setBusy(true)
    try { await put('/api/mail/smtp', body()); toast.ok(tr("Настройки почты сохранены")); onSaved() } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const test = async () => {
    setTesting(true); setRes(null)
    try { await post('/api/mail/test', { to: to.trim(), smtp: body() }); setRes({ ok: true, text: trf("Тестовое письмо отправлено на {0}. Проверьте ящик (и папку «Спам»).", to.trim()) }) }
    catch (e: any) { setRes({ ok: false, text: e.message }) } finally { setTesting(false) }
  }
  return (
    <Card collapsible="mail-smtp" title={<span className="flex items-center gap-2"><Mail size={18} className="text-blue-600" />{tr("Сервер исходящей почты (SMTP)")}</span>}
      actions={<Button kind="primary" icon={<Save size={16} />} loading={busy} onClick={save}>{tr("Сохранить")}</Button>}>
      <div className="grid grid-cols-1 md:grid-cols-6 gap-4">
        <Field label={tr("Сервер")} className="md:col-span-3" hint={tr("Имя или IP-адрес сервера почты организации (Exchange, Postfix, Яндекс 360, Mail.ru для бизнеса…)")}>
          <Input value={f.host} onChange={e => set('host', e.target.value)} placeholder="mail.company.ru" />
        </Field>
        <Field label={tr("Шифрование")} className="md:col-span-2">
          <Select value={f.security || 'starttls'} onChange={e => set('security', e.target.value)}>
            <option value="starttls">{tr("STARTTLS (порт 587)")}</option>
            <option value="tls">{tr("TLS (порт 465)")}</option>
            <option value="none">{tr("Без шифрования (порт 25)")}</option>
          </Select>
        </Field>
        <Field label={tr("Порт")}><Input type="number" value={f.port || ''} onChange={e => set('port', e.target.value)} placeholder={String(PORTS[f.security || 'starttls'])} /></Field>
        <Field label={tr("Пользователь")} className="md:col-span-3" hint={tr("Пусто — без входа (ретранслятор принимает почту с адреса сервера)")}>
          <Input value={f.username} onChange={e => set('username', e.target.value)} autoComplete="off" placeholder="hopebackup@company.ru" />
        </Field>
        <Field label={tr("Пароль")} className="md:col-span-3" hint={smtp.has_password ? <Check checked={f.clear_password} onChange={v => set('clear_password', v)} label={tr("удалить сохранённый пароль")} /> : undefined}>
          <Input type="password" value={f.password} onChange={e => set('password', e.target.value)} autoComplete="new-password" placeholder={smtp.has_password ? tr("сохранён — не менять") : ''} disabled={f.clear_password} />
        </Field>
        <Field label={tr("Адрес отправителя")} className="md:col-span-3"><Input value={f.from} onChange={e => set('from', e.target.value)} placeholder="hopebackup@company.ru" /></Field>
        <Field label={tr("Имя отправителя")} className="md:col-span-3"><Input value={f.from_name} onChange={e => set('from_name', e.target.value)} placeholder="HopeBackup" /></Field>
        <Field label={tr("Адрес панели в письмах")} className="md:col-span-6" hint={<>{tr("Для ссылок сброса пароля и ссылок на задания. Укажите имя, которое открывается у администраторов. Пусто —")} {smtp.panel_url_default}</>}>
          <Input value={f.panel_url} onChange={e => set('panel_url', e.target.value)} placeholder={smtp.panel_url_default} />
        </Field>
      </div>
      <details className="mt-4">
        <summary className="text-sm text-slate-600 cursor-pointer">{tr("Сертификат сервера почты")}</summary>
        <div className="mt-3 space-y-3">
          <Field label={tr("Доверенный сертификат ЦС (PEM)")} hint={tr("Если сертификат сервера почты выдан внутренним ЦС организации. Корневые ЦС Mozilla и системные сертификаты Linux доверены всегда.")}>
            <Textarea rows={4} value={f.ca_pem} onChange={e => set('ca_pem', e.target.value)} placeholder="-----BEGIN CERTIFICATE-----" className="text-xs" />
          </Field>
          <Toggle checked={f.insecure_tls} onChange={v => set('insecure_tls', v)} label={tr("Не проверять сертификат сервера почты")} hint={tr("Только для проверки: письма шифруются, но подлинность сервера не подтверждается.")} />
        </div>
      </details>
      <div className="mt-5 pt-4 border-t border-slate-100">
        <div className="flex flex-wrap items-end gap-3">
          <Field label={tr("Проверка: отправить тестовое письмо на")} className="flex-1 min-w-64"><Input value={to} onChange={e => setTo(e.target.value)} placeholder="admin@company.ru" /></Field>
          <Button icon={<Send size={16} />} loading={testing} disabled={!to.trim() || !f.host.trim()} onClick={test}>{tr("Отправить тестовое письмо")}</Button>
        </div>
        <p className="text-xs text-slate-400 mt-1">{tr("Письмо уходит с параметрами, введёнными в форме, — их можно проверить до сохранения.")}</p>
        {res && <div className="mt-3"><Alert tone={res.ok ? 'success' : 'error'}>{res.text}</Alert></div>}
      </div>
    </Card>
  )
}

function NotifyCard({ data, onSaved }: { data: MailData; onSaved: () => void }) {
  const toast = useToast()
  const catalog = data.catalog || []
  const [enabled, setEnabled] = useState<boolean>(data.notify.enabled)
  const [rcpt, setRcpt] = useState<string>((data.notify.recipients || []).join('\n'))
  const [events, setEvents] = useState<string[]>(data.notify.events || [])
  const [busy, setBusy] = useState(false)
  const [testing, setTesting] = useState(false)
  useEffect(() => { setEnabled(data.notify.enabled); setRcpt((data.notify.recipients || []).join('\n')); setEvents(data.notify.events || []) }, [data.notify])
  const groups = useMemo(() => {
    const m: Record<string, Ev[]> = {}
    for (const e of catalog) (m[e.group] ||= []).push(e)
    return Object.entries(m)
  }, [catalog])
  const recipients = rcpt.split(/[\s,;]+/).map(x => x.trim()).filter(Boolean)
  const save = async () => {
    setBusy(true)
    try { await put('/api/mail/notify', { enabled, recipients, events }); toast.ok(tr("Настройки уведомлений сохранены")); onSaved() } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  const test = async () => {
    setTesting(true)
    try { const r = await post<{ to: string[] }>('/api/mail/test-notify'); toast.ok(trf("Проверочное уведомление отправлено: {0}", (r.to || []).join(', '))); onSaved() } catch (e) { toast.err(e); onSaved() } finally { setTesting(false) }
  }
  const toggle = (id: string, v: boolean) => setEvents(x => v ? [...x.filter(y => y !== id), id] : x.filter(y => y !== id))
  return (
    <Card collapsible="mail-notify" title={<span className="flex items-center gap-2"><BellRing size={18} className="text-blue-600" />{tr("Уведомления")}</span>}
      actions={<>
        <Button icon={<Send size={16} />} loading={testing} disabled={!data.notify.recipients?.length} onClick={test} title={tr("Отправить сохранённым получателям письмо со списком выбранных событий")}>{tr("Проверить")}</Button>
        <Button kind="primary" icon={<Save size={16} />} loading={busy} onClick={save}>{tr("Сохранить")}</Button>
      </>}>
      {!data.configured && <div className="mb-4"><Alert tone="warn">{tr("Сервер исходящей почты не настроен — уведомления не будут отправляться, пока не заданы сервер и адрес отправителя.")}</Alert></div>}
      <Toggle checked={enabled} onChange={setEnabled} label={tr("Отправлять уведомления по почте")} hint={tr("Письма уходят сразу при событии; о том же состоянии повторно не пишем, пока оно не изменится. Не больше 20 писем в час по одному событию.")} />
      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6 mt-5">
        <Field label={tr("Получатели")} hint={tr("По одному адресу в строке (или через запятую). Например, адрес группы администраторов.")}>
          <Textarea rows={6} value={rcpt} onChange={e => setRcpt(e.target.value)} placeholder={'admin@company.ru\nit-duty@company.ru'} className="text-sm" />
        </Field>
        <div className="lg:col-span-2">
          <div className="flex items-center justify-between mb-2">
            <span className="text-sm font-medium text-slate-700">{tr("Что отправлять")}</span>
            <span className="flex gap-3 text-xs">
              <button type="button" className="text-blue-600 hover:underline" onClick={() => setEvents(catalog.map(e => e.id))}>{tr("Выбрать все")}</button>
              <button type="button" className="text-slate-500 hover:underline" onClick={() => setEvents([])}>{tr("Снять все")}</button>
            </span>
          </div>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {groups.map(([g, list]) => (
              <div key={g} className="rounded-lg border border-slate-200 p-3">
                <div className="text-[11px] font-semibold uppercase tracking-wider text-slate-400 mb-2">{g}</div>
                <div className="space-y-2">
                  {list.map(e => (
                    <div key={e.id}>
                      <Check checked={events.includes(e.id)} onChange={v => toggle(e.id, v)} label={<span className="font-medium">{e.title}</span>} />
                      <p className="text-xs text-slate-500 ml-6 leading-snug">{e.text}</p>
                    </div>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>
    </Card>
  )
}

function LogCard({ log, catalog }: { log: LogEntry[]; catalog: Ev[] }) {
  const title = (id: string) => id === 'test' ? tr("Проверка уведомлений") : catalog.find(e => e.id === id)?.title || id
  return (
    <Card padded={false} title={tr("Журнал отправки")}>
      <Table head={[tr("Время"), tr("Тема"), tr("Кому"), tr("Результат")]} empty={tr("Писем пока не было")}>
        {log.slice(0, 50).map((e, i) => (
          <tr key={i} className="hover:bg-slate-50 align-top">
            <Td className="whitespace-nowrap text-xs text-slate-500">{fmtTime(e.at)}</Td>
            <Td className="text-sm">{e.subject}<div className="text-[11px] text-slate-400">{title(e.event)}</div></Td>
            <Td className="text-xs text-slate-600">{(e.to || []).join(', ')}</Td>
            <Td className="text-xs">{e.ok ? <Badge tone="green"><CheckCircle2 size={11} />{tr("отправлено")}</Badge> : <span className="text-red-600 flex items-start gap-1"><XCircle size={13} className="shrink-0 mt-0.5" />{e.error}</span>}</Td>
          </tr>
        ))}
      </Table>
    </Card>
  )
}
