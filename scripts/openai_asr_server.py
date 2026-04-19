"""OpenAI-compatible ASR server for microsoft/VibeVoice-ASR-HF.

Exposes:
  POST /v1/audio/transcriptions   (OpenAI spec, multipart/form-data)
  GET  /v1/models                 (discoverability)
  GET  /healthz                   (container healthcheck)

Accepts any audio container ffmpeg/librosa can decode (wav, mp3, m4a,
webm, ogg, flac, ...). Returns the four OpenAI response formats: json,
text, verbose_json, srt, vtt. The verbose_json segments carry a
non-standard `speaker` field alongside start/end/text so clients that
understand VibeVoice diarization can use it; OpenAI-only clients
ignore it.

Optional bearer-token auth via VIBEVOICE_API_KEY env (leave empty to
disable).
"""
from __future__ import annotations

import argparse
import logging
import os
import tempfile
import time
from typing import Optional

import torch
import uvicorn
from fastapi import FastAPI, File, Form, Header, HTTPException, UploadFile
from fastapi.responses import PlainTextResponse
from transformers import AutoProcessor, VibeVoiceAsrForConditionalGeneration

LOG = logging.getLogger("vibevoice-asr-openai")

DTYPE_MAP = {
    "bf16": torch.bfloat16,
    "bfloat16": torch.bfloat16,
    "fp16": torch.float16,
    "float16": torch.float16,
    "fp32": torch.float32,
    "float32": torch.float32,
}


def load_model(model_id: str, dtype: str):
    torch_dtype = DTYPE_MAP.get(dtype.lower(), torch.bfloat16)
    processor = AutoProcessor.from_pretrained(model_id)
    model = VibeVoiceAsrForConditionalGeneration.from_pretrained(
        model_id, device_map="auto", torch_dtype=torch_dtype
    )
    model.eval()
    return processor, model


def build_prompt(language: Optional[str], prompt: Optional[str]) -> Optional[str]:
    """Combine the OpenAI `language` hint and the OpenAI `prompt` field into
    a single text turn passed to the processor. VibeVoice-ASR's
    apply_transcription_request only accepts a `prompt` argument — there is
    no `language` kwarg — so language has to be baked into the prompt text
    or the model free-hallucinates the output language (EN/DE commonly)
    when the utterance is short or code-mixed.
    """
    parts: list[str] = []
    if language and language.strip():
        parts.append(f"Transcribe this audio in {language.strip()}.")
    if prompt and prompt.strip():
        parts.append(prompt.strip())
    if not parts:
        return None
    return " ".join(parts)


def transcribe(
    processor,
    asr_model,
    audio_path: str,
    language: Optional[str],
    prompt: Optional[str],
    max_new_tokens: int,
):
    kwargs = {"audio": audio_path}
    combined_prompt = build_prompt(language, prompt)
    if combined_prompt:
        kwargs["prompt"] = combined_prompt
    inputs = processor.apply_transcription_request(**kwargs).to(asr_model.device, asr_model.dtype)

    prompt_len = inputs["input_ids"].shape[1]
    t0 = time.perf_counter()
    with torch.inference_mode():
        output_ids = asr_model.generate(**inputs, max_new_tokens=max_new_tokens)
    elapsed = time.perf_counter() - t0

    generated_ids = output_ids[:, prompt_len:]
    try:
        parsed = processor.decode(generated_ids, return_format="parsed")[0]
    except Exception:
        parsed = []
    text = processor.decode(generated_ids, return_format="transcription_only")[0]
    return text, parsed, elapsed


def segments_from_parsed(parsed) -> list:
    if not isinstance(parsed, list):
        return []
    segments = []
    for i, entry in enumerate(parsed):
        if not isinstance(entry, dict):
            continue
        segments.append(
            {
                "id": i,
                "start": float(entry.get("Start", entry.get("start", 0.0)) or 0.0),
                "end": float(entry.get("End", entry.get("end", 0.0)) or 0.0),
                "text": entry.get("Content", entry.get("content", "")) or "",
                "speaker": entry.get("Speaker", entry.get("speaker")),
            }
        )
    return segments


