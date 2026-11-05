// Оборачивает новые русские строки панели в tr()/trf(). Запуск из admin: node tools/i18n_wrap.cjs src [--dry]
// Пишет keys.json (ключи по файлам в порядке появления) и review.txt (сомнительные места — не тронуты).
const ts = require(require('path').join(__dirname, '..', 'node_modules', 'typescript'));
const fs = require('fs'), path = require('path');
const SRC = path.resolve(process.argv[2]); const DRY = process.argv.includes('--dry');
const CYR = /[А-Яа-яЁё]/;
const SKIP = new Set(['i18n.tsx', 'i18n_en.ts', 'vite-env.d.ts']);
// Пишет в текущую папку keys.json (новые ключи) и review.txt (места, оставленные без изменений: сравнения, ключи и т. п.).
const ENT = { nbsp: ' ', laquo: '«', raquo: '»', mdash: '—', ndash: '–', amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", hellip: '…', times: '×', larr: '←', rarr: '→', middot: '·' };
const decode = (s, f) => s.replace(/&(#x?[0-9a-fA-F]+|[a-z]+);/g, (all, e) => {
  if (e[0] === '#') return String.fromCodePoint(e[1] === 'x' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10));
  if (ENT[e] !== undefined) return ENT[e];
  console.error('?entity', f, all); return all;
});
const files = []; (function walk(d) { for (const e of fs.readdirSync(d, { withFileTypes: true })) { const p = path.join(d, e.name); if (e.isDirectory()) walk(p); else if (/\.tsx?$/.test(e.name) && !SKIP.has(e.name)) files.push(p) } })(SRC);
const keys = {}; const review = [];
const q = s => JSON.stringify(s);
const CMP = new Set([ts.SyntaxKind.EqualsEqualsEqualsToken, ts.SyntaxKind.ExclamationEqualsEqualsToken, ts.SyntaxKind.EqualsEqualsToken, ts.SyntaxKind.ExclamationEqualsToken, ts.SyntaxKind.InKeyword]);
const STRM = new Set(['includes', 'startsWith', 'endsWith', 'indexOf', 'match', 'test', 'split', 'replace', 'replaceAll', 'search', 'lastIndexOf']);
let total = 0;
for (const f of files) {
  const text = fs.readFileSync(f, 'utf8');
  const sf = ts.createSourceFile(f, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const rel = path.relative(SRC, f); const fk = keys[rel] = [];
  let used = { tr: false, trf: false };
  const line = n => sf.getLineAndCharacterOfPosition(n.getStart()).line + 1;
  const add = k => { fk.push(k); total++; };
  // Решение по строковому литералу: 'wrap' | 'skip' | 'review'
  const decide = n => {
    const p = n.parent;
    if (ts.isImportDeclaration(p) || ts.isExportDeclaration(p) || ts.isExternalModuleReference(p)) return 'skip';
    if (ts.isLiteralTypeNode(p)) return 'skip';
    if (ts.isCallExpression(p) && ts.isIdentifier(p.expression) && ['tr', 'trf', 'trx'].includes(p.expression.text)) return 'skip'; // уже обёрнута
    if ((ts.isPropertyAssignment(p) || ts.isPropertySignature(p) || ts.isMethodDeclaration(p) || ts.isPropertyDeclaration(p) || ts.isEnumMember(p)) && p.name === n) return 'skip';
    if (ts.isElementAccessExpression(p) && p.argumentExpression === n) return 'review';
    if (ts.isBinaryExpression(p) && CMP.has(p.operatorToken.kind)) return 'review';
    if (ts.isCaseClause(p)) return 'review';
    if (ts.isCallExpression(p) && ts.isPropertyAccessExpression(p.expression) && STRM.has(p.expression.name.text) && p.arguments.includes(n)) return 'review';
    return 'wrap';
  };
  const out = (node) => {
    // JSX-текст
    if (ts.isJsxText(node) && CYR.test(node.text)) {
      const raw = node.text; const m = /^(\s*)([\s\S]*?)(\s*)$/.exec(raw);
      const core = decode(m[2].replace(/[ \t]*\r?\n\s*/g, ' '), rel);
      // Значимые пробелы JSX: пробел без перевода строки на краю — часть текста.
      add(core); used.tr = true;
      return m[1] + '{tr(' + q(core) + ')}' + m[3];
    }
    if ((ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) && CYR.test(node.text)) {
      const d = decide(node);
      if (d === 'skip') return node.getText();
      if (d === 'review') { review.push(`${rel}:${line(node)}: ${node.parent.getText().slice(0, 160)}`); return node.getText(); }
      const isAttr = ts.isJsxAttribute(node.parent);
      const k = isAttr ? decode(node.text, rel) : node.text;
      add(k); used.tr = true;
      return isAttr ? '{tr(' + q(k) + ')}' : 'tr(' + q(k) + ')';
    }
    if (ts.isTemplateExpression(node) && (CYR.test(node.head.text) || node.templateSpans.some(s => CYR.test(s.literal.text)))) {
      if (ts.isTaggedTemplateExpression(node.parent)) { review.push(`${rel}:${line(node)}: tagged`); return node.getText(); }
      let k = node.head.text; const args = [];
      node.templateSpans.forEach((s, i) => { k += '{' + i + '}' + s.literal.text; args.push(out(s.expression)); });
      const d = decide(node);
      if (d === 'review') { review.push(`${rel}:${line(node)}: TEMPLATE ${node.parent.getText().slice(0, 160)}`); return node.getText(); }
      add(k); used.trf = true;
      return 'trf(' + q(k) + ', ' + args.join(', ') + ')';
    }
    const kids = node.getChildren(sf);
    if (!kids.length) return node.getText(sf);
    let res = '', cur = node.getStart(sf);
    for (const c of kids) { if (ts.isJSDoc(c)) continue; res += text.slice(cur, c.getStart(sf)); res += out(c); cur = c.end; }
    return res;
  };
  let body = text.slice(0, sf.getStart(sf)) + out(sf);
  body += text.slice(sf.end);
  if (used.tr || used.trf) {
    let imp = path.relative(path.dirname(f), path.join(SRC, 'i18n')).replace(/\\/g, '/'); if (!imp.startsWith('.')) imp = './' + imp;
    const had = new Set(); for (const s of sf.statements) if (ts.isImportDeclaration(s) && /i18n['"]$/.test(s.moduleSpecifier.getText(sf)) && s.importClause?.namedBindings && ts.isNamedImports(s.importClause.namedBindings)) s.importClause.namedBindings.elements.forEach(e => had.add(e.name.text));
    const names = [used.tr && !had.has('tr') && 'tr', used.trf && !had.has('trf') && 'trf'].filter(Boolean).join(', ');
    if (names) {
    // После последнего import в начале файла
    const imps = sf.statements.filter(s => ts.isImportDeclaration(s));
    const idx = imps.length ? body.indexOf(imps[imps.length - 1].getText(sf)) + imps[imps.length - 1].getText(sf).length : 0;
      body = body.slice(0, idx) + (idx ? '\n' : '') + `import { ${names} } from '${imp}'` + (idx ? '' : '\n') + body.slice(idx);
    }
  }
  if (!DRY && body !== text) fs.writeFileSync(f, body);
}
const dir = process.cwd();
fs.writeFileSync(path.join(dir, 'keys.json'), JSON.stringify(keys, null, 1));
fs.writeFileSync(path.join(dir, 'review.txt'), review.join('\n') + '\n');
console.log('files', files.length, 'strings', total, 'unique', new Set(Object.values(keys).flat()).size, 'review', review.length);
