@echo off
rem =============================================================================
rem  Bootstraps the VibeVoice STT backend on Windows (Docker Desktop host).
rem
rem  Mirrors setup_vibevoice.sh. Default --arch x86 since GB10 is Linux-only.
rem  The OpenAI-compat ASR shim (port 8080) needs NO VibeVoice checkout —
rem  transformers 5.3+ carries microsoft/VibeVoice-ASR-HF natively.
rem
rem  For the Gradio demo / realtime TTS / vLLM / B-opcio prebuilt image,
rem  see vibevoice-lab\ (separate stack, profile-gated).
rem
rem  Usage:
rem    setup_vibevoice.bat                   :: x86 stack, seed + pull
rem    setup_vibevoice.bat --arch gb10       :: (rare on Windows) GB10 stack
rem    setup_vibevoice.bat --up              :: also bring the stack up
rem    setup_vibevoice.bat --arch x86 --up
rem =============================================================================
setlocal enabledelayedexpansion

set "ARCH=x86"
set "DO_UP=0"

:parse
if "%~1"=="" goto :parsed
if /I "%~1"=="--arch"    ( set "ARCH=%~2" & shift & shift & goto :parse )
if /I "%~1"=="--up"      ( set "DO_UP=1" & shift & goto :parse )
if /I "%~1"=="-h"        ( goto :usage )
if /I "%~1"=="--help"    ( goto :usage )
echo unknown flag: %~1 1>&2
goto :usage
:parsed

if /I "%ARCH%"=="gb10" (
    set "ENV_EXAMPLE=.env.vibevoice-gb10.example"
    set "COMPOSE_FILE=docker-compose-vibevoice-asr-gb10.yml"
) else if /I "%ARCH%"=="x86" (
    set "ENV_EXAMPLE=.env.vibevoice.example"
    set "COMPOSE_FILE=docker-compose-vibevoice.yml"
) else (
    echo --arch must be gb10 or x86 1>&2
    exit /b 2
)

rem Operate from the repo root (where this script lives).
pushd "%~dp0" >nul

where docker >nul 2>&1 || (
    echo docker not found on PATH 1>&2
    popd >nul
    exit /b 1
)

rem -----------------------------------------------------------------
rem 1. .env
rem -----------------------------------------------------------------
if not exist ".env" (
    echo [setup] seeding .env from %ENV_EXAMPLE%
    copy /Y "%ENV_EXAMPLE%" ".env" >nul
    if /I "%ARCH%"=="gb10" (
        echo [setup] NOTE: edit .env -^> VIBEVOICE_BASE_DIR ^(host path for cache volumes^)
    )
    echo [setup] NOTE: set HUGGING_FACE_HUB_TOKEN in .env to skip anon rate limits
) else (
    echo [setup] .env already present, leaving it alone
)

rem -----------------------------------------------------------------
rem 2. Pre-pull base image
rem -----------------------------------------------------------------
echo [setup] docker compose pull
docker compose --env-file .env -f "%COMPOSE_FILE%" pull
if errorlevel 1 echo [setup] pull reported errors -- continuing

rem -----------------------------------------------------------------
rem 3. Optional up
rem -----------------------------------------------------------------
if "%DO_UP%"=="1" (
    echo [setup] docker compose up -d
    docker compose --env-file .env -f "%COMPOSE_FILE%" up -d
)

echo.
echo [setup] done. Next steps:
echo   - edit .env if you haven't ^(%ENV_EXAMPLE% has the template^)
echo   - start:  docker compose -f %COMPOSE_FILE% up -d
echo   - stop:   docker compose -f %COMPOSE_FILE% down
echo   - logs:   docker compose -f %COMPOSE_FILE% logs -f

popd >nul
exit /b 0

:usage
echo.
echo Usage:
echo   setup_vibevoice.bat                   :: x86 stack, seed + pull
echo   setup_vibevoice.bat --arch gb10       :: GB10 stack
echo   setup_vibevoice.bat --up              :: also bring the stack up
exit /b 2
