#!/usr/bin/env bash
# =============================================================================
#  VibeVoice no-build entrypoint (runtime install on first boot).
#
#  On first container start, installs VibeVoice + minimum runtime deps
#  into the container. Idempotent via marker file: subsequent starts skip
#  install. pip cache lives on a named volume so the marker reset only
#  pays the network cost once.
# =============================================================================
set -euo pipefail

MARKER=/root/.vibevoice_installed
REPO_DIR=/workspace/VibeVoice

if [[ ! -f "$MARKER" ]]; then
    echo "[entrypoint] First boot, installing VibeVoice..."
    cd "$REPO_DIR"

    # --no-deps + explicit runtime deps: the [streamingtts] extra pins
    # transformers==4.51.3, which clashes with the newer transformers we
    # install here. Sidestepping the extra keeps both versions happy.
    pip install --quiet --no-deps -e .
    pip install --quiet \
        "transformers>=4.51.3,<5.0.0" \
        accelerate diffusers librosa soundfile pydub \
        ml-collections absl-py gradio \
        "uvicorn[standard]" fastapi aiortc av llvmlite numba

    # liger-kernel is optional (the Gradio demo guards it with try/except).
    pip install --quiet liger-kernel || echo "[entrypoint] liger-kernel skip"

    touch "$MARKER"
    echo "[entrypoint] Install complete."
else
    echo "[entrypoint] Already installed, skipping pip install."
fi

cd "$REPO_DIR"
exec "$@"
