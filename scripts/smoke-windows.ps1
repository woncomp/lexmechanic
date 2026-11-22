param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$CopilotHome,
    [Parameter(Mandatory)][string]$DataDirectory,
    [string]$VsCodeUser,
    [string]$Screenshot,
    [switch]$NavigateToSecondMessage
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
if (-not ('ReaderSmokeNative' -as [type])) {
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ReaderSmokeNative {
    [StructLayout(LayoutKind.Sequential)] public struct Rect { public int Left, Top, Right, Bottom; }
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint key, uint mapType);
}
'@
}

$env:COPILOT_HOME = (Resolve-Path -LiteralPath $CopilotHome).Path
$env:AGENT_CHAT_READER_DATA_DIR = [IO.Path]::GetFullPath($DataDirectory)
if (Test-Path -LiteralPath (Join-Path $DataDirectory 'settings.json')) {
    throw 'Use a fresh application data directory so saved tabs cannot produce a false positive.'
}
[void](New-Item -ItemType Directory -Force -Path $DataDirectory)
$codeSources = if ($VsCodeUser) { @((Resolve-Path -LiteralPath $VsCodeUser).Path) } else { @() }
@{ sources = @($env:COPILOT_HOME); vscode_sources = @($codeSources) } | ConvertTo-Json |
    Set-Content -LiteralPath (Join-Path $DataDirectory 'settings.json') -Encoding utf8NoBOM
$executablePath = (Resolve-Path -LiteralPath $Executable).Path
$process = Start-Process -FilePath $executablePath -WorkingDirectory (Split-Path -Parent $executablePath) -PassThru
try {
    Start-Sleep -Seconds 5
    $process.Refresh()
    if ($process.HasExited) { throw "Application exited early: $($process.ExitCode)" }
    if ($process.MainWindowTitle -ne 'AgentChatReader' -or -not $process.Responding) {
        throw 'AgentChatReader did not open a responsive window.'
    }
    $window = $process.MainWindowHandle
    [void][ReaderSmokeNative]::SetForegroundWindow($window)
    # Open a visible fixture row in preview, then explicitly retain it by double-clicking.
    $position = [IntPtr]((190 -shl 16) -bor 220)
    [void][ReaderSmokeNative]::PostMessage($window, 0x0201, [IntPtr]1, $position)
    [void][ReaderSmokeNative]::PostMessage($window, 0x0202, [IntPtr]0, $position)
    Start-Sleep -Seconds 2
    $scanCode = [ReaderSmokeNative]::MapVirtualKey(13, 0)
    [void][ReaderSmokeNative]::PostMessage($window, 0x0100, [IntPtr]13, [IntPtr](1 -bor ($scanCode -shl 16)))
    [void][ReaderSmokeNative]::PostMessage($window, 0x0101, [IntPtr]13, [IntPtr]([long]0xC0000001 -bor ($scanCode -shl 16)))
    Start-Sleep -Seconds 2
    $keyboardState = Get-Content -Raw -LiteralPath (Join-Path $DataDirectory 'settings.json') | ConvertFrom-Json
    if ($keyboardState.tabs.Count -lt 1) { throw 'Enter did not retain the selected session.' }
    foreach ($click in 1..2) {
        [void][ReaderSmokeNative]::PostMessage($window, 0x0201, [IntPtr]1, $position)
        [void][ReaderSmokeNative]::PostMessage($window, 0x0202, [IntPtr]0, $position)
        Start-Sleep -Milliseconds 80
    }
    Start-Sleep -Seconds 2
    if ($NavigateToSecondMessage) {
        $client = [ReaderSmokeNative+Rect]::new()
        [void][ReaderSmokeNative]::GetClientRect($window, [ref]$client)
        $navigationPosition = [IntPtr]((169 -shl 16) -bor ($client.Right - 20))
        [void][ReaderSmokeNative]::PostMessage($window, 0x0201, [IntPtr]1, $navigationPosition)
        [void][ReaderSmokeNative]::PostMessage($window, 0x0202, [IntPtr]0, $navigationPosition)
        Start-Sleep -Seconds 2
    }
    # Send Unicode through the real native input path, not through application state.
    $searchPosition = [IntPtr]((27 -shl 16) -bor 500)
    [void][ReaderSmokeNative]::PostMessage($window, 0x0201, [IntPtr]1, $searchPosition)
    [void][ReaderSmokeNative]::PostMessage($window, 0x0202, [IntPtr]0, $searchPosition)
    foreach ($character in '文搜索'.ToCharArray()) {
        [void][ReaderSmokeNative]::PostMessage($window, 0x0102, [IntPtr][int]$character, [IntPtr]0)
    }
    Start-Sleep -Seconds 2
    $process.Refresh()
    if ($process.HasExited -or -not $process.Responding) { throw 'Application stopped responding during preview.' }
    if ($Screenshot) {
        $rect = [ReaderSmokeNative+Rect]::new()
        [void][ReaderSmokeNative]::GetWindowRect($window, [ref]$rect)
        $bitmap = [Drawing.Bitmap]::new($rect.Right - $rect.Left, $rect.Bottom - $rect.Top)
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size)
            $bitmap.Save([IO.Path]::GetFullPath($Screenshot), [Drawing.Imaging.ImageFormat]::Png)
        } finally {
            $graphics.Dispose()
            $bitmap.Dispose()
        }
    }
    Write-Output "Responsive native window verified (PID $($process.Id))."
    Write-Output "Private memory: $([math]::Round($process.PrivateMemorySize64 / 1MB, 1)) MiB."
} finally {
    $process.Refresh()
    if (-not $process.HasExited) {
        [void]$process.CloseMainWindow()
        if (-not $process.WaitForExit(10000)) {
            Stop-Process -Id $process.Id
            throw 'The application did not exit after its window closed.'
        }
    }
}
$settings = Get-Content -Raw -LiteralPath (Join-Path $DataDirectory 'settings.json') | ConvertFrom-Json
if ($settings.tabs.Count -lt 1) { throw 'Double-click did not retain a permanent tab.' }
if ($settings.positions.PSObject.Properties.Count -lt 1) { throw 'Reading position was not persisted.' }
if ($NavigateToSecondMessage -and -not ($settings.positions.PSObject.Properties.Value.message_id -contains 'message-1')) {
    throw 'The navigation bar did not reach the second message.'
}
Write-Output 'Permanent tab and reading-state persistence verified.'
