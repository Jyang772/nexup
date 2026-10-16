#!/usr/bin/env python3
"""Opt-in public registry smoke test with real pip/npm/Docker clients.

Uses temporary client/cache directories and removes the test Docker image tag.
The second pass restarts Nexup with an unreachable outbound proxy.
"""
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/nexup").resolve())
    for command in ("python3", "npm", "docker"):
        if not shutil.which(command):
            raise RuntimeError(f"{command} is required")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    base = f"http://localhost:{port}"
    image = f"localhost:{port}/library/busybox:1.37.0"
    with tempfile.TemporaryDirectory(prefix="nexup-clients-") as directory:
        root = Path(directory)
        try:
            for mode in ("online", "offline"):
                env = {k: v for k, v in os.environ.items() if not k.startswith("NEXUP_")}
                env.update({"NEXUP_LISTEN": f"127.0.0.1:{port}", "NEXUP_PUBLIC_URL": base, "NEXUP_DATA_DIR": str(root / "data"), "NEXUP_METADATA_TTL_SECONDS": "3600"})
                if mode == "offline":
                    for k in list(env):
                        if k.lower() in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
                            del env[k]
                    env.update({"HTTP_PROXY": "http://127.0.0.1:1", "HTTPS_PROXY": "http://127.0.0.1:1", "NO_PROXY": ""})
                work = root / mode
                work.mkdir()
                (work / "package.json").write_text('{"name":"nexup-smoke","version":"1.0.0","private":true}')
                with (work / "server.log").open("w") as log:
                    process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
                    try:
                        for _ in range(100):
                            if process.poll() is not None:
                                raise RuntimeError((work / "server.log").read_text())
                            try:
                                with opener.open(base + "/health", timeout=1):
                                    break
                            except urllib.error.URLError:
                                time.sleep(0.05)
                        else:
                            raise RuntimeError("Nexup did not start")
                        client_env = os.environ.copy()
                        client_env.update({"NO_PROXY": "localhost,127.0.0.1", "no_proxy": "localhost,127.0.0.1", "PIP_CONFIG_FILE": os.devnull, "PIP_DISABLE_PIP_VERSION_CHECK": "1"})
                        client_env.pop("PIP_EXTRA_INDEX_URL", None)
                        subprocess.run(["python3", "-m", "pip", "install", "--no-cache-dir", "--no-deps", "--index-url", base + "/pypi/simple/", "--target", str(work / "python"), "packaging==25.0"], env=client_env, check=True, timeout=180)
                        subprocess.run(["npm", "install", "--ignore-scripts", "--no-audit", "--no-fund", "--registry", base + "/npm/", "--cache", str(work / "npm-cache"), "is-number@7.0.0", "@types/node@22.10.0"], cwd=work, env=client_env, check=True, timeout=180)
                        subprocess.run(["docker", "pull", image], check=True, timeout=180)
                        with opener.open(base + "/api/status", timeout=5) as response:
                            status = json.load(response)
                        if mode == "offline":
                            assert status["misses"] == 0, status
                            assert status["hits"] > 0, status
                        print(f"PASS {mode}: hits={status['hits']}, misses={status['misses']}, bytes={status['blob_bytes']}", flush=True)
                        subprocess.run(["docker", "image", "rm", image], check=True, stdout=subprocess.DEVNULL)
                    finally:
                        process.terminate()
                        try:
                            process.wait(timeout=15)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
        finally:
            subprocess.run(["docker", "image", "rm", image], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
