import { useState } from 'react'
import { AlertTriangle, Check, CheckCircle2, ClipboardCheck, Copy, ShieldAlert, Cpu, HardDrive, Info, KeyRound, Lightbulb, Network, Power, ShieldCheck, Wrench, XCircle } from 'lucide-react'
import { fmtBytes } from '../api'
import { Alert, Badge, Card, KV, Table, Td } from './ui'
import { tr, trf } from '../i18n'

/** Анализ описи и рекомендации (плагин «Сервер Linux целиком», раздел «Серверы и ВМ»). */
export type Rec = {
  id: string; priority: 'high' | 'medium' | 'low'; title: string; reason: string; action: string; catalog_id: string
  config: Record<string, any>; schedule: string; rpo_min: number; keep_last: number; available: boolean; details: string[]
}
export type AnalysisT = {
  summary: [string, string][]; recommendations: Rec[]; warnings: { level: 'warn' | 'info'; text: string }[]
  restore_notes: string[]; estimate: { parts: [string, number][]; first: number; over: boolean; site_7: number }
  prepare?: Prep[]
}
export type Step = { title: string; text: string; commands: string[] }
export type Prep = { level: 'ok' | 'info' | 'warn' | 'bad'; area: string; title: string; text: string; commands: string[]; steps?: Step[]; caution?: string }

