# =============================================================================
# mihomo-freebox — single-container image (built with podman/docker).
#
# Build (from repo root)::
#
#   podman build -f ci/docker/Dockerfile -t freebox .
#   # or:  docker build -f ci/docker/Dockerfile -t freebox .
#
# Run::
#
#   podman run -d --restart=always \
#     -p 7890:7890 -p 9090:9090 \
#     -e MIHOMO_SECRET=... -e PROBE_TIMEOUT=2 \
#     --name freebox freebox
#
# The entrypoint re-crawls fresh free nodes on every (re)start, renders the
# mihomo config, validates it, then runs mihomo (supervised) behind the bundled
# MetaCubeXD dashboard gateway.
# =============================================================================
# NOTE: this image intentionally avoids BuildKit-only features so it builds with
# plain podman/docker (no `# syntax=docker/dockerfile` buildkit directive).

ARG MIHOMO_VERSION=v1.19.30

# ─── Stage 1: MetaCubeXD official dashboard UI (gh-pages branch) ────────────
FROM alpine:3.20 AS ui
ARG MIHOMO_VERSION
RUN apk add --no-cache ca-certificates curl unzip
WORKDIR /src
# gh-pages branch of the official MetaCubeXD UI (jsDelivr CDN mirror as a
# fallback for environments where raw github.com is slow/blocked).
RUN curl -fsSL --max-time 180 -o metacubexd.zip \
      "https://ghpxy.hwinzniej.top/https://github.com/MetaCubeX/metacubexd/archive/refs/heads/gh-pages.zip"
RUN unzip -q metacubexd.zip \
    && mv metacubexd-gh-pages /ui \
    && rm -f metacubexd.zip
# sanity
RUN test -s /ui/index.html && test -s /ui/config.js

# ─── Stage 2: mihomo kernel (per architecture) ─────────────────────────────
# Assets (v1.19.x):
#   mihomo-linux-amd64-compatible-v<ver>.gz
#   mihomo-linux-arm64-v<ver>.gz
FROM alpine:3.20 AS mihomo
ARG MIHOMO_VERSION
ARG TARGETARCH
RUN apk add --no-cache ca-certificates curl gzip
RUN set -ex; \
  case "${TARGETARCH:-amd64}" in \
    amd64) MCH="amd64-compatible" ;; \
    arm64|aarch64) MCH="arm64" ;; \
    arm|armv7) MCH="armv7" ;; \
    *) echo "ERROR: unsupported TARGETARCH='${TARGETARCH:-}'" >&2; exit 1 ;; \
  esac; \
  curl -fsSL --max-time 300 --retry 3 --retry-delay 2 -o /tmp/m.gz \
    "https://ghpxy.hwinzniej.top/https://github.com/MetaCubeX/mihomo/releases/download/${MIHOMO_VERSION}/mihomo-linux-${MCH}-${MIHOMO_VERSION}.gz" \
    && gzip -dc /tmp/m.gz > /mihomo \
    && chmod +x /mihomo \
    && rm -f /tmp/m.gz \
    && /mihomo -v

# ─── Stage 3: build the Rust crawler (static, musl) ────────────────────────
# Pure-Rust deps (base64 / yaml-rust / linked-hash-map) + rust-lld linking =>
# a fully static binary that needs no C toolchain and runs on the alpine
# runtime below. Built per-TARGETARCH so the multi-arch buildx matrix yields
# the right binary for amd64 and arm64 alike.
FROM --platform=$BUILDPLATFORM rust:1.98-alpine AS crawler
ARG TARGETARCH
WORKDIR /src
COPY crawler/ ./
RUN case "${TARGETARCH:-amd64}" in \
      amd64) T=x86_64-unknown-linux-musl ;; \
      arm64|aarch64) T=aarch64-unknown-linux-musl ;; \
      *) echo "ERROR: unsupported TARGETARCH='${TARGETARCH:-}'" >&2; exit 1 ;; \
    esac; \
    rustup target add "$T" && \
    RUSTFLAGS="-C linker=rust-lld" \
    cargo build --release --target "$T" && \
    cp "target/${T}/release/freebox" /freebox && \
    /freebox --help >/dev/null

# ─── Stage 3: runtime image ───────────────────────────────────────────────
FROM alpine:3.20 AS runtime
ARG MIHOMO_VERSION
ENV MIHOMO_VERSION="${MIHOMO_VERSION}" \
    MIHOMO_HOME=/etc/mihomo \
    EXTERNAL_UI=/etc/mihomo/ui \
    APP_DIR=/app \
    TZ=UTC
# tini = PID 1 (reaps zombies, forwards signals); bash for the entrypoint.
RUN apk add --no-cache --no-progress bash ca-certificates curl coreutils python3 py3-yaml tini \
    && mkdir -p /etc/mihomo/providers /etc/mihomo/rules /etc/mihomo/ui

COPY --from=mihomo /mihomo /usr/local/bin/mihomo
COPY --from=ui /ui /etc/mihomo/ui
# Fallback node snapshot (shipped so the image boots even if the live crawl at
# startup returns nothing).  The entrypoint always tries to refresh it.
COPY providers/proxies.yaml /etc/mihomo/providers/proxies.yaml
# Static Rust crawler binary + config template/renderer + dashboard gateway + entrypoint.
COPY --from=crawler /freebox /usr/local/bin/freebox
COPY config ${APP_DIR}/config
COPY ci/docker/dashboard.py ${APP_DIR}/dashboard.py
COPY ci/docker/entrypoint.sh /entrypoint.sh

# Pre-download GeoData (geoip.dat, geosite.dat, country.mmdb) from the
# jsDelivr CDN mirror so the first `mihomo -t` in the entrypoint does not have
# to download them (which could time out on a slow / blocked network).  If the
# download fails the build still succeeds — mihomo will retry at runtime via
# the geox-url configured in the template.
RUN set -ex; \
    chmod +x /usr/local/bin/mihomo /usr/local/bin/freebox ${APP_DIR}/dashboard.py /entrypoint.sh; \
    python3 -c "import yaml" ; \
    mihomo -v; \
    cd /etc/mihomo; \
    for f in geosite.dat country.mmdb geoip.dat; do \
      curl -fsSL --max-time 90 -o "$f" \
        "https://testingcf.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/$f" \
      || curl -fsSL --max-time 90 -o "$f" \
        "https://cdn.jsdelivr.net/gh/MetaCubeX/meta-rules-dat@release/$f" \
      || echo "WARN: $f download failed; mihomo will retry at runtime"; \
    done; \
    ls -lh /etc/mihomo/*.dat /etc/mihomo/*.mmdb 2>/dev/null || true

WORKDIR ${APP_DIR}

EXPOSE 7890 9090
# 7890 = mixed proxy (http+socks5); 9090 = MetaCubeXD dashboard (UI + API proxy)
ENTRYPOINT ["tini", "--", "/entrypoint.sh"]
