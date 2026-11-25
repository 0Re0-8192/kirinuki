# Window and screen modes: a pattern window (from --test-record) is the target for window mode.
# Screen-mode PNGs contain the user's screen, so only their size is checked and they are deleted.
param([string]$Exe = "$PSScriptRoot\..\target\release\kirinuki.exe")
$ErrorActionPreference = 'Stop'
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
$cap = "$dir\captures"
$exePath = (Resolve-Path $Exe).Path
Add-Type -Namespace KM -Name U -MemberDefinition @'
[DllImport("user32", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, IntPtr name);
[DllImport("user32")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
[StructLayout(LayoutKind.Explicit, Size=40)] public struct INPUT { [FieldOffset(0)] public uint type; [FieldOffset(8)] public ushort vk; [FieldOffset(10)] public ushort scan; [FieldOffset(12)] public uint flags; [FieldOffset(16)] public uint time; [FieldOffset(24)] public IntPtr extra; }
[DllImport("user32")] public static extern uint SendInput(uint n, INPUT[] i, int size);
public static void Key(ushort vk, bool up) { var a = new INPUT[1]; a[0].type = 1; a[0].vk = vk; a[0].flags = (uint)(up ? 2 : 0); if (vk == 0x5B) a[0].flags |= 1; SendInput(1, a, 40); }
public static void Combo(ushort k) { Key(0x5B,false); System.Threading.Thread.Sleep(30); Key(0xA0,false); System.Threading.Thread.Sleep(30); Key(k,false); System.Threading.Thread.Sleep(30); Key(k,true); System.Threading.Thread.Sleep(30); Key(0xA0,true); System.Threading.Thread.Sleep(30); Key(0x5B,true); }
'@
function Ovl { [KM.U]::FindWindow('KirinukiOverlay', [IntPtr]::Zero) }
function Wait-Until([scriptblock]$cond, [int]$ms = 3000) { $t = [Diagnostics.Stopwatch]::StartNew(); while ($t.ElapsedMilliseconds -lt $ms) { if (& $cond) { return $true }; Start-Sleep -Milliseconds 50 }; return $false }
function L([int]$x, [int]$y) { [IntPtr](($y -shl 16) -bor ($x -band 0xFFFF)) }
function Start-K([string]$args_) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exePath; $psi.Arguments = $args_; $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['TEMP'] = $dir; $psi.EnvironmentVariables['TMP'] = $dir
    $psi.EnvironmentVariables['KIRINUKI_NO_CLIPBOARD'] = '1'
    [System.Diagnostics.Process]::Start($psi)
}
function Click-Shape([string]$shape, [int]$x, [int]$y) {
    Remove-Item $cap -Recurse -Force -ErrorAction SilentlyContinue
    "save_dir=$cap`r`naudio=0`r`nvideo=0`r`nshape=$shape`r`n" | Set-Content -Encoding ascii (Join-Path (Split-Path $exePath) 'kirinuki.ini')
    $app = Start-K ''
    try {
        Start-Sleep -Milliseconds 700
        [KM.U]::Combo(0x53)
        if (-not (Wait-Until { (Ovl) -ne [IntPtr]::Zero })) { return 'no overlay' }
        Start-Sleep -Milliseconds 300
        [KM.U]::PostMessage((Ovl), 0x0201, [IntPtr]1, (L $x $y)) | Out-Null
        [KM.U]::PostMessage((Ovl), 0x0202, [IntPtr]0, (L $x $y)) | Out-Null
        if (-not (Wait-Until { (Get-ChildItem $cap -Filter *.png -ErrorAction SilentlyContinue).Count -eq 1 })) { return 'no png' }
        $png = Get-ChildItem $cap -Filter *.png | Select-Object -First 1
        $size = (& ffprobe -hide_banner -loglevel error -show_entries 'stream=width,height' -of csv=p=0 $png.FullName)
        return $size
    } finally {
        $h = [KM.U]::FindWindow('KirinukiApp', [IntPtr]::Zero)
        if ($h -ne [IntPtr]::Zero) { [KM.U]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
        if (-not $app.WaitForExit(5000)) { $app.Kill() }
    }
}
Remove-Item "$dir\kirinuki.log" -ErrorAction SilentlyContinue
# pattern window at a known place (320x200, bottom right of the work area)
$pat = Start-K "--test-record --seconds 14 --no-audio --out $dir\t3.mp4"
try {
    Start-Sleep -Milliseconds 1500
    $line = Select-String -Path "$dir\kirinuki.log" -Pattern 'region=([-\d]+),([-\d]+),([-\d]+),([-\d]+)' | Select-Object -First 1
    $g = @($line.Matches[0].Groups | Select-Object -Skip 1 | ForEach-Object { [int]$_.Value })
    $cx = [int](($g[0] + $g[2]) / 2); $cy = [int](($g[1] + $g[3]) / 2)
    "window mode (expect $($g[2]-$g[0]),$($g[3]-$g[1])): " + (Click-Shape 'window' $cx $cy)
    "screen mode (expect monitor size): " + (Click-Shape 'screen' $cx $cy)
} finally {
    if (-not $pat.WaitForExit(20000)) { $pat.Kill() }
    Remove-Item $cap -Recurse -Force -ErrorAction SilentlyContinue
}
Add-Type -AssemblyName System.Windows.Forms
"monitor: " + [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
Get-Content "$dir\kirinuki.log" | Select-String 'still|FAIL|failed'
