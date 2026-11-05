// Форма по схеме плагина (подмножество JSON Schema): string, integer, boolean, array строк;
// format: "secret" — поле пароля, "textarea" — многострочный текст, "secret-textarea" — многострочный
// секрет (профиль VPN, закрытый ключ). Заданный на сервере секрет приходит как "********": поле показывается
// пустым, и, пока его не изменили, обратно уходит та же маска («не менять»).
import { useEffect, useRef, useState, type ReactNode } from 'react'
import { fmtBytes, SECRET_MASK } from '../api'
import type { Instance, Schema, SchemaProp, Site } from '../types'
import { cx, Field, Input, Select, Textarea, Toggle, useApi } from './ui'
import { ImageNeeds } from './ImageNeeds'
import { tr, trf } from '../i18n'

/** Значения по умолчанию: default из схемы, поверх — defaults каталога, поверх — заданные. */
export function schemaInitial(schema: Schema | undefined, defaults?: Record<string, any>, value?: Record<string, any>) {
  const out: Record<string, any> = {}
  for (const [k, p] of Object.entries(schema?.properties || {})) {
    if (p.default !== undefined) out[k] = p.default
    else if (p.type === 'boolean') out[k] = false
    else if (p.type === 'array') out[k] = []
  }
  return { ...out, ...(defaults || {}), ...(value || {}) }
}

/** Значения формы для отправки: пустые числа убираются. */
export function schemaClean(schema: Schema | undefined, v: Record<string, any>) {
  const out: Record<string, any> = { ...v }
  for (const [k, p] of Object.entries(schema?.properties || {})) {
    if (p.type === 'integer' && (out[k] === '' || out[k] === null || Number.isNaN(out[k]))) delete out[k]
  }
  return out
}

const MONO = /dir|path|command|cmd|hook|data|bin|host|url/i

export function SchemaForm({ schema, value, onChange, disabled, columns = 1 }: {
  schema?: Schema; value: Record<string, any>; onChange: (v: Record<string, any>) => void; disabled?: boolean; columns?: 1 | 2
}) {
  const props = Object.entries(schema?.properties || {})
  // Маски секретов на момент открытия формы: пустое поле = «не менять».
  const masked = useRef<Set<string>>(new Set())
  useEffect(() => { for (const [k] of props) if (value[k] === SECRET_MASK) masked.current.add(k) })
  if (!props.length) return <div className="text-sm text-slate-500 border border-dashed border-slate-300 rounded-lg px-3 py-2.5 bg-slate-50">{tr("У плагина нет настраиваемых параметров.")}</div>
  const req = new Set(schema?.required || [])
  const set = (k: string, v: any) => onChange({ ...value, [k]: v })
  return (
    <div className={cx('grid gap-4', columns === 2 && 'sm:grid-cols-2')}>
      {props.map(([k, p]) => {
        const label = <>{p.title || k}{req.has(k) && <span className="text-red-500"> *</span>}<span className="ml-1.5 text-xs font-normal font-mono text-slate-400">{k}</span></>
        const wide = columns === 2 && (p.type === 'array' || p.type === 'boolean' || p.format === 'textarea' || p.format === 'secret-textarea' || (p.description?.length || 0) > 80)
        const w = p['x-widget']
        if (w === 'machine') return <div key={k} className={cx(columns === 2 && 'sm:col-span-2')}><MachinePick label={label} hint={p.description} value={value[k] ?? ''} onChange={v => set(k, v)} disabled={disabled} /></div>
        if (w?.startsWith('instance:')) return <div key={k} className={cx(columns === 2 && 'sm:col-span-2')}><InstancePick plugin={w.slice(9)} label={label} hint={p.description} value={value[k] ?? ''} onChange={v => set(k, v)} disabled={disabled} form={value} /></div>
        if (w === 'server-dir') return <div key={k} className={cx(columns === 2 && 'sm:col-span-2')}><ServerDirPick label={label} hint={p.description} value={value[k] ?? ''} onChange={v => set(k, v)} disabled={disabled} /></div>
        if (w === 'remote-sites') return <div key={k} className={cx(columns === 2 && 'sm:col-span-2')}><RemoteSitesPick label={label} hint={p.description} value={Array.isArray(value[k]) ? value[k] : []} onChange={v => set(k, v)} disabled={disabled} /></div>
        return <div key={k} className={cx(wide && 'sm:col-span-2')}>{renderField(k, p, value[k], v => set(k, v), label, disabled, masked.current.has(k) || value[k] === SECRET_MASK)}</div>
      })}
    </div>
  )
}

