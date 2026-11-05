// Закладка «Репликация» карточки продукта: куда ещё отправлять копии этого экземпляра.
// Зашифрованные копии — блоки как есть (ключ не нужен), у «Образов qcow2» — готовые файлы qcow2.
// По умолчанию — никуда; включённая площадка сразу настроена «после выполнения задания».
// Репликация площадки целиком (настройка площадки «Принимающие площадки») работает как раньше — здесь видно её состояние.
import { useEffect, useState } from 'react'
import { ArrowRightLeft, CheckCircle2, Copy, Save, Send, X, XCircle } from 'lucide-react'
import { fmtAgo, fmtBytes, fmtTime, post, put } from '../api'
import { siteReplExcluded, type Instance, type ReplStatus, type ReplTarget, type Site } from '../types'
import { Alert, Badge, Button, Card, Field, Input, Select, Spinner, Toggle, useApi, useToast } from './ui'
import { JobBadge, notifyInstances } from './common'
import { CRON_PRESETS } from './InstanceFields'
import { usePerms } from '../perms'
import { tr, trf } from '../i18n'

const blank = (site_id: string): ReplTarget => ({ site_id, enabled: false, when: 'after_job', cron: '0 3 * * *', keep: 0 })

export function ReplicationTab({ inst, openJob }: { inst: Instance; openJob: (id: string) => void }) {
  const perms = usePerms()
  const toast = useToast()
  const { data: st, reload, setData } = useApi<ReplStatus>(`/api/instances/${encodeURIComponent(inst.id)}/replication`, [inst.id], 10000)
  const { data: sites } = useApi<Site[]>('/api/sites', [], 15000)
  const [v, setV] = useState<ReplTarget[] | null>(null)
  const [busy, setBusy] = useState('')
  // Передавать ли продукт при репликации площадки целиком (null — ещё не загружено).
  const [sr, setSr] = useState<boolean | null>(null)
  const qcow = inst.plugin === 'qcow2images'
  useEffect(() => { if (st && v === null) setV(st.targets || []) }, [st])
  useEffect(() => { if (st && sr === null) setSr(!st.site_repl_excluded) }, [st])
  if (!st || !sites || v === null || sr === null) return <Spinner />
  const own = sites.find(x => x.id === inst.site_id)
  // Куда можно: для зашифрованных копий — любые площадки, кроме своей; для qcow2 — узлы HopeBackup.
  const avail = sites.filter(x => qcow ? x.kind === 'node' : x.id !== inst.site_id)
  const get = (id: string) => v.find(t => t.site_id === id) || blank(id)
  const set = (id: string, patch: Partial<ReplTarget>) => setV(prev => { const cur = (prev || []).find(t => t.site_id === id) || blank(id); return [...(prev || []).filter(t => t.site_id !== id), { ...cur, ...patch }] })
  const srChanged = !qcow && sr !== !st.site_repl_excluded
  const dirty = srChanged || JSON.stringify((v || []).filter(t => t.enabled || (st.targets || []).some(x => x.site_id === t.site_id)).sort((a, b) => a.site_id.localeCompare(b.site_id))) !== JSON.stringify([...(st.targets || [])].sort((a, b) => a.site_id.localeCompare(b.site_id)))
  const save = async () => {
    setBusy('save')
    try { const r = await put<ReplStatus>(`/api/instances/${encodeURIComponent(inst.id)}/replication`, { targets: v.filter(t => t.enabled || (st.targets || []).some(x => x.site_id === t.site_id)), ...(srChanged ? { site_repl: sr } : {}) }); setData(r); setV(r.targets || []); setSr(!r.site_repl_excluded); notifyInstances(); toast.ok(tr("Репликация сохранена")) }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const now = async (site: string) => {
    setBusy('run:' + site)
    try { const r = await post<{ job_id: string }>(`/api/instances/${encodeURIComponent(inst.id)}/replicate`, { site_id: site }); openJob(r.job_id); toast.ok(tr("Репликация запущена")); reload() }
    catch (e) { toast.err(e) } finally { setBusy('') }
  }
  const lastCopy = inst.runtime?.last_backup?.time
  return (
    <div className="space-y-4 max-w-5xl">
      <Card title={<span className="flex items-center gap-2"><Send size={18} className="text-blue-600" />{qcow ? tr("Готовые qcow2 — на площадки") : tr("Копии этого продукта — на другие площадки")}</span>}
        actions={perms.edit && <Button kind="primary" icon={<Save size={16} />} disabled={!dirty} loading={busy === 'save'} onClick={save}>{tr("Сохранить")}</Button>}>
        <div className="text-sm text-slate-600 mb-4">
          {qcow
            ? <>{tr("После каждой сборки последний набор qcow2 отправляется на отмеченные узлы HopeBackup и лежит там открыто, рядом с репозиторием (каталог")} <span className="font-mono">images</span>{tr(") — его можно сразу подключить к гипервизору. Первый раз — целиком, дальше — только изменившиеся участки.")}</>
            : <>{tr("Копии этого экземпляра уходят на отмеченные площадки как есть — сжатыми и зашифрованными (пароль репозитория не нужен); блоки, которые там уже есть, не передаются. Восстановить можно с любой из них: «Мастер восстановления» → площадка-источник.")}</>}
          {' '}{tr("По умолчанию — никуда. Отметьте площадку: отправка сразу настроена «после выполнения задания»; можно выбрать своё расписание.")}
        </div>
        {avail.length === 0 ? <Alert tone="info">{qcow ? tr("Узлов HopeBackup нет — добавьте площадку-узел.") : tr("Других площадок нет — добавьте площадку, чтобы реплицировать копии.")}</Alert> : (
          <div className="space-y-3">
            {avail.map(site => {
              const t = get(site.id)
              const last = st.last[site.id]
              const ok = last?.status === 'ok'
              return (
                <div key={site.id} className={`rounded-xl border px-4 py-3 ${t.enabled ? 'border-blue-200 bg-blue-50/40' : 'border-slate-200'}`}>
                  <div className="flex flex-wrap items-center gap-3 justify-between">
                    <Toggle checked={t.enabled} disabled={!perms.edit} onChange={on => set(site.id, { enabled: on, when: t.when || 'after_job' })}
                      label={<span><span className="font-semibold">{site.name}</span> <span className="font-mono text-xs text-slate-400">{site.kind === 'node' ? site.url : site.kind}</span>{site.id === inst.site_id && <span className="text-xs text-slate-500">  {tr("· площадка источника")}</span>}</span>}
                      hint={site.node_disk ? trf("свободно {0} из {1}", fmtBytes(site.node_disk.free), fmtBytes(site.node_disk.total)) : undefined} />
                    <div className="flex items-center gap-2">
                      {last && <span className="flex items-center gap-2 text-xs text-slate-500"><JobBadge status={last.status} />{last.finished ? fmtAgo(last.finished) : last.started ? trf("с {0}", fmtTime(last.started)) : ''}
                        <button className="text-blue-600 hover:underline" onClick={() => openJob(last.job_id)}>{tr("журнал")}</button></span>}
                      {perms.edit && t.enabled && (st.targets || []).some(x => x.site_id === site.id && x.enabled) && <Button className="!py-1 text-xs" icon={<ArrowRightLeft size={14} />} loading={busy === 'run:' + site.id} onClick={() => now(site.id)}>{tr("Реплицировать сейчас")}</Button>}
                    </div>
                  </div>
                  {t.enabled && (
                    <div className="grid gap-3 sm:grid-cols-3 mt-3">
                      <Field label={tr("Когда")}>
                        <Select value={t.when} disabled={!perms.edit} onChange={e => set(site.id, { when: e.target.value as ReplTarget['when'] })}>
                          <option value="after_job">{tr("Сразу после выполнения задания")}</option>
                          <option value="cron">{tr("По расписанию")}</option>
                        </Select>
                      </Field>
                      {t.when === 'cron' ? (
                        <Field label={tr("Расписание (cron)")} hint={<span className="flex flex-wrap gap-1 mt-1">{CRON_PRESETS.map(([c, l]) => <button key={c} type="button" className="text-[11px] px-1.5 py-0.5 rounded border border-slate-200 hover:border-slate-300" onClick={() => set(site.id, { cron: c })}>{l}</button>)}</span>}>
                          <Input className="font-mono text-[13px]" value={t.cron} disabled={!perms.edit} onChange={e => set(site.id, { cron: e.target.value })} placeholder="0 3 * * *" />
                        </Field>
                      ) : <div className="text-xs text-slate-500 self-center">{qcow ? tr("после каждой сборки образов") : tr("после каждой успешной копии этого продукта")}</div>}
                      <Field label={qcow ? tr("Хранить наборов на площадке") : tr("Хранить копий на площадке")} hint={tr("Старое удаляется, только если у площадки задан токен очистки")}>
                        <Input type="number" min={0} value={t.keep || ''} placeholder={trf("{0} (как {1})", st.keep_default, qcow ? tr("по умолчанию") : tr("у продукта"))} disabled={!perms.edit} onChange={e => set(site.id, { keep: Number(e.target.value) || 0 })} />
                      </Field>
                    </div>
                  )}
                  {last?.status === 'ok' && last.result && <div className="text-xs text-slate-500 mt-2 flex items-center gap-1.5"><CheckCircle2 size={13} className="text-emerald-600" />
                    {qcow ? (last.result.already ? trf("набор {0} уже был на площадке", last.result.set) : trf("набор {0}: изменилось {1} из {2} участков, по каналу {3}", last.result.set, last.result.changed, last.result.chunks, fmtBytes(last.result.sent)))
                      : trf("передано копий {0}, блоков {1} ({2})", last.result.snapshots, last.result.chunks, fmtBytes(last.result.bytes))}</div>}
                  {last?.status === 'failed' && <div className="text-xs text-red-700 mt-2 flex items-start gap-1.5"><XCircle size={13} className="mt-0.5 shrink-0" />{last.error}</div>}
                  {!ok && !last && t.enabled && (st.targets || []).some(x => x.site_id === site.id && x.enabled) && <div className="text-xs text-slate-500 mt-2">{tr("Ещё не выполнялась — запустится")} {t.when === 'cron' ? trf("по расписанию {0}", t.cron) : tr("после следующего задания")}  {tr("или по кнопке «Реплицировать сейчас».")}</div>}
                </div>
              )
            })}
          </div>
        )}
      </Card>

      {!qcow && (
        <Card title={<span className="flex items-center gap-2"><Copy size={18} className="text-slate-500" />{tr("Репликация площадки «")}{own?.name || inst.site_id}{tr("» целиком")}</span>}>
          {st.site_level.length === 0 ? (
            <div className="text-sm text-slate-500">{tr("У площадки «")}{own?.name || inst.site_id}{tr("» нет принимающих площадок. Её можно настроить на странице «Площадки» — тогда туда уходит весь репозиторий площадки (все продукты).")}</div>
          ) : (
            <div className="space-y-2">
              <div className="text-sm text-slate-600">{tr("Настройка площадки (страница «Площадки» → «Принимающие площадки»): репозиторий площадки копируется на принимающие площадки после каждой копии и каждые")} {own?.replication_interval_min || 10}  {tr("мин.")}</div>
              <div className={`rounded-lg border px-3 py-2.5 ${sr ? 'border-slate-200' : 'border-amber-200 bg-amber-50/50'}`}>
                <Toggle checked={sr} disabled={!perms.edit} onChange={setSr} label={<span className="font-medium">{tr("Передавать этот продукт при репликации площадки")}</span>}
                  hint={sr ? tr("Копии продукта уходят на принимающие площадки вместе со всем репозиторием.")
                    : tr("Копии этого продукта (и его блоки) на принимающие площадки не передаются; остальные продукты площадки — передаются. Уже переданные копии там остаются и удаляются по хранению. Отправить продукт на нужную площадку можно своей репликацией выше.")} />
                {inst.plugin === 'machine' && inst.site_repl == null && <div className="text-xs text-slate-500 mt-1.5 ml-12">{tr("Образы машин по умолчанию не передаются: они большие и нужны обычно на одной площадке.")}</div>}
              </div>
              {st.site_level.map(r => {
                const rs = own?.replication?.find(x => x.site === r.id)?.state
                const synced = rs?.last_ok || (rs?.ok ? rs.finished : null)
                const has = synced && lastCopy && new Date(synced) >= new Date(lastCopy)
                return (
                  <div key={r.id} className="flex flex-wrap items-center gap-3 rounded-lg border border-slate-200 px-3 py-2 text-sm">
                    <span className="font-medium">{r.name}</span>
                    {rs?.running ? <Badge tone="blue">{tr("идёт синхронизация")}{rs.progress ? ` · ${rs.progress}` : ''}</Badge>
                      : rs?.ok === false ? <Badge tone="red">{tr("ошибка")}</Badge>
                      : synced ? <Badge tone="green">{tr("синхронизирована")} {fmtTime(synced)}</Badge> : <Badge tone="slate">{tr("ещё не синхронизировалась")}</Badge>}
                    {!sr ? <span className="text-xs text-slate-500">{tr("этот продукт не передаётся")}</span>
                      : lastCopy && <span className={`text-xs ${has ? 'text-emerald-700' : 'text-amber-700'}`}>{has ? trf("последняя копия продукта ({0}) уже там", fmtTime(lastCopy)) : trf("последняя копия продукта ({0}) попадёт туда при следующей синхронизации", fmtTime(lastCopy))}</span>}
                    {rs?.ok === false && rs.error && <span className="text-xs text-red-700 w-full">{rs.error}</span>}
                  </div>
                )
              })}
            </div>
          )}
        </Card>
      )}
    </div>
  )
}

const NOTE_KEY = 'hopebackup_repl_note_'
/**
 * Уведомление в карточке образа (образ машины, «Образы qcow2»): копии никуда не реплицируются — и где это включить.
 * Можно скрыть (в браузере, до изменения состояния).
 */
export function ImageReplNotice({ inst, sites, open }: { inst: Instance; sites: Site[] | null; open: () => void }) {
  const [, force] = useState(0)
  if (!sites || (inst.plugin !== 'machine' && inst.plugin !== 'qcow2images')) return null
  if ((inst.replicate || []).some(t => t.enabled)) return null
  const site = sites.find(x => x.id === inst.site_id)
  const others = sites.filter(x => x.id !== inst.site_id && (inst.plugin !== 'qcow2images' || x.kind === 'node'))
  let tone: 'warn' | 'info', title: string, text: string
  if (inst.plugin === 'machine') {
    const viaSite = (site?.replicas || []).length > 0
    if (viaSite && !siteReplExcluded(inst)) return null
    if (!viaSite && others.length === 0) return null
    tone = viaSite ? 'warn' : 'info'
    title = viaSite ? tr("Репликация образа на другие площадки остановлена") : tr("Образ хранится только на одной площадке")
    text = viaSite ? trf("Копии образа остаются на площадке «{0}»: при репликации площадки целиком этот продукт не передаётся, своя репликация не включена.", site?.name || inst.site_id)
      : trf("Копии образа хранятся только на площадке «{0}».", site?.name || inst.site_id)
  } else {
    if (others.length === 0) return null
    const paused = (inst.replicate || []).length > 0 && !inst.repl_set
    tone = paused ? 'warn' : 'info'
    title = paused ? tr("Репликация образов остановлена") : tr("Готовые qcow2 — только на площадке источника")
    text = paused ? tr("Готовые qcow2 больше не отправляются на другие площадки автоматически.") : tr("Готовые qcow2 на другие узлы HopeBackup не отправляются.")
  }
  const key = NOTE_KEY + inst.id
  const sig = `${tone}|${title}`
  try { if (localStorage.getItem(key) === sig) return null } catch { /* */ }
  const hide = () => { try { localStorage.setItem(key, sig) } catch { /* */ } force(x => x + 1) }
  return (
    <div className={`mb-4 rounded-xl border px-4 py-3 text-sm flex items-start gap-3 ${tone === 'warn' ? 'border-amber-200 bg-amber-50 text-amber-900' : 'border-blue-200 bg-blue-50 text-blue-900'}`}>
      <Send size={16} className="mt-0.5 shrink-0" />
      <div className="flex-1">
        <div className="font-semibold">{title}</div>
        <div>{text} {tr("Включается на закладке")} <button type="button" className="underline font-medium" onClick={open}>{tr("«Репликация»")}</button>.</div>
      </div>
      <button type="button" className="opacity-60 hover:opacity-100" title={tr("Скрыть")} onClick={hide}><X size={16} /></button>
    </div>
  )
}
