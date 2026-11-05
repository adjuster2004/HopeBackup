// Строки панели без английского перевода. Запуск из admin: node tools/i18n_check.cjs
// Ищет tr("…")/trf("…") в src и сверяет с ключами src/i18n_en.ts; выводит недостающие ключи.
const fs = require('fs'), path = require('path');
const SRC = path.join(__dirname, '..', 'src');
const dict = fs.readFileSync(path.join(SRC, 'i18n_en.ts'), 'utf8');
const have = new Set(); for (const m of dict.matchAll(/^\s*("(?:[^"\\]|\\.)*"):/gm)) have.add(JSON.parse(m[1]));
const miss = new Map();
(function walk(d) { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name);
  if (e.isDirectory()) walk(p); else if (/\.tsx?$/.test(e.name) && e.name !== 'i18n_en.ts') {
    for (const m of fs.readFileSync(p, 'utf8').matchAll(/\btrf?\(("(?:[^"\\]|\\.)*")/g)) { const k = JSON.parse(m[1]).trim(); if (k && !have.has(k) && /[А-Яа-яЁё]/.test(k)) miss.set(k, path.relative(SRC, p)) }
  } } })(SRC);
for (const [k, f] of miss) console.log(`${f}\t${JSON.stringify(k)}`);
console.error(`без перевода: ${miss.size}`); process.exit(miss.size ? 1 : 0);
