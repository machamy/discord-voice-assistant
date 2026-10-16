# 보조 PC에서 실행: 아짱·클짱 봇을 (다시) 띄운다. SSH 세션이 끝나도 살아 있게 Win32_Process로 만든다.
# 봇이 사이드카(Python)를 직접 띄우고 감시한다(SPEC N-09). 웹 콘솔 "재시작"도 이 스크립트를 부른다.
param([string]$Dir = "C:\Users\macham\voicebot")
$ErrorActionPreference = 'Continue'
# 명령 문자열에 들어가는 값 — 따옴표·셸 메타문자가 있으면 거부
if ($Dir -match '["&|<>^%]') { throw "Dir에 쓸 수 없는 문자가 있습니다: $Dir" }
if (-not (Test-Path (Join-Path $Dir 'voicebot.exe'))) { throw "voicebot.exe가 없습니다: $Dir" }
Set-Location $Dir

# 1) 이 폴더의 봇만 끈다(다른 배포본·같은 이름 프로세스는 건드리지 않는다)
$exe = Join-Path $Dir 'voicebot.exe'
Get-Process voicebot -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe } |
    Stop-Process -Force -ErrorAction SilentlyContinue

# 2) 이 폴더의 사이드카도 새 코드로 — 남아 있으면 새 봇이 옛 사이드카를 물려받는다.
#    대상: 이 폴더 venv(sidecar\.venv, melo-venv, rvc-venv)의 python. venv python은 진짜 python을 자식으로
#    띄우므로 트리째(/T) 끈다. 부모가 먼저 죽어 고아가 된 자식은 "부모 없음 + app.py/melo_worker.py"로 찾는다
# 끝에 \ 를 붙여 경로 경계까지 비교(.venv-old, melo-venv-backup 같은 다른 폴더 제외)
$venvs = @(((Join-Path $Dir 'sidecar\.venv') + '\'), ((Join-Path $Dir 'melo-venv') + '\'), ((Join-Path $Dir 'rvc-venv') + '\'))
$procs = @(Get-CimInstance Win32_Process -Filter "Name='python.exe'")
$alive = @{}; Get-Process | ForEach-Object { $alive[[int]$_.Id] = $true }
$procs | Where-Object {
    $p = $_.ExecutablePath
    ($p -and ($venvs | Where-Object { $p.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) })) -or
    ((-not $alive[[int]$_.ParentProcessId]) -and $_.CommandLine -and
        $_.CommandLine.IndexOf($Dir, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and
        ($_.CommandLine -like "*sidecar?app.py*" -or $_.CommandLine -like "*sidecar?melo_worker.py*" -or $_.CommandLine -like "*sidecar?rvc_worker.py*"))
} | ForEach-Object { taskkill /PID $_.ProcessId /T /F 2>&1 | Out-Null }

New-Item -ItemType Directory -Force data | Out-Null
$log = Join-Path $Dir 'data\voicebot.log'

# 3) 로그 파일이 풀릴 때까지 최대 10초(옛 프로세스가 핸들을 쥐고 있으면 cmd가 >> 를 못 연다)
for ($i = 0; $i -lt 20; $i++) {
    try { $fs = [IO.File]::Open($log, 'Append', 'Write', 'ReadWrite'); $fs.Close(); break } catch { Start-Sleep -Milliseconds 500 }
}
if ((Test-Path $log) -and (Get-Item $log).Length -gt 5MB) { Move-Item $log "$log.1" -Force }

# 4) 띄우기
$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
    # cmd /c ""exe" >> "log" 2>&1" — 경로에 공백이 있어도 되게 각각 인용
    CommandLine = "cmd.exe /c `"`"$exe`" >> `"$log`" 2>&1`""
    CurrentDirectory = $Dir
}
Start-Sleep -Seconds 3
$p = Get-Process voicebot -ErrorAction SilentlyContinue
"voicebot start rc=$($r.ReturnValue) running=$([bool]$p) pid=$($p.Id)"
