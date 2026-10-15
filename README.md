# mihomo-freebox

A **single Docker image** (built with **podman** or docker) that gives you a
working [Mihomo Meta](https://github.com/MetaCubeX/mihomo) (Clash.Meta)
local proxy on **port 7890**, backed by **freshly-crawled free nodes** and the
**official MetaCubeXD dashboard** on **port 9090**.

- **no `docker-compose`** — one `Dockerfile`, one container, `--restart=always`.
- **no TUN** — proxying only, via `mixed-port: 7890` (HTTP + SOCKS5).
- on every `(re)start` the entrypoint **re-crawls** free nodes, renders the
  config, validates it with `mihomo -t`, then boots mihomo (supervised) behind
  the MetaCubeXD UI.
- **Geo data** (GeoSite/GeoIP) is **auto-downloaded** by mihomo from the
  actively-maintained [meta-rules-dat](https://github.com/MetaCubeX/meta-rules-dat)
  releases — nothing large is baked into the image.
- **Rule sets** come from the MetaCubeX CDN (daily updated); if a set is
  unreachable at start it stays empty — it never blocks startup.

---

## Quick start

```bash
# 1a. option A: pull the pre-built multi-arch image from Docker Hub
docker pull suiwenfeng/mihomo-freebox:latest

# 1b. (or) option B: build it yourself (from the repo root)
podman build -f ci/docker/Dockerfile -t freebox .

# 2. run — every start re-crawls the latest nodes
podman run -d --restart=always \
  --name freebox \
  -p 7890:7890 -p 9090:9090 \
  -e MIHOMO_SECRET="$(openssl rand -hex 16)" \
  freebox   # use suiwenfeng/mihomo-freebox:latest if you pulled instead of built

# 3. open http://127.0.0.1:9090 → MetaCubeXD dashboard, already connected.
# 4. set your browser/system proxy to 127.0.0.1:7890 (HTTP & SOCKS5).
```

The dashboard auto-connects (no secret prompt) because it reverse-proxies
mihomo's Clash API on `/clash-api` with the secret injected.

---

## Architecture

```
container (single process tree)
┌─────────────────────────────────────────────────┐
│ tini (PID 1)                                    │
│  └─ entrypoint.sh                               │
│     ├─ freebox (Rust, static) ──► providers/proxies.yaml   (re-built every start)
│     ├─ render_config.py  ──► config.yaml               (env-substituted)
│     ├─ mihomo -t        ──► validate                    (fail-fast)
│     ├─ mihomo (supervised, restarts on crash)            127.0.0.1:19090 API
│     │                                              0.0.0.0:7890  mixed proxy
│     └─ dashboard.py (foreground)                 0.0.0.0:9090  MetaCubeXD UI
│          ├─ static UI   ← /etc/mihomo/ui
│          └─ reverse proxy /clash-api/* → mihomo:19090 (Bearer injected, ws bridged)
└─────────────────────────────────────────────────┘
```

### Data flow
1. **crawl** — the `crawler/` crate (Rust, statically linked) fetches from
   multiple subscription sources defined in `crawler/config/sources.yaml`
   (zhuhaiuk/free-nodes, shuaidaoya Gist, Pawdroid, ermaozi, learnhard,
   nodefree.me indirect, and any `EXTRA_SUBS`). When direct access to a source
   fails, the crawler auto-detects the local mihomo SOCKS5 proxy
   (`127.0.0.1:7890`) and retries through it — so GFW-blocked sources are still
   reachable when the proxy is available. It parses `vless`/`vmess`/`ss`/
   `trojan`/`http(s)` into Clash.Meta `proxies`. A TCP-reachability probe is run
   in the daily CI run; by default the container crawls *fast*
   (`--no-test`) and lets mihomo's runtime `health-check` filter dead nodes. A
   non-empty result always wins; an empty crawl keeps the last-known provider
   (or the baked-in snapshot).
2. **template** — on every start the entrypoint may pull a `config.yaml.template`
   from a URL (`CONFIG_URL`, empty by default); when unset it uses the template
   baked into the image, so the container boots fully offline. Runtime env
   (secret/ports) is substituted during rendering.
3. **render** — `config/render_config.py` substitutes `${VAR}` tokens from the
   environment into the template (`mode: global`, `mixed-port: 7890`).
4. **validate** — `mihomo -t`; abort if invalid.
5. **serve** — mihomo runs supervised on `7890` + internal `127.0.0.1:19090`;
   `dashboard.py` serves MetaCubeXD on `9090` and proxies the API on
   `/clash-api`.

### Why a custom dashboard gateway (not `external-ui`)?
mihomo v1.19 does **not** serve a full static UI from `external-ui` (a `GET /`
returns `{"hello":"mihomo"}`), and `external-controller-cors` cannot be set as a
plain list/string. The tiny stdlib `dashboard.py` serves the **official**
MetaCubeXD assets on `:9090` and reverse-proxies the Clash API on the same origin
under `/clash-api` — so the dashboard auto-connects with no CORS and no secret
prompt, with no `caddy`/`nginx` added to the image.

---

## Crawler subscription sources & proxy

The `crawler/` crate (Rust, statically linked) harvests free proxy nodes from
multiple subscription sources. Sources are defined declaratively in
`crawler/config/sources.yaml`; new sources can be added **without touching the
fetch logic** — just add an entry and the generic discovery engine handles it.

### Source kinds

| Kind | How it works |
|---|---|
| `direct` | Fetch the URL, decode the body (base64 blob or plain URL-per-line), extract proxy URLs. |
| `indirect` | A page-listing source (e.g. a blog). Fetch the listing page, follow a chain of discovery steps (each finding the first URL matching a predicate), and decode the final page's content. |

### Proxy support (翻墙)

Many subscription sources are GFW-blocked (DNS fails or connections time out
from direct access). The crawler auto-detects a local mihomo SOCKS5 proxy
(`127.0.0.1:7890` — the container's own `mixed-port`) and routes blocked sources
through it via curl's `--socks5-hostname` flag (DNS resolved by the proxy to
avoid DNS poisoning).

- **`via_proxy: false`** (default): the crawler tries direct access first, then
  falls back to the proxy if a direct fetch fails and a proxy is available.
- **`via_proxy: true`**: always use the proxy (skip the direct attempt).

Proxy auto-detection order:
1. `PROXY_URL` env var (e.g. `socks5://127.0.0.1:7890`)
2. `SOCKS5_PROXY` env var (`host:port`)
3. Well-known local mihomo port `127.0.0.1:7890`

Set `SKIP_PROXY=1` to disable proxy auto-detection entirely (useful in CI where
no local mihomo runs).

### Declarative source config (`crawler/config/sources.yaml`)

```yaml
sources:
  - name: "my-direct-source"
    url: "https://example.com/sub.txt"
    timeout: 25          # optional, default 25
    via_proxy: false      # optional, default false

  - name: "my-blog"
    url: "https://example-blog.com/"
    kind: indirect
    via_proxy: true
    discovery:
      - label: "article"
        predicate:
          all:
            - contains: "example-blog.com/p/"
            - ends_with: ".html"
      - label: "subscription"
        predicate:
          contains: "sub.example-blog.com"
```

Predicate types: `contains`, `ends_with`, `starts_with`, `all` (AND), `any` (OR).


---

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `MIHOMO_SECRET` | `freebox` | Clash API secret (also the proxy-auth secret in some setups) |
| `MIXED_PORT` | `7890` | Public mixed proxy port (HTTP + SOCKS5) |
| `API_PORT` | `9090` | Public MetaCubeXD dashboard port |
| `CONTROLLER_PORT` | `19090` | **Internal** mihomo Clash API port (proxied by the dashboard) |
| `ALLOW_LAN` | `true` | `allow-lan` |
| `LOG_LEVEL` | `info` | mihomo log level (`info`/`debug`/`warning`/`error`) |
| `RULESET_BASE_URL` | `https://cdn.metacubex.com/RuleSet` | MetaCubeX RuleSet base for `rule-providers` |
| `MAX_NODES` | `150` | Cap on nodes per crawl |
| `RUN_CRAWL_TEST` | `0` | `1` → run the TCP probe before writing nodes (slower) |
| `PROBE_TIMEOUT` | `2` | Per-node TCP probe timeout (seconds) |
| `CRAWL_TIMEOUT` | `150` | Overall crawl budget (seconds). Raised from 90s to fit the slower reachable mirror sources (see below). Set higher in slow/blocked networks or `0` to disable the bound. |
| `EXTRA_SUBS` | `""` | Extra subscription URLs (newline/comma separated) |
| `PROXY_URL` | auto-detect | Preferred proxy override (`socks5://host:port` or `host:port`) |
| `SOCKS5_PROXY` | auto-detect | Fallback proxy env var (same format) |
| `SKIP_PROXY` | `0` | `1` → disable proxy auto-detection entirely |
| `CONFIG_URL` | *(empty)* | Config template URL pulled on start; empty = baked-in template |
| `MIHOMO_VERSION` | `v1.19.30` | mihomo release (build arg; also a CI project env var override) |

---

## Rule & Geo data sources (actively maintained)

- **Geo data** — mihomo's built-in default, `https://github.com/MetaCubeX/meta-rules-dat/releases/latest/` (`geoip.dat`, `geosite.dat`, `geoip.metadb`, `country.mmdb`). Auto-downloaded on first run. Powers the built-in `GEOSITE,CN → DIRECT` and `GEOIP,CN → DIRECT` rules.
- **Rule sets** — MetaCubeX RuleSet on `cdn.metacubex.com/RuleSet` (categories: `block`, `hijacking`, `gfw`, `proxy`, `telegram`, `ir`, `allowlist`, `domestic`). Lazy-loaded at runtime, so a transient CDN miss only leaves one category empty.

---

## CI: Codeberg Woodpecker

CI lives in [`.woodpecker/build.yaml`](.woodpecker/build.yaml) on **Codeberg
Woodpecker CI** (the old GitHub Actions workflows are gone). It builds the
multi-arch (`linux/amd64` + `linux/arm64`) image — baking in the Rust crawler
and the mihomo kernel — and pushes it to **Docker Hub**.

| Trigger | What happens |
|---|---|
| **push** to `main` touching `ci/docker/**`, `ci/scripts/**`, `config/**`, `crawler/**`, `.woodpecker/**` or `providers/proxies.yaml` | rebuild + republish |
| **cron** (schedule configured in the Woodpecker UI → Project → Cron, e.g. daily) | always rebuild with the **latest mihomo release** (unless `MIHOMO_VERSION` is pinned) + republish |
| **manual** (Run workflow button) | same as cron |

**mihomo version.** Each run resolves the latest
[MetaCubeX/mihomo](https://github.com/MetaCubeX/mihomo) release tag from the
GitHub API and builds with it. To pin a specific release, set the project-level
environment variable `MIHOMO_VERSION` (e.g. `v1.19.30`) in the Woodpecker
project settings — when unset, "latest" is resolved automatically.

**Image tags** pushed to Docker Hub (`suiwenfeng/mihomo-freebox`):

| Tag | Meaning |
|---|---|
| `latest` | newest build |
| `v1.19.30` | the baked mihomo release |
| `1.19.30` | release without the `v` prefix |
| `1.19` | major.minor rolling tag |
| `sha-<commit>` | short commit SHA |

### Required Woodpecker setup

1. **Secrets** (Project → Secrets): `dockerhub_username` and `dockerhub_token`
   (a Docker Hub access token with `write:packages`).
2. **Trusted** (Project → Settings → *Trusted*): enable it — the build step
   mounts the Docker socket and uses Buildx with BuildKit's `docker-container`
   driver (+ qemu for `arm64`).
3. **Cron** (Project → Cron, optional): add a schedule such as `0 1 * * *` to
   keep the image refreshed daily with the newest mihomo release.

### Pulling from Docker Hub

```bash
# pre-built multi-arch image
docker pull suiwenfeng/mihomo-freebox:latest
# or pin to a specific mihomo release
docker pull suiwenfeng/mihomo-freebox:v1.19.30
```

### What happened to GitHub Pages / node publishing?

`publish-config.yml` and `update-nodes.yml` were removed — their jobs (crawling
fresh free nodes, rendering/validating the config) now run **inside the image's
entrypoint on every `(re)start`**, so there is no longer a separate node-publish
job or a GitHub Pages site hosting a config template. `CONFIG_URL` now defaults
to empty and the baked-in `config.yaml.template` is used unless you override
`CONFIG_URL` at runtime. The container always re-crawls fresh nodes itself (see
Data flow #1), keeping mihomo in lock-step with the repo even between image
rebuilds.

---

## Notes / caveats

- **node sources** — the crawler harvests from multiple free-node subscription
  sources (listed in `crawler/config/sources.yaml`):
  • `zhuhaiuk/free-nodes` `nodes.txt` (jsdelivr CDN **+ fastly.jsdelivr.net mirror**) + raw GitHub mirror, base64
  • `zhuhaiuk/free-nodes` `extra_nodes.txt` (jsdelivr CDN + fastly mirror) + raw GitHub mirror, base64, distinct nodes
  • `shuaidaoya` FreeNodes Gist (base64, percent-encoded)
  • `Pawdroid/Free-servers` `sub` (base64 blob; + `ghproxy.net` mirror)
  • `ermaozi/get_subscribe` `v2ray.txt` (plain URL-per-line; + `ghproxy.net` mirror, the largest pool)
  • `learnhard-cn/free_proxy_ss` `free` (base64; + `ghproxy.net` mirror)
  • `nodefree.me` — **indirect page-listing** source: the crawler fetches the
    blog index, follows the first article link (`…/p/N.html`), then takes the
    first subscription address link on that page (the v2ray `.txt`
    subscription) and decodes it.  Fetched last so the reliable `zhuhaiuk`
    source wins on de-duplication.

  > **Why mirrors were added.** Several sources sit on `raw.githubusercontent.com`
  > or `cdn.jsdelivr.net`, which the GFW blocks/interferes with from many
  > networks (connection reset / `000`). The crawler's SOCKS5 proxy fallback
  > (`127.0.0.1:7890`) only helps once mihomo is already up — on a **fresh
  > container start** it is not, so those sources return 0 nodes and the crawl
  > shrinks to a handful. The added `ghproxy.net` / `fastly.jsdelivr.net` mirror
  > entries are reachable *directly*, breaking the bootstrap dependency. They are
  > slower than a native fetch, so `CRAWL_TIMEOUT` was raised (default 150s).
  New sources can be added **without code changes** by editing
  `crawler/config/sources.yaml` (or passing `--sources-config PATH`). The
  declarative config supports `direct` and `indirect` source kinds, per-source
  `via_proxy` flags, and composable URL-matching predicates (`contains`,
  `ends_with`, `starts_with`, `all`, `any`).
  Any additional subscription can also be plugged in at runtime via the
  `EXTRA_SUBS` env var.
- The container **re-crawls on every start**, so a restart always picks up new
  nodes. If you need a *static* node set, set `EXTRA_SUBS` and pin the image tag.
- No TUN / `NET_ADMIN` required — pure forwarding on `mixed-port`.
- The dashboard's WebSocket (`/logs`, `/connections`) is bridged transparently
  to mihomo, so the live log & connections views work too.
