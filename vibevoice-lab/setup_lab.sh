#!/usr/bin/env bash
# =============================================================================
#  Bootstraps the VibeVoice lab stack.
#    1. Clone (or update) microsoft/VibeVoice into ./VibeVoice/
#    2. Copy .env.example → .env if missing
#    3. Pre-pull the base image
#
#  Usage:
#    ./setup_lab.sh
# =============================================================================
set -euo pipefail

UPSTREAM_URL="https://github.com/microsoft/VibeVoice"
COMPOSE_FILE="docker-compose.yml"

cd "$(dirname "${BASH_SOURCE[0]}")"

command -v docker >/dev/null 2>&1 || { echo "docker not found" >&2; exit 1; }
command -v git    >/dev/null 2>&1 || { echo "git not found" >&2; exit 1; }

if [[ -d VibeVoice/.git ]]; then
    echo "[lab] VibeVoice/ exists, fetching upstream"
    git -C VibeVoice fetch --quiet origin
    git -C VibeVoice merge --ff-only origin/HEAD \
        || echo "[lab] VibeVoice has local commits — skipping ff merge"
else
    echo "[lab] cloning $UPSTREAM_URL → VibeVoice/"
    git clone --depth 1 "$UPSTREAM_URL" VibeVoice
fi

if [[ ! -f .env ]]; then
    echo "[lab] seeding .env from .env.example"
    cp .env.example .env
    echo "[lab] NOTE: set HUGGING_FACE_HUB_TOKEN in .env to skip anon rate limits"
else
    echo "[lab] .env already present, leaving it alone"
fi

echo "[lab] docker compose pull (base image only; profiled services are gated)"
docker compose --env-file .env -f "$COMPOSE_FILE" pull \
    || echo "[lab] pull reported errors — continuing"

cat <<EOF

[lab] done. All services are profile-gated — pick what to start:
  docker compose --profile gradio      up -d     # legacy ASR on :7860
  docker compose --profile realtime    up -d     # realtime TTS on :3000
  docker compose --profile vllm        up -d     # experimental ASR on :8005
  docker compose --profile openai-tts  up -d     # OpenAI-compat TTS on :8006
EOF
