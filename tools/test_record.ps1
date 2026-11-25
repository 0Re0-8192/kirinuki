# Records our own pattern window for N seconds and, while recording, grabs the stop bar
# through Desktop Duplication (the same path CRD/VD use) to prove it is visible to them.
# -Launch runas starts it with a basic-user token (unelevated), direct starts it as a child.
param([string]$Exe = "$PSScriptRoot\..\target\debug\kirinuki.exe", [int]$Seconds = 6, [string]$Extra = '', [string]$Out = 't.mp4', [string]$Launch = 'direct')
$ErrorActionPreference = 'Stop'
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
New-Item -ItemType Directory -Force $dir | Out-Null
icacls $dir /setintegritylevel '(OI)(CI)L' | Out-Null
Remove-Item "$dir\kirinuki.log", "$dir\$Out", "$dir\bar_dda.png" -ErrorAction SilentlyContinue
$exePath = (Resolve-Path $Exe).Path
$cmd = Join-Path (Split-Path $exePath) 'run_test.cmd'
"@echo off`r`nset TEMP=$dir`r`nset TMP=$dir`r`n`"$exePath`" --test-record --seconds $Seconds --out $dir\$Out $Extra`r`n" | Set-Content -Encoding ascii $cmd
switch ($Launch) {
    'runas' { & runas.exe /trustlevel:0x20000 "cmd.exe /c $cmd" | Out-Null }
    default { & cmd.exe /c $cmd }
}
$deadline = (Get-Date).AddSeconds(15)
$line = $null
while (-not $line -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 200
    if (Test-Path "$dir\kirinuki.log") {
        $line = Select-String -Path "$dir\kirinuki.log" -Pattern 'bar_rect=([-\d]+),([-\d]+),([-\d]+),([-\d]+)' | Select-Object -First 1
        if (Select-String -Path "$dir\kirinuki.log" -Pattern 'TEST FAIL' -Quiet) { break }
    }
}
if ($line) {
    Start-Sleep -Milliseconds 1500
    $g = $line.Matches[0].Groups
    $l = [int]$g[1].Value - 8; $t = [int]$g[2].Value - 8; $r = [int]$g[3].Value + 8; $b = [int]$g[4].Value + 8
    $w = ($r - $l) -band -2; $h = ($b - $t) -band -2
    & ffmpeg -hide_banner -loglevel error -f lavfi -i "ddagrab=output_idx=0:framerate=10:offset_x=$($l):offset_y=$($t):video_size=$($w)x$($h),hwdownload,format=bgra" -frames:v 1 -y "$dir\bar_dda.png"
} else { 'no bar_rect in log' }
$deadline = (Get-Date).AddSeconds($Seconds + 30)
while ((Get-Date) -lt $deadline) {
    if (Select-String -Path "$dir\kirinuki.log" -Pattern 'TEST (OK|FAIL)' -Quiet -ErrorAction SilentlyContinue) { break }
    Start-Sleep -Milliseconds 300
}
Get-Content "$dir\kirinuki.log"
if (Test-Path "$dir\$Out") {
    & ffprobe -hide_banner -loglevel error -show_entries 'stream=codec_name,width,height,nb_frames,duration,sample_rate:format=duration' -of compact "$dir\$Out"
}
