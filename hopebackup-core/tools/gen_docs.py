#!/usr/bin/env python3
"""
Документация HopeBackup («Ковчег») в HTML — оформление как у документов других продуктов Hope
(HopeDFS, HopeInventory): обложка, содержание, печать в A4.

Исходники — Markdown в docs/*.md, результат — docs/*.html и docs/index.html (содержание документации).

Запуск из корня HopeBackup:  python3 hopebackup-core/tools/gen_docs.py   (нужен пакет markdown-it-py: pip install markdown-it-py)
Разметка — CommonMark с таблицами (вложенные списки с отступом 2–3 пробела, код внутри пунктов списка).
"""
import datetime, html, os, re, sys, urllib.parse

from markdown_it import MarkdownIt

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CORE = os.path.join(ROOT, "hopebackup-core")
DOCS_DIR = os.path.join(ROOT, "docs")
CSS = open(os.path.join(os.path.dirname(__file__), "doc_style.css"), encoding="utf-8").read() + """
  .cards{ display:grid; grid-template-columns:repeat(auto-fill,minmax(260px,1fr)); gap:14px; margin:18px 0; }
  .card{ display:block; border:1px solid var(--line); border-radius:10px; padding:14px 16px; color:var(--ink); text-decoration:none; }
  .card:hover{ border-color:var(--blue); box-shadow:0 2px 10px rgba(29,78,216,.08); }
  .card .n{ color:var(--blue); font-weight:700; font-size:12px; letter-spacing:.08em; }
  .card .t{ font-weight:700; font-size:16px; margin:4px 0; }
  .card .d{ color:var(--muted); font-size:13px; }
  .nav{ font-size:13px; margin-bottom:6px; } .nav a{ color:var(--blue); text-decoration:none; }
  a{ color:var(--blue); }
  td code, li code{ word-break:break-word; }
"""
VER = re.search(r'^version\s*=\s*"([^"]+)"', open(os.path.join(CORE, "Cargo.toml"), encoding="utf-8").read(), re.M).group(1)
TODAY = datetime.date.today().strftime("%d.%m.%Y")
PRODUCT = "HopeBackup «Ковчег»"

# (файл без .md, метка на карточке, подзаголовок)
DOCS = [
    ("01_Установка", "ДОКУМЕНТ 1", "Роли и требования, ресурсы, установка скриптом и с Windows, узлы площадок, сервер управления, агенты"),
    ("02_Администрирование", "ДОКУМЕНТ 2", "Разделы панели, площадки и коннекторы, расписания и хранение, продукты, восстановление, командная строка"),
    ("03_Аварийное_восстановление", "ДОКУМЕНТ 3", "Принимающие площадки и узлы управления, потеря основной площадки, «Принять управление»"),
    ("06_Переезд", "ДОКУМЕНТ 6", "Перенос Linux-машин на Proxmox VE и zVirt / oVirt / РОСА: реплика, тест без сети, переключение, откат"),
    ("07_Обновление", "ДОКУМЕНТ 7", "Обновление работающих серверов: из панели и скриптом, порядок для узлов и агентов, откат"),
    ("ARCHITECTURE", "СПРАВОЧНИК", "Модули, формат репозитория, протоколы сервера, агентов и площадок, устойчивость к сбоям"),
    ("API", "СПРАВОЧНИК", "REST API сервера управления: вход, экземпляры, площадки, восстановление, переезд, обновления"),
    ("05_Дорожная_карта", "ДОКУМЕНТ 5", "Что сделано по версиям и ближайшие задачи"),
]


def slug(text):
    t = re.sub(r"<[^>]+>", "", html.unescape(text)).lower()
    t = re.sub(r"[^\w\s-]", "", t, flags=re.U)
    return re.sub(r"\s+", "-", t.strip())


def page(title, sub, label, body, toc=True, nav=True):
    heads = re.findall(r'<h2 id="([^"]+)">(.*?)</h2>', body)
    toc_html = ""
    if toc and len(heads) > 2:
        toc_html = '<h2 id="toc">Содержание</h2>\n<div class="toc"><ol>\n' + "\n".join(
            f'  <li><a href="#{i}">{re.sub("<[^>]+>", "", t)}</a></li>' for i, t in heads) + "\n</ol></div>\n"
    nav_html = '<div class="nav"><a href="index.html">← Содержание документации</a></div>' if nav else ""
    meta = f"{label.capitalize()} · HopeBackup {VER} · {TODAY}" if label else f"HopeBackup {VER} · {TODAY}"
    return f"""<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>HopeBackup — {html.escape(title)}</title>
<style>
{CSS}
</style>
</head>
<body>

<section class="cover">
  <div class="tag">{PRODUCT} · Документация</div>
  <h1>{html.escape(title)}</h1>
  <div class="sub">{html.escape(sub)}</div>
  <div class="meta">{meta}</div>
</section>

<div class="page">
{nav_html}
{toc_html}
{body}

<footer>
HopeBackup {VER} · {html.escape(title)}
</footer>

</div>
</body>
</html>
"""


