import { useEffect, useState } from 'react'
import { CalendarClock, Filter, RotateCcw, Save, Settings as SettingsIcon } from 'lucide-react'
import { put } from '../api'
import type { RetryPolicy, Settings } from '../types'
import { RetryFields } from '../components/RetryFields'
import { Alert, Button, Card, Field, Input, PageHeader, Spinner, Textarea, Toggle, useApi, useToast } from '../components/ui'
import { usePerms } from '../perms'
import { tr } from '../i18n'

/** Частые исключения — одной кнопкой. */
const QUICK: [string, string][] = [
  ['/var/log', tr("журналы")], ['/var/cache', tr("кэш пакетов")], ['*.log', tr("файлы *.log")], ['node_modules', tr("зависимости Node.js")],
  ['.cache', tr("кэши в домашних каталогах")], ['/home/*/Downloads', tr("загрузки пользователей")], ['*.iso', tr("образы ISO")],
  ['/var/lib/docker', tr("слои образов Docker")], ['__pycache__', tr("байт-код Python")], ['.git/objects', tr("объекты git")],
]

export function SettingsPage() {
  const toast = useToast()
  const perms = usePerms()
  const { data, error, reload } = useApi<Settings>('/api/settings')
  const [text, setText] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  useEffect(() => { if (data && text === null) setText(data.settings.default_excludes.join('\n')) }, [data, text])
  const list = (text || '').split('\n').map(s => s.trim()).filter(Boolean)
  const add = (p: string) => { if (!list.includes(p)) setText([...list, p].join('\n')) }
  const save = async () => {
    setBusy(true)
    try { await put('/api/settings', { default_excludes: list }); toast.ok(tr("Исключения сохранены — действуют со следующей копии")); reload() } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <div>
      <PageHeader title={tr("Настройки")} icon={<SettingsIcon size={22} />} subtitle={tr("Общие параметры сервера управления.")} />
      {error && !data ? <Alert tone="error">{error}</Alert> : !data || text === null ? <Spinner /> : (<div className="space-y-6">
        <RetryCard data={data} onSaved={reload} />
        <Card title={tr("Исключения по умолчанию")} icon={<Filter size={18} className="text-blue-600" />}
          actions={perms.admin && <>
            <Button icon={<RotateCcw size={16} />} onClick={() => setText(data.default_excludes_builtin.join('\n'))}>{tr("Как при установке")}</Button>
            <Button kind="primary" icon={<Save size={16} />} loading={busy} onClick={save}>{tr("Сохранить")}</Button>
          </>}>
          <p className="text-sm text-slate-600 mb-3">
            
            {tr("Что не копировать ни у одного экземпляра файлового уровня («Файлы и каталоги», «Сервер Linux целиком»), если у экземпляра не выключено «Общие исключения сервера». У каждого экземпляра есть и свой список. Каталог, который указан у экземпляра явно, общие исключения не отменяют.")}
          </p>
          <div className="grid lg:grid-cols-[1fr_320px] gap-5">
            <div>
              <Textarea rows={16} value={text} disabled={!perms.admin} onChange={e => setText(e.target.value)} spellCheck={false} />
              <div className="text-xs text-slate-500 mt-1">{tr("по одному шаблону в строке · сейчас")} {list.length}</div>
            </div>
            <div className="space-y-4">
              <div>
                <div className="text-xs font-semibold uppercase tracking-wide text-slate-500 mb-2">{tr("Добавить")}</div>
                <div className="flex flex-wrap gap-1.5">
                  {QUICK.map(([p, t]) => (
                    <button key={p} disabled={!perms.admin || list.includes(p)} onClick={() => add(p)} title={t}
                      className="px-2 py-1 rounded-md border border-slate-200 text-xs font-mono hover:bg-slate-50 disabled:opacity-40 disabled:cursor-default">{p}</button>
                  ))}
                </div>
              </div>
              <div>
                <div className="text-xs font-semibold uppercase tracking-wide text-slate-500 mb-2">{tr("Шаблоны")}</div>
                <table className="w-full text-xs"><tbody>
                  {([['/var/log', tr("путь от корня и всё внутри")], ['/home/*/Downloads', tr("* — любое имя каталога")], ['*.log', tr("имя файла на любой глубине")], ['node_modules', tr("имя каталога на любой глубине")], ['build/tmp', tr("часть пути на любой глубине")], ['**/target/debug', tr("** — любое число каталогов")]] as [string, string][]).map(([p, t]) => (
                    <tr key={p} className="border-b border-slate-100 last:border-0"><td className="py-1.5 pr-3 font-mono text-slate-800 whitespace-nowrap align-top">{p}</td><td className="py-1.5 text-slate-600">{t}</td></tr>
                  ))}
                </tbody></table>
              </div>
              <Alert tone="info">{tr("Ограничение размера файла («Не копировать файлы больше, МБ») задаётся у экземпляра.")}</Alert>
            </div>
          </div>
        </Card>
      </div>)}
    </div>
  )
}

function RetryCard({ data, onSaved }: { data: Settings; onSaved: () => void }) {
  const toast = useToast()
  const perms = usePerms()
  const [retry, setRetry] = useState<RetryPolicy>(data.settings.retry)
  const [catchUp, setCatchUp] = useState(data.settings.catch_up)
  const [hours, setHours] = useState(data.settings.catch_up_hours)
  const [busy, setBusy] = useState(false)
  const dirty = JSON.stringify([retry, catchUp, hours]) !== JSON.stringify([data.settings.retry, data.settings.catch_up, data.settings.catch_up_hours])
  const save = async () => {
    setBusy(true)
    try { await put('/api/settings', { retry, catch_up: catchUp, catch_up_hours: hours }); toast.ok(tr("Настройки повторов сохранены")); onSaved() } catch (e) { toast.err(e) } finally { setBusy(false) }
  }
  return (
    <Card title={tr("Повторы и пропущенные запуски")} icon={<CalendarClock size={18} className="text-blue-600" />}
      actions={perms.admin && <Button kind="primary" icon={<Save size={16} />} loading={busy} disabled={!dirty} onClick={save}>{tr("Сохранить")}</Button>}>
      <div className="grid lg:grid-cols-2 gap-8">
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-1">{tr("Повторы неудачных копий")}</div>
          <p className="text-xs text-slate-500 mb-3">{tr("Для всех экземпляров, у которых не заданы свои (вкладка «Расписание и хранение»). Повтор отменяется, если копию уже снял другой запуск; если итог задания не дошёл до сервера, перед повтором Ковчег ищет снапшот этого задания на площадке. Письмо «Ошибка копирования» приходит, когда повторов больше не будет.")}</p>
          <RetryFields value={retry} onChange={setRetry} disabled={!perms.admin} />
        </div>
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-1">{tr("Пропущенные запуски")}</div>
          <p className="text-xs text-slate-500 mb-3">{tr("Если в момент запуска по расписанию сервер управления не работал (выключен, перезагружался, узел ещё не принял управление), копия запустится после старта.")}</p>
          <Toggle checked={catchUp} disabled={!perms.admin} onChange={setCatchUp} label={tr("Догонять пропущенные запуски")} />
          {catchUp && <Field label={tr("Если пропуск не старше, часов")} hint={tr("1–168; более старый пропуск ждёт следующего запуска по расписанию")} className="mt-3 max-w-xs">
            <Input type="number" min={1} max={168} disabled={!perms.admin} value={hours} onChange={e => setHours(Math.max(1, Math.min(168, Number(e.target.value) || 1)))} />
          </Field>}
        </div>
      </div>
    </Card>
  )
}
