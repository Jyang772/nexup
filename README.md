# Nexup

A small Rust pull-through cache for Docker Hub images, PyPI distributions, and npm packages. Includes an embedded, read-only dashboard, SQLite metadata, and deduplicated SHA-256-addressed artifact storage.

## Run in WSL

```bash
cd /home/quaxton/Downloads/nexup
cargo build --release --locked
./target/release/nexup serve
```

Dashboard: <http://localhost:8080>. Health: `/health`. Metrics: `/metrics`. JSON dashboard data: `/api/status`.

### Registry browser

Click a registry on the overview to browse its cached images/packages:

- `/ui/registries/docker` — repositories, with detail pages showing cached tags, manifest digests, platforms, and layer counts.
- `/ui/registries/pypi` — normalized Python package names, distribution versions, Python requirements, hashes, and yanked status.
- `/ui/registries/npm` — packages (including scopes), descriptions, licenses, dist-tags, versions, engine requirements, and integrity information.

Lists support name search and pagination (50 names per page). Click a name for its detail page, including associated cached files, sizes, SHA-256 digests, and UTC cache/access timestamps. Version tables distinguish cached artifacts from versions merely listed in cached upstream metadata; they display up to 200 records, cached artifacts first. Blob sizes deduplicate within each displayed package, so package totals should not be summed to measure global disk usage.

Browsing reads local cache data only and works offline; it never downloads or refreshes an artifact. Information depends on which indexes/manifests are cached. Standard artifact paths identify packages even when index metadata has been evicted; unrecognized/legacy paths are counted as unclassified. Registry pages use the same `dashboard_public` and admin credentials as the overview, independently of anonymous pull access.

Defaults: anonymous downloads, public read-only dashboard, `./data` storage, five-minute metadata freshness, 50 GiB storage target. Set `public_url` to the address your clients can reach: package links are rewritten using this value, never the incoming Host header.

To use a file:

```bash
cp nexup.example.toml nexup.local.toml
./target/release/nexup serve --config nexup.local.toml
```

## Docker deployment

```bash
docker compose up -d --build
docker compose logs -f nexup
```

The image runs as UID/GID 10001, serves port 8080, stores everything in `/data`, and handles SIGTERM gracefully. Compose uses a persistent named volume. One process must own each data directory; Nexup enforces an exclusive filesystem lock. Use a local filesystem, not a shared/network volume.

For a bind-mounted data directory, give UID 10001 write access. Terminate inbound HTTPS at your existing reverse proxy and configure `NEXUP_PUBLIC_URL=https://nexup.example.com`. Nexup's listener is HTTP; outbound HTTPS always verifies certificates.

## Client configuration

### Docker Hub

Explicit proxy hostname:

```bash
docker pull localhost:8080/library/alpine:latest
```

To keep original image names, merge this into Docker daemon configuration (`/etc/docker/daemon.json`) and restart Docker:

```json
{
  "registry-mirrors": ["https://nexup.example.com"]
}
```

Docker Hub mirror mode can fall back to Docker Hub if the mirror fails; use the explicit proxy hostname if you need cache-only routing. Non-local HTTP endpoints require Docker's insecure-registry configuration; HTTPS is the normal deployment setup. BuildKit/containerd have their own mirror settings.

When anonymous pulls are disabled, use `docker login nexup.example.com` with the configured client username/password and pull using that hostname. Upstream Docker Hub credentials are separate, via `NEXUP_DOCKER_USERNAME` and `NEXUP_DOCKER_PASSWORD` (a Docker Hub access token can be used as the password).

### PyPI

```bash
pip install --index-url http://localhost:8080/pypi/simple/ requests
# Persistent setting:
pip config set global.index-url https://nexup.example.com/pypi/simple/
```

Supports normalized project names, PEP 503 HTML, PEP 691 JSON, hashes, yanked/requires-python attributes, and PEP 658/714 metadata downloads. For private access, use pip's standard `.netrc` credentials for the Nexup hostname. Avoid adding PyPI as an extra index if every request should use Nexup.

