# B3-8 repro script: window resize path blocks pane term/pty resize
# Usage:
#   Scenario A (drag divider then window resize):
#     $env:LUMEN_DIAG_IGNORE_CURSOR_LEFT = "1"
#     .\b3_8_repro.ps1 -Scenario A
#   Scenario B (clean start, direct window resize):
#     .\b3_8_repro.ps1 -Scenario B
#
# Pass criteria:
#   - Log shows "pane id=N grid MxC -> RxC" lines = resize executed (PASS)
#   - Log missing that line = resize blocked (FAIL/repro)
#   - Log shows "B3-8 diag: divider_resize_held=true" = root cause confirmed

param(
    [ValidateSet("A","B")]
    [string]$Scenario = "A",
    [string]$LumenExe = "$PSScriptRoot\target\release\lumen.exe",
    [string]$LogDir   = "$PSScriptRoot\target\b3_8_logs"
)

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Win32B38 {
    [DllImport("user32.dll", SetLastError=true)]
    public static extern IntPtr FindWindow(string lpClassName, string lpWindowName);
    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter,
        int X, int Y, int cx, int cy, uint uFlags);
    [DllImport("user32.dll", SetLastError=true)]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")]
    public static extern bool PostMessage(IntPtr hWnd, uint Msg, IntPtr wParam, IntPtr lParam);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT {
        public int Left, Top, Right, Bottom;
    }
    public const uint WM_LBUTTONDOWN = 0x0201;
    public const uint WM_LBUTTONUP   = 0x0202;
    public const uint WM_MOUSEMOVE   = 0x0200;
    public const uint SWP_NOACTIVATE  = 0x0010;
    public const uint SWP_NOZORDER    = 0x0004;
    public static IntPtr MakeLP(int x, int y) {
        return new IntPtr((uint)((y << 16) | (x & 0xFFFF)));
    }
}
"@

if (!(Test-Path $LumenExe)) {
    Write-Error "lumen.exe not found: $LumenExe"
    exit 1
}

New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
$logFile = "$LogDir\b3_8_scenario_${Scenario}.log"
$dumpDir = "$LogDir\b3_8_dump"
New-Item -ItemType Directory -Force -Path $dumpDir | Out-Null
if (Test-Path $logFile) { Remove-Item $logFile }

Write-Host "=== B3-8 repro: Scenario $Scenario ===" -ForegroundColor Cyan
Write-Host "lumen.exe: $LumenExe"
Write-Host "log: $logFile"
Write-Host "dump: $dumpDir"

$env:RUST_LOG = "debug"
$env:LUMEN_DUMP_PTY = $dumpDir

if ($Scenario -eq "A") {
    $env:LUMEN_DIAG_IGNORE_CURSOR_LEFT = "1"
    Write-Host "Scenario A: LUMEN_DIAG_IGNORE_CURSOR_LEFT=1 (cursor stays in window during drag)" -ForegroundColor Yellow
} else {
    Remove-Item Env:\LUMEN_DIAG_IGNORE_CURSOR_LEFT -ErrorAction SilentlyContinue
    Write-Host "Scenario B: clean start, no LUMEN_DIAG_IGNORE_CURSOR_LEFT" -ForegroundColor Green
}

Write-Host "Starting lumen..."
$pinfo = New-Object System.Diagnostics.ProcessStartInfo
$pinfo.FileName = $LumenExe
$pinfo.UseShellExecute = $false
$pinfo.RedirectStandardOutput = $true
$pinfo.RedirectStandardError = $true
$proc = New-Object System.Diagnostics.Process
$proc.StartInfo = $pinfo

# Capture output to log file asynchronously
$logStream = [System.IO.StreamWriter]::new($logFile, $false, [System.Text.Encoding]::UTF8)
$logStream.AutoFlush = $true
$outHandler = { $logStream.WriteLine($Event.SourceArgs[1].Data) }
$errHandler = { $logStream.WriteLine($Event.SourceArgs[1].Data) }
Register-ObjectEvent -InputObject $proc -EventName OutputDataReceived -Action $outHandler | Out-Null
Register-ObjectEvent -InputObject $proc -EventName ErrorDataReceived -Action $errHandler | Out-Null

$proc.Start() | Out-Null
$proc.BeginOutputReadLine()
$proc.BeginErrorReadLine()

Write-Host "lumen PID: $($proc.Id)"
Start-Sleep -Milliseconds 3000

# Find window handle
$hwnd = [IntPtr]::Zero
for ($i = 0; $i -lt 20; $i++) {
    # Try common class names for winit windows on Windows
    $hwnd = [Win32B38]::FindWindow("WINIT_APP", $null)
    if ($hwnd -eq [IntPtr]::Zero) {
        $hwnd = [Win32B38]::FindWindow($null, "Lumen")
    }
    if ($hwnd -ne [IntPtr]::Zero) { break }
    # Try via process main window handle
    $pObj = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
    if ($pObj -and $pObj.MainWindowHandle -ne [IntPtr]::Zero) {
        $hwnd = $pObj.MainWindowHandle
        break
    }
    Start-Sleep -Milliseconds 500
}

if ($hwnd -eq [IntPtr]::Zero) {
    Write-Error "Could not find Lumen window handle"
    $proc.Kill()
    exit 1
}
Write-Host "Window handle: 0x$([Convert]::ToString($hwnd.ToInt64(), 16))"

