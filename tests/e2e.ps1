# End-to-end check of a release build on a real desktop. Drives Clipper with its global
# shortcuts and inspects the files it writes. Needs ffmpeg/ffprobe on PATH for the analysis only
# (Clipper itself does not use them). Run from the repository root:
#   cargo build --release; pwsh tests/e2e.ps1
# It plays a test video full screen and beeps, so do not touch the PC while it runs (~3 min).

param([string]$Exe = "target\release\clipper.exe")
$ErrorActionPreference = 'Stop'
$work = Join-Path $env:TEMP "clipper-e2e"
New-Item -ItemType Directory -Force $work | Out-Null
$failures = 0
function Check($name, [bool]$ok, $detail) {
    $mark = if ($ok) { 'PASS' } else { 'FAIL'; $script:failures++ }
    "{0}  {1}  {2}" -f $mark, $name, $detail
}

Add-Type -Namespace E2E -Name Win -MemberDefinition @'
[DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, System.UIntPtr extra);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern System.IntPtr FindWindowW(string cls, System.IntPtr title);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(System.IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool PostMessageW(System.IntPtr h, uint msg, System.UIntPtr w, System.IntPtr l);
public delegate bool EnumProc(System.IntPtr h, System.IntPtr l);
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, System.IntPtr l);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(System.IntPtr h);
[DllImport("user32.dll")] public static extern bool SetWindowPos(System.IntPtr h, System.IntPtr after, int x, int y, int w, int hh, uint flags);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
'@
function Keys([byte[]]$k) {
    foreach ($x in $k) { [E2E.Win]::keybd_event($x, 0, 0, [UIntPtr]::Zero) }
    [array]::Reverse($k); foreach ($x in $k) { [E2E.Win]::keybd_event($x, 0, 2, [UIntPtr]::Zero) }
}
function Status {
    $h = [E2E.Win]::FindWindowW("ClipperTray", [IntPtr]::Zero)
    if ($h -eq [IntPtr]::Zero) { return "" }
    $sb = New-Object System.Text.StringBuilder 512; [void][E2E.Win]::GetWindowTextW($h, $sb, 512); $sb.ToString()
}
function Reload { [void][E2E.Win]::PostMessageW([E2E.Win]::FindWindowW("ClipperTray", [IntPtr]::Zero), 0x8002, [UIntPtr]::Zero, [IntPtr]::Zero) }
$ALT = 0x12; $F9 = 0x78; $CLIP = 0x47; $SHOT = 0x70   # Alt+G clip, Alt+F1 screenshot, Alt+F9 pause

# Known settings for the run; the user's file is restored at the end.
$cfgDir = Join-Path $env:APPDATA "Clipper"; $cfg = Join-Path $cfgDir "config.ini"
New-Item -ItemType Directory -Force $cfgDir | Out-Null
$backup = if (Test-Path $cfg) { Get-Content $cfg -Raw } else { $null }
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$runBackup = (Get-ItemProperty $runKey -ErrorAction SilentlyContinue).Clipper
$clips = Join-Path $work "clips"; $shots = Join-Path $work "shots"
Remove-Item $clips, $shots -Recurse -Force -ErrorAction SilentlyContinue
function WriteConfig($secs, $fps, $follow = 'true', $monitor = '') {
    @"
follow_focus = $follow
monitor = $monitor
clip_seconds = $secs
fps = $fps
quality = medium
clips_folder = $clips
screenshots_folder = $shots
start_with_windows = false
notifications = false
"@ | Set-Content $cfg
}
# The memory check needs the main screen: a monitor on a laptop's dedicated GPU costs more.
WriteConfig 30 60 'false' ''
function Latest($dir, $filter) { Get-ChildItem $dir -Filter $filter -ErrorAction SilentlyContinue | Sort-Object LastWriteTime | Select-Object -Last 1 }
function Probe($file) { ffprobe -v error -show_entries "format=duration,bit_rate:stream=codec_type,avg_frame_rate,nb_frames" -of json $file | ConvertFrom-Json }

