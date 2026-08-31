#!/usr/bin/env bash
# =============================================================================
# mihomo-freebox container entrypoint.
#
#   1. Crawl fresh free nodes -> /etc/mihomo/providers/proxies.yaml
#   2. Render /etc/mihomo/config.yaml from the template (env-substituted)
#   3. Validate with `mihomo -t`
#   4. Supervise mihomo in the background (auto-restart on crash)
#   5. Exec the MetaCubeXD dashboard gateway in the foreground (PID 2 under tini)
#
# With `--restart=always` every container (re)start re-crawls the latest nodes,
# so the proxy set is always fresh.  Set RUN_CRAWL_TEST=1 to also run the TCP
# reachability probe (slower; recommended for the daily GitHub Action commit,
# not for every container start).
# =============================================================================
set -euo pipefail

# ── defaults (env-overridable) ─────────────────────────────────────────────
: "${MIHOMO_HOME:=/etc/mihomo}"
: "${EXTERNAL_UI:=${MIHOMO_HOME}/ui}"
: "${PROVIDERS_DIR:=${MIHOMO_HOME}/providers}"
: "${RULES_DIR:=${MIHOMO_HOME}/rules}"
: "${PROVIDERS_FILE:=${PROVIDERS_DIR}/proxies.yaml}"
APP_DIR="${APP_DIR:-/app}"
CONFIG_TEMPLATE="${CONFIG_TEMPLATE:-${APP_DIR}/config/config.yaml.template}"
CONFIG_FILE="${MIHOMO_HOME}/config.yaml"
: "${MIHOMO_BINARY:=mihomo}"
: "${FREEBOX_BIN:=freebox}"   # the baked Rust crawler

: "${MIXED_PORT:=7890}"
: "${API_PORT:=9090}"
: "${CONTROLLER_HOST:=127.0.0.1}"
: "${CONTROLLER_PORT:=19090}"
: "${ALLOW_LAN:=true}"
: "${LOG_LEVEL:=info}"
: "${SECRET:=freebox}"
: "${RULESET_BASE_URL:=https://cdn.metacubex.com/RuleSet}"

: "${MAX_NODES:=150}"
: "${PROBE_TIMEOUT:=2}"
: "${RUN_CRAWL_TEST:=${SKIP_TEST:-0}}"
# RUN_CRAWL_TEST=0 -> crawl with --no-test (fast); =1 -> run the TCP probe.

log() { echo "[entrypoint] $*"; }

# ── layout ─────────────────────────────────────────────────────────────────
mkdir -p "${EXTERNAL_UI}" "${PROVIDERS_DIR}" "${RULES_DIR}"
chown -R 1000:1000 "${MIHOMO_HOME}" 2>/dev/null || true

# ── 1. crawl fresh nodes ───────────────────────────────────────────────────
# Run the (statically-linked, baked) Rust crawler. Crawl to a temp file first so
# an empty result (all sources down) does NOT clobber the last-known-good
# provider; mihomo still boots with a stale-but-live node set.
CRAWL_OUT="${PROVIDERS_FILE}.new"
log "crawling free nodes (RUN_CRAWL_TEST=${RUN_CRAWL_TEST}) ..."
CRAWL_ARGS=("${FREEBOX_BIN}" --out "${CRAWL_OUT}")
if [ "${RUN_CRAWL_TEST}" = "1" ]; then
  CRAWL_ARGS+=(--timeout "${PROBE_TIMEOUT}" --max "${MAX_NODES}")
else
  CRAWL_ARGS+=(--no-test --max "${MAX_NODES}")
fi
# Bound the crawl so a down/flaky network cannot delay a fresh start forever;
# on timeout we fall through to the last-known provider (or the baked snapshot).
CRAWL_TIMEOUT="${CRAWL_TIMEOUT:-90}"
if ! ( timeout "${CRAWL_TIMEOUT}" "${CRAWL_ARGS[@]}" ); then
  rc=$?
  if [ "${rc}" = "124" ]; then
    log "WARNING: crawl timed out after ${CRAWL_TIMEOUT}s; keeping last-known provider"
  else
    log "WARNING: crawl exited non-zero (rc=${rc}); keeping last-known provider"
  fi
fi