### npm

```bash
npm config set registry http://localhost:8080/npm/
npm install is-number
```

For private access, configure `.npmrc`:

```ini
registry=https://nexup.example.com/npm/
//nexup.example.com/npm/:_authToken=${NEXUP_CLIENT_TOKEN}
```

Set `NEXUP_CLIENT_TOKEN` in the client environment to the server's `NEXUP_AUTH_PASSWORD`. Scoped packages, full/abbreviated packuments, and rewritten tarball links are supported. Existing lockfiles can retain direct upstream URLs; regenerate or inspect those when migrating. npm audit, search, login APIs, and package publication are outside this read-only cache's scope; use `--no-audit` when needed.

## Authentication

```bash
export NEXUP_ALLOW_ANONYMOUS_PULL=false
export NEXUP_AUTH_USERNAME=nexup
export NEXUP_AUTH_PASSWORD='your-pull-secret'
export NEXUP_DASHBOARD_PUBLIC=false
export NEXUP_ADMIN_TOKEN='your-separate-dashboard-secret'
./target/release/nexup
```

Clients support HTTP Basic or Bearer authentication. Dashboard/metrics/API access supports Bearer `NEXUP_ADMIN_TOKEN`, or browser Basic authentication with username `admin` and that token as password. The dashboard is read-only; there are no administrative mutation endpoints. `/health` remains unauthenticated. Credentials are deployment-wide: upstream content cached using a configured account is accessible to all authorized pull clients (including anonymous clients if enabled).

## Corporate proxies and CA certificates

```bash
export HTTP_PROXY=http://proxy.example.com:3128
export HTTPS_PROXY=http://proxy.example.com:3128
export NO_PROXY=localhost,127.0.0.1,.example.com
export SSL_CERT_FILE=/home/quaxton/certs/corporate-ca.pem
./target/release/nexup
```

Lowercase `http_proxy`, `https_proxy`, and `no_proxy` are supported and take precedence over uppercase variants. Standard environment variables override `[proxy]` fields in TOML. An empty proxy environment variable disables that proxy. `NO_PROXY` uses Reqwest's host/domain/IP matching.

`SSL_CERT_FILE` loads a PEM bundle of additional trusted CA certificates alongside the public WebPKI roots. It applies to all outbound requests, including Docker Hub authentication. It never disables certificate or hostname verification. Missing, unreadable, or empty certificate bundles fail startup. The PEM must contain certificates, not a private key. This configures **runtime** trust; Cargo and Docker build-time proxy/CA settings are configured separately.

For Compose, uncomment the corporate CA bind mount in `compose.yaml`, then set the **container path**:

```bash
export SSL_CERT_FILE=/certs/corporate-ca.pem
export HTTPS_PROXY=http://proxy.example.com:3128
docker compose up -d --build
```

The supplied Compose file forwards uppercase/lowercase proxy variables, `SSL_CERT_FILE`, and optional credentials from your shell.

## Configuration reference

`--config FILE` overrides `NEXUP_CONFIG`. Otherwise built-in defaults are used. Unknown TOML fields are errors. Environment overrides:

| Environment | TOML field |
|---|---|
| `NEXUP_LISTEN` | `server.listen` |
| `NEXUP_PUBLIC_URL` | `server.public_url` |
| `NEXUP_REQUEST_TIMEOUT_SECONDS` | `server.request_timeout_seconds` |
| `NEXUP_DATA_DIR` | `cache.path` |
| `NEXUP_METADATA_TTL_SECONDS` | `cache.metadata_ttl_seconds` |
| `NEXUP_SERVE_STALE_ON_ERROR` | `cache.serve_stale_on_error` |
| `NEXUP_MAX_SIZE_BYTES` | `cache.max_size_bytes` (0 = unlimited) |
| `NEXUP_CLEANUP_INTERVAL_SECONDS` | `cache.cleanup_interval_seconds` |
| `NEXUP_ALLOW_ANONYMOUS_PULL` | `auth.allow_anonymous_pull` |
| `NEXUP_AUTH_USERNAME`, `NEXUP_AUTH_PASSWORD` | `auth.username`, `auth.password` |
| `NEXUP_DASHBOARD_PUBLIC`, `NEXUP_ADMIN_TOKEN` | `auth.dashboard_public`, `auth.admin_token` |
| `NEXUP_DOCKER_ENABLED`, `NEXUP_NPM_ENABLED`, `NEXUP_PYPI_ENABLED` | `<registry>.enabled` |
| `NEXUP_DOCKER_UPSTREAM`, `NEXUP_NPM_UPSTREAM`, `NEXUP_PYPI_UPSTREAM` | `<registry>.upstream` |
| `NEXUP_DOCKER_USERNAME`, `NEXUP_DOCKER_PASSWORD` | `docker.username`, `docker.password` |
| `NEXUP_NPM_TOKEN`, `NEXUP_PYPI_TOKEN` | `<registry>.token` |
| `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` | `proxy.http_proxy`, `proxy.https_proxy`, `proxy.no_proxy` |
| `SSL_CERT_FILE` | `proxy.ssl_cert_file` |
| `RUST_LOG` | Logging filter (default `nexup=info`) |

`npm.artifact_hosts` and `pypi.artifact_hosts` allow additional HTTPS artifact hosts; same-origin URLs are allowed automatically. npm/PyPI upstream Bearer tokens are sent only to the configured upstream origin. Docker supports Docker Hub's Bearer-token exchange and same-origin token services for custom upstreams.

## Storage and cache semantics

```text
data/
├── metadata.sqlite          # URL-key hashes, refs, freshness, recent activity
├── metadata.sqlite-wal      # SQLite WAL while running
├── blobs/sha256/ab/<digest>  # Shared content-addressed bytes
├── tmp/                     # Incomplete downloads, never served
└── .nexup.lock
```

- Downloads stream to disk, not RAM. Nexup verifies known SHA-256 hashes and atomically publishes complete blobs before serving the first client; clients wait for the initial download. npm integrity metadata is preserved for client-side validation.
- Concurrent requests for the same URL/representation share one upstream download. Artifacts with identical content deduplicate across URLs and protocols.
- Docker digest references, wheels, source distributions, and npm tarballs are immutable cache entries. Tags and package indexes have configurable TTLs and conditional revalidation.
- Metadata can be served stale on upstream connection errors, HTTP 5xx, or 429. Upstream 401/403/404 responses are not intentionally masked. Cache misses still require upstream connectivity.
- Range downloads, HEAD, ETags, and Docker digest headers are supported. Multi-range requests fall back to the full representation.
- Periodic cleanup evicts least-recently-used blobs until the storage target is met and removes orphaned content. It waits for active cache operations; open Linux file streams survive unlink. The limit is a **soft target**, not a filesystem quota: active downloads, SQLite, and the cleanup interval can temporarily exceed it. An individual object larger than the target is rejected.
- Dashboard counts are cache objects rather than unique package versions. Upstream health is the last real fetch result, not a periodic active probe. Process counters reset on restart; cached data and recent activity persist.

This first version is a single-node, read-only proxy with one upstream per format. It does not implement hosted uploads, Nexus repository groups, per-user ACLs, active-active clustering, S3 storage, or Docker tag/catalog listing. Back up the data directory with Nexup stopped so SQLite and blob files are consistent.

## Development checks

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
python3 tests/tls_proxy_smoke.py target/release/nexup
docker compose build
docker build -t nexup:dev .
python3 tests/container_smoke.py nexup:dev
# Optional: public registries + installed pip, npm, and Docker daemon:
python3 tests/clients_smoke.py target/release/nexup
```

Integration tests use local mock upstreams to cover authentication, Docker token exchange, integrity rejection, range requests, metadata revalidation, npm/PyPI rewriting, concurrent misses, and offline cache hits. The Python test uses OpenSSL and a local HTTPS/CONNECT proxy to verify custom CA trust, certificate rejection, and NO_PROXY bypass.
