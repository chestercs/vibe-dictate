#!/usr/bin/env bash
# =============================================================================
#  OpenAI-compat ASR entrypoint (runtime install on first boot).
#
#  Installs transformers>=5.3.0 + FastAPI stack into the container, then
#  exec's into the passed command. Idempotent via separate marker file
#  (distinct from the Gradio-stack marker — the OpenAI container needs
#  transformers 5.x, the Gradio demo runs on transformers 4.x).
# =============================================================================
set -euo pipefail

MARKER=/root/.vibevoice_openai_installed

if [[ ! -f "$MARKER" ]]; then
    echo "[openai-entrypoint] First boot, installing deps..."

    # transformers>=5.3.0 is required for VibeVoiceAsrForConditionalGeneration
    # (microsoft/VibeVoice-ASR-HF). Everything else is the FastAPI stack plus
    # audio decode deps for librosa/soundfile to chew arbitrary containers.
    pip install --quiet \
        "transformers>=5.3.0" \
        accelerate \
        librosa soundfile \
        "uvicorn[standard]" fastapi python-multipart

    touch "$MARKER"
    echo "[openai-entrypoint] Install complete."
else
    echo "[openai-entrypoint] Already installed, skipping pip install."
fi

exec "$@"
