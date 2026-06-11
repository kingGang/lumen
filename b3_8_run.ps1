# B3-8 simplified test runner
# Runs lumen with logging, does SetWindowPos resize, checks for resize in dump

param(
    [string]$Scenario = "B",
    [string]$LumenExe = "$PSScriptRoot\target\release\lumen.exe"
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class W32 {
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x, int y, int cx, int cy, uint f);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    public const uint WM_LBUTTONDOWN = 0x201, WM_LBUTTONUP = 0x202, WM_MOUSEMOVE = 0x200;
    public const uint SWP_NA = 0x10, SWP_NZ = 0x04;
    public static IntPtr LP(int x, int y) { return new IntPtr((uint)((y << 16) | (x & 0xFFFF))); }
}
"@

$logOut = "$PSScriptRoot\target\b3_8_logs\out_${Scenario}.log"
$logErr = "$PSScriptRoot\target\b3_8_logs\err_${Scenario}.log"
$dumpDir = "$PSScriptRoot\target\b3_8_logs\dump_${Scenario}"
New-Item -ItemType Directory -Force -Path "$PSScriptRoot\target\b3_8_logs" | Out-Null
New-Item -ItemType Directory -Force -Path $dumpDir | Out-Null
Remove-Item $logOut,$logErr -ErrorAction SilentlyContinue

$env:RUST_LOG = "debug"
$env:LUMEN_DUMP_PTY = $dumpDir
if ($Scenario -eq "A") { $env:LUMEN_DIAG_IGNORE_CURSOR_LEFT = "1" }
else { Remove-Item Env:\LUMEN_DIAG_IGNORE_CURSOR_LEFT -ErrorAction SilentlyContinue }

Write-Host "Starting lumen (Scenario $Scenario)..."
$proc = Start-Process -FilePath $LumenExe -PassThru `
    -RedirectStandardOutput $logOut `
    -RedirectStandardError $logErr
Write-Host "PID $($proc.Id)"
Start-Sleep 4

$pObj = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
$hwnd = if ($pObj) { $pObj.MainWindowHandle } else { [IntPtr]::Zero }
Write-Host "HWND: 0x$([Convert]::ToString([long]$hwnd, 16))"

$rect = New-Object W32+RECT
[W32]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w0 = $rect.R - $rect.L; $h0 = $rect.B - $rect.T
Write-Host "Initial size: ${w0}x${h0}"

Start-Sleep 2

if ($Scenario -eq "A") {
    Write-Host "Injecting divider drag..."
    $dx = [int]($w0/2); $dy = [int]($h0/2)
    [W32]::PostMessage($hwnd, [W32]::WM_LBUTTONDOWN, [IntPtr]1, [W32]::LP($dx,$dy)) | Out-Null
    Start-Sleep -Milliseconds 100
    for ($i = 0; $i -lt 5; $i++) {
        [W32]::PostMessage($hwnd, [W32]::WM_MOUSEMOVE, [IntPtr]1, [W32]::LP($dx+$i*10,$dy)) | Out-Null
        Start-Sleep -Milliseconds 30
    }
    [W32]::PostMessage($hwnd, [W32]::WM_LBUTTONUP, [IntPtr]0, [W32]::LP($dx+50,$dy)) | Out-Null
    Start-Sleep 1
    Write-Host "Drag injected"
}

Write-Host "Resizing window in 10 steps..."
for ($s = 1; $s -le 10; $s++) {
    $nw = $w0 + $s * 60; $nh = $h0 + $s * 36
    [W32]::SetWindowPos($hwnd, [IntPtr]::Zero, $rect.L, $rect.T, $nw, $nh, [W32]::SWP_NA -bor [W32]::SWP_NZ) | Out-Null
    Start-Sleep -Milliseconds 60
}
Start-Sleep 2

Write-Host "=== Check dump files ==="
Get-ChildItem $dumpDir -Filter "*.txt" | ForEach-Object {
    $c = Get-Content $_.FullName -Raw
    $rs = [regex]::Matches($c, "ESC\[8;\d+;\d+t")
    Write-Host "$($_.Name): $($rs.Count) resize seqs"
    $rs | Select-Object -Last 3 | ForEach-Object { Write-Host "  $($_.Value)" }
}

Write-Host "=== Check err log for grid resize ==="
if (Test-Path $logErr) {
    Select-String -Path $logErr -Pattern "x\d+" -Encoding UTF8 | Select-Object -Last 20 | ForEach-Object { Write-Host $_ }
}
if (Test-Path $logOut) {
    Select-String -Path $logOut -Pattern "x\d+" -Encoding UTF8 | Select-Object -Last 20 | ForEach-Object { Write-Host $_ }
}

Stop-Process $proc -Force -ErrorAction SilentlyContinue
Write-Host "Done. Logs: $logOut / $logErr"