Get-Process clipper -ErrorAction SilentlyContinue | Stop-Process -Force; Start-Sleep 1
$p = Start-Process $Exe -PassThru
try {
    Start-Sleep 4
    Check "tray is running" ((Status) -match 'state=recording') (Status)

    # 1. Static screen with a tone: the clip must still be full length with audio.
    $tone = Join-Path $work "tone.wav"
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "sine=frequency=1000:duration=5" -ar 48000 -ac 2 $tone
    Start-Sleep 18
    $ram = (Get-Counter "\Process(clipper)\Working Set - Private").CounterSamples[0].CookedValue / 1MB
    Check "RAM while recording the main screen (Task Manager)" ($ram -lt 60) ("{0:N0} MB" -f $ram)
    (New-Object System.Media.SoundPlayer $tone).PlaySync(); Start-Sleep 12
    Keys @($ALT, $CLIP); Start-Sleep 5
    $c = Latest $clips "*.mp4"
    $info = if ($c) { Probe $c.FullName } else { $null }
    $dur = if ($info) { [double]$info.format.duration } else { 0 }
    Check "static clip length" ($dur -gt 29 -and $dur -lt 32) ("{0:N1} s" -f $dur)
    $loud = if ($c) { ffmpeg -hide_banner -i $c.FullName -map 0:a -af volumedetect -f null - 2>&1 | Select-String 'max_volume: (-?[\d\.]+)' | ForEach-Object { [double]$_.Matches[0].Groups[1].Value } } else { -99 }
    Check "static clip has the tone" ($loud -gt -40) "max $loud dB"

    # 2. Motion: frame rate, bitrate and lip sync against a flash/beep test video.
    WriteConfig 30 60; Reload; Start-Sleep 2
    $sync = Join-Path $work "synctest.mp4"
    if (-not (Test-Path $sync)) {
        ffmpeg -hide_banner -loglevel error -y -f lavfi -i "testsrc2=s=1920x1080:r=60,drawbox=c=white:t=fill:enable='lt(mod(t,1),0.1)'" -f lavfi -i "aevalsrc='if(lt(mod(t,1),0.1),0.5*sin(2*PI*1000*t),0)':s=48000:c=stereo" -t 45 -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -c:a aac $sync
    }
    $playerSince = Get-Date
    Start-Process $sync; Start-Sleep 4
    # Windows opens the player wherever it likes; put it on the main screen, then full screen.
    $player = [IntPtr]::Zero
    $find = [E2E.Win+EnumProc]{ param($h, $l) $sb = New-Object System.Text.StringBuilder 256; [void][E2E.Win]::GetWindowTextW($h, $sb, 256); if ([E2E.Win]::IsWindowVisible($h) -and $sb.ToString() -match 'synctest|Media Player') { $script:player = $h; return $false }; $true }
    [void][E2E.Win]::EnumWindows($find, [IntPtr]::Zero)
    if ($player -ne [IntPtr]::Zero) { [void][E2E.Win]::SetWindowPos($player, [IntPtr]::Zero, 100, 100, 1280, 720, 0x0044); [void][E2E.Win]::SetForegroundWindow($player); Start-Sleep 1 }
    Keys @(0x7A); Start-Sleep 14
    $p.Refresh(); $c1 = $p.TotalProcessorTime.TotalSeconds; $t1 = Get-Date; Start-Sleep 18; $p.Refresh()
    $cpu = ($p.TotalProcessorTime.TotalSeconds - $c1) / ((Get-Date) - $t1).TotalSeconds * 100 / [Environment]::ProcessorCount
    $ramMotion = (Get-Counter "\Process(clipper)\Working Set - Private").CounterSamples[0].CookedValue / 1MB
    Check "CPU while recording motion" ($cpu -lt 5) ("{0:N1} %" -f $cpu)
    $gpuNow = (Status) -replace '.*gpu=([^;]*).*','$1'
    "INFO  RAM while recording motion: {0:N0} MB on {1}" -f $ramMotion, $gpuNow
    Keys @($ALT, $CLIP); Start-Sleep 1; Keys @($ALT, $SHOT); Start-Sleep 6
    # Close only the player this test opened.
    Get-Process | Where-Object { ($_.ProcessName -eq 'Microsoft.Media.Player' -or $_.MainWindowTitle -match 'synctest') -and $_.StartTime -ge $playerSince } | Stop-Process -Force -ErrorAction SilentlyContinue
    $c = Latest $clips "*.mp4"; $info = Probe $c.FullName
    $v = $info.streams | Where-Object codec_type -eq video
    $fps = [double]$v.nb_frames / [double]$info.format.duration
    Check "motion clip frame rate" ($fps -gt 55) ("{0:N1} fps" -f $fps)
    Check "motion clip bitrate" ([double]$info.format.bit_rate -lt 20e6) ("{0:N1} Mbps" -f ([double]$info.format.bit_rate / 1e6))
    $luma = ffprobe -v error -f lavfi -i "movie='$($c.FullName -replace '\\','/' -replace ':','\:')',signalstats" -show_entries frame=pts_time:frame_tags=lavfi.signalstats.YAVG -of csv=p=0 2>$null
    $rows = $luma | ForEach-Object { $a = $_.Split(','); [pscustomobject]@{ t = [double]$a[0]; y = [double]$a[1] } }
    $flashes = for ($i = 1; $i -lt $rows.Count; $i++) { if ($rows[$i].y - $rows[$i - 1].y -gt 25) { $rows[$i].t } }
    $beeps = ffmpeg -hide_banner -i $c.FullName -map 0:a -af silencedetect=n=-35dB:d=0.05 -f null - 2>&1 | Select-String 'silence_end: ([\d\.]+)' | ForEach-Object { [double]$_.Matches[0].Groups[1].Value }
    $offsets = foreach ($f in $flashes) { $n = $beeps | Sort-Object { [math]::Abs($_ - $f) } | Select-Object -First 1; if ($null -ne $n -and [math]::Abs($n - $f) -lt 0.5) { ($n - $f) * 1000 } }
    $median = if ($offsets) { ($offsets | Sort-Object)[[int]($offsets.Count / 2)] } else { 9999 }
    Check "audio/video sync" ([math]::Abs($median) -lt 45 -and $offsets.Count -gt 20) ("median {0:N0} ms over {1} flashes" -f $median, $offsets.Count)
    $png = Latest $shots "*.png"
    $dims = if ($png) { ffprobe -v error -show_entries stream=width,height -of csv=p=0 $png.FullName } else { "none" }
    Check "screenshot" ($null -ne $png) $dims

    # 3. Pause and resume with the shortcut.
    Keys @($ALT, $F9); Start-Sleep 2
    Check "pause shortcut" ((Status) -match 'state=paused') (Status)
    Keys @($ALT, $F9); Start-Sleep 2
    Check "resume shortcut" ((Status) -match 'state=recording') (Status)

    # 4. If the settings window dies while capturing a key, the shortcuts come back on their own.
    $tray = [E2E.Win]::FindWindowW("ClipperTray", [IntPtr]::Zero)
    [void][E2E.Win]::PostMessageW($tray, 0x8003, [UIntPtr]::Zero, [IntPtr]::Zero)   # WM_KEYS_OFF
    $before = (Get-ChildItem $clips -Filter *.mp4).Count
    Start-Sleep 17; Keys @($ALT, $CLIP); Start-Sleep 5
    Check "shortcuts recover after 15 s" ((Get-ChildItem $clips -Filter *.mp4).Count -eq $before + 1) "$before -> $((Get-ChildItem $clips -Filter *.mp4).Count) clips"

    # 5. Live settings change: 15 s clips at 30 fps.
    WriteConfig 15 30; Reload; Start-Sleep 22
    Keys @($ALT, $CLIP); Start-Sleep 5
    $c = Latest $clips "*.mp4"; $info = Probe $c.FullName
    $dur = [double]$info.format.duration
    $rate = ($info.streams | Where-Object codec_type -eq video).avg_frame_rate
    Check "reloaded clip length" ($dur -gt 14 -and $dur -lt 17) ("{0:N1} s" -f $dur)
    Check "reloaded frame rate cap" ([double]($rate.Split('/')[0]) / [double]($rate.Split('/')[1]) -le 31) $rate
}
finally {
    Get-Process clipper -ErrorAction SilentlyContinue | Stop-Process -Force
    if ($null -ne $backup) { Set-Content $cfg $backup -NoNewline } else { Remove-Item $cfg -ErrorAction SilentlyContinue }
    if ($runBackup) { Set-ItemProperty $runKey -Name Clipper -Value $runBackup } else { Remove-ItemProperty $runKey -Name Clipper -ErrorAction SilentlyContinue }
}
""
if ($failures) { "$failures check(s) failed. Log: $env:APPDATA\Clipper\clipper.log"; exit 1 } else { "All checks passed." }
