# Device HUB をユーザー領域に置き、ログイン時に最小化で自動起動させる。
#
#   ./scripts/install-autostart.ps1              # target/release/device-hub.exe を配置して自動起動を登録
#   ./scripts/install-autostart.ps1 -Uninstall   # 自動起動の登録と配置した exe を削除
#
# 管理者権限は不要 (HKCU\...\Run と %LOCALAPPDATA% だけを触る)。
# 設定はマウス本体 (オンボードメモリ) に保存されるので、アプリが閉じていても設定は保たれる。
# 常駐させるのはバッテリー表示とアプリ連動 (前面アプリでプロファイル切替) のため。
param(
    [switch]$Uninstall,
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA 'Programs\DeviceHub')
)

$ErrorActionPreference = 'Stop'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$valueName = 'DeviceHub'
$exe = Join-Path $InstallDirectory 'device-hub.exe'

if ($Uninstall) {
    Remove-ItemProperty -Path $runKey -Name $valueName -ErrorAction SilentlyContinue
    Get-Process -Name 'device-hub' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -eq $exe } | Stop-Process -Force
    if (Test-Path -LiteralPath $InstallDirectory) {
        Remove-Item -LiteralPath $InstallDirectory -Recurse -Force
    }
    Write-Host "自動起動を解除し、$InstallDirectory を削除しました"
    return
}

$root = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$built = Join-Path $root 'target\release\device-hub.exe'
if (-not (Test-Path -LiteralPath $built)) {
    throw "先に cargo build --release -p device-hub を実行してください ($built が無い)"
}

# 配置先で動いている古い版を止めてから上書きする。
Get-Process -Name 'device-hub' -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $exe } | Stop-Process -Force
New-Item -ItemType Directory -Force -Path $InstallDirectory | Out-Null
Copy-Item -LiteralPath $built -Destination $exe -Force

Set-ItemProperty -Path $runKey -Name $valueName -Value ('"{0}" --minimized' -f $exe)
Write-Host "配置: $exe"
Write-Host "自動起動: $runKey\$valueName"
