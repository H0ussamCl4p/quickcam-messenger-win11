#Requires -RunAsAdministrator
# Removes the "QuickCam Messenger (Rust)" virtual camera.
# Run from an ADMIN PowerShell:   powershell -ExecutionPolicy Bypass -File .\uninstall.ps1
$clsid = '{7B1E4C2A-9D3F-4E8B-A6C1-0F2D5E8B3A91}'
$dest  = Join-Path $env:ProgramFiles 'QCVCam'

$exe = Join-Path $dest 'qcvcam.exe'
if (Test-Path $exe) { & $exe remove }

Remove-Item -Recurse -Force "HKLM:\SOFTWARE\Classes\CLSID\$clsid" -ErrorAction SilentlyContinue

# Frame Server may still hold the DLL; restart it so the files can be deleted.
Stop-Service FrameServerMonitor, FrameServer -Force -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $dest -ErrorAction SilentlyContinue
Start-Service FrameServer -ErrorAction SilentlyContinue

Write-Host 'Virtual camera removed. (Log folder C:\ProgramData\QCVCam left in place.)'
