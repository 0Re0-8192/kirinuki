# While a test recording runs, grab bar + region through Desktop Duplication (what CRD/VD see).
param([string]$Exe = "$PSScriptRoot\..\target\debug\kirinuki.exe")
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
Remove-Item "$dir\kirinuki.log", "$dir\dda_*.png", "$dir\ffmpeg_dda.txt" -ErrorAction SilentlyContinue
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path $Exe).Path
$psi.Arguments = "--test-record --seconds 10 --out $dir\t2.mp4 --no-audio"
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['TEMP'] = $dir; $psi.EnvironmentVariables['TMP'] = $dir
$p = [System.Diagnostics.Process]::Start($psi)
$line = $null; $deadline = (Get-Date).AddSeconds(10)
while (-not $line -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    if (Test-Path "$dir\kirinuki.log") { $line = Select-String -Path "$dir\kirinuki.log" -Pattern 'bar_rect=([-\d]+),([-\d]+),([-\d]+),([-\d]+) region=([-\d]+),([-\d]+),([-\d]+),([-\d]+)' | Select-Object -First 1 }
}
if (-not $line) { 'no bar_rect'; return }
$g = @($line.Matches[0].Groups | Select-Object -Skip 1 | ForEach-Object { [int]$_.Value })
$l = [math]::Min($g[0], $g[4]) - 12; $t = [math]::Min($g[1], $g[5]) - 12
$r = [math]::Max($g[2], $g[6]) + 12; $b = [math]::Max($g[3], $g[7]) + 12
$w = ($r - $l) -band -2; $h = ($b - $t) -band -2
"grab $l,$t ${w}x$h at $(Get-Date -Format HH:mm:ss.fff)"
Start-Sleep -Seconds 2
& ffmpeg -hide_banner -loglevel warning -f lavfi -i "ddagrab=output_idx=0:framerate=10:offset_x=$($l):offset_y=$($t):video_size=$($w)x$($h),hwdownload,format=bgra" -frames:v 20 -update 1 -y "$dir\dda_bar.png" 2> "$dir\ffmpeg_dda.txt"
"ffmpeg rc=$LASTEXITCODE done $(Get-Date -Format HH:mm:ss.fff)"
Get-Content "$dir\ffmpeg_dda.txt"
$p.WaitForExit(30000) | Out-Null
Select-String -Path "$dir\kirinuki.log" -Pattern 'TEST'
