# End-to-end test of the resident app. Shows the overlay and a short recording on screen for a few seconds.
# Keys are injected with SendInput; mouse actions are posted to the overlay window (the real cursor is not moved).
# The clipboard is left alone (KIRINUKI_NO_CLIPBOARD). The test instance is always closed at the end.
param([string]$Exe = "$PSScriptRoot\..\target\debug\kirinuki.exe")
$ErrorActionPreference = 'Stop'
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
$cap = "$dir\captures"
Remove-Item "$dir\kirinuki.log" -ErrorAction SilentlyContinue
Remove-Item $cap -Recurse -Force -ErrorAction SilentlyContinue
$exePath = (Resolve-Path $Exe).Path
"save_dir=$cap`r`naudio=1`r`nvideo=0`r`nshape=rect`r`n" | Set-Content -Encoding ascii (Join-Path (Split-Path $exePath) 'kirinuki.ini')

Add-Type -Namespace KT -Name U -MemberDefinition @'
[DllImport("user32", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, IntPtr name);
[DllImport("user32")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32")] public static extern int GetSystemMetrics(int i);
[DllImport("user32")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
[DllImport("user32")] public static extern IntPtr GetForegroundWindow();
[DllImport("user32")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[StructLayout(LayoutKind.Explicit, Size=40)] public struct INPUT { [FieldOffset(0)] public uint type; [FieldOffset(8)] public ushort vk; [FieldOffset(10)] public ushort scan; [FieldOffset(12)] public uint flags; [FieldOffset(16)] public uint time; [FieldOffset(24)] public IntPtr extra; }
[DllImport("user32")] public static extern uint SendInput(uint n, INPUT[] i, int size);
public static void Key(ushort vk, bool up) { var a = new INPUT[1]; a[0].type = 1; a[0].vk = vk; a[0].flags = (uint)(up ? 2 : 0); if (vk == 0x5B) a[0].flags |= 1; SendInput(1, a, 40); }
public static void Combo(ushort k) { Key(0x5B,false); System.Threading.Thread.Sleep(30); Key(0xA0,false); System.Threading.Thread.Sleep(30); Key(k,false); System.Threading.Thread.Sleep(30); Key(k,true); System.Threading.Thread.Sleep(30); Key(0xA0,true); System.Threading.Thread.Sleep(30); Key(0x5B,true); }
'@
function Ovl { [KT.U]::FindWindow('KirinukiOverlay', [IntPtr]::Zero) }
function Bar { [KT.U]::FindWindow('KirinukiBar', [IntPtr]::Zero) }
function Wait-Until([scriptblock]$cond, [int]$ms = 3000) { $t = [Diagnostics.Stopwatch]::StartNew(); while ($t.ElapsedMilliseconds -lt $ms) { if (& $cond) { return $true }; Start-Sleep -Milliseconds 50 }; return $false }
function Fg { $p = 0; [KT.U]::GetWindowThreadProcessId([KT.U]::GetForegroundWindow(), [ref]$p) | Out-Null; (Get-Process -Id $p).ProcessName }
function L([int]$x, [int]$y) { [IntPtr](($y -shl 16) -bor ($x -band 0xFFFF)) }
function Drag([IntPtr]$h, $x0, $y0, $x1, $y1) {
    [KT.U]::PostMessage($h, 0x0201, [IntPtr]1, (L $x0 $y0)) | Out-Null
    [KT.U]::PostMessage($h, 0x0200, [IntPtr]1, (L (($x0+$x1)/2) (($y0+$y1)/2))) | Out-Null
    [KT.U]::PostMessage($h, 0x0200, [IntPtr]1, (L $x1 $y1)) | Out-Null
    [KT.U]::PostMessage($h, 0x0202, [IntPtr]0, (L $x1 $y1)) | Out-Null
}
# a 300x200 region near the bottom-right of the primary monitor, in overlay (virtual screen) coordinates
$rx = [KT.U]::GetSystemMetrics(0) - 420 - [KT.U]::GetSystemMetrics(76)
$ry = [KT.U]::GetSystemMetrics(1) - 340 - [KT.U]::GetSystemMetrics(77)
$results = [ordered]@{}
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exePath; $psi.UseShellExecute = $false
$psi.EnvironmentVariables['TEMP'] = $dir; $psi.EnvironmentVariables['TMP'] = $dir
$psi.EnvironmentVariables['KIRINUKI_NO_CLIPBOARD'] = '1'
$p = [System.Diagnostics.Process]::Start($psi)
try {
    Start-Sleep -Milliseconds 800
    $app = [KT.U]::FindWindow('KirinukiApp', [IntPtr]::Zero)
    $results['app window'] = $app -ne [IntPtr]::Zero

    # 1. Win+Shift+S opens our overlay (not Snipping Tool); Esc closes it
    [KT.U]::Combo(0x53)
    $results['overlay opens'] = Wait-Until { (Ovl) -ne [IntPtr]::Zero -and [KT.U]::IsWindowVisible((Ovl)) }
    $results['snipping tool stayed away'] = (Fg) -ne 'SnippingTool'
    Start-Sleep -Milliseconds 300
    [KT.U]::Key(0x1B, $false); [KT.U]::Key(0x1B, $true)
    $results['esc closes'] = Wait-Until { (Ovl) -eq [IntPtr]::Zero }

    # 2. still: drag a region
    [KT.U]::Combo(0x53)
    Wait-Until { (Ovl) -ne [IntPtr]::Zero } | Out-Null
    Start-Sleep -Milliseconds 300
    Drag (Ovl) $rx $ry ($rx + 300) ($ry + 200)
    $results['still closes overlay'] = Wait-Until { (Ovl) -eq [IntPtr]::Zero }
    $results['png saved'] = Wait-Until { (Get-ChildItem $cap -Filter *.png -ErrorAction SilentlyContinue).Count -eq 1 }

    # 3. video: Win+Shift+R, drag, record ~3 s, Win+Shift+R stops
    [KT.U]::Combo(0x52)
    Wait-Until { (Ovl) -ne [IntPtr]::Zero } | Out-Null
    Start-Sleep -Milliseconds 300
    Drag (Ovl) $rx $ry ($rx + 300) ($ry + 200)
    $results['bar shown'] = Wait-Until { (Bar) -ne [IntPtr]::Zero }
    Start-Sleep -Seconds 2
    # overlay while recording: recording pauses until it closes (expect ~3 s of video for ~4.5 s wall time)
    [KT.U]::Combo(0x53)
    $results['overlay during rec'] = Wait-Until { (Ovl) -ne [IntPtr]::Zero }
    Start-Sleep -Milliseconds 1500
    [KT.U]::Key(0x1B, $false); [KT.U]::Key(0x1B, $true)
    Wait-Until { (Ovl) -eq [IntPtr]::Zero } | Out-Null
    Start-Sleep -Seconds 1
    [KT.U]::Combo(0x52)
    $results['bar gone'] = Wait-Until { (Bar) -eq [IntPtr]::Zero } 5000
    $results['mp4 saved'] = Wait-Until { (Get-ChildItem $cap -Filter *.mp4 -ErrorAction SilentlyContinue).Count -eq 1 } 5000
    $results['start menu stayed closed'] = (Fg) -ne 'StartMenuExperienceHost'
} finally {
    $app = [KT.U]::FindWindow('KirinukiApp', [IntPtr]::Zero)
    if ($app -ne [IntPtr]::Zero) { [KT.U]::PostMessage($app, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
    if (-not $p.WaitForExit(5000)) { $p.Kill(); $results['clean exit'] = $false } else { $results['clean exit'] = $true }
}
$results.GetEnumerator() | ForEach-Object { '{0,-28} {1}' -f $_.Key, $_.Value }
Get-ChildItem $cap -ErrorAction SilentlyContinue | Select-Object Name, Length | Format-Table -AutoSize | Out-String
$mp4 = Get-ChildItem $cap -Filter *.mp4 -ErrorAction SilentlyContinue | Select-Object -First 1
if ($mp4) { & ffprobe -hide_banner -loglevel error -show_entries 'stream=codec_name,width,height,nb_frames,duration' -of compact $mp4.FullName }
$png = Get-ChildItem $cap -Filter *.png -ErrorAction SilentlyContinue | Select-Object -First 1
if ($png) { & ffprobe -hide_banner -loglevel error -show_entries 'stream=width,height' -of compact $png.FullName }
Get-Content "$dir\kirinuki.log"
