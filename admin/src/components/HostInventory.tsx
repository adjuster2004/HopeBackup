import { useState } from 'react'
import { Link } from 'react-router-dom'
import { Boxes, CalendarClock, Cog, Container, Cpu, FolderPlus, HardDrive, KeyRound, Network, Package, Plus, Users } from 'lucide-react'
import { fmtBytes } from '../api'
import { Alert, Badge, Button, Code, KV, Table, Tabs, Td } from './ui'
import { AnalysisView, SystemView } from './Analysis'
import { tr, trf } from '../i18n'

/** Опись сервера (плагин «Сервер Linux целиком»): что найдено и что попадёт в копию. */
type Inv = Record<string, any>
type Tab = 'analysis' | 'system' | 'apps' | 'sched' | 'svc' | 'mounts' | 'docker' | 'users' | 'ports' | 'pkgs' | 'plan'

const CLASS: Record<string, [string, 'blue' | 'violet' | 'amber' | 'slate' | 'green']> = {
  system: [tr("системный"), 'slate'], disk: [tr("отдельный диск"), 'blue'], network: [tr("сетевой"), 'violet'], bind: ['bind', 'amber'], docker: [tr("контейнеры"), 'slate'], pseudo: [tr("служебный"), 'slate'],
}
const SCHED: Record<string, string> = { cron: 'cron', 'cron-dir': 'cron.*', anacron: 'anacron', timer: tr("таймер systemd"), at: 'at' }

