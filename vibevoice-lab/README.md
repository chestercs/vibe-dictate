# vibevoice-lab

Playground for Microsoft VibeVoice features that aren't part of the
`vibe-dictate` dictation client:

- **Gradio ASR demo** — upstream's web UI, structured speaker-diarized
  output, hotword biasing, language anchoring.
- **Realtime 0.5B TTS** — WebSocket text-to-speech with voice cloning.
- **vLLM-backed ASR** — experimental high-throughput batched inference.
- **OpenAI-compat TTS wrapper** — community `/v1/audio/speech` shim
  around the realtime TTS model.

None of these are needed for dictation — `vibe-dictate` talks to a
separate OpenAI-compat STT server (`/v1/audio/transcriptions`) shipped
in the parent repo. This folder exists so the experimental stack can be
lifted out into its own project without pulling dictation along.

## Layout

```
vibevoice-lab/
├── docker-compose.yml         # all services, profile-gated
├── Dockerfile.vibevoice-gb10  # B-opció prebuilt image (GB10 aarch64)
├── .env.example               # copy to .env before first up
├── setup_lab.sh               # clone VibeVoice + seed .env + pull
├── scripts/
│   └── vibevoice_entrypoint.sh  # runtime pip install (transformers 4.x)
└── VibeVoice/                 # upstream microsoft/VibeVoice (gitignored)
```

## Quick start

```bash
./setup_lab.sh
# then pick whatever services you want:
docker compose --profile gradio     up -d       # Gradio ASR on :7860
docker compose --profile realtime   up -d       # realtime TTS on :3000
docker compose --profile vllm       up -d       # vLLM ASR on :8005
docker compose --profile openai-tts up -d       # OpenAI TTS wrapper on :8006
```

## GB10 (aarch64 Blackwell)

In `.env`:

```
VIBEVOICE_BASE_IMAGE=nvcr.io/nvidia/pytorch:25.12-py3
TORCH_CUDA_ARCH_LIST=12.0+PTX
```

For faster restarts you can swap the runtime-install pattern for a
prebuilt image:

```bash
docker build -f Dockerfile.vibevoice-gb10 \
  -t vibevoice-lab:gb10 \
  ./VibeVoice
```

Then replace `image: ...` with `image: vibevoice-lab:gb10` on the
relevant service. The Dockerfile's default CMD runs the Gradio demo —
change it if you want a different entrypoint.

## Notes on coexistence with vibe-dictate

The lab uses its own named volumes (`vibevoiceLab*`), its own
`COMPOSE_PROJECT_NAME`, and distinct container names, so running it
alongside the dictation stack won't collide. The only shared resource
is the GPU; plan VRAM accordingly.
