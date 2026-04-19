#!/usr/bin/env bash
# =============================================================================
#  Bootstraps the VibeVoice STT backend next to vibe-dictate.
#
#  The OpenAI-compat ASR shim (port 8080) needs NO VibeVoice checkout —
#  transformers>=5.3 ships microsoft/VibeVoice-ASR-HF natively. The
#  entrypoint script (scripts/openai_asr_entrypoint.sh) pip-installs
#  deps on first boot.
#
#  For the Gradio demo / realtime TTS / vLLM experiments / B-opció
#  prebuilt image, see vibevoice-lab/ (separate stack, profile-gated).
#
#  Steps (all idempotent):
#    1. Copy the matching .env.*.example → .env if missing
#    2. Pre-pull the compose-referenced base image
#    3. Optional: docker compose up -d (--up)
#
#  Usage:
#    ./setup_vibevoice.sh                   # GB10 stack, just seed + pull
#    ./setup_vibevoice.sh --arch x86        # consumer NVIDIA stack
#    ./setup_vibevoice.sh --up              # also bring the stack up
#    ./setup_vibevoice.sh --arch x86 --up
# =============================================================================
set -euo pipefail

ARCH="gb10"
DO_UP=0

usage() {
    sed -n '2,22p' "$0"
    exit "${1:-0}"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)  ARCH="${2:-}"; shift 2 ;;
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

# Always operate from the repo root (where this script lives).
cd "$(dirname "${BASH_SOURCE[0]}")"

command -v docker >/dev/null 2>&1 || {
    echo "docker not found on PATH" >&2; exit 1;
}

# -----------------------------------------------------------------
# 1. .env
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
# 2. Pre-pull base image
# -----------------------------------------------------------------
echo "[setup] docker compose pull"
docker compose --env-file .env -f "$COMPOSE_FILE" pull \
    || echo "[setup] pull reported errors — continuing"

# -----------------------------------------------------------------
# 3. Optional up
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
