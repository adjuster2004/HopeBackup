// Поля политики повторов: включено, сколько повторов, через сколько минут.
import type { RetryPolicy } from '../types'
import { Field, Input, Toggle } from './ui'
import { lang, tr } from '../i18n'

export const plural = (n: number, one: string, few: string, many: string) => {
  if (lang === 'en') return Math.abs(n) === 1 ? tr(one) : tr(many)
  const a = Math.abs(n) % 100, b = a % 10
  return a > 10 && a < 20 ? many : b > 1 && b < 5 ? few : b === 1 ? one : many
}

export function RetryFields({ value, onChange, disabled }: { value: RetryPolicy; onChange: (v: RetryPolicy) => void; disabled?: boolean }) {
  const total = value.enabled ? value.attempts : 0
  return (
    <div className="space-y-3">
      <Toggle checked={value.enabled} disabled={disabled} onChange={v => onChange({ ...value, enabled: v })} label={tr("Повторять неудачные копии по расписанию")} />
      {value.enabled && (
        <div className="grid grid-cols-2 gap-4 max-w-md">
          <Field label={tr("Повторных попыток")} hint="0–24"><Input type="number" min={0} max={24} disabled={disabled} value={value.attempts} onChange={e => onChange({ ...value, attempts: Math.max(0, Math.min(24, Number(e.target.value) || 0)) })} /></Field>
          <Field label={tr("Пауза перед попыткой, мин")} hint={tr("1–1440 (сутки)")}><Input type="number" min={1} max={1440} disabled={disabled} value={value.interval_min} onChange={e => onChange({ ...value, interval_min: Math.max(1, Math.min(1440, Number(e.target.value) || 1)) })} /></Field>
        </div>
      )}
      <div className="text-xs text-slate-500">{total > 0
        ? <>{tr("Пример: копия в 01:00 не удалась — повторы в")} {Array.from({ length: Math.min(total, 4) }, (_, i) => { const m = 60 + (i + 1) * value.interval_min; return `${String(Math.floor(m / 60) % 24).padStart(2, '0')}:${String(m % 60).padStart(2, '0')}` }).join(', ')}{total > 4 ? ' …' : ''}{tr(". Если копия уже снята другим запуском — повтор не нужен.")}</>
        : tr("Без повторов: неудачная копия ждёт следующего запуска по расписанию.")}</div>
    </div>
  )
}
