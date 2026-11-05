#!/usr/bin/env python3
"""Заглушка SMTP для проверок: принимает письма без шифрования и входа и складывает их в каталог.

    python3 smtp_sink.py <порт> <каталог>

Каждое письмо — файл NNNN.eml (RCPT TO в заголовке X-Rcpt). Без внешних зависимостей (asyncio).
"""
import asyncio
import os
import sys

PORT = int(sys.argv[1])
OUT = sys.argv[2]
os.makedirs(OUT, exist_ok=True)
counter = [len(os.listdir(OUT))]


async def handle(reader, writer):
    def send(line):
        writer.write((line + "\r\n").encode())

    send("220 hb-test ESMTP")
    await writer.drain()
    rcpt, mail_from = [], ""
    while True:
        line = await reader.readline()
        if not line:
            break
        cmd = line.decode("utf-8", "replace").strip()
        up = cmd.upper()
        if up.startswith("EHLO"):
            writer.write(b"250-hb-test\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250 OK\r\n")
        elif up.startswith("HELO"):
            send("250 hb-test")
        elif up.startswith("MAIL FROM"):
            mail_from, rcpt = cmd[10:].strip(), []
            send("250 OK")
        elif up.startswith("RCPT TO"):
            rcpt.append(cmd[8:].strip().strip("<>"))
            send("250 OK")
        elif up == "DATA":
            send("354 end with .")
            await writer.drain()
            data = []
            while True:
                l = await reader.readline()
                if not l or l in (b".\r\n", b".\n"):
                    break
                if l.startswith(b".."):
                    l = l[1:]
                data.append(l)
            counter[0] += 1
            path = os.path.join(OUT, "%04d.eml" % counter[0])
            with open(path + ".tmp", "wb") as f:
                f.write(("X-Rcpt: %s\r\nX-From: %s\r\n" % (", ".join(rcpt), mail_from)).encode())
                f.write(b"".join(data))
            os.rename(path + ".tmp", path)
            send("250 queued")
        elif up == "RSET":
            rcpt = []
            send("250 OK")
        elif up == "NOOP":
            send("250 OK")
        elif up == "QUIT":
            send("221 bye")
            await writer.drain()
            break
        else:
            send("502 not implemented")
        await writer.drain()
    writer.close()


async def main():
    srv = await asyncio.start_server(handle, "127.0.0.1", PORT)
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