function renderField(k: string, p: SchemaProp, v: any, set: (v: any) => void, label: ReactNode, disabled?: boolean, wasMasked?: boolean) {
  if (p.type === 'boolean') return <Toggle checked={!!v} onChange={set} label={label} hint={p.description} disabled={disabled} />
  if (p.type === 'array') return <Field label={label} hint={p.description || tr("По одному на строку или через запятую")}><ListInput value={Array.isArray(v) ? v : []} onChange={set} disabled={disabled} /></Field>
  if (p.type === 'integer') return (
    <Field label={label} hint={p.description}>
      <Input type="number" step={1} value={v ?? ''} disabled={disabled} placeholder={p.default !== undefined ? String(p.default) : undefined}
        onChange={e => set(e.target.value === '' ? '' : parseInt(e.target.value, 10))} />
    </Field>
  )
  if (Array.isArray(p.enum) && p.enum.length) return (
    <Field label={label} hint={p.description}>
      <Select value={v ?? ''} disabled={disabled} onChange={e => set(e.target.value)}>
        {p.enum.map(o => <option key={String(o)} value={o}>{String(o)}</option>)}
      </Select>
    </Field>
  )
  if (p.format === 'secret-textarea') return (
    <Field label={label} hint={p.description}>
      <Textarea rows={6} disabled={disabled} value={v === SECRET_MASK ? '' : (v ?? '')} spellCheck={false}
        placeholder={wasMasked ? tr("•••••••• задан — оставьте пустым, чтобы не менять") : ''}
        onChange={e => set(e.target.value === '' && wasMasked ? SECRET_MASK : e.target.value)} />
    </Field>
  )
  if (p.format === 'textarea') return (
    <Field label={label} hint={p.description}>
      <Textarea rows={5} disabled={disabled} value={v ?? ''} spellCheck={false} onChange={e => set(e.target.value)} />
    </Field>
  )
  if (p.format === 'secret') {
    // null — удалить заданный секрет при сохранении (сервер понимает null как «очистить»).
    const removing = wasMasked && v === null
    const hint = wasMasked && !disabled
      ? <>{p.description}{p.description ? ' ' : ''}{removing
          ? <>{tr("Будет удалён при сохранении.")} <button type="button" className="text-blue-600 hover:underline" onClick={() => set(SECRET_MASK)}>{tr("Не удалять")}</button></>
          : (v === SECRET_MASK || v === '' || v === undefined) && <button type="button" className="text-blue-600 hover:underline" onClick={() => set(null)}>{tr("Удалить")} {String(p.title || tr("секрет")).toLowerCase()}</button>}</>
      : p.description
    return (
      <Field label={label} hint={hint}>
        <Input type="password" autoComplete="new-password" disabled={disabled} value={v === SECRET_MASK || v === null ? '' : (v ?? '')}
          placeholder={removing ? tr("будет удалён при сохранении") : wasMasked ? tr("•••••••• задан — оставьте пустым, чтобы не менять") : ''}
          onChange={e => set(e.target.value === '' && wasMasked ? SECRET_MASK : e.target.value)} />
      </Field>
    )
  }
  return (
    <Field label={label} hint={p.description}>
      <Input value={v ?? ''} disabled={disabled} className={MONO.test(k) ? 'font-mono text-[13px]' : undefined}
        placeholder={p.default !== undefined ? String(p.default) : undefined} onChange={e => set(e.target.value)} />
    </Field>
  )
}

const parseList = (s: string) => s.split(/[\n,]/).map(x => x.trim()).filter(Boolean)

