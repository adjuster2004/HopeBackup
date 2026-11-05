import { useEffect, useState } from 'react'
import { ChevronLeft, ChevronRight, ChevronsLeft, ChevronsRight, Download, ScrollText } from 'lucide-react'
import { auth, fmtTime, netError, q } from '../api'
import type { AuditEntry } from '../types'
import { Alert, Badge, Button, Card, Empty, Input, PageHeader, Select, Spinner, Table, Td, useApi, useToast, RefreshButton } from '../components/ui'
import { tr, trf } from '../i18n'

const tone = (a: string) => /fail|error|denied|отказ|ошиб/i.test(a) ? 'red' : /delete|remove|удал/i.test(a) ? 'amber' : /restore|восстан/i.test(a) ? 'violet' : /login|вход/i.test(a) ? 'green' : 'blue'

type Page = { items: AuditEntry[]; total: number; offset: number; limit: number }

/** Дата из поля <input type="date"> → RFC 3339 (начало или конец дня по местному времени). */
const day = (d: string, end: boolean) => d ? new Date(`${d}T${end ? '23:59:59.999' : '00:00:00'}`).toISOString() : ''

export function AuditPage() {
  const toast = useToast()
  const [size, setSize] = useState(100)
  const [page, setPage] = useState(0)
  const [text, setText] = useState('')
  const [search, setSearch] = useState('')
  const [from, setFrom] = useState('')
  const [to, setTo] = useState('')
  const [busy, setBusy] = useState('')
  // Поиск — по всему журналу на сервере; запрос — после паузы в наборе.
  useEffect(() => { const t = setTimeout(() => { setSearch(text.trim()); setPage(0) }, 350); return () => clearTimeout(t) }, [text])
  const filter = { q: search, from: day(from, false), to: day(to, true) }
  const { data, error, reload } = useApi<Page>(`/api/audit/page${q({ ...filter, offset: page * size, limit: size })}`, [], 15000)
  const total = data?.total ?? 0
  const pages = Math.max(1, Math.ceil(total / size))
  useEffect(() => { if (page > 0 && page >= pages) setPage(pages - 1) }, [page, pages])
  const go = (p: number) => setPage(Math.min(Math.max(0, p), pages - 1))

  const download = async (format: 'csv' | 'json') => {
    setBusy(format)
    try {
      let res: Response
      try { res = await fetch(`/api/audit/export${q({ ...filter, format })}`, { headers: { Authorization: `Bearer ${auth.token()}` } }) } catch (e) { throw netError(e) }
      if (!res.ok) { const j = await res.json().catch(() => ({})); throw new Error(j.error || `HTTP ${res.status}`) }
      const name = /filename="([^"]+)"/.exec(res.headers.get('content-disposition') || '')?.[1] || `audit.${format}`
      const url = URL.createObjectURL(await res.blob())
      const a = document.createElement('a'); a.href = url; a.download = name; document.body.appendChild(a); a.click(); a.remove()
      setTimeout(() => URL.revokeObjectURL(url), 5000)
      toast.ok(trf("Журнал выгружен: {0}", name))
    } catch (e) { toast.err(e) } finally { setBusy('') }
  }

  const first = total === 0 ? 0 : page * size + 1
  const last = Math.min(total, (page + 1) * size)
  const filtered = !!(search || from || to)
  const pager = (
    <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3 border-t border-slate-100 text-sm">
      <span className="text-slate-500">{total ? <>{tr("записи")} {first}–{last}  {tr("из")} {total}{filtered ? tr(" (по фильтру)") : ''}</> : tr("нет записей")}</span>
      <div className="flex items-center gap-1">
        <Button className="!px-2 !py-1.5" title={tr("Первая страница")} disabled={page === 0} onClick={() => go(0)}><ChevronsLeft size={16} /></Button>
        <Button className="!px-2 !py-1.5" title={tr("Предыдущая")} disabled={page === 0} onClick={() => go(page - 1)}><ChevronLeft size={16} /></Button>
        <span className="px-3 text-slate-600 whitespace-nowrap">{tr("стр.")} {page + 1}  {tr("из")} {pages}</span>
        <Button className="!px-2 !py-1.5" title={tr("Следующая")} disabled={page >= pages - 1} onClick={() => go(page + 1)}><ChevronRight size={16} /></Button>
        <Button className="!px-2 !py-1.5" title={tr("Последняя страница")} disabled={page >= pages - 1} onClick={() => go(pages - 1)}><ChevronsRight size={16} /></Button>
      </div>
    </div>
  )
  return (
    <div>
      <PageHeader title={tr("Журнал аудита")} icon={<ScrollText size={22} />}
        subtitle={tr("Входы в панель и все изменения: продукты, площадки, коннекторы, агенты, восстановления и пользователи.")}
        actions={<>
          <Button icon={<Download size={16} />} loading={busy === 'csv'} onClick={() => download('csv')} title={tr("Таблица для Excel: UTF-8, разделитель «;»")}>{tr("Скачать CSV")}</Button>
          <Button icon={<Download size={16} />} loading={busy === 'json'} onClick={() => download('json')}>JSON</Button>
          <RefreshButton onClick={reload} />
        </>} />
      <Card className="mb-4">
        <div className="grid grid-cols-1 md:grid-cols-[1fr_auto_auto_auto] gap-3 items-center">
          <Input placeholder={tr("Пользователь, действие, подробности…")} value={text} onChange={e => setText(e.target.value)} />
          <div className="flex items-center gap-2 text-sm text-slate-500"><span>{tr("с")}</span><Input type="date" className="w-40" value={from} onChange={e => { setFrom(e.target.value); setPage(0) }} /></div>
          <div className="flex items-center gap-2 text-sm text-slate-500"><span>{tr("по")}</span><Input type="date" className="w-40" value={to} onChange={e => { setTo(e.target.value); setPage(0) }} /></div>
          <Select className="w-40" value={size} onChange={e => { setSize(+e.target.value); setPage(0) }}>
            {[50, 100, 200, 500].map(n => <option key={n} value={n}>{tr("по")} {n}  {tr("на стр.")}</option>)}
          </Select>
        </div>
        <div className="text-xs text-slate-400 mt-2">{tr("Поиск и период — по всему журналу; «Скачать» выгружает все записи под фильтром, не только эту страницу.")}</div>
      </Card>
      {error && !data ? <Alert tone="error">{error}</Alert> : !data ? <Spinner /> : (
        <Card padded={false}>
          {data.items.length === 0 ? <Empty icon={<ScrollText size={28} />} title={filtered ? tr("Под фильтр ничего не попало") : tr("Событий нет")} /> : (
            <Table head={[tr("Время"), tr("Пользователь"), tr("Действие"), tr("Подробности")]}>
              {data.items.map((e, i) => (
                <tr key={page * size + i} className="hover:bg-slate-50 align-top">
                  <Td className="text-xs whitespace-nowrap">{fmtTime(e.time, true)}</Td>
                  <Td className="font-medium">{e.user || '—'}</Td>
                  <Td><Badge tone={tone(e.action) as any}>{e.action}</Badge></Td>
                  <Td className="text-xs text-slate-600 break-all max-w-2xl">{e.detail}</Td>
                </tr>
              ))}
            </Table>
          )}
          {pager}
        </Card>
      )}
    </div>
  )
}