def title_of(first_line, name):
    t = first_line.strip().lstrip("#").strip()
    t = re.sub(r"^HopeBackup\s+—\s+", "", t)
    return (t[0].upper() + t[1:]) if t else name


def md_doc(name, label, sub):
    text = open(os.path.join(DOCS_DIR, name + ".md"), encoding="utf-8").read()
    m = re.match(r"#\s+(.+)\n", text)
    title = title_of(m.group(1), name) if m else name
    text = text[m.end():] if m else text
    body = MarkdownIt("commonmark").enable("table").render(text)
    # Якоря заголовков (для содержания и ссылок вида документ.html#раздел).
    seen = {}
    def anchor(m):
        lvl, inner = m.group(1), m.group(2)
        a = slug(inner) or "section"
        n = seen.get(a, 0); seen[a] = n + 1
        if n: a = f"{a}-{n}"
        return f'<h{lvl} id="{a}">{inner}</h{lvl}>'
    body = re.sub(r"<h([2-4])>(.*?)</h\1>", anchor, body)
    # Ссылки между документами: X.md → X.html (документы без HTML-версии остаются как есть).
    def fix(m):
        target = urllib.parse.unquote(m.group(1))
        if os.path.exists(os.path.join(DOCS_DIR, target + ".md")):
            return f'href="{target}.html{m.group(2) or ""}"'
        return m.group(0)
    body = re.sub(r'href="([^"#:]+)\.md(#[^"]*)?"', fix, body)
    body = body.replace("\\*", "*")
    open(os.path.join(DOCS_DIR, name + ".html"), "w", encoding="utf-8").write(page(title, sub, label, body))
    return title


def index(titles):
    cards = [f'<a class="card" href="{n}.html"><div class="n">{l}</div><div class="t">{html.escape(t)}</div><div class="d">{html.escape(s)}</div></a>'
             for (n, l, s), t in titles]
    body = f"""<p>HopeBackup («Ковчег») — резервное копирование и восстановление инфраструктуры: продукты Hope, СУБД,
приложения, серверы и образы ВМ целиком; свои узлы хранения, объектные хранилища и облачные диски; восстановление
на «чистый» сервер или новую ВМ, в том числе на момент времени; принимающие площадки и узлы управления на случай
потери основной площадки; переезд машин на другие платформы виртуализации. Документы открываются в любом браузере
и печатаются в A4 (Ctrl+P).</p>
<div class="cards">
{chr(10).join(cards)}
</div>
<h2 id="files">Файлы поставки</h2>
<table>
<tr><th>Путь</th><th>Назначение</th></tr>
<tr><td><code>bin/release/hopebackup-&lt;версия&gt;-linux.tar.gz</code>, <code>hopebackup-install.sh</code>, <code>SHA256SUMS</code></td><td>Архив выпуска со всеми сборками и скрипт установки и обновления на Linux; этот же архив загружается в раздел «Обновления»</td></tr>
<tr><td><code>bin/hopebackup.gz</code>, <code>bin/dist/</code></td><td>Основная программа (x86_64, glibc ≥ 2.17) и комплект сборок: x86_64 и ARM64, glibc и musl</td></tr>
<tr><td><code>_deploy/deploy.cmd</code>, <code>deploy.ps1</code></td><td>Установка и обновление с Windows по SSH (PuTTY)</td></tr>
<tr><td><code>linux/</code></td><td>Исходники скриптов установки</td></tr>
<tr><td><code>hopebackup-core/</code>, <code>admin/</code></td><td>Исходный код: программа (Rust) и веб-панель (React)</td></tr>
<tr><td><code>tests/</code></td><td>Сквозные проверки на одной машине</td></tr>
<tr><td><code>docs/*.md</code></td><td>Исходники документации (Markdown); HTML пересобирается командой <code>python3 hopebackup-core/tools/gen_docs.py</code></td></tr>
</table>
"""
    open(os.path.join(DOCS_DIR, "index.html"), "w", encoding="utf-8").write(page("Документация", "Состав документации и файлы поставки", "", body, toc=False, nav=False))


def main():
    titles = [((n, l, s), md_doc(n, l, s)) for n, l, s in DOCS]
    index(titles)
    print(f"Документация HopeBackup {VER}: {len(titles)} документов + содержание (docs/index.html)")
    bad = []
    for f in sorted(os.listdir(DOCS_DIR)):
        if not f.endswith(".html"): continue
        s = open(os.path.join(DOCS_DIR, f), encoding="utf-8").read()
        ids = set(re.findall(r'id="([^"]+)"', s))
        for href in re.findall(r'href="([^"]+)"', s):
            if href.startswith(("http", "mailto:")): continue
            target, _, anchor = urllib.parse.unquote(href).partition("#")
            if target and not os.path.exists(os.path.join(DOCS_DIR, target)): bad.append(f"{f}: {href}")
            elif anchor:
                tids = ids if not target else set(re.findall(r'id="([^"]+)"', open(os.path.join(DOCS_DIR, target), encoding="utf-8").read()))
                if anchor not in tids: bad.append(f"{f}: {href}")
    if bad:
        print("битые ссылки:\n  " + "\n  ".join(bad)); return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
