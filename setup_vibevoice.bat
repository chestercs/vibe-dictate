@echo off
rem =============================================================================
rem  Bootstraps the VibeVoice backend on Windows (Docker Desktop host).
rem
rem  Mirrors setup_vibevoice.sh. Default --arch x86 since GB10 is Linux-only.
rem
rem  Usage:
rem    setup_vibevoice.bat                   :: x86 stack, clone + pull
rem    setup_vibevoice.bat --arch gb10       :: (rare on Windows) GB10 stack
rem    setup_vibevoice.bat --build           :: prebuilt image (GB10 only)
rem    setup_vibevoice.bat --up              :: also bring the stack up
rem    setup_vibevoice.bat --arch x86 --up
rem =============================================================================
setlocal enabledelayedexpansion

set "ARCH=x86"
set "DO_BUILD=0"
set "DO_UP=0"
set "UPSTREAM_URL=https://github.com/microsoft/VibeVoice"

:parse
if "%~1"=="" goto :parsed
if /I "%~1"=="--arch"    ( set "ARCH=%~2" & shift & shift & goto :parse )
if /I "%~1"=="--build"   ( set "DO_BUILD=1" & shift & goto :parse )
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
where git >nul 2>&1 || (
    echo git not found on PATH 1>&2
    popd >nul
    exit /b 1
)

rem -----------------------------------------------------------------
rem 1. Upstream checkout
rem -----------------------------------------------------------------
if exist "VibeVoice\.git" (
    echo [setup] VibeVoice\ exists, fetching upstream
    git -C VibeVoice fetch --quiet origin
    git -C VibeVoice merge --ff-only origin/HEAD
    if errorlevel 1 (
        echo [setup] VibeVoice has local commits -- skipping ff merge
    )
) else (
    echo [setup] cloning %UPSTREAM_URL% -^> VibeVoice\
    git clone --depth 1 %UPSTREAM_URL% VibeVoice
    if errorlevel 1 (
        echo [setup] git clone failed 1>&2
        popd >nul
        exit /b 1
    )
)

rem -----------------------------------------------------------------
rem 2. .env
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
rem 3. Pre-pull base images
rem -----------------------------------------------------------------
echo [setup] docker compose pull
docker compose --env-file .env -f "%COMPOSE_FILE%" pull
if errorlevel 1 echo [setup] pull reported errors -- continuing

rem -----------------------------------------------------------------
rem 4. Optional prebuilt image (GB10 B-opció)
rem -----------------------------------------------------------------
if "%DO_BUILD%"=="1" (
    if /I not "%ARCH%"=="gb10" (
        echo [setup] --build only applies to --arch gb10; skipping
    ) else (
        set "CTX=%VIBEVOICE_SRC%"
        if not defined VIBEVOICE_SRC set "CTX=.\VibeVoice"
        echo [setup] building vibevoice-gb10:latest from !CTX!
        docker build -f Dockerfile.vibevoice-gb10 -t vibevoice-gb10:latest "!CTX!"
        if errorlevel 1 (
            echo [setup] docker build failed 1>&2
            popd >nul
            exit /b 1
        )
    )
)

rem -----------------------------------------------------------------
rem 5. Optional up
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
echo   setup_vibevoice.bat                   :: x86 stack, clone + pull
echo   setup_vibevoice.bat --arch gb10       :: GB10 stack
echo   setup_vibevoice.bat --build           :: prebuilt image ^(GB10 only^)
echo   setup_vibevoice.bat --up              :: also bring the stack up
exit /b 2
