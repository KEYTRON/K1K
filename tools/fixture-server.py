#!/usr/bin/env python3
"""The host side of the K1K network tests, on 127.0.0.1 (QEMU's user-mode
network reaches it as 10.0.2.2): plain HTTP on 18791 and HTTPS on 18792, both
serving one directory. The HTTPS certificate is the test-only one in
tools/testcert. Usage: fixture-server.py <directory>"""
import functools
import http.server
import os
import ssl
import sys
import threading

root = sys.argv[1]
here = os.path.dirname(os.path.abspath(__file__))
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=root)
handler.log_message = lambda *a, **k: None  # type: ignore

plain = http.server.ThreadingHTTPServer(("127.0.0.1", 18791), handler)
secure = http.server.ThreadingHTTPServer(("127.0.0.1", 18792), handler)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(os.path.join(here, "testcert", "cert.pem"), os.path.join(here, "testcert", "key.pem"))
secure.socket = ctx.wrap_socket(secure.socket, server_side=True)

threading.Thread(target=plain.serve_forever, daemon=True).start()
secure.serve_forever()
