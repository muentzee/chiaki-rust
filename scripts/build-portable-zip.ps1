# Portable-Zip-Bau für chiaki-remaster-rs (Windows x64).
#
# Rust-Gegenstück zu scripts/build-portable-remaster.sh + deploy-windows-msys2.sh
# des C++-Baus (Referenz: F:\projekte\chiaki-rust-remaster). Unterschiede:
# - Qt-DLLs / windeployqt entfallen (GPUI/DirectX11 statt Qt; chiaki.exe braucht
#   keine Framework-DLLs, alles andere lädt chiaki-media dynamisch).
# - KEINE VFX-SDK-DLLs — Lizenz (wie im C++: "Das SDK selbst ist nicht Teil der
#   portable Zip"); ohne SDK deaktiviert sich VSR automatisch.
# - FFmpeg-DLLs fest verdrahtet statt per ldd: avcodec-61.dll zieht zusätzlich
#   swresample-5.dll nach (dumpbin /DEPENDENTS), avutil-59/swscale-8 hängen nur
#   an System-DLLs. libopus-0.dll braucht nur KERNEL32/msvcrt.
# - data/ wird bei Wiederholung erhalten (portabler Nutzer-Datenordner mit
#   Settings/Profilen/Logs bleibt beim Re-Run bestehen, wie im C++-Workflow).
#
# Aufruf:  powershell -ExecutionPolicy Bypass -File scripts/build-portable-zip.ps1
#          [-Version 1.10.0] [-SmokeTest]