NEW_NODES=0
if [ -f "${CRAWL_OUT}" ]; then
  # Validate + count via PyYAML so a truncated/partial write (e.g. killed by
  # CRAWL_TIMEOUT mid-write) is rejected and we keep the last-known provider.
  NEW_NODES=$(python3 -c "import yaml,sys
try:
    d=yaml.safe_load(open('${CRAWL_OUT}')) or {}
    print(len(d.get('proxies') or []))
except Exception:
    print(0)" 2>/dev/null || echo 0)
fi
if [ "${NEW_NODES}" -gt 0 ]; then
  mv -f "${CRAWL_OUT}" "${PROVIDERS_FILE}"
  NODES="${NEW_NODES}"
elif [ -f "${PROVIDERS_FILE}" ]; then
  NODES=$(grep -cE '^[[:space:]]*- name:' "${PROVIDERS_FILE}" 2>/dev/null || echo 0)
  log "WARNING: crawl returned 0 nodes; keeping last-known provider (${NODES} proxies)"
else
  NODES=0
  log "WARNING: crawl returned 0 nodes and no cached provider exists"
fi
rm -f "${CRAWL_OUT}"

log "node provider: ${PROVIDERS_FILE} (${NODES} proxies)"

# ── 2. render config ──────────────────────────────────────────────────────
# Pull the latest config template published to GitHub Pages (req: image always
# pulls the latest config). Fall back to the template baked into the image so
# the container still boots offline. Runtime env (secret/ports) is substituted
# from the container environment during rendering.
: "${CONFIG_URL:=https://wenfengsui.github.io/mihomo-freebox/config.yaml.template}"
RUNTIME_TEMPLATE="${CONFIG_TEMPLATE}"
if curl -fsSL --max-time 10 "${CONFIG_URL}" -o /tmp/freebox_config_template.yaml 2>/dev/null \
   && [ -s /tmp/freebox_config_template.yaml ]; then
  RUNTIME_TEMPLATE=/tmp/freebox_config_template.yaml
  log "pulled latest config template from ${CONFIG_URL}"
else
  rm -f /tmp/freebox_config_template.yaml
  log "could not reach ${CONFIG_URL}; using baked-in template (${CONFIG_TEMPLATE})"
fi

log "rendering config.yaml ..."
EXTERNAL_UI="${EXTERNAL_UI}" \
PROVIDERS_DIR="${PROVIDERS_DIR}" \
MIXED_PORT="${MIXED_PORT}" \
API_PORT="${API_PORT}" \
CONTROLLER_HOST="${CONTROLLER_HOST}" \
CONTROLLER_PORT="${CONTROLLER_PORT}" \
ALLOW_LAN="${ALLOW_LAN}" \
LOG_LEVEL="${LOG_LEVEL}" \
SECRET="${SECRET}" \
RULESET_BASE_URL="${RULESET_BASE_URL}" \
python3 "${APP_DIR}/config/render_config.py" \
  --template "${RUNTIME_TEMPLATE}" --out "${CONFIG_FILE}"

# ── 3. validate ───────────────────────────────────────────────────────────
log "validating config with mihomo -t ..."
if ! "${MIHOMO_BINARY}" -t -d "${MIHOMO_HOME}" -f "${CONFIG_FILE}"; then
  log "FATAL: config validation failed; not starting mihomo."
  exit 1
fi
log "config OK"

# ── 4. run mihomo (foreground if alone, else supervised in background) ───
if [ "${RUN_DASHBOARD:-1}" != "1" ]; then
  # No dashboard gateway requested -> hand off to mihomo as PID 1.
  exec "${MIHOMO_BINARY}" -d "${MIHOMO_HOME}" -f "${CONFIG_FILE}"
fi

# Supervised mihomo.  It logs to stderr (captured by the shell); if it ever
# exits, restart it after a short back-off.  The dashboard gateway (step 5)
# is the container's foreground process; tini (PID 1) reaps both on shutdown.
(
  trap 'kill "${MIHOMO_PID:-}" 2>/dev/null || true; exit 0' TERM INT
  while true; do
    log "starting mihomo (mixed:${MIXED_PORT}, controller:${CONTROLLER_HOST}:${CONTROLLER_PORT}) ..."
    "${MIHOMO_BINARY}" -d "${MIHOMO_HOME}" -f "${CONFIG_FILE}" || true
    log "mihomo exited; restarting in 3s ..."
    sleep 3
  done
) &
MIHOMO_PID=$!

# Give mihomo a moment to open the Clash API port before the dashboard starts.
sleep 4

# ── 5. MetaCubeXD dashboard gateway (foreground; container PID 2) ─────────
log "starting MetaCubeXD dashboard gateway on :${API_PORT} ..."
# dashboard.py rewrites <ui>/config.js to point at /clash-api and serves the UI
# + reverse-proxies the Clash API (secret auto-injected) on the same origin.
exec python3 "${APP_DIR}/dashboard.py" \
  --ui "${EXTERNAL_UI}" \
  --api "http://${CONTROLLER_HOST}:${CONTROLLER_PORT}" \
  --secret "${SECRET}" \
  --backend-url "/clash-api" \
  --listen "0.0.0.0:${API_PORT}"
