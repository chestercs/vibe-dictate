#!/usr/bin/env bash
# =============================================================================
#  Bootstraps the VibeVoice backend next to vibe-dictate.
#
#  Steps (all idempotent):
#    1. Clone (or update) microsoft/VibeVoice into ./VibeVoice/
#    2. Copy the matching .env.*.example → .env if missing
#    3. Pre-pull the compose-referenced base image
#    4. Optional: build the B-opció Dockerfile (GB10, --build)
#    5. Optional: docker compose up -d (--up)
#
#  Usage:
#    scripts/setup_vibevoice.sh                   # GB10 stack, clone + pull
#    scripts/setup_vibevoice.sh --arch x86        # consumer NVIDIA stack
#    scripts/setup_vibevoice.sh --build           # prebuilt image (GB10 only)
#    scripts/setup_vibevoice.sh --up              # also bring the stack up
#    scripts/setup_vibevoice.sh --arch x86 --up
# =============================================================================
set -euo pipefail

ARCH="gb10"
DO_BUILD=0
DO_UP=0
UPSTREAM_URL="https://github.com/microsoft/VibeVoice"

usage() {
    sed -n '2,20p' "$0"
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)  ARCH="${2:-}"; shift 2 ;;
        --build) DO_BUILD=1; shift ;;
        --up)    DO_UP=1; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown flag: $1" >&2; usage 2 ;;
    esac
done

case "$ARCH" in
    gb10)
        ENV_EXAMPLE=".env.vibevoice-gb10.example"
        COMPOSE_FILE="docker-compose-vibevoice-asr-gb10.yml"
        ;;
    x86)
        ENV_EXAMPLE=".env.vibevoice.example"
        COMPOSE_FILE="docker-compose-vibevoice.yml"
        ;;
    *) echo "--arch must be gb10 or x86" >&2; exit 2 ;;
esac

# Move to repo root (one up from scripts/).
cd "$(dirname "${BASH_SOURCE[0]}")/.."

command -v docker >/dev/null 2>&1 || {
    echo "docker not found on PATH" >&2; exit 1;
}
command -v git >/dev/null 2>&1 || {
    echo "git not found on PATH" >&2; exit 1;
}

# -----------------------------------------------------------------
# 1. Upstream checkout
# -----------------------------------------------------------------
if [[ -d VibeVoice/.git ]]; then
    echo "[setup] VibeVoice/ exists, fetching upstream"
    git -C VibeVoice fetch --quiet origin
    # Fast-forward if possible; leave untouched if the user has local edits.
    git -C VibeVoice merge --ff-only origin/HEAD \
        || echo "[setup] VibeVoice has local commits — skipping ff merge"
else
    echo "[setup] cloning $UPSTREAM_URL → VibeVoice/"
    git clone --depth 1 "$UPSTREAM_URL" VibeVoice
fi

# -----------------------------------------------------------------
# 2. .env
# -----------------------------------------------------------------
if [[ ! -f .env ]]; then
    echo "[setup] seeding .env from $ENV_EXAMPLE"
    cp "$ENV_EXAMPLE" .env
    if [[ "$ARCH" == "gb10" ]]; then
        echo "[setup] NOTE: edit .env → VIBEVOICE_BASE_DIR (host path for cache volumes)"
    fi
    echo "[setup] NOTE: set HUGGING_FACE_HUB_TOKEN in .env to skip anon rate limits"
else
    echo "[setup] .env already present, leaving it alone"
fi

# -----------------------------------------------------------------
# 3. Pre-pull base images (skips services in inactive profiles)
# -----------------------------------------------------------------
echo "[setup] docker compose pull"
docker compose --env-file .env -f "$COMPOSE_FILE" pull \
    || echo "[setup] pull reported errors — continuing"

# -----------------------------------------------------------------
# 4. Optional prebuilt image (GB10 B-opció)
# -----------------------------------------------------------------
if [[ $DO_BUILD -eq 1 ]]; then
    if [[ "$ARCH" != "gb10" ]]; then
        echo "[setup] --build only applies to --arch gb10; skipping"
    else
        CTX="${VIBEVOICE_SRC:-./VibeVoice}"
        echo "[setup] building vibevoice-gb10:latest from $CTX"
        docker build \
            -f Dockerfile.vibevoice-gb10 \
            -t vibevoice-gb10:latest \
            "$CTX"
        echo "[setup] remember to swap the compose image: line for"
        echo "        build: { context: \${VIBEVOICE_SRC:-./VibeVoice}, dockerfile: ../Dockerfile.vibevoice-gb10 }"
    fi
fi

# -----------------------------------------------------------------
# 5. Optional up
# -----------------------------------------------------------------
if [[ $DO_UP -eq 1 ]]; then
    echo "[setup] docker compose up -d"
    docker compose --env-file .env -f "$COMPOSE_FILE" up -d
fi

cat <<EOF

[setup] done. Next steps:
  - edit .env if you haven't ($ENV_EXAMPLE has the template)
  - start:  docker compose -f $COMPOSE_FILE up -d
  - stop:   docker compose -f $COMPOSE_FILE down
  - logs:   docker compose -f $COMPOSE_FILE logs -f
EOF