export function HostInventory({ res, config, canEdit, onConfig }: { res: Inv; config: Record<string, any>; canEdit: boolean; onConfig: (c: Record<string, any>) => Promise<void> }) {
  const [tab, setTab] = useState<Tab>(res.analysis ? 'analysis' : 'apps')
  const [busy, setBusy] = useState('')
  const mounts: string[] = config.mounts || []
  const extra: string[] = config.extra_paths || []
  const apps: any[] = res.apps || []
  const sched: any[] = res.schedules || []
  const svc: any[] = res.services || []
  const ms: any[] = (res.mounts || []).filter((m: any) => m.class !== 'pseudo' && m.class !== 'docker')
  const cont: any[] = res.containers || []
  const users: any[] = res.users || []
  const ports: any[] = res.listening || []
  const pk = res.packages || { manual: [], total: 0, manager: '' }
  const plan = res.plan || {}
  const change = async (key: string, c: Record<string, any>) => { setBusy(key); try { await onConfig(c) } finally { setBusy('') } }
  const toggleMount = (t: string) => change('m:' + t, { ...config, mounts: mounts.includes(t) ? mounts.filter(x => x !== t) : [...mounts, t] })
  const addExtra = (p: string) => change('x:' + p, { ...config, extra_paths: [...extra, p] })
  const tabs: { id: Tab; label: string }[] = [
    ...(res.analysis ? [{ id: 'analysis' as Tab, label: tr("Анализ") }] : []), ...(res.system?.os ? [{ id: 'system' as Tab, label: tr("Система") }] : []),
    { id: 'apps', label: trf("Приложения · {0}", apps.length) }, { id: 'sched', label: trf("Расписания · {0}", sched.filter(s => !s.packaged).length) },
    { id: 'svc', label: trf("Службы · {0}", svc.length) }, { id: 'mounts', label: trf("Диски · {0}", ms.length) }, { id: 'docker', label: trf("Контейнеры · {0}", cont.length) },
    { id: 'users', label: trf("Пользователи · {0}", users.length) }, { id: 'ports', label: trf("Порты · {0}", ports.length) }, { id: 'pkgs', label: trf("Пакеты · {0}", pk.manual.length) },
    { id: 'plan', label: tr("Что в копии") },
  ]
  return (
    <div className="space-y-3">
      <div className="grid sm:grid-cols-2 gap-x-6">
        <KV k={tr("Сервер")}>{res.host} <span className="text-slate-400">· {res.os}  {tr("· ядро")} {res.kernel}</span></KV>
        <KV k={tr("Процессов")}>{(res.processes || []).length}{tr(", из них не из пакетов")} {(res.processes || []).filter((p: any) => !p.packaged).length}</KV>
      </div>
      {(res.products || []).length > 0 && (
        <Alert tone="info" title={tr("Найдены продукты — для них есть отдельные плагины")}>
          <ul className="space-y-1 mt-1">{res.products.map((p: any) => (
            <li key={p.id} className="text-sm"><b>{p.title}</b>{p.note ? <span className="text-slate-600"> — {p.note}</span> : null}
              {p.available && <Link className="ml-2 text-blue-600 hover:underline inline-flex items-center gap-1" to={`/products/new?catalog=${encodeURIComponent(p.plugin && p.plugin !== 'files' ? p.plugin : p.id)}`}><Plus size={12} />{tr("добавить")}</Link>}</li>
          ))}</ul>
        </Alert>
      )}
      {(res.warnings || []).length > 0 && <Alert tone="warn" title={tr("Внимание")}><ul className="list-disc ml-4 space-y-0.5">{res.warnings.map((w: string, i: number) => <li key={i}>{w}</li>)}</ul></Alert>}
      <Tabs tabs={tabs} value={tab} onChange={setTab} />
      {tab === 'analysis' && res.analysis && <AnalysisView a={res.analysis} />}
      {tab === 'system' && <SystemView sys={res.system} method={res.method} asRoot={res.as_root} />}
      {tab === 'apps' && (
        <Table head={[tr("Каталог или файл"), tr("Откуда известно"), tr("Размер"), tr("В копии"), '']} empty={<span>{tr("Своих программ и скриптов не найдено — всё работающее установлено из пакетов.")}</span>}>
          {apps.map(a => (
            <tr key={a.path}>
              <Td className="font-mono text-xs">{a.path}{a.files.length > 1 || a.files[0] !== a.path ? <div className="text-slate-400">{a.files.slice(0, 4).join(', ')}{a.files.length > 4 ? ' …' : ''}</div> : null}</Td>
              <Td className="text-xs text-slate-600">{a.sources.slice(0, 3).join('; ')}{a.sources.length > 3 ? ' …' : ''}</Td>
              <Td className="whitespace-nowrap">{a.size_over ? trf("больше {0}", fmtBytes(a.size)) : fmtBytes(a.size)}</Td>
              <Td>{extra.includes(a.path) ? <Badge tone="green">{tr("целиком (вручную)")}</Badge> : a.include ? <Badge tone="green">{tr("целиком")}</Badge> : <Badge tone="amber">{tr("файлы запуска")}</Badge>}</Td>
              <Td className="text-right">{canEdit && !a.include && !extra.includes(a.path) && <Button className="!py-1 !px-2 text-xs" icon={<FolderPlus size={14} />} loading={busy === 'x:' + a.path} onClick={() => addExtra(a.path)}>{tr("Копировать целиком")}</Button>}</Td>
            </tr>
          ))}
        </Table>
      )}
      {tab === 'sched' && (
        <Table head={[tr("Вид"), tr("Когда"), tr("Пользователь"), tr("Команда"), tr("Скрипты")]} empty={tr("Заданий по расписанию нет")}>
          {sched.map((s, i) => (
            <tr key={i} className={s.packaged ? 'opacity-60' : ''}>
              <Td><Badge tone={s.packaged ? 'slate' : 'blue'}>{SCHED[s.kind] || s.kind}</Badge>{s.packaged && <div className="text-[11px] text-slate-400 mt-0.5">{tr("из пакета")}</div>}</Td>
              <Td className="font-mono text-xs whitespace-nowrap">{s.schedule}</Td>
              <Td className="text-xs">{s.user}</Td>
              <Td className="font-mono text-xs break-all">{s.command}<div className="text-slate-400">{s.source}</div></Td>
              <Td className="font-mono text-xs">{s.scripts.join(', ') || '—'}</Td>
            </tr>
          ))}
        </Table>
      )}
      {tab === 'svc' && (
        <Table head={[tr("Служба"), tr("Запускает"), tr("Пользователь"), tr("Состояние")]} empty={tr("Своих служб systemd нет")}>
          {svc.map(s => (
            <tr key={s.unit}>
              <Td className="font-mono text-xs">{s.unit}<div className="text-slate-400">{s.file}</div></Td>
              <Td className="font-mono text-xs break-all">{s.exec.join(' ; ')}</Td>
              <Td className="text-xs">{s.user || 'root'}</Td>
              <Td>{s.enabled ? <Badge tone="green">{tr("включена")}</Badge> : <Badge>{tr("выключена")}</Badge>}{s.custom && <Badge tone="blue">{tr("своя")}</Badge>}</Td>
            </tr>
          ))}
        </Table>
      )}
      {tab === 'mounts' && (<>
        <div className="text-xs text-slate-500">{tr("Отдельные диски, сетевые каталоги (NFS, SMB, sshfs) и bind-монтирования копируются, только если их отметить. Каждый — отдельной частью копии.")}</div>
        <Table head={[tr("Точка монтирования"), tr("Вид"), tr("Источник"), tr("Занято"), tr("Копировать")]}>
          {ms.map(m => {
            const [label, tone] = CLASS[m.class] || [m.class, 'slate']
            const selectable = m.class === 'disk' || m.class === 'network' || m.class === 'bind'
            return (
              <tr key={m.target}>
                <Td className="font-mono text-xs">{m.target}</Td>
                <Td><Badge tone={tone}>{label}</Badge> <span className="text-xs text-slate-400">{m.fstype}</span></Td>
                <Td className="font-mono text-xs break-all">{m.source}{m.root ? <span className="text-slate-400"> : {m.root}</span> : null}</Td>
                <Td className="whitespace-nowrap text-xs">{m.total ? trf("{0} из {1}", fmtBytes(m.used), fmtBytes(m.total)) : '—'}</Td>
                <Td>{selectable ? <label className="inline-flex items-center gap-2 text-sm cursor-pointer"><input type="checkbox" disabled={!canEdit || busy === 'm:' + m.target} checked={mounts.includes(m.target)} onChange={() => toggleMount(m.target)} />{mounts.includes(m.target) ? tr("да") : tr("нет")}</label> : <span className="text-xs text-slate-400">{m.class === 'system' ? tr("входит в /etc, /root…") : '—'}</span>}</Td>
              </tr>
            )
          })}
        </Table>
      </>)}
      {tab === 'docker' && (
        <Table head={[tr("Контейнер"), tr("Образ"), tr("Compose-проект"), tr("Тома и каталоги")]} empty={tr("Контейнеров Docker нет")}>
          {cont.map(c => (
            <tr key={c.id}>
              <Td className="text-sm">{c.name}<div className="text-xs text-slate-400">{c.state}</div></Td>
              <Td className="font-mono text-xs">{c.image}</Td>
              <Td className="font-mono text-xs">{c.compose_dir || '—'}</Td>
              <Td className="font-mono text-xs">{c.mounts.map((m: any, i: number) => <div key={i}>{m.kind === 'volume' ? trf("том {0}", m.name) : m.source} → {m.dest}</div>)}</Td>
            </tr>
          ))}
        </Table>
      )}
      {tab === 'users' && (
        <Table head={[tr("Пользователь"), 'UID', tr("Домашний каталог"), tr("Группы"), tr("Доступ")]}>
          {users.map(u => (
            <tr key={u.name}>
              <Td className="font-medium">{u.name}</Td>
              <Td className="text-xs">{u.uid}</Td>
              <Td className="font-mono text-xs">{u.home}<div className="text-slate-400">{u.shell}</div></Td>
              <Td className="text-xs">{u.groups.join(', ')}</Td>
              <Td className="space-x-1">{u.has_password && <Badge tone="blue">{tr("пароль")}</Badge>}{u.ssh_files.length > 0 && <Badge tone="violet" ><KeyRound size={11} />SSH: {u.ssh_files.join(', ')}</Badge>}{u.sudo && <Badge tone="amber">sudo</Badge>}{u.crontab && <Badge>crontab</Badge>}</Td>
            </tr>
          ))}
        </Table>
      )}
      {tab === 'ports' && (
        <Table head={[tr("Протокол"), tr("Адрес"), tr("Порт"), tr("Процесс")]} empty={tr("Открытых портов нет")}>
          {ports.map((p, i) => <tr key={i}><Td className="text-xs">{p.proto}</Td><Td className="font-mono text-xs">{p.addr}</Td><Td className="font-mono">{p.port}</Td><Td className="text-sm">{p.process || '—'}{p.pid ? <span className="text-xs text-slate-400"> · pid {p.pid}</span> : null}</Td></tr>)}
        </Table>
      )}
      {tab === 'pkgs' && (
        <div className="text-sm">
          <div className="mb-2 text-slate-600">{tr("Менеджер пакетов:")} <b>{pk.manager || tr("не найден")}</b>  {tr("· установлено")} {pk.total}{tr(", вручную")} {pk.manual.length}{tr(". Список сохраняется в копии; при полном восстановлении на тот же дистрибутив пакеты можно поставить автоматически.")}</div>
          <Code className="max-h-64 overflow-y-auto text-xs">{pk.manual.join(' ') || '—'}</Code>
        </div>
      )}
      {tab === 'plan' && (
        <div className="grid md:grid-cols-2 gap-4 text-sm">
          {([['system', tr("Система (system.tar)"), <Cog size={14} />], ['homes', tr("Домашние каталоги (homes.tar)"), <Users size={14} />], ['apps', tr("Приложения (apps.tar)"), <Boxes size={14} />], ['docker', 'Docker (docker.tar)', <Container size={14} />], ['mounts', tr("Диски (mount-*.tar)"), <HardDrive size={14} />], ['exclude', tr("Исключения"), <Network size={14} />]] as [string, string, React.ReactNode][]).map(([k, t, ic]) => (
            <div key={k}><div className="font-medium text-slate-800 inline-flex items-center gap-1.5 mb-1">{ic}{t}</div><Code className="text-xs max-h-40 overflow-y-auto">{(plan[k] || []).join('\n') || '—'}</Code></div>
          ))}
          <div className="md:col-span-2 text-xs text-slate-500 inline-flex items-center gap-1.5"><Cpu size={13} />{tr("Пользователи с паролями (passwd, shadow, group) и опись сохраняются всегда.")} <Package size={13} className="ml-2" />{tr("Пакеты — списком.")}</div>
        </div>
      )}
    </div>
  )
}
