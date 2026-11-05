#!/usr/bin/env python3
"""Письма, принятые smtp_sink.py, — построчно JSON {file, to, subject, body} (тема и текст раскодированы)."""
import email
import email.policy
import json
import os
import sys

d = sys.argv[1]
for name in sorted(os.listdir(d)):
    if not name.endswith(".eml"):
        continue
    with open(os.path.join(d, name), "rb") as f:
        msg = email.message_from_binary_file(f, policy=email.policy.default)
    body = msg.get_body(preferencelist=("plain",))
    print(json.dumps({"file": name, "to": msg.get("X-Rcpt", ""), "subject": str(msg.get("Subject", "")),
                      "body": body.get_content() if body else ""}, ensure_ascii=False))