/** Команды с кнопкой «Скопировать» (комментарии после # не копируются в буфер — только сами команды). */
function Cmds({ lines }: { lines: string[] }) {
  const [done, setDone] = useState(false)
  const copy = async () => {
    const text = lines.filter(l => !l.trim().startsWith('#')).map(l => l.replace(/\s+#\s.*$/, '')).join('\n')
    try { await navigator.clipboard.writeText(text); setDone(true); setTimeout(() => setDone(false), 1500) } catch { /* буфер обмена недоступен */ }
  }
  return (
    <div className="relative mt-2 group">
      <pre className="bg-slate-900 text-emerald-300 text-xs font-mono rounded-lg p-3 pr-24 overflow-x-auto whitespace-pre">{lines.join('\n')}</pre>
      <button type="button" onClick={copy} title={tr("Скопировать команды (без комментариев)")} className="absolute top-2 right-2 inline-flex items-center gap-1 px-2 py-1 rounded-md text-[11px] bg-slate-700 text-slate-100 hover:bg-slate-600">{done ? <Check size={12} /> : <Copy size={12} />}{done ? tr("Скопировано") : tr("Скопировать")}</button>
    </div>
  )
}

const PREP_TONE: Record<string, [string, React.ReactNode]> = {
  bad: ['border-red-200 bg-red-50/60', <XCircle size={16} className="text-red-600 shrink-0 mt-0.5" />],
  warn: ['border-amber-200 bg-amber-50/60', <AlertTriangle size={16} className="text-amber-600 shrink-0 mt-0.5" />],
  info: ['border-slate-200 bg-slate-50', <Info size={16} className="text-blue-600 shrink-0 mt-0.5" />],
  ok: ['border-emerald-200 bg-emerald-50/40', <CheckCircle2 size={16} className="text-emerald-600 shrink-0 mt-0.5" />],
}

/** Что донастроить на машине: место под снимки ФС, память, процессор, место для агента, время, сеть. */
export function PrepareView({ items }: { items: Prep[] }) {
  const order: Record<string, number> = { bad: 0, warn: 1, info: 2, ok: 3 }
  const sorted = [...items].sort((a, b) => order[a.level] - order[b.level])
  const todo = items.filter(p => p.level === 'bad' || p.level === 'warn').length
  return (
    <div>
      <div className="text-sm font-semibold text-slate-800 mb-1 inline-flex items-center gap-2"><ClipboardCheck size={16} className="text-blue-600" />{tr("Подготовка машины")}</div>
      <div className="text-xs text-slate-500 mb-2">{todo > 0 ? trf("Донастроить: {0}. Снимки файловой системы нужны, чтобы все файлы копии были в одном состоянии; без места под снимки копия идёт «на ходу».", todo) : tr("Машина готова к копиям.")}</div>
      <div className="space-y-2">
        {sorted.map((p, i) => {
          const [cls, icon] = PREP_TONE[p.level] || PREP_TONE.info
          return (
            <div key={i} className={`rounded-xl border p-3 ${cls}`}>
              <div className="flex gap-2">
                {icon}
                <div className="flex-1 min-w-0">
                  <div className="flex flex-wrap items-center gap-2"><span className="font-medium text-slate-900">{p.title}</span><Badge tone="slate">{p.area}</Badge></div>
                  <div className="text-sm text-slate-700 mt-0.5">{p.text}</div>
                  {p.caution && <div className="mt-2 flex gap-2 rounded-lg border border-red-300 bg-white p-2.5 text-sm text-red-800"><ShieldAlert size={16} className="shrink-0 mt-0.5 text-red-600" /><span>{p.caution}</span></div>}
                  {p.commands.length > 0 && <Cmds lines={p.commands} />}
                  {p.steps && p.steps.length > 0 && <ol className="mt-3 space-y-3">
                    {p.steps.map((st, j) => <li key={j} className="flex gap-3">
                      <span className="shrink-0 w-6 h-6 rounded-full bg-blue-600 text-white text-xs font-semibold flex items-center justify-center">{j + 1}</span>
                      <div className="flex-1 min-w-0">
                        <div className="font-medium text-slate-900 text-sm">{st.title}</div>
                        {st.text && <div className="text-sm text-slate-600 mt-0.5">{st.text}</div>}
                        {st.commands.length > 0 && <Cmds lines={st.commands} />}
                      </div>
                    </li>)}
                  </ol>}
                </div>
              </div>
            </div>
          )
        })}
      </div>
    </div>
  )
}

const PRIO: Record<string, [string, 'red' | 'amber' | 'slate']> = { high: [tr("важно"), 'red'], medium: [tr("желательно"), 'amber'], low: [tr("по желанию"), 'slate'] }
const ACTION: Record<string, string> = { instance: tr("экземпляр"), agent: tr("установка агента"), config: tr("параметр копии"), manual: tr("вручную") }

export function AnalysisView({ a, selectable, selected, onToggle }: { a: AnalysisT; selectable?: boolean; selected?: string[]; onToggle?: (id: string) => void }) {
  const canPick = (r: Rec) => selectable && (r.action === 'instance' || r.action === 'config') && r.available
  return (
    <div className="space-y-5">
      <div className="grid md:grid-cols-2 gap-x-8">{a.summary.map(([k, v]) => <KV key={k} k={k}>{v}</KV>)}</div>
      <div>
        <div className="text-sm font-semibold text-slate-800 mb-2 inline-flex items-center gap-2"><Lightbulb size={16} className="text-amber-500" />{tr("Что копировать на этой машине")}</div>
        <div className="space-y-2">
          {a.recommendations.map(r => {
            const [pl, pt] = PRIO[r.priority] || ['', 'slate']
            const on = selected?.includes(r.id)
            return (
              <label key={r.id} className={`block rounded-xl border p-3 ${on ? 'border-blue-300 bg-blue-50/40' : 'border-slate-200'} ${canPick(r) ? 'cursor-pointer hover:border-slate-300' : ''}`}>
                <div className="flex items-start gap-3">
                  {selectable && <input type="checkbox" className="mt-1" disabled={!canPick(r)} checked={!!on} onChange={() => onToggle?.(r.id)} />}
                  <div className="flex-1 min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium text-slate-900">{r.title}</span>
                      <Badge tone={pt}>{pl}</Badge>
                      <Badge tone="blue">{ACTION[r.action] || r.action}</Badge>
                      {r.schedule && <span className="text-xs text-slate-500 font-mono">{r.schedule}</span>}
                      {r.rpo_min > 0 && r.rpo_min < 1440 && <span className="text-xs text-slate-500">RPO {r.rpo_min}  {tr("мин")}</span>}
                      {!r.available && <Badge tone="slate">{tr("недоступно")}</Badge>}
                    </div>
                    <div className="text-sm text-slate-600 mt-1">{r.reason}</div>
                    {r.details.length > 0 && <ul className="mt-1.5 text-xs text-slate-500 list-disc ml-4 space-y-0.5">{r.details.map((d, i) => <li key={i} className="break-words">{d}</li>)}</ul>}
                  </div>
                </div>
              </label>
            )
          })}
        </div>
      </div>
      {a.prepare && a.prepare.length > 0 && <PrepareView items={a.prepare} />}
      {a.warnings.length > 0 && (
        <div className="space-y-2">
          {a.warnings.map((w, i) => (
            <div key={i} className={`flex gap-2 text-sm rounded-lg px-3 py-2 ${w.level === 'warn' ? 'bg-amber-50 text-amber-900' : 'bg-slate-50 text-slate-700'}`}>
              {w.level === 'warn' ? <AlertTriangle size={16} className="shrink-0 mt-0.5" /> : <Info size={16} className="shrink-0 mt-0.5" />}{w.text}
            </div>
          ))}
        </div>
      )}
      <div className="grid lg:grid-cols-2 gap-5">
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-2 inline-flex items-center gap-2"><HardDrive size={16} className="text-blue-600" />{tr("Оценка объёма")}</div>
          <Table head={[tr("Часть"), tr("Объём")]}>
            {a.estimate.parts.filter(p => p[1] > 0).map(([k, v]) => <tr key={k}><Td className="font-mono text-xs">{k}</Td><Td>{fmtBytes(v)}</Td></tr>)}
            <tr className="bg-slate-50"><Td className="font-medium">{tr("Первая копия")}</Td><Td className="font-medium">{a.estimate.over ? tr("больше ") : '≈ '}{fmtBytes(a.estimate.first)}</Td></tr>
          </Table>
          <div className="text-xs text-slate-500 mt-1.5">{tr("Сжатие zstd и дедупликация обычно уменьшают объём в 1,5–3 раза. На площадке при хранении 7 копий — около")} {fmtBytes(a.estimate.site_7)}  {tr("(меняется немного данных в день).")}</div>
        </div>
        <div>
          <div className="text-sm font-semibold text-slate-800 mb-2 inline-flex items-center gap-2"><Wrench size={16} className="text-violet-600" />{tr("Для восстановления на новую машину")}</div>
          {a.restore_notes.length === 0 ? <div className="text-sm text-slate-500">{tr("Особенностей нет.")}</div> : <ul className="text-sm text-slate-700 space-y-1.5 list-disc ml-4">{a.restore_notes.map((n, i) => <li key={i}>{n}</li>)}</ul>}
        </div>
      </div>
    </div>
  )
}

/** Сведения о системе из описи: ОС, ядро, загрузка, железо, диски, сеть, безопасность. */
export function SystemView({ sys, method, asRoot }: { sys: Record<string, any>; method?: string; asRoot?: boolean }) {
  if (!sys || !sys.os) return <div className="text-sm text-slate-500">{tr("Сведений о системе нет — опись снята старой версией. Повторите опись.")}</div>
  const os = sys.os, k = sys.kernel, b = sys.boot, h = sys.hardware, st = sys.storage, n = sys.network, se = sys.security, t = sys.time
  const yes = (v: any) => v ? <Badge tone="green"><CheckCircle2 size={11} />{tr("да")}</Badge> : <Badge>{tr("нет")}</Badge>
  return (
    <div className="space-y-5">
      {method === 'shell' && <Alert tone="warn">{tr("Сокращённая опись (программа HopeBackup на машине не запустилась): часть сведений недоступна.")}</Alert>}
      {asRoot === false && <Alert tone="warn">{tr("Опись снята без прав root — часть сведений недоступна.")}</Alert>}
      <div className="grid xl:grid-cols-2 gap-5">
        <Card title={tr("Операционная система")} icon={<Cpu size={18} className="text-blue-600" />}>
          <KV k={tr("Дистрибутив")}>{os.pretty} <span className="text-slate-400 text-xs">{tr("· семейство")} {os.family}</span></KV>
          <KV k={tr("Версия")}>{os.version || os.version_id}{os.codename ? <span className="text-slate-400"> · {os.codename}</span> : null}</KV>
          <KV k="ID" mono>{os.id}{os.id_like ? trf(" (как {0})", os.id_like) : ''}</KV>
          <KV k={tr("Ядро")} mono>{k.release} · {k.arch}</KV>
          <KV k={tr("Сборка ядра")} mono>{k.version}</KV>
          <KV k={tr("Ядра в /boot")} mono>{(k.installed || []).join(', ') || '—'}</KV>
          <KV k={tr("Локаль, часовой пояс")}>{sys.locale || '—'} · {t?.timezone || '—'}</KV>
          <KV k={tr("Синхронизация времени")}>{t?.ntp || <span className="text-amber-700">{tr("не найдена")}</span>}</KV>
          <KV k={tr("Работает")}>{sys.uptime_secs ? trf("{0} дн. {1} ч", Math.floor(sys.uptime_secs / 86400), Math.floor(sys.uptime_secs % 86400 / 3600)) : '—'}</KV>
        </Card>
        <Card title={tr("Загрузка")} icon={<Power size={18} className="text-violet-600" />}>
          <KV k={tr("Прошивка")}>{(b.firmware || '').toUpperCase()}{b.secure_boot != null && <span className="ml-2">Secure Boot: {b.secure_boot ? tr("включена") : tr("выключена")}</span>}</KV>
          <KV k={tr("Загрузчик")}>{b.bootloader}{b.config ? <span className="font-mono text-xs text-slate-500 ml-2">{b.config}</span> : null}</KV>
          <KV k={tr("Пункт по умолчанию")} mono>{b.default_entry || '—'}</KV>
          <KV k="GRUB_CMDLINE" mono>{b.grub_cmdline || '—'}</KV>
          <KV k={tr("Параметры ядра")} mono><span className="break-all">{k.cmdline}</span></KV>
          <KV k={tr("Свои параметры")}>{b.custom_params?.length ? <span className="font-mono text-xs">{b.custom_params.join(' ')}</span> : tr("нет")}</KV>
          <KV k={tr("Инициализация")}>{b.init}{b.default_target ? <span className="text-slate-500"> · {b.default_target}</span> : null}</KV>
          <KV k="initramfs">{b.initramfs || '—'}</KV>
          <KV k="/boot, EFI" mono>{b.boot_mount || '—'} · {b.efi_mount || '—'}</KV>
        </Card>
        <Card title={tr("Железо")} icon={<Cpu size={18} className="text-emerald-600" />}>
          <KV k={tr("Процессор")}>{h.cpu_model || '—'} · {h.cpus}  {tr("шт.")}</KV>
          <KV k={tr("Память")}>{fmtBytes(h.mem_total)}{h.swap_total ? trf(" · подкачка {0}", fmtBytes(h.swap_total)) : ''}</KV>
          <KV k={tr("Виртуализация")}>{h.virt === 'none' ? tr("физический сервер") : h.virt}{h.cloud ? <Badge tone="violet">{h.cloud}</Badge> : null}</KV>
          <KV k={tr("Производитель, модель")}>{[h.vendor, h.product].filter(Boolean).join(' · ') || '—'}</KV>
          <KV k="cloud-init">{yes(h.cloud_init)}</KV>
          <KV k={tr("Гостевые агенты")}>{h.guest_agent?.join(', ') || '—'}</KV>
        </Card>
        <Card title={tr("Безопасность")} icon={<ShieldCheck size={18} className="text-red-600" />}>
          <KV k="SELinux">{se.selinux || '—'}</KV>
          <KV k="AppArmor">{se.apparmor == null ? '—' : se.apparmor ? tr("включён") : tr("выключен")}</KV>
          <KV k={tr("Файрвол")}>{se.firewall?.join(', ') || tr("не найден")}</KV>
          <KV k="sshd">{tr("порт")} {se.sshd_port}  {tr("· вход root:")} {se.sshd_root_login}  {tr("· пароли:")} {se.sshd_password_auth}</KV>
          <KV k="FIPS">{yes(se.fips)}</KV>
        </Card>
      </div>
      <Card title={tr("Диски")} icon={<HardDrive size={18} className="text-blue-600" />}>
        <Table head={[tr("Устройство"), tr("Размер"), tr("Модель"), tr("Разделы")]}>
          {(st.disks || []).map((d: any) => (
            <tr key={d.name}>
              <Td className="font-mono text-xs">{d.name}</Td><Td className="whitespace-nowrap">{fmtBytes(d.size)}</Td>
              <Td className="text-xs">{d.model || '—'}{d.rotational ? ' · HDD' : ''}</Td>
              <Td className="font-mono text-xs">{d.partitions.map((p: any) => <div key={p.name}>{p.name} · {fmtBytes(p.size)}{p.mount ? ` → ${p.mount} (${p.fstype})` : ''}</div>)}</Td>
            </tr>
          ))}
        </Table>
        <div className="grid md:grid-cols-2 gap-x-8 mt-3">
          <KV k={tr("Корень")} mono>{st.root_device} ({st.root_fstype})</KV>
          <KV k="LVM" mono>{st.lvm?.join(', ') || tr("нет")}</KV>
          <KV k="RAID" mono>{st.raid?.join('; ') || tr("нет")}</KV>
          <KV k={tr("Шифрование (crypttab)")} mono>{st.luks?.join('; ') || tr("нет")}</KV>
          <KV k={tr("Подкачка")} mono>{st.swap?.join(', ') || tr("нет")}</KV>
          <KV k="fstab">{(st.fstab || []).length}  {tr("записей")}</KV>
        </div>
      </Card>
      <Card title={tr("Сеть")} icon={<Network size={18} className="text-violet-600" />}>
        <div className="grid md:grid-cols-2 gap-x-8">
          <KV k={tr("Имя")}>{n.hostname}{n.fqdn && n.fqdn !== n.hostname ? <span className="text-slate-500"> · {n.fqdn}</span> : null}</KV>
          <KV k={tr("Шлюз")} mono>{n.gateway || '—'}</KV>
          <KV k="DNS" mono>{n.dns?.join(', ') || '—'}{n.search?.length ? ` · ${n.search.join(' ')}` : ''}</KV>
          <KV k={tr("Настройка сети")}>{n.config_tool?.join(', ') || '—'}</KV>
        </div>
        <Table head={[tr("Интерфейс"), tr("Адреса"), 'MAC', tr("Состояние")]}>
          {(n.interfaces || []).map((i: any) => <tr key={i.name}><Td className="font-mono text-xs">{i.name}</Td><Td className="font-mono text-xs">{i.addrs.join(', ') || '—'}</Td><Td className="font-mono text-xs">{i.mac}</Td><Td className="text-xs">{i.state}</Td></tr>)}
        </Table>
        {sys.repos?.length > 0 && <details className="mt-3"><summary className="text-xs text-slate-500 cursor-pointer inline-flex items-center gap-1"><KeyRound size={12} />{tr("Репозитории пакетов ·")} {sys.repos.length}</summary><div className="font-mono text-xs text-slate-600 mt-2 space-y-0.5">{sys.repos.map((r: string) => <div key={r}>{r}</div>)}</div></details>}
      </Card>
    </div>
  )
}