def _fmt_ts(seconds: float, ms_sep: str) -> str:
    h = int(seconds // 3600)
    m = int((seconds % 3600) // 60)
    s = int(seconds % 60)
    ms = int(round((seconds - int(seconds)) * 1000))
    return f"{h:02d}:{m:02d}:{s:02d}{ms_sep}{ms:03d}"


def format_srt(segments: list) -> str:
    lines = []
    for i, seg in enumerate(segments, start=1):
        lines.append(str(i))
        lines.append(f"{_fmt_ts(seg['start'], ',')} --> {_fmt_ts(seg['end'], ',')}")
        lines.append(seg["text"])
        lines.append("")
    return "\n".join(lines)


def format_vtt(segments: list) -> str:
    lines = ["WEBVTT", ""]
    for seg in segments:
        lines.append(f"{_fmt_ts(seg['start'], '.')} --> {_fmt_ts(seg['end'], '.')}")
        lines.append(seg["text"])
        lines.append("")
    return "\n".join(lines)


def build_app(processor, asr_model, model_id: str, api_key: Optional[str], max_new_tokens: int) -> FastAPI:
    app = FastAPI(title="VibeVoice OpenAI-compat ASR")

    def check_auth(authorization: Optional[str]):
        if not api_key:
            return
        if not authorization or not authorization.startswith("Bearer "):
            raise HTTPException(status_code=401, detail="missing bearer token")
        token = authorization[len("Bearer "):].strip()
        if token != api_key:
            raise HTTPException(status_code=401, detail="invalid api key")

    @app.get("/healthz")
    def healthz():
        return {"ok": True, "model": model_id}

    @app.get("/v1/models")
    def list_models(authorization: Optional[str] = Header(default=None)):
        check_auth(authorization)
        return {
            "object": "list",
            "data": [
                {
                    "id": model_id,
                    "object": "model",
                    "owned_by": "microsoft",
                    "created": 0,
                }
            ],
        }

    @app.post("/v1/audio/transcriptions")
    async def transcriptions(
        file: UploadFile = File(...),
        model: str = Form(default=model_id),  # noqa: ARG001 — ignored, we serve one model
        language: Optional[str] = Form(default=None),
        prompt: Optional[str] = Form(default=None),
        response_format: str = Form(default="json"),
        temperature: float = Form(default=0.0),  # noqa: ARG001 — deterministic for now
        authorization: Optional[str] = Header(default=None),
    ):
        check_auth(authorization)

        data = await file.read()
        if not data:
            raise HTTPException(status_code=400, detail="empty file")

        suffix = os.path.splitext(file.filename or "audio.wav")[1] or ".wav"
        with tempfile.NamedTemporaryFile(delete=False, suffix=suffix) as tf:
            tf.write(data)
            tmp_path = tf.name
        try:
            text, parsed, elapsed = transcribe(
                processor,
                asr_model,
                tmp_path,
                language=language,
                prompt=prompt,
                max_new_tokens=max_new_tokens,
            )
        finally:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass

        LOG.info(
            "transcribed %.2f KB in %.2fs (%d chars)",
            len(data) / 1024.0, elapsed, len(text),
        )

        fmt = (response_format or "json").lower()
        if fmt == "text":
            return PlainTextResponse(text)
        if fmt == "srt":
            return PlainTextResponse(
                format_srt(segments_from_parsed(parsed)), media_type="application/x-subrip"
            )
        if fmt == "vtt":
            return PlainTextResponse(
                format_vtt(segments_from_parsed(parsed)), media_type="text/vtt"
            )
        if fmt == "verbose_json":
            segs = segments_from_parsed(parsed)
            duration = max((s["end"] for s in segs), default=0.0)
            return {
                "task": "transcribe",
                "language": language or "",
                "duration": duration,
                "text": text,
                "segments": segs,
            }
        return {"text": text}

    app.state.model = asr_model
    app.state.processor = processor
    return app


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", default=os.getenv("VIBEVOICE_MODEL", "microsoft/VibeVoice-ASR-HF"))
    parser.add_argument("--host", default=os.getenv("HOST", "0.0.0.0"))
    parser.add_argument("--port", type=int, default=int(os.getenv("PORT", "8080")))
    parser.add_argument("--dtype", default=os.getenv("VIBEVOICE_DTYPE", "bf16"))
    parser.add_argument("--api-key", default=os.getenv("VIBEVOICE_API_KEY") or None)
    parser.add_argument(
        "--max-new-tokens",
        type=int,
        default=int(os.getenv("VIBEVOICE_MAX_NEW_TOKENS", "8192")),
    )
    parser.add_argument("--log-level", default=os.getenv("LOG_LEVEL", "info"))
    args = parser.parse_args()

    logging.basicConfig(
        level=args.log_level.upper(),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    LOG.info("loading %s (dtype=%s)", args.model, args.dtype)
    processor, asr_model = load_model(args.model, args.dtype)
    LOG.info("model on %s (dtype=%s)", asr_model.device, asr_model.dtype)

    app = build_app(processor, asr_model, args.model, args.api_key, args.max_new_tokens)
    uvicorn.run(app, host=args.host, port=args.port, log_level=args.log_level)


if __name__ == "__main__":
    main()
