#!/bin/sh
# =============================================================================
# ci/scripts/build-image.sh
#
# Build the multi-arch (linux/amd64 + linux/arm64) mihomo-freebox image and
# publish it to Docker Hub.  This is invoked by the "Build & publish" step of
# .woodpecker/build.yaml.
#
# Requirements (provided by the Woodpecker agent / step):
#   * A Docker daemon reachable through the Docker socket mounted at
#     /var/run/docker.sock  (the `docker:cli` step image + that volume).
#   * The host daemon must permit running privileged containers so that the
#     qemu-user-static image can register binfmt_misc handlers for arm64.
#
# Inputs (step environment):
#   IMAGE_NAME          Docker Hub repository (default: suiwenfeng/mihomo-freebox)
#   DOCKERHUB_USERNAME  Docker Hub account name      (from secret)
#   DOCKERHUB_TOKEN     Docker Hub access token      (from secret)
#   CI_COMMIT_SHA       commit SHA of the build      (set by Woodpecker)
#
# Inputs (workspace artifact written by the "Resolve mihomo version" step):
#   .mihomo-version     the mihomo release tag, e.g. "v1.19.30"
# =============================================================================
set -eu

# Resolve the target image (allow an env / project-level override).
: "${IMAGE_NAME:=suiwenfeng/mihomo-freebox}"

# ── mihomo version (resolved by the prior step) ─────────────────────────────
if [ ! -f .mihomo-version ]; then
  echo "FATAL: .mihomo-version not found — the resolve step did not run." >&2
  exit 1
fi
VER=$(cat .mihomo-version)
if [ -z "$VER" ]; then
  echo "FATAL: .mihomo-version is empty." >&2
  exit 1
fi

# Derive the set of image tags Woodpecker CI used to produce on GitHub:
#   latest, <tag>, <tag-without-v>, <major.minor>, sha-<short-sha>
VER_NO_V=${VER#v}                                   # v1.19.30 -> 1.19.30
MAJOR_MINOR=$(printf '%s\n' "$VER_NO_V" \
  | sed -E 's/^([0-9]+\.[0-9]+).*/\1/')             # 1.19.30    -> 1.19
SHA_TAG=sha-$(printf '%s\n' "${CI_COMMIT_SHA:-}" | cut -c1-12)

echo "==> image:  $IMAGE_NAME"
echo "==> mihomo: $VER"
echo "==> tags:   latest $VER $VER_NO_V $MAJOR_MINOR $SHA_TAG"

# ── Docker Buildx (multi-arch + qemu) ───────────────────────────────────────
# Register qemu binfmt handlers on the host so the arm64 buildkit RUN steps
# can execute.  Made best-effort: some runners already register them, and a
# runner that forbids privileged containers will simply skip it.
echo "==> registering qemu binfmt handlers (best-effort) ..."
docker run --rm --privileged multiarch/qemu-user-static --reset -p yes \
  || echo "WARN: qemu registration skipped (host may already provide binfmt_misc)"

echo "==> setting up Docker Buildx ..."
docker buildx version
# Create a docker-container builder (runs BuildKit in a sidecar container);
# reuse it if it already exists from a previous run.
docker buildx create --use --name multiarch --driver docker-container 2>/dev/null \
  || docker buildx use multiarch
# Bootstrap the builder (starts BuildKit; this is also where cross-arch
# emulation becomes available once qemu is registered below).
docker buildx inspect --bootstrap

# ── Docker Hub login ────────────────────────────────────────────────────────
# The token never appears on the process command line.
echo "==> logging in to Docker Hub as ${DOCKERHUB_USERNAME} ..."
printf '%s' "$DOCKERHUB_TOKEN" | docker login -u "$DOCKERHUB_USERNAME" --password-stdin

# ── build & push ────────────────────────────────────────────────────────────
echo "==> building & pushing $IMAGE_NAME [linux/amd64,linux/arm64] ..."
docker buildx build \
  --platform linux/amd64,linux/arm64 \
  --build-arg MIHOMO_VERSION="$VER" \
  --file ci/docker/Dockerfile \
  --tag "$IMAGE_NAME:latest" \
  --tag "$IMAGE_NAME:$VER" \
  --tag "$IMAGE_NAME:$VER_NO_V" \
  --tag "$IMAGE_NAME:$MAJOR_MINOR" \
  --tag "$IMAGE_NAME:$SHA_TAG" \
  --push \
  .

echo "==> done: $IMAGE_NAME:$VER pushed."
