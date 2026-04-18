# vibe-dictate

Windows push-to-talk diktafon a lokális VibeVoice ASR endpointhoz.

## Mit csinál

- System tray ikon háttérben
- Global hotkey (default `RightAlt+Space`) nyomva tartva: mikrofonról felvesz
- Felengedve: elküldi a VibeVoice Gradio endpointnak, transzkripciót kap
- Az eredményt a fókuszált ablakba szúrja (clipboard + Ctrl+V, vagy SendInput mode)
- Magyarul is megy (VibeVoice-ASR 50+ nyelvet ért)

## Build (Docker cross-compile, nincs toolchain install)

A repo gyökerében:

```bash
docker compose -f docker-compose-vibedictate-build.yml run --rm vibedictate-build
```

Kimenet: `tools/vibe-dictate/target/x86_64-pc-windows-msvc/release/vibe-dictate.exe`

Első build ~5-10 perc (xwin letölti az MSVC headereket).
Incremental build ~10-30 sec.

## Futtatás

Dupla klikk az `.exe`-re. Tray-be megy, megkeresed az ikonját.

Első indításkor létrejön: `%APPDATA%\vibe-dictate\config.toml` — itt állíthatod:
- `gradio.url` — default `http://localhost:7860`
- `gradio.api_token` — üres lokálhoz; távoli szervernél ide jön a Bearer
- `gradio.extra_ca_cert` — üres lokálhoz; self-signed / internal CA esetén
  abszolút path a PEM fájlhoz (cert vagy bundle, a rendszer root store
  mellé töltődik be)
- `hotkey.binding` — default `F8`
- `output.mode` — `clipboard` (default) vagy `sendinput`
- `audio.mic_device` — üres = default device
- `stt.language_hint` — default `Hungarian`, a modelbe mint preferált
  nyelv-prompt megy be
- `stt.context_info` — default "speaker is Hungarian, mixes English
  technical terms" — szabad szöveg, tetszőlegesen szigorítható

Config módosítás után: tray menü → "Reload config".

## Remote backend

Ha a VibeVoice Gradio egy szerveren fut (pl. az ASUS GB10 DGX Spark-en,
lásd `docker-compose-vibevoice-asr-gb10.yml`):

```toml
[gradio]
url = "https://vibevoice.example.internal"
api_token = "sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxx"
extra_ca_cert = "C:/Users/you/certs/internal-ca.pem"
```

A Bearer tokent a reqwest minden /upload, /call, /poll kéréshez beteszi
(`Authorization: Bearer <token>`). A CA fájl egy vagy több PEM-kódolt
cert lehet — reqwest rustls-sel bundlekezeli.

## Funkciók v0.1

- [x] Tray ikon + Quit
- [x] Global hotkey (config-ból)
- [x] Mic capture (WASAPI, cpal)
- [x] Gradio HTTP: upload + call + SSE result
- [x] Clipboard + Ctrl+V output
- [ ] SendInput output (config-ban választható, de v0.1-ben clipboard a default)
- [ ] Autostart toggle
- [ ] Mic picker tray menüben
- [ ] Hotkey átkonfigurálás UI-ból
- [ ] Settings ablak
