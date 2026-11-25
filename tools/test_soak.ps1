# Long recording of a large region; samples the recorder's memory to catch per-frame leaks.
# The recorded file shows the user's screen: only its stream info is read, then it is deleted.
param([string]$Exe = "$PSScriptRoot\..\target\release\kirinuki.exe", [int]$Seconds = 60, [string]$Region = '200,200,1480,920')
$dir = Join-Path ([IO.Path]::GetTempPath()) 'kirinuki_test'
Remove-Item "$dir\kirinuki.log", "$dir\soak.mp4" -ErrorAction SilentlyContinue
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path $Exe).Path
$psi.Arguments = "--test-record --seconds $Seconds --region $Region --out $dir\soak.mp4"
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['TEMP'] = $dir; $psi.EnvironmentVariables['TMP'] = $dir
$p = [System.Diagnostics.Process]::Start($psi)
$t0 = Get-Date
while (-not $p.HasExited) {
    Start-Sleep -Seconds 10
    if ($p.HasExited) { break }
    $p.Refresh()
    '{0,4:N0}s  private={1,7:N1} MB  ws={2,7:N1} MB  handles={3}' -f ((Get-Date) - $t0).TotalSeconds, ($p.PrivateMemorySize64 / 1MB), ($p.WorkingSet64 / 1MB), $p.HandleCount
}
"exit=$($p.ExitCode)"
Select-String -Path "$dir\kirinuki.log" -Pattern 'record:|TEST|failed'
& ffprobe -hide_banner -loglevel error -show_entries 'stream=codec_name,width,height,nb_frames,duration' -of compact "$dir\soak.mp4"
Remove-Item "$dir\soak.mp4" -ErrorAction SilentlyContinue
