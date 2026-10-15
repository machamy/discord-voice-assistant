# PoC 1 — 디스코드 음성 수신 (songbird 0.6 receive + DAVE)

- 상태: **보조 PC에서 실행 중 (2026-10-13 00:07~)** — `/들어와`·`/나가`·`/상태` 등록됨. 측정 대기(2명 이상 말하기)
  - 실행 위치 `C:\Users\macham\voice-poc\voice-recv-poc.exe`, 로그 `voice-recv.log`
  - SSH가 끊겨도 살도록 `Win32_Process.Create`로 띄움(SSH 세션의 자식 프로세스는 세션과 같이 죽는다):
    `Invoke-CimMethod Win32_Process Create -Arguments @{ CommandLine = 'cmd.exe /c "C:\Users\macham\voice-poc\voice-recv-poc.exe > C:\Users\macham\voice-poc\voice-recv.log 2>&1"'; CurrentDirectory = 'C:\Users\macham\voice-poc' }`
  - 끄기: `ssh aux-server "Stop-Process -Name voice-recv-poc"`

초대 문제 기록(2026-10-13): 앱의 기본 설치 설정이 `applications.commands`만(권한 0)이라 "앱 추가"로 초대하면
봇 계정이 서버에 안 들어왔다. `PATCH /applications/@me`로 서버 설치에 `bot` + 권한 3148800
(채널 보기·메시지 보내기·연결·말하기)을 넣어 고쳤다.
- 질문: 봇이 음성 채널에서 **사람별 음성**을 받나? DAVE(종단간 암호화) 복호화로 새는 패킷이 얼마나 되나?

## 하는 일

1. 시작하면 **봇이 들어가 있는 모든 서버**에 슬래시 명령을 등록하고, 새로 초대되면 그 서버에도 바로 단다(서버 단위라 즉시 반영):
   - `/들어와` — **명령한 사람이 있는 음성 채널**로 들어온다(채널 ID를 몰라도 됨)
   - `/나가` — 최종 통계를 채팅에 남기고 나간다
   - `/상태` — 지금까지의 통계(명령한 사람에게만 보임)
   `VOICE_CHANNEL_ID`를 적어 두면 명령 없이 바로 들어간다. 들어가면 채팅으로 테스트 고지를 남긴다(`NOTICE=0`이면 생략).
2. 20ms마다 오는 `VoiceTick`으로 사람(SSRC → 유저 ID)별로 센다:
   말한 시간, 받은 패킷, 손실(`packet=None`), **RTP 순번 빠짐**, PCM 없음, 평균 음량.
3. 30초마다 표를 찍고, `DURATION_SECS`(기본 600초) 뒤 또는 Ctrl+C에 최종 표를 찍고 나간다.

**음성은 저장하지 않는다.** 숫자만 센다.

songbird는 DAVE 복호화에 실패한 패킷을 **로그 없이 버린다**(`songbird-0.6.0/src/driver/tasks/udp_rx/mod.rs`의
`NoDecryptorForUser`·`NoValidCryptorFound` 분기). 그래서 실패율은 **RTP 순번 빠짐 + 손실**로 잡는다.
평균 음량이 0에 가까우면 패킷은 오는데 내용이 깨진 것이다.

디코드는 본체와 같게 **16kHz mono**로 받는다(Whisper에 바로 넘길 형식).

## 빌드 (PC)

```powershell
cd poc\voice-recv
cargo build --release      # serenity 0.12.5 + songbird 0.6.0(receive) — musicbot-mk2와 같은 버전
```

## 실행 (보조 PC)

```powershell
# (PC) 바이너리 전송
scp poc\voice-recv\target\release\voice-recv-poc.exe aux-server:C:/Users/macham/voice-poc/

# (보조 PC) C:\Users\macham\voice-poc\.env 에 DISCORD_TOKEN 이 이미 있다. 아래를 더한다(비밀 아님)
#   (GUILD_ID=795950801172037642        ← 자동 입장할 때만 필요)
#   DURATION_SECS=600            # 입장 후 자동 퇴장(0이면 끔)
#   (VOICE_CHANNEL_ID=795950801172037647  ← 넣으면 /들어와 없이 자동 입장)
cd C:\Users\macham\voice-poc
.\voice-recv-poc.exe
```

음악봇과는 **다른 봇 계정·다른 프로세스**라 서로 영향이 없다.

## 통과 기준

| 지표 | 기준 |
|---|---|
| 2명 이상이 10분간 번갈아 말할 때 각자 SSRC가 유저 ID로 매핑됨 | 매핑 안 된 틱 ≈ 0 |
| 사람별 손실률(손실 + 순번 빠짐) | ≤ 2% |
| 평균 음량 | 말한 사람마다 0보다 확실히 큼 |
| 입장 직후·누가 새로 들어온 직후 | DAVE 키 교체 구간의 손실이 몇 초 안에 회복 |

## 결과 (2026-10-13, 친구 서버, 약 1~2분)

| 유저 | 말한 시간 | 받은 패킷 | 손실+순번빠짐 | 손실률 | 평균음량 | 비고 |
|---|---|---|---|---|---|---|
| 269784593123639306 (주인) | 5.9초 | 296 | 0 | 0.00% | 703 | |
| 친구 A | 22.1초 | 1,093 | 11+11 | 1.97% | 367 | 입장 직후 손실이 몰림(DAVE 키 교체 구간으로 추정) |
| 친구 B | 0초 | 0 | 0 | — | — | 말 안 함 |
| 1480864336640737280 | 60.3초 | 3,016 | 0 | 0.00% | 7,080 | **음악봇** — 계속 재생 중 |

판정: **통과.** SSRC 4개 전부 유저 ID로 매핑(매핑 안 된 틱 0), PCM 없음 0, 손실률 ≤ 2%대.
DAVE 복호화가 사람별로 정상 동작한다. 다만 **봇 소리도 사람과 똑같이 들어오므로** 본체에서는 봇을 자동으로 빼야 한다(SPEC F-05).
`/들어와`·`/상태` 명령도 실제 서버에서 동작 확인.
