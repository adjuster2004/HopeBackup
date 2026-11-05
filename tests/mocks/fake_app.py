#!/usr/bin/env python3
"""Имитация службы продукта Hope для проверок: HTTPS на порту (ответ на любой путь) и, по желанию,
дозапись в «WAL» каталога данных раз в 0,2 с — чтобы проверить, что копия снимается при остановленной
службе.

  fake_app.py <порт> <сертификат> <ключ> [файл WAL]
"""
import http.server, ssl, sys, threading, time, os

port, cert, key = int(sys.argv[1]), sys.argv[2], sys.argv[3]
wal = sys.argv[4] if len(sys.argv) > 4 else ""

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"ok":true}')
    def log_message(self, *a):
        pass

def writer():
    n = 0
    while True:
        try:
            with open(wal, "a") as f:
                f.write("rec %d\n" % n)
            n += 1
        except OSError:
            pass
        time.sleep(0.2)

if wal:
    threading.Thread(target=writer, daemon=True).start()
srv = http.server.ThreadingHTTPServer(("127.0.0.1", port), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(cert, key)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