/** Список строк: по одному на строку или через запятую. Текст хранится локально, чтобы не мешать вводу. */
export function ListInput({ value, onChange, disabled, rows = 3 }: { value: string[]; onChange: (v: string[]) => void; disabled?: boolean; rows?: number }) {
  const [text, setText] = useState(value.join('\n'))
  useEffect(() => {
    if (JSON.stringify(parseList(text)) !== JSON.stringify(value)) setText(value.join('\n'))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [JSON.stringify(value)])
  return <Textarea rows={rows} value={text} disabled={disabled} onChange={e => { setText(e.target.value); onChange(parseList(e.target.value)) }} />
}

/** Машина из раздела «Серверы и ВМ» (доступ SSH берётся оттуда). */
function MachinePick({ label, hint, value, onChange, disabled }: { label: ReactNode; hint?: string; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  const { data, error } = useApi<{ id: string; name: string; host: string; port: number; user: string }[]>('/api/machines')
  const known = !value || (data || []).some(m => m.id === value)
  return (
    <Field label={label} hint={error ? trf("Список машин недоступен: {0}", error) : data && data.length === 0 ? tr("Машин пока нет — добавьте её в разделе «Серверы и ВМ» (адрес, пользователь, пароль или ключ SSH).") : hint}>
      <Select value={value} disabled={disabled} onChange={e => onChange(e.target.value)}>
        <option value="">{tr("— выберите машину —")}</option>
        {!known && <option value={value}>{value}  {tr("(нет в «Серверы и ВМ»)")}</option>}
        {(data || []).map(m => <option key={m.id} value={m.id}>{m.name} · {m.user}@{m.host}{m.port && m.port !== 22 ? `:${m.port}` : ''}</option>)}
      </Select>
    </Field>
  )
}

/** Экземпляр продукта-источника; для «Образа машины по SSH» — сколько места понадобится под образы. */
function InstancePick({ plugin, label, hint, value, onChange, disabled, form }: { plugin: string; label: ReactNode; hint?: string; value: string; onChange: (v: string) => void; disabled?: boolean; form: Record<string, any> }) {
  const { data, error } = useApi<Instance[]>('/api/instances')
  const list = (data || []).filter(i => i.plugin === plugin)
  return (
    <div className="space-y-2">
      <Field label={label} hint={error ? trf("Список экземпляров недоступен: {0}", error) : data && list.length === 0 ? tr("Сначала добавьте продукт-источник «Образ машины по SSH» (в «Серверы и ВМ» — кнопка «Образ дисков») и сделайте первую копию.") : hint}>
        <Select value={value} disabled={disabled} onChange={e => onChange(e.target.value)}>
          <option value="">{tr("— выберите экземпляр —")}</option>
          {value && !list.some(i => i.id === value) && <option value={value}>{value}</option>}
          {list.map(i => <option key={i.id} value={i.id}>{i.name}</option>)}
        </Select>
      </Field>
      {plugin === 'machine' && value && <ImageNeeds mode="qcow2" instanceId={value} dir={form.dir} keep={Number(form.keep) || undefined} keepRemote={Number(form.keep_remote) || undefined} sites={Array.isArray(form.copy_sites) ? form.copy_sites : []} format={form.format} />}
    </div>
  )
}

type Mount = { mount: string; fstype: string; device: string; total: number; free: number; system: boolean; readonly: boolean }

/** Локальная папка на сервере HopeBackup: путь и подключённые диски сервера (свободно/всего) для выбора. */
function ServerDirPick({ label, hint, value, onChange, disabled }: { label: ReactNode; hint?: string; value: string; onChange: (v: string) => void; disabled?: boolean }) {
  const { data } = useApi<Mount[]>('/api/server/mounts')
  const mounts = (data || []).filter(m => !m.readonly)
  // Диск, на котором окажется папка: самая длинная подходящая точка монтирования.
  const on = value ? mounts.filter(m => value === m.mount || value.startsWith(m.mount === '/' ? '/' : m.mount + '/')).sort((a, b) => b.mount.length - a.mount.length)[0] : undefined
  return (
    <Field label={label} hint={hint}>
      <Input value={value} disabled={disabled} className="font-mono text-[13px]" placeholder="/mnt/data/hopebackup-images" onChange={e => onChange(e.target.value)} />
      {mounts.length > 0 && <div className="mt-2 space-y-1">
        <div className="text-xs text-slate-500">{tr("Подключённые диски сервера — нажмите, чтобы выбрать:")}</div>
        <div className="flex flex-wrap gap-1.5">
          {mounts.map(m => (
            <button key={m.mount} type="button" disabled={disabled} onClick={() => onChange(`${m.mount === '/' ? '' : m.mount}/hopebackup-images`)}
              className={cx('text-xs px-2 py-1 rounded-md border text-left', on?.mount === m.mount ? 'border-blue-500 bg-blue-50 text-blue-800' : 'border-slate-200 bg-white hover:border-slate-300 text-slate-700')}>
              <span className="font-mono">{m.mount}</span>  {tr("· свободно")} {fmtBytes(m.free)}  {tr("из")} {fmtBytes(m.total)}{m.system && <span className="text-amber-700">  {tr("· системный")}</span>}
            </button>
          ))}
        </div>
        {on?.system && <div className="text-xs text-amber-700">{tr("Папка на системном диске сервера: образы могут занять его целиком. Лучше подключить отдельный диск (например, смонтировать его в /mnt/data). Новую папку на системном диске продукт сам не создаёт.")}</div>}
        {value && !value.startsWith('/') && <div className="text-xs text-red-600">{tr("Нужен полный путь, начиная с «/».")}</div>}
      </div>}
    </Field>
  )
}

/** Удалённые площадки (узлы HopeBackup), куда отправлять готовые qcow2. */
function RemoteSitesPick({ label, hint, value, onChange, disabled }: { label: ReactNode; hint?: string; value: string[]; onChange: (v: string[]) => void; disabled?: boolean }) {
  const { data } = useApi<Site[]>('/api/sites')
  const sites = (data || []).filter(s => s.kind === 'node')
  const toggle = (id: string) => onChange(value.includes(id) ? value.filter(x => x !== id) : [...value, id])
  return (
    <Field label={label} hint={hint}>
      {data && sites.length === 0 ? <div className="text-sm text-slate-500">{tr("Удалённых узлов HopeBackup нет — образы останутся только в локальной папке сервера.")}</div> : (
        <div className="space-y-1.5">
          {sites.map(s => (
            <label key={s.id} className="flex items-start gap-2 text-sm cursor-pointer">
              <input type="checkbox" className="mt-0.5" disabled={disabled} checked={value.includes(s.id)} onChange={() => toggle(s.id)} />
              <span><span className="font-medium">{s.name}</span> <span className="text-slate-400 font-mono text-xs">{s.url}</span>
                {s.node_disk && <span className="text-slate-500 text-xs">  {tr("· свободно")} {fmtBytes(s.node_disk.free)}</span>}
                {value.includes(s.id) && !s.has_prune_token && <span className="block text-xs text-amber-700">{tr("у площадки нет токена очистки — старые наборы там удаляться не будут")}</span>}
              </span>
            </label>
          ))}
        </div>
      )}
    </Field>
  )
}
