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
- `hotkey.binding` — default `RightAlt+Space`
- `output.mode` — `clipboard` (default) vagy `sendinput`
- `audio.mic_device` — üres = default device

Config módosítás után: tray menü → "Reload config".

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