param(
    # Versions-Suffix für Ordner- und Zipname (C++-Referenz: 1.10.0).
    [string]$Version = "1.10.0",
    # Workspace-Root (Default: eine Ebene über scripts/).
    [string]$WorkspaceRoot = (Split-Path -Parent $PSScriptRoot),
    # FFmpeg n7.1 Shared-Build (avutil-59/avcodec-61/swscale-8/swresample-5).
    [string]$FfmpegBin = "F:\projekte\chiaki-rust-remaster\ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin",
    # C++-Remaster-Bauordner, Quelle für libopus-0.dll (MSYS2-Build).
    [string]$RemasterWinDir = "F:\projekte\chiaki-rust-remaster\chiaki-remaster-Win",
    # Kurzer Start-Smoke (3 s Lauf, Log-Prüfung) vor dem Zippen.
    [switch]$SmokeTest,
    # Build überspringen (Assemble/Zip aus vorhandener target\release\chiaki.exe).
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$OutDir = Join-Path $WorkspaceRoot ("chiaki-remaster-rs-win64-portable-{0}" -f $Version)
$ZipFile = "$OutDir.zip"

# ---------------------------------------------------------------------------
# 1. Build (Release)
# ---------------------------------------------------------------------------
if (-not $SkipBuild) {
    Push-Location $WorkspaceRoot
    try {
        cargo build --release -p chiaki-app
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release -p chiaki-app fehlgeschlagen" }
    } finally {
        Pop-Location
    }
}
$Exe = Join-Path $WorkspaceRoot "target\release\chiaki.exe"

# ---------------------------------------------------------------------------
# 2. Eingaben prüfen (Trockentest, siehe chiaki-app portable_zip_tests)
# ---------------------------------------------------------------------------
$FfmpegDlls = @("avutil-59.dll", "avcodec-61.dll", "swscale-8.dll", "swresample-5.dll")
$Inputs = @($Exe, (Join-Path $RemasterWinDir "libopus-0.dll"), (Join-Path $PSScriptRoot "portable-data-readme.txt")) +
    ($FfmpegDlls | ForEach-Object { Join-Path $FfmpegBin $_ })
foreach ($f in $Inputs) {
    if (-not (Test-Path $f)) { throw "Erforderliche Eingabe fehlt: $f" }
}

# ---------------------------------------------------------------------------
# 3. Assemble-Ordner neu aufbauen, data/ erhalten (wie C++-Workflow:
#    rm -rf Output-Dir, aber der portable Nutzerdatenordner bleibt)
# ---------------------------------------------------------------------------
$DataBackup = $null
if (Test-Path $OutDir) {
    $Data = Join-Path $OutDir "data"
    if (Test-Path $Data) {
        $DataBackup = Join-Path ([IO.Path]::GetTempPath()) ("chiaki-portable-data-" + [guid]::NewGuid().ToString("N"))
        Move-Item $Data $DataBackup
        Write-Host "data/ wird für den Re-Run gesichert: $DataBackup"
    }
    Remove-Item $OutDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
if ($DataBackup) {
    Move-Item $DataBackup (Join-Path $OutDir "data")
    Write-Host "data/ wiederhergestellt."
}

# ---------------------------------------------------------------------------
# 4. Dateien zusammenstellen
# ---------------------------------------------------------------------------
Copy-Item $Exe $OutDir
foreach ($dll in $FfmpegDlls) { Copy-Item (Join-Path $FfmpegBin $dll) $OutDir }
Copy-Item (Join-Path $RemasterWinDir "libopus-0.dll") $OutDir

# data/ mit README als portabler Marker (AppPaths nutzt <exe-dir>/data, wenn
# beschreibbar) — LEER außer der Doku, keine Settings/Profile-Daten!
$DataDir = Join-Path $OutDir "data"
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
Copy-Item (Join-Path $PSScriptRoot "portable-data-readme.txt") (Join-Path $DataDir "README.txt")

# ---------------------------------------------------------------------------
# 5. Optionaler Smoke-Test: starten, 3 s laufen lassen, beenden, Log prüfen.
#    (Erzeugt data/log/chiaki-ui.log — Beweis, dass data/ benutzt wird.)
# ---------------------------------------------------------------------------
if ($SmokeTest) {
    $LogDir = Join-Path $DataDir "log"
    $Proc = Start-Process -FilePath (Join-Path $OutDir "chiaki.exe") -WorkingDirectory $OutDir -PassThru
    Start-Sleep -Seconds 3
    $WasRunning = -not $Proc.HasExited
    if ($WasRunning) {
        Stop-Process -Id $Proc.Id -Force
        $Proc.WaitForExit()
    }
    $Log = Join-Path $LogDir "chiaki-ui.log"
    if (-not (Test-Path $Log)) {
        throw "Smoke-Test fehlgeschlagen: kein Log unter $Log (exe gestartet: $WasRunning). Fehlen DLLs?"
    }
    Write-Host "Smoke-Test OK: exe lief 3 s (beendet: $(-not $WasRunning)), Log:"
    Get-Content $Log | Select-Object -First 5 | ForEach-Object { Write-Host "  | $_" }
}

# ---------------------------------------------------------------------------
# 6. Zip — selektiv: aus data/ kommt NUR README.txt ins Zip (KEINE
#    Settings/Profile-Daten); Nutzerdaten bleiben im lokalen Ordner erhalten.
# ---------------------------------------------------------------------------
if (Test-Path $ZipFile) { Remove-Item $ZipFile -Force }
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$Zip = [System.IO.Compression.ZipFile]::Open($ZipFile, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    $Prefix = Split-Path -Leaf $OutDir
    $Included = @()
    Get-ChildItem $OutDir -Recurse -File | ForEach-Object {
        $Rel = $_.FullName.Substring($OutDir.Length + 1).Replace('\', '/')
        if ($Rel -like 'data/*' -and $Rel -ne 'data/README.txt') { return }  # Nutzerdaten
        [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($Zip, $_.FullName, "$Prefix/$Rel")
        $Included += "  $Rel  ($([math]::Round($_.Length)) bytes)"
    }
} finally {
    $Zip.Dispose()
}

Write-Host ""
Write-Host "portable package: $ZipFile"
Write-Host "Zip-Inhalt:"
$Included | ForEach-Object { Write-Host $_ }