$rect = New-Object Win32B38+RECT
[Win32B38]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$initW = $rect.Right - $rect.Left
$initH = $rect.Bottom - $rect.Top
Write-Host "Initial window size: ${initW}x${initH}"

# Wait for terminal to be ready
Start-Sleep -Milliseconds 2000

if ($Scenario -eq "A") {
    Write-Host ""
    Write-Host "--- Scenario A: injecting divider drag ---" -ForegroundColor Yellow
    # Inject mouse drag near center of window (approximate divider location)
    $divX = [int]($initW / 2)
    $divY = [int]($initH / 2)
    $lp = [Win32B38]::MakeLP($divX, $divY)

    Write-Host "Injecting WM_LBUTTONDOWN at ($divX, $divY)"
    [Win32B38]::PostMessage($hwnd, [Win32B38]::WM_LBUTTONDOWN, [IntPtr]1, $lp) | Out-Null
    Start-Sleep -Milliseconds 100

    # Drag right by 50px in steps
    for ($dx = 0; $dx -lt 50; $dx += 10) {
        $lp2 = [Win32B38]::MakeLP($divX + $dx, $divY)
        [Win32B38]::PostMessage($hwnd, [Win32B38]::WM_MOUSEMOVE, [IntPtr]1, $lp2) | Out-Null
        Start-Sleep -Milliseconds 30
    }

    Write-Host "Injecting WM_LBUTTONUP (simulating release)"
    $lp3 = [Win32B38]::MakeLP($divX + 50, $divY)
    [Win32B38]::PostMessage($hwnd, [Win32B38]::WM_LBUTTONUP, [IntPtr]0, $lp3) | Out-Null
    Start-Sleep -Milliseconds 500
    Write-Host "Divider drag injection complete"
}

# Note resize log count before window resize
Start-Sleep -Milliseconds 500

Write-Host ""
Write-Host "--- Continuous SetWindowPos window resize (simulating border drag) ---" -ForegroundColor Cyan
$steps = 10
$stepSize = 60
$startX = $rect.Left
$startY = $rect.Top
$startW = $initW
$startH = $initH

for ($s = 1; $s -le $steps; $s++) {
    $newW = $startW + $s * $stepSize
    $newH = $startH + [int]($s * $stepSize * 0.6)
    $flags = [Win32B38]::SWP_NOACTIVATE -bor [Win32B38]::SWP_NOZORDER
    [Win32B38]::SetWindowPos($hwnd, [IntPtr]::Zero, $startX, $startY, $newW, $newH, $flags) | Out-Null
    Write-Host "  Step ${s}/${steps}: ${newW}x${newH}"
    Start-Sleep -Milliseconds 60
}

Start-Sleep -Milliseconds 1500
Write-Host "Window resize complete"

# Wait for PTY dump to update
Start-Sleep -Milliseconds 2000

Write-Host ""
Write-Host "=== Results ===" -ForegroundColor Cyan

# Check log for grid resize entries
Write-Host "Checking log for grid resize entries..."
$logContent = Get-Content $logFile -ErrorAction SilentlyContinue
if ($logContent) {
    $resizeLines = $logContent | Select-String -Pattern "grid \d+x\d+ . \d+x\d+"
    if ($resizeLines) {
        Write-Host "[PASS] Found resize log entries (resize executed correctly):" -ForegroundColor Green
        $resizeLines | Select-Object -Last 20 | ForEach-Object { Write-Host "  $_" }
    } else {
        Write-Host "[FAIL] No resize log entries found (resize may be blocked!)" -ForegroundColor Red
    }

    # Check B3-8 diagnostic (held blocker)
    $heldLines = $logContent | Select-String -Pattern "divider_resize_held=true"
    if ($heldLines) {
        Write-Host "[DIAG] divider_resize_held blocker activated:" -ForegroundColor Yellow
        $heldLines | ForEach-Object { Write-Host "  $_" }
    } else {
        Write-Host "[INFO] No divider_resize_held blocker (normal path)"
    }

    # Check B3-8 bypass
    $bypassLines = $logContent | Select-String -Pattern "B3-8"
    if ($bypassLines) {
        Write-Host "[INFO] B3-8 bypass mechanism triggered:" -ForegroundColor Cyan
        $bypassLines | ForEach-Object { Write-Host "  $_" }
    }
} else {
    Write-Host "[WARN] Log file is empty or not found" -ForegroundColor Yellow
}

# Check dump files for resize sequences
Write-Host ""
Write-Host "Dump files:"
Get-ChildItem $dumpDir -Filter "*.txt" -ErrorAction SilentlyContinue | ForEach-Object {
    $fc = Get-Content $_.FullName -Raw -ErrorAction SilentlyContinue
    if ($fc) {
        $matches2 = [regex]::Matches($fc, "ESC\[8;\d+;\d+t")
        if ($matches2.Count -gt 0) {
            Write-Host "  $($_.Name): found $($matches2.Count) resize sequences"
            $matches2 | Select-Object -Last 5 | ForEach-Object { Write-Host "    $($_.Value)" }
        } else {
            Write-Host "  $($_.Name): no ESC[8;...t resize sequences"
        }
    }
}

# Stop lumen
Write-Host ""
Write-Host "Stopping lumen..."
$proc.Kill()
$proc.WaitForExit(3000) | Out-Null
$logStream.Close()

Write-Host ""
Write-Host "=== Done ===" -ForegroundColor Cyan
Write-Host "Full log: $logFile"
