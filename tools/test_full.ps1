# Full-monitor recording: the bar has no free space, sits inside the region and must be blanked in the video.
# Also shows whether the encoder keeps up at full resolution. The video shows the user's screen:
# only the bar area's pixel values are read, then the file is deleted.
param([string]$Exe = "$PSScriptRoot\..\target\release\kirinuki.exe", [int]$Seconds = 5)
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
Remove-Item "$dir\kirinuki.log", "$dir\full.mp4" -ErrorAction SilentlyContinue
Add-Type -AssemblyName System.Windows.Forms
$b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path $Exe).Path
$psi.Arguments = "--test-record --seconds $Seconds --no-audio --region $($b.X),$($b.Y),$($b.Right),$($b.Bottom) --out $dir\full.mp4"
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['TEMP'] = $dir; $psi.EnvironmentVariables['TMP'] = $dir
$p = [Diagnostics.Process]::Start($psi)
$p.WaitForExit(($Seconds + 20) * 1000) | Out-Null
Select-String -Path "$dir\kirinuki.log" -Pattern 'bar at|done|behind|FAIL' | ForEach-Object { $_.Line }
& ffprobe -hide_banner -loglevel error -show_entries 'stream=width,height,nb_frames,duration' -of compact "$dir\full.mp4"
$l = Select-String -Path "$dir\kirinuki.log" -Pattern 'bar at (-?\d+),(-?\d+) (\d+)x(\d+)' | Select-Object -First 1
$g = $l.Matches[0].Groups
$x = [int]$g[1].Value - $b.X; $y = [int]$g[2].Value - $b.Y; $w = [int]$g[3].Value; $h = [int]$g[4].Value
& ffmpeg -hide_banner -loglevel error -i "$dir\full.mp4" -vf "select='eq(n\,60)',crop=$($w-8):$($h-8):$($x+4):$($y+4),format=gray" -frames:v 1 -f rawvideo -y "$dir\mask.raw"
$px = [IO.File]::ReadAllBytes("$dir\mask.raw")
$st = $px | Measure-Object -Minimum -Maximum
"bar area in video: n=$($px.Length) min=$($st.Minimum) max=$($st.Maximum) (blanked = uniform ~33)"
Remove-Item "$dir\full.mp4", "$dir\mask.raw" -ErrorAction SilentlyContinue
