// «Сколько места понадобится» под образы машины: до первой копии — по описи дисков машины (её снимает сервер
// по SSH или агент на машине), после — точно по последней копии. Рядом — сколько свободно там, куда ляжут
// данные: хранилище площадки, локальная папка сервера управления, удалённые площадки.
import { useEffect, useState } from 'react'
import { CheckCircle2, HardDrive, Radar, XCircle } from 'lucide-react'
import { fmtBytes, fmtTime, post, q } from '../api'
import { Button, useApi, useToast } from './ui'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

type Check = { need: number; free: number | null; ok: boolean | null }
type Needs = {
  machine?: string | null; machine_id?: string; inventory?: boolean; time?: string; basis?: 'inventory' | 'copy' | 'none'
  disks?: { name: string; size: number; model?: string }[]; disk_total?: number; used?: number; estimate?: number; largest?: number
  copy_time?: string; copy_disks?: { name: string; size: number; used: number }[]; copy_error?: string
  repo?: Check & { site: string }
  local?: Check & { dir: string; keep: number; system: boolean; at: string | null }
  remote?: (Check & { site: string; keep: number })[]
}

/** Значение с задержкой (чтобы не считать на каждую букву пути). */
function useDebounced<T>(v: T, ms = 700): T {
  const [d, setD] = useState(v)
  useEffect(() => { const t = setTimeout(() => setD(v), ms); return () => clearTimeout(t) }, [JSON.stringify(v)])
  return d
}

function Row({ title, c, extra }: { title: React.ReactNode; c: Check; extra?: React.ReactNode }) {
  const bad = c.ok === false
  return (
    <div className={`flex items-start gap-2 ${bad ? 'text-red-700' : ''}`}>
      {c.ok === true ? <CheckCircle2 size={15} className="text-emerald-600 mt-0.5 shrink-0" /> : c.ok === false ? <XCircle size={15} className="text-red-600 mt-0.5 shrink-0" /> : <HardDrive size={15} className="text-slate-400 mt-0.5 shrink-0" />}
      <div>
        <div>{title}{tr(": нужно ~")}<b>{fmtBytes(c.need)}</b>{c.free != null ? <>  {tr("— свободно")} {fmtBytes(c.free)}{bad && <b>  {tr("— не хватит")}</b>}</> : <span className="text-slate-500">  {tr("— свободное место неизвестно")}</span>}</div>
        {extra}
      </div>
    </div>
  )
}

