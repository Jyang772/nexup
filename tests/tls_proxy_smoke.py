#!/usr/bin/env python3
"""Exercise the binary with a private TLS CA, CONNECT proxy, and NO_PROXY."""
import http.server
import json
import os
from pathlib import Path
import select
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request


class Upstream(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        data = json.dumps({"name": "demo", "versions": {}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_):
        pass


class Proxy(http.server.BaseHTTPRequestHandler):
    connections = 0

    def do_CONNECT(self):
        host, port = self.path.rsplit(":", 1)
        type(self).connections += 1
        with socket.create_connection((host, int(port)), timeout=10) as upstream:
            self.send_response(200)
            self.end_headers()
            peers = [self.connection, upstream]
            while True:
                ready, _, _ = select.select(peers, [], [], 10)
                if not ready:
                    return
                for peer in ready:
                    data = peer.recv(65536)
                    if not data:
                        return
                    (upstream if peer is self.connection else self.connection).sendall(data)

    def log_message(self, *_):
        pass


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/nexup").resolve())
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with tempfile.TemporaryDirectory(prefix="nexup-tls-") as directory:
        root = Path(directory)
        cert, key = root / "ca.pem", root / "ca-key.pem"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-addext", "basicConstraints=critical,CA:TRUE", "-keyout", str(key), "-out", str(cert),
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        server_key, csr, server_cert = root / "server-key.pem", root / "server.csr", root / "server.pem"
        subprocess.run(["openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", str(server_key), "-out", str(csr)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        extensions = root / "extensions.cnf"
        extensions.write_text("subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
        subprocess.run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(cert), "-CAkey", str(key), "-CAcreateserial", "-days", "1", "-extfile", str(extensions), "-out", str(server_cert)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(server_cert, server_key)
        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        upstream.socket = context.wrap_socket(upstream.socket, server_side=True)
        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
        for server in (upstream, proxy):
            threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            for name, trusted, bypass, lowercase in [
                ("untrusted", False, False, False),
                ("trusted-proxy", True, False, False),
                ("trusted-no-proxy", True, True, True),
            ]:
                listen = port()
                env = {k: v for k, v in os.environ.items() if k.lower() not in {"http_proxy", "https_proxy", "no_proxy", "all_proxy", "ssl_cert_file"} and not k.startswith("NEXUP_")}
                env.update({"NEXUP_LISTEN": f"127.0.0.1:{listen}", "NEXUP_PUBLIC_URL": f"http://127.0.0.1:{listen}", "NEXUP_DATA_DIR": str(root / name), "NEXUP_NPM_UPSTREAM": f"https://localhost:{upstream.server_port}"})
                env["https_proxy" if lowercase else "HTTPS_PROXY"] = f"http://127.0.0.1:{proxy.server_port}"
                env["no_proxy" if lowercase else "NO_PROXY"] = "localhost" if bypass else ""
                if trusted:
                    env["SSL_CERT_FILE"] = str(cert)
                before = Proxy.connections
                with (root / f"{name}.log").open("w") as log:
                    process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
                    try:
                        for _ in range(100):
                            if process.poll() is not None:
                                raise RuntimeError((root / f"{name}.log").read_text())
                            try:
                                with opener.open(f"http://127.0.0.1:{listen}/health", timeout=1):
                                    break
                            except urllib.error.URLError:
                                time.sleep(0.05)
                        else:
                            raise RuntimeError("Nexup did not start")
                        try:
                            with opener.open(f"http://127.0.0.1:{listen}/npm/demo", timeout=20) as response:
                                status = response.status
                        except urllib.error.HTTPError as error:
                            status = error.code
                        assert status == (200 if trusted else 502), (name, status)
                        assert (Proxy.connections > before) != bypass, (name, Proxy.connections, before)
                        print(f"PASS {name}: status={status}, CONNECT requests={Proxy.connections - before}")
                    finally:
                        process.terminate()
                        try:
                            process.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
        finally:
            for server in (upstream, proxy):
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    main()
