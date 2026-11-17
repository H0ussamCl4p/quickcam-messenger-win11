#Requires -RunAsAdministrator
# Installs the "QuickCam Messenger (Rust)" virtual camera for all users.
# Run from an ADMIN PowerShell in this folder:   powershell -ExecutionPolicy Bypass -File .\install.ps1
$ErrorActionPreference = 'Stop'
$clsid  = '{7B1E4C2A-9D3F-4E8B-A6C1-0F2D5E8B3A91}'
$src    = Join-Path $PSScriptRoot 'target\release'
$dest   = Join-Path $env:ProgramFiles 'QCVCam'
$logDir = Join-Path $env:ProgramData 'QCVCam'

foreach ($f in 'qccam.dll','qcvcam.exe') {
    if (-not (Test-Path (Join-Path $src $f))) { throw "$f not found - run 'cargo build --release' first" }
}

# 1. Copy binaries somewhere the Frame Server service (LocalService) can read.
#    If an older DLL is loaded, restart Frame Server so the file is released.
New-Item -ItemType Directory -Force $dest | Out-Null
try { Copy-Item (Join-Path $src 'qccam.dll'),(Join-Path $src 'qcvcam.exe') $dest -Force }
catch {
    Write-Host 'DLL in use - restarting Frame Server services...'
    Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue
    Copy-Item (Join-Path $src 'qccam.dll'),(Join-Path $src 'qcvcam.exe') $dest -Force
}

# 2. Log folder writable by LocalService (S-1-5-19).
New-Item -ItemType Directory -Force $logDir | Out-Null
icacls $logDir /grant '*S-1-5-19:(OI)(CI)M' | Out-Null

# 3. Register the COM class that Frame Server will load.
$key = "HKLM:\SOFTWARE\Classes\CLSID\$clsid"
New-Item -Force $key | Out-Null
Set-ItemProperty $key '(default)' 'QuickCam Messenger virtual camera source'
New-Item -Force "$key\InprocServer32" | Out-Null
Set-ItemProperty "$key\InprocServer32" '(default)' (Join-Path $dest 'qccam.dll')
Set-ItemProperty "$key\InprocServer32" 'ThreadingModel' 'Both'

# 4. Create the virtual camera (persists across reboots).
& (Join-Path $dest 'qcvcam.exe') install
if ($LASTEXITCODE -ne 0) { throw 'qcvcam install failed' }

Write-Host "`nDone. Open the Windows Camera app and pick 'QuickCam Messenger (Rust)'."
Write-Host "Log: $logDir\log.txt"
