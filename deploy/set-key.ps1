# 보조 PC voicebot .env에 API 키를 넣거나 바꾼다 — 본인 PC의 PowerShell 창에서 실행
#   powershell -File deploy\set-key.ps1                      # 기본 목록을 차례로 물어봄
#   powershell -File deploy\set-key.ps1 GEMINI_API_KEY       # 이름을 골라서
# 키는 화면·대화 기록에 남지 않는다: 숨김 입력 → base64로 원격 스크립트에 실어 전달.
# 같은 이름이 있으면 그 줄을 바꾸고, 없으면 끝에 붙인다. 파일은 UTF-8(BOM 없음)로 다시 쓴다.
param(
    [string[]]$Names = @('ANTHROPIC_API_KEY', 'GROQ_API_KEY', 'GEMINI_API_KEY', 'OPENAI_API_KEY', 'DEEPGRAM_API_KEY'),
    [string]$HostName = 'aux-server',
    [string]$EnvPath = 'C:\Users\macham\voicebot\.env'
)
$ErrorActionPreference = 'Stop'

$pairs = @()
foreach ($n in $Names) {
    if ($n -notmatch '^[A-Z][A-Z0-9_]*$') { throw "이름은 대문자·숫자·_ 만: $n" }
    $s = Read-Host "$n (건너뛰려면 Enter)" -AsSecureString
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
    try { $v = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) } finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
    $v = $v.Trim()
    if (-not $v) { continue }
    if ($v -match '[\s=]' -or $v -match '[^\x21-\x7E]') { Write-Warning "$n 값에 공백·=·한글 같은 글자가 있어 건너뜀(붙여넣기 확인)"; continue }
    $pairs += "$n=$v"
}
if (-not $pairs) { Write-Host '넣을 키가 없어요.'; return }

$payload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($pairs -join "`n")))
# 원격 스크립트(키 포함)는 사용자 임시 폴더의 파일로 scp해서 실행하고 바로 지운다 — 명령줄(프로세스 목록·감사 로그)에
# 키가 남지 않게. (ssh 표준입력은 원격 PowerShell에 전달되지 않았고, 예전 UTF-16 파이프는 .env를 깨뜨렸다.)
# 기존 .env에 덮어써서 ACL 유지. 같은 이름이 여러 줄이면 첫 줄 자리에 새 값 하나만 남긴다(dotenvy는 첫 선언을 쓴다).
$remote = @"
`$ErrorActionPreference = 'Stop'
Remove-Item -LiteralPath `$PSCommandPath -Force
`$p = '$EnvPath'
`$in = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$payload'))
`$lines = New-Object System.Collections.Generic.List[string]
if (Test-Path `$p) { foreach (`$l in [IO.File]::ReadAllLines(`$p)) { if (`$l.Trim()) { `$lines.Add(`$l.Trim()) } } }
foreach (`$kv in (`$in -split "``n")) {
  `$k = (`$kv -split '=',2)[0]
  if (`$k -notmatch '^[A-Z][A-Z0-9_]*$') { throw 'bad name' }
  `$i = -1; for (`$j = `$lines.Count - 1; `$j -ge 0; `$j--) { if (`$lines[`$j].StartsWith(`$k + '=')) { if (`$i -ge 0) { `$lines.RemoveAt(`$i) }; `$i = `$j } }
  if (`$i -ge 0) { `$lines[`$i] = `$kv; "updated: `$k" } else { `$lines.Add(`$kv); "added: `$k" }
}
[IO.File]::WriteAllText(`$p, ((`$lines -join "``r``n") + "``r``n"), (New-Object Text.UTF8Encoding(`$false)))
"@
$name = "vb-setkey-$([guid]::NewGuid().ToString('N')).ps1"
$local = Join-Path $env:TEMP $name
$remotePath = "C:/Users/macham/AppData/Local/Temp/$name"
try {
    # 원격 스크립트는 ASCII만 담는다(출력 문구도 영어) — 인코딩 문제 없이 PowerShell 5.1이 읽게
    # 원격 스크립트는 시작하자마자 자기 파일부터 지운다(내용은 이미 메모리에 있음) — 실행 중 오류가 나도 키가 디스크에 남지 않게
    [IO.File]::WriteAllText($local, $remote, (New-Object Text.ASCIIEncoding))
    scp -q -o BatchMode=yes $local "${HostName}:$remotePath"
    if ($LASTEXITCODE -ne 0) { throw "복사 실패(exit $LASTEXITCODE) — .env는 바뀌지 않았어요" }
    $out = ssh -o BatchMode=yes $HostName "powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $remotePath"
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        # 실행 자체가 안 됐을 수 있다(연결 끊김) — 지우기를 몇 번 시도하고, 그래도 남으면 경로를 알려 준다
        $gone = $false
        foreach ($t in 1..3) {
            ssh -o BatchMode=yes $HostName "if (Test-Path -LiteralPath '$remotePath') { Remove-Item -LiteralPath '$remotePath' -Force }; if (Test-Path -LiteralPath '$remotePath') { exit 1 }" | Out-Null
            if ($LASTEXITCODE -eq 0) { $gone = $true; break }
            Start-Sleep 2
        }
        if (-not $gone) { Write-Warning "보조 PC에 키가 든 임시 파일이 남았을 수 있어요: $remotePath — 직접 지워 주세요." }
    }
} finally {
    Remove-Item -LiteralPath $local -Force -ErrorAction SilentlyContinue
}
if ($code -ne 0) { throw "원격 반영 실패(exit $code) — .env는 바뀌지 않았어요" }

$out | Where-Object { $_ -notmatch 'CLIXML|<Objs' }
Write-Host '적용하려면 웹 콘솔 → 조작 → 재시작(또는 Claude에게 "넣었어").'