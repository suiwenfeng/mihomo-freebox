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

## Quick start (podman)

```bash
# 1. build (from the repo root)
podman build -f ci/docker/Dockerfile -t freebox .

# 2. run — every start re-crawls the latest nodes
podman run -d --restart=always \
  --name freebox \
  -p 7890:7890 -p 9090:9090 \
  -e MIHOMO_SECRET="$(openssl rand -hex 16)" \
  freebox

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
1. **crawl** — the `crawler/` crate (Rust, statically linked) fetches the
   `zhuhaiuk/free-nodes` `nodes.txt` (the live, reliable source), follows the
   latest article on `https://nodefree.me/` to its v2ray subscription link, and
   any `EXTRA_SUBS` subscriptions; it parses `vless`/`vmess`/`ss`/`trojan`/`http(s)`
   into Clash.Meta `proxies`. A TCP-reachability probe is run in the daily GH
   Action commit; by default the container crawls *fast* (`--no-test`) and lets
   mihomo's runtime `health-check` filter dead nodes. A non-empty result always
   wins; an empty crawl keeps the last-known provider (or the baked-in snapshot).
2. **template** — on every start the entrypoint tries to pull the *latest*
   `config.yaml.template` from GitHub Pages (`CONFIG_URL`), falling back to the
   template baked into the image if Pages is unreachable.
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
| `EXTRA_SUBS` | `""` | Extra subscription URLs (newline/comma separated) |
| `CONFIG_URL` | `https://<user>.github.io/<repo>/config.yaml.template` | Latest config template pulled on start (fall-back: baked-in) |
| `MIHOMO_VERSION` | `v1.19.30` | mihomo release (build arg) |

---

## Rule & Geo data sources (actively maintained)

- **Geo data** — mihomo's built-in default, `https://github.com/MetaCubeX/meta-rules-dat/releases/latest/` (`geoip.dat`, `geosite.dat`, `geoip.metadb`, `country.mmdb`). Auto-downloaded on first run. Powers the built-in `GEOSITE,CN → DIRECT` and `GEOIP,CN → DIRECT` rules.
- **Rule sets** — MetaCubeX RuleSet on `cdn.metacubex.com/RuleSet` (categories: `block`, `hijacking`, `gfw`, `proxy`, `telegram`, `ir`, `allowlist`, `domestic`). Lazy-loaded at runtime, so a transient CDN miss only leaves one category empty.

---

## GitHub Actions

| Workflow | Trigger | What it does |
|---|---|---|
| `update-nodes.yml` | cron nightly (`01:15 UTC`) + manual + `workflow_dispatch` | builds the Rust crawler, crawls **with** the TCP probe, commits `providers/proxies.yaml` if changed |
| `publish-config.yml` | cron nightly (`01:45 UTC`) + manual + `workflow_run` (after the above) | crawls + probes, renders `config.yaml` (`mode: global`), deploys the template + config + nodes to **GitHub Pages** |
| `build-image.yml` | `providers/proxies.yaml` / `ci/docker` / `config` / `crawler` change + manual | builds multi-arch (`linux/amd64` + `linux/arm64`) image (bakes the Rust crawler + mihomo) and pushes to `ghcr.io/<repo>:latest` |

> The workflows are **disabled by default on forks**. Enable them (Settings →
> Actions → General → *Workflow permissions*: **Read & write**) and they will
> keep both nodes and the image fresh automatically.

---

## GitHub Pages (latest config)

`publish-config.yml` (cron `01:45 UTC`, also re-runs after every node refresh,
plus manual `workflow_dispatch`) publishes to the `gh-pages` branch
([GitHub Pages](https://docs.github.com/en/actions/deployment/about-deploying-via-github-pages)):

- `config.yaml.template` — the mihomo config template (always rendered with
  `mode: global`)
- `config.yaml` — a rendered preview (default env)
- `providers/proxies.yaml` — the latest probed free nodes

On every container `(re)start` the entrypoint pulls the **latest template** from
`https://<org>.github.io/<repo>/config.yaml.template` (override with `CONFIG_URL`),
falling back to the baked-in template if Pages is unreachable. The container then
always re-crawls fresh nodes itself (see Data flow #1), so the running mihomo is
kept in lock-step with the repo even between image rebuilds.

---

## Notes / caveats

- **node sources** — the default crawler sources are the `zhuhaiuk/free-nodes`
  `nodes.txt` CDN mirror + raw fallback (reliable from this environment) and
  `https://nodefree.me/`.  `nodefree.me` is an **indirect page-listing source**:
  the crawler fetches the index page, follows the first article link, then takes
  the first subscription address link on that article page (the v2ray `.txt`
  subscription) and decodes it.  `nodefree.me` is fetched last so the reliable
  `zhuhaiuk` source wins on de-duplication.  Any additional subscription can
  still be plugged in via `EXTRA_SUBS`.
- The container **re-crawls on every start**, so a restart always picks up new
  nodes. If you need a *static* node set, set `EXTRA_SUBS` and pin the image tag.
- No TUN / `NET_ADMIN` required — pure forwarding on `mixed-port`.
- The dashboard's WebSocket (`/logs`, `/connections`) is bridged transparently
  to mihomo, so the live log & connections views work too.