export function ImageNeeds({ mode, machineId, instanceId, siteId, dir, keep, keepRemote, sites, format }: {
  mode: 'machine' | 'qcow2'; machineId?: string; instanceId?: string; siteId?: string; dir?: string; keep?: number; keepRemote?: number; sites?: string[]; format?: string
}) {
  const perms = usePerms()
  const toast = useToast()
  const p = useDebounced({ machine_id: machineId || '', instance_id: instanceId || '', site_id: siteId || '', dir: dir || '', keep: keep || '', keep_remote: keepRemote || '', sites: (sites || []).join(','), format: format || '' })
  const ready = !!(p.machine_id || p.instance_id)
  const [inv, setInv] = useState(false)
  const { data, error, loading, reload } = useApi<Needs>(ready ? `/api/image/requirements${q(p)}` : null, [], inv ? 15000 : undefined)
  useEffect(() => { if (data?.basis && data.basis !== 'none') setInv(false) }, [data?.basis])
  if (!ready) return null
  const runInv = async () => {
    if (!data?.machine_id) return
    try { await post(`/api/machines/${encodeURIComponent(data.machine_id)}/inventory`, {}); setInv(true); toast.ok(tr("Опись машины запущена — оценка появится через минуту")); setTimeout(() => reload(), 20000) }
    catch (e) { toast.err(e) }
  }
  const disks = data?.basis === 'copy' ? data.copy_disks || [] : data?.disks || []
  return (
    <div className="text-[13px] rounded-lg border border-amber-200 bg-amber-50 text-amber-950 px-3 py-2.5 space-y-1.5">
      <div className="font-medium flex items-center gap-1.5"><HardDrive size={15} />{tr("Сколько места понадобится")}</div>
      {loading && !data ? <div className="text-amber-700">{tr("Считаю…")}</div>
        : error ? <div>{tr("Оценку получить не удалось:")} {error}{tr(". Объём всё равно проверяется перед началом — при нехватке места работа не начнётся.")}</div>
        : !data || data.basis === 'none' ? (
          <div>
            
            {tr("Нет описи машины")}{data?.machine ? <> «{data.machine}»</> : ''}  {tr("— по ней видно размер её дисков и сколько на них занято.")}
            {perms.edit && data?.machine_id && <div className="mt-1.5"><Button className="!py-1 text-xs" icon={<Radar size={14} />} loading={inv} onClick={runInv}>{tr("Снять опись машины")}</Button></div>}
          </div>
        ) : <>
          {data.basis === 'copy'
            ? <div>{tr("По последней копии от")} {fmtTime(data.copy_time)}{tr(": данные образа")} <b>{fmtBytes(data.estimate)}</b>.</div>
            : <div>{tr("По описи машины от")} {fmtTime(data.time)}{tr(": диски")} {fmtBytes(data.disk_total)}{tr(", занято на файловых системах")} {fmtBytes(data.used)}  {tr("→ образ займёт около")} <b>{fmtBytes(data.estimate)}</b>  {tr("(с запасом; больше — если на дисках остались данные удалённых файлов, но не больше")} {fmtBytes(data.disk_total)}{tr("). Точнее — после первой копии.")}</div>}
          {disks.length > 0 && <div className="text-amber-800">{disks.map(d => `${d.name}: ${fmtBytes(d.size)}${'used' in d ? trf(", данные ~{0}", fmtBytes((d as any).used)) : ''}`).join(' · ')}</div>}
          {data.copy_error && <div className="text-xs text-amber-700">{tr("Точная оценка по копии недоступна (")}{data.copy_error}{tr(") — показана по описи.")}</div>}
          <div className="pt-1 space-y-1">
            {mode === 'machine' && data.repo && <Row title={<>{tr("Хранилище площадки «")}{data.repo.site}{tr("» (первая копия, сжатие zstd уменьшит; дальше — только изменения)")}</>} c={data.repo} />}
            {mode === 'qcow2' && (data.local
              ? <Row title={<>{tr("Локальная папка")} <span className="font-mono">{data.local.dir}</span> ({data.local.keep}  {tr("набор")}{data.local.keep === 1 ? '' : data.local.keep < 5 ? tr("а") : tr("ов")}{(format || 'qcow2') !== 'raw' ? tr(" + временный файл на время сборки") : ''})</>} c={data.local}
                  extra={data.local.system ? <div className="text-xs text-red-700">{tr("Папка на системном диске сервера — образы могут занять его целиком. Подключите отдельный диск (например, смонтируйте его в /mnt/data).")}</div> : data.local.at && data.local.at !== data.local.dir ? <div className="text-xs text-amber-700">{tr("Папки ещё нет — будет создана в")} {data.local.at}.</div> : null} />
              : <div className="text-amber-800">{tr("Выберите локальную папку на подключённом диске — покажу, хватит ли места.")}</div>)}
            {mode === 'qcow2' && (data.remote || []).map(r => <Row key={r.site} title={<>{tr("Площадка «")}{r.site}» ({r.keep}  {tr("набор")}{r.keep === 1 ? '' : r.keep < 5 ? tr("а") : tr("ов")}  {tr("+ принимаемый)")}</>} c={r}
              extra={r.free == null ? <div className="text-xs text-slate-500">{tr("Нажмите «Проверить» у площадки — узнаем её свободное место.")}</div> : null} />)}
          </div>
        </>}
      <div className="text-xs text-amber-800 pt-1 border-t border-amber-200">
        {mode === 'qcow2'
          ? <>{tr("Образы собирает сам сервер управления, поэтому")} <b>{tr("к серверу должен быть подключён диск большого объёма")}</b>  {tr("(например, отдельный диск, смонтированный в /mnt/data): каждый хранимый набор занимает примерно столько, сколько данных на дисках машины. Файлы образов не шифруются.")}</>
          : <>{tr("Копии образа ложатся в хранилище площадки (у площадки «диск сервера» — на диск сервера управления):")} <b>{tr("для образов машин нужен диск большого объёма")}</b>{tr(". Следующие копии добавляют только изменившиеся блоки.")}</>}
      </div>
    </div>
  )
}
