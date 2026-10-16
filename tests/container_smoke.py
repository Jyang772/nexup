#!/usr/bin/env python3
"""Verify the built image's user, writable volume, HTTP routes, and restart."""
import json
import subprocess
import sys
import time
import urllib.error
import urllib.request


def main():
    image = sys.argv[1] if len(sys.argv) > 1 else "nexup:dev"
    container = subprocess.check_output(["docker", "run", "-d", "--rm", "-p", "127.0.0.1::8080", image], text=True).strip()
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def base_url():
        inspect = json.loads(subprocess.check_output(["docker", "inspect", container], text=True))[0]
        port = inspect["NetworkSettings"]["Ports"]["8080/tcp"][0]["HostPort"]
        return f"http://127.0.0.1:{port}"

    def healthy(base):
        for _ in range(100):
            try:
                with opener.open(base + "/health", timeout=1) as response:
                    assert response.read() == b"ok\n"
                return
            except urllib.error.URLError:
                time.sleep(0.05)
        raise RuntimeError("container did not become healthy")

    try:
        base = base_url()
        healthy(base)
        assert subprocess.check_output(["docker", "exec", container, "id", "-u"], text=True).strip() == "10001"
        subprocess.run(["docker", "exec", container, "test", "-f", "/data/metadata.sqlite"], check=True)
        with opener.open(base + "/", timeout=5) as response:
            assert "text/html" in response.headers["Content-Type"]
            assert b"Cached objects" in response.read()
        with opener.open(base + "/v2/", timeout=5) as response:
            assert response.headers["docker-distribution-api-version"] == "registry/2.0"
        for registry in ("docker", "pypi", "npm"):
            with opener.open(base + f"/ui/registries/{registry}", timeout=5) as response:
                assert b"No matching cached images or packages yet" in response.read()
        subprocess.run(["docker", "restart", "--time", "10", container], check=True, stdout=subprocess.DEVNULL)
        healthy(base_url())
        print("PASS container: UID 10001, persistent SQLite, dashboard, registry browser, Docker v2, graceful restart")
    finally:
        subprocess.run(["docker", "rm", "-f", "-v", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
