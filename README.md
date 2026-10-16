# discord-voice-assistant

<p align="center">
  <img src="assets/avatar/keuljjang-v2.png" width="120" alt="클짱">
  <img src="assets/avatar/ajjang-v3.png" width="120" alt="아짱">
</p>

<p align="center">
  디스코드 음성 채널에서 대화를 듣고 있다가, 이름을 부르면 <b>그때까지의 대화 맥락으로 음성 대답</b>하는 한국어 AI 캐릭터 봇
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-2024-orange?logo=rust">
  <img alt="Python" src="https://img.shields.io/badge/Python-3.11%2B-blue?logo=python">
  <img alt="Discord DAVE" src="https://img.shields.io/badge/Discord-DAVE%20E2EE-5865F2?logo=discord&logoColor=white">
  <img alt="Platform" src="https://img.shields.io/badge/platform-Windows-lightgrey">
</p>

```
친구: 클짱 오늘 저녁 뭐 먹지?
클짱: 아까 다들 매운 거 땡긴다며? 마라탕 어때. (음성 + 채팅에 긴 답)
```

> [!NOTE]
> 개인 프로젝트를 공개한 저장소입니다. 작성자가 실제로 친구 서버에서 돌리고 있고, 문서·주석은 한국어입니다.
> 디스코드는 음성 **수신**을 공식 지원하지 않습니다 — 디스코드 쪽 변경으로 언제든 깨질 수 있습니다.

## 목차

- [기능](#기능)
- [동작 방식](#동작-방식)
- [빠른 시작](#빠른-시작)
- [설정](#설정)
- [디스코드 명령](#디스코드-명령)
- [웹 콘솔](#웹-콘솔)
- [공급자](#공급자)
- [개인정보와 녹음 고지](#개인정보와-녹음-고지)
- [저장소 구조](#저장소-구조)
- [개발](#개발)
- [알려진 제약](#알려진-제약)
- [기여](#기여)
- [라이선스](#라이선스)
- [감사의 말](#감사의-말)

## 기능

- **호출어로 부르는 캐릭터** — 기본 캐릭터 셋: 클짱(Claude), 아짱(OpenAI), 테토(RVC 목소리). 캐릭터마다 두뇌 모델·말투·목소리가 따로다.
  웹 콘솔에서 캐릭터를 더 만들 수 있다.
- **대화 맥락으로 대답** — 사람별로 받아쓴 최근 대화를 기억했다가, 부른 순간의 대화를 넣어 짧게 답한다. 음성으로 짧게, 텍스트 채널엔 부른 사람을 멘션해 길게.
- **부름과 언급 구분** — "클짱 이거 봐"는 대답하고 "클짱도 아니고"는 무시한다. 받아쓰기가 이름을 틀리게 적는 경우("클장"·"크짱")는 변형 사전과 자모 거리로 잡고,
  애매하면 작은 **판단 모델**에 "정말 부른 말인가"를 확률로 묻는다.
- **사람별 무시** — 디스코드가 사람마다 음성 스트림을 따로 주기 때문에 화자 식별 모델 없이 유저·역할 단위로 무시한다. 누구나 `/무시해줘`로 스스로 빠질 수 있다.
- **최대한 무료** — 받아쓰기는 로컬 GPU(faster-whisper), 목소리는 edge-tts가 기본이다. 두뇌는 최저가 모델 + 하루 호출·금액 상한 + 무료 모델(Groq) 대체.
- **공급자 교체** — 받아쓰기·두뇌·목소리를 각각 체인으로 두고, 실패하면 다음 공급자로 넘어간다. 설정 파일이나 웹 콘솔에서 바꾼다.
- **웹 콘솔** — 캐릭터·호출어·공급자·판단 모델·한도를 바꾸고, 받아쓰기 상황·호출별 지연·두뇌 입출력 원문·비용을 본다.
- **장기 기억** — "클짱, 나 민트초코 싫어하는 거 기억해"처럼 분명히 부탁한 것만 서버별로 남긴다.

## 동작 방식

```
디스코드 음성 채널
   │ 사람별 Opus 스트림 (DAVE 복호화: songbird 0.6 receive)
   ▼
[수신] 무시 목록·다른 봇 제외 → Opus 디코드 → 사람별 침묵 감지로 발화 자르기
   ▼
[받아쓰기] 사이드카 faster-whisper (GPU → CPU → 클라우드 체인)
   ▼
[대화 버퍼] 메모리에만, 최근 15분 ── 호출어 찾기 ── 애매하면 판단 모델
   ▼
[대답] 캐릭터별 두뇌 (Claude / OpenAI / Groq / OpenAI 호환) → 문장 단위 스트리밍
   ▼
[목소리] 사이드카 TTS 체인 (edge-tts → Azure → MeloTTS / RVC …) → 같은 채널에 재생 + 텍스트 채널에 사본
```

- **본체**(`bot/`, Rust): serenity + songbird 0.6(`receive`). 수신·발화 자르기·호출어·두뇌 호출·재생 큐·슬래시 명령·웹 콘솔.
- **사이드카**(`sidecar/`, Python): FastAPI. 받아쓰기·목소리 공급자. 봇이 띄우고 감시하며 localhost에서만 받는다.

말이 끝난 뒤 첫 소리까지 약 2.9초(받아쓰기 0.9초 + 두뇌 첫 문장 1.3초 + TTS 0.65초, GTX 1060 기준 측정·추정).

## 빠른 시작

### 필요한 것

| | 내용 |
|---|---|
| OS | Windows 10/11 (개발·운영 환경. 다른 OS는 확인하지 않았다) |
| GPU | NVIDIA, CUDA 12 지원 (GTX 1060 6GB에서 확인). 없으면 CPU 받아쓰기로 돌지만 발화당 약 2.5초로 느리다 |
| Rust | stable (edition 2024) |
| Python | 3.11 이상 (사이드카). MeloTTS·RVC를 쓰려면 별도 3.11 venv |
| 디스코드 봇 | [개발자 포털](https://discord.com/developers/applications)에서 만든 앱의 봇 토큰 |
| 두뇌 API 키 | 기본 설정은 클짱·테토 = Anthropic, 아짱 = OpenAI, 대체 = [Groq](https://console.groq.com)(무료). 키가 없는 두뇌는 건너뛰고 Groq로 대답하므로 **`GROQ_API_KEY` 하나면 일단 전부 동작**한다. 판단 모델 기본값은 OpenRouter |

### 1. 디스코드 앱 만들기

1. 개발자 포털 → New Application → **Bot** → 토큰 발급.
   권한이 필요한 게이트웨이 인텐트(Message Content 등)는 쓰지 않는다(`GUILDS`·`GUILD_VOICE_STATES`만).
2. **OAuth2 → URL Generator**에서 scope `bot`·`applications.commands`, 권한 `View Channels`·`Connect`·`Speak`·`Send Messages`를 골라 서버에 초대한다.

### 2. 빌드

```powershell
git clone https://github.com/machamy/discord-voice-assistant
cd discord-voice-assistant\bot
cargo build --release
```

### 3. 실행 폴더 만들기

저장소 밖에 실행 폴더를 하나 만들고 필요한 파일을 복사한다. 봇은 **현재 폴더**에서 `config.toml`·`.env`·`sidecar\`를 찾고, `data\`(설정·로그)는 알아서 만든다. 웹 콘솔은 바이너리에 들어 있다.

```
voicebot\
├─ voicebot.exe          ← bot\target\release\voicebot.exe
├─ config.toml           ← bot\config.example.toml 복사 후 수정
├─ .env                  ← 토큰·키 (아래)
└─ sidecar\              ← 저장소 sidecar\ 통째로 (app.py, *_worker.py, requirements.txt)
   └─ .venv\             ← 사이드카 Python 환경
```

저장소 루트(`discord-voice-assistant\`)에서:

```powershell
cd ..                                   # bot\ 에서 저장소 루트로
$run = "C:\voicebot"                   # 원하는 실행 폴더
New-Item -ItemType Directory -Force $run\sidecar | Out-Null
Copy-Item bot\target\release\voicebot.exe, bot\config.example.toml $run
Copy-Item sidecar\* $run\sidecar
Rename-Item $run\config.example.toml config.toml
New-Item -ItemType File $run\.env | Out-Null

cd $run
py -3 -m venv sidecar\.venv
sidecar\.venv\Scripts\python -m pip install -r sidecar\requirements.txt
```

`config.toml`에서 최소한 이것만 바꾼다.

```toml
owner_id = 123456789012345678   # 내 디스코드 유저 ID (주인 전용 명령·비싼 모델 선택)

[web]
public_url = "http://localhost:8694"   # 외부에 공개할 주소가 있으면 그걸로
```

`.env`(커밋 금지):

```dotenv
DISCORD_TOKEN=...
CONSOLE_PASSWORD=...        # 웹 콘솔 비밀번호
GROQ_API_KEY=...            # 무료 대체 두뇌 — 이것만 있어도 대답한다
ANTHROPIC_API_KEY=...       # 그 밖에 쓰는 공급자 키만 (OPENAI_API_KEY, OPENROUTER_API_KEY …)
```

### 4. 실행

```powershell
.\voicebot.exe
```

사이드카는 봇이 자동으로 띄운다(`[sidecar] autostart`). 첫 실행은 Whisper 모델을 받느라 몇 분 걸린다.
음성 채널에 들어가 `/들어와` → "클짱, 안녕?"

웹 콘솔은 `http://localhost:8694`(비밀번호 = `CONSOLE_PASSWORD`).

## 설정

설정은 두 군데다.

- **`config.toml`** — 비밀이 아닌 기본값. 모든 항목에 주석이 있다: [`bot/config.example.toml`](bot/config.example.toml).
  바꾼 뒤 `/설정다시읽기` 또는 웹 콘솔 "설정 다시 읽기"(형식이 틀리면 기존 설정 유지).
- **`data/state.json`** — 명령·웹 콘솔로 바꾼 값(서버별 설정·무시 목록·사용량). 대화 내용은 들어가지 않는다.

| 섹션 | 내용 |
|---|---|
| `[listen]` | 기억 시간, 주인이 없으면 안 듣기, 헤이 필수, 침묵 감지·발화 길이 |
| `[stt]` / `[tts]` | 받아쓰기·목소리 공급자 체인 |
| `[limits]` | 하루 유료 호출 수·금액 상한 (넘으면 무료 모델로 대체) |
| `[judge]` | 판단 모델 — 애매한 호출을 확률로 판정, 부름 기준 확률 |
| `[llm.presets.*]` | 두뇌 프리셋 (공급자·모델·가격·키 환경변수) |
| `[characters.*]` | 캐릭터 — 이모지·호출어·두뇌·대체 두뇌·페르소나·목소리 |
| `[web]` | 웹 콘솔 주소, 디스코드 로그인 허용 목록 |
| `[notice]` | 입장 고지 문구 |
| `[sidecar]` | 사이드카 자동 실행, Python·스크립트 경로, 포트, 미리 올릴 받아쓰기 모델 |

### 환경변수 (`.env`)

| 이름 | 쓰는 곳 |
|---|---|
| `DISCORD_TOKEN` | 필수 |
| `CONSOLE_PASSWORD` | 웹 콘솔 비밀번호 로그인 |
| `DISCORD_CLIENT_ID` · `DISCORD_CLIENT_SECRET` | 웹 콘솔 디스코드 로그인 (있으면 비밀번호 로그인 대신) |
| `ANTHROPIC_API_KEY` · `OPENAI_API_KEY` · `GROQ_API_KEY` · `GEMINI_API_KEY` · `OPENROUTER_API_KEY` | 두뇌·판단 모델 |
| `AZURE_SPEECH_KEY` · `AZURE_SPEECH_REGION` · `TYPECAST_API_KEY` · `ELEVENLABS_API_KEY` | 목소리 (선택) |
| `DEEPGRAM_API_KEY` | 받아쓰기 (선택) |
| `VOICEBOT_CONFIG` | `config.toml` 대신 다른 경로 |

## 디스코드 명령

| 명령 | 누가 | 하는 일 |
|---|---|---|
| `/도움말` | 누구나 | 쓰는 법과 명령 목록(나만 보기) |
| `/들어와` · `/나가` | 누구나 | 음성 채널 입장·퇴장 |
| `/무시해줘` · `/다시들어줘` | 본인 | 내 목소리 안 듣기(기억도 지움)·다시 듣기 |
| `/잊어줘` | 누구나 | 기억한 대화 전부 지우기 |
| `/기억 보기·지우기·전부지우기` | 보기는 누구나, 지우기는 본인 것(관리자는 전부) | 장기 기억 |
| `/상태` | 누구나 | 듣는 채널·사람·사이드카·응답 지연 |
| `/모델 캐릭터 프리셋` | 누구나(비싼 건 주인) | 캐릭터 두뇌 바꾸기 |
| `/설정 보기` | 누구나 | 지금 설정을 카드 한 장으로 |
| `/설정 …` | 주인·서버 관리 (호출어·한도는 주인) | 두뇌·대체두뇌·목소리·말투선택·말투·채널·주인없으면·호출방식·음성대답·음량·인사말·기억시간·호출어·한도·초기화 |
| `/채널지정` | 주인·서버 관리 | 긴 답을 올릴 텍스트 채널 |
| `/무시목록 …` | 주인·서버 관리 | 유저·역할 무시 목록 |
| `/공급자` · `/설정다시읽기` | 주인 | 공급자·사용량 보기 / 설정 다시 읽기 |

부르는 법: "클짱 …", "헤이 클짱 …", "…하지, 클짱?". 이름만 부르고 멈추면 다음 말을 질문으로 듣는다.
음성 채널 이모지 반응(캐릭터 이모지 — 🧡 클짱·🩵 아짱·🥖 테토)으로도 부를 수 있다.

## 웹 콘솔

| 탭 | 할 수 있는 것 |
|---|---|
| 대화 | 받아쓰기 상황(발화별 결과·공급자·지연), 호출별 지연, 두뇌 입출력 원문, 장기 기억 |
| 캐릭터 | 호출어·위험 별칭, 두뇌, 말투(성격/존댓말), 목소리 미리 듣기, 캐릭터 추가 |
| 이 서버 | 부르는 방식, 음성 대답, 음량, 인사말 |
| 공급자 | 받아쓰기·목소리 체인, 두뇌 프리셋 켜기·시험, 판단 모델(모델·기준 확률·요청 원문 보기) |
| 사용량 | 기간별 비용 요약(모델별·캐릭터별), 날짜별 호출 |

외부에 공개할 때는 리버스 프록시·접근 제어 뒤에 두고, `.env`에 `DISCORD_CLIENT_SECRET`을 넣어 디스코드 로그인(주인·`allowed_users`만)을 켜는 걸 권장한다.

## 공급자

| 종류 | 공급자 (`config.toml` 이름) | 비용 |
|---|---|---|
| 받아쓰기 | `local-whisper:<cuda\|cpu>:<모델>` (faster-whisper, 기본 `local-whisper:cuda:large-v3-turbo` → `local-whisper:cpu:small`) | 무료 |
| | `groq-whisper`, `groq-whisper-turbo` (Groq 키) | 무료 등급 |
| | `deepgram`, `openai-transcribe` | 유료 |
| 두뇌 | `anthropic` (Claude), `openai`, OpenAI 호환 (Groq·Gemini·Ollama 등) | 모델별 |
| 판단 모델 | `decisions` ([OpenRouter Decisions](https://openrouter.ai), 기본 `jev`) 또는 OpenAI 호환 YES/NO | 회당 약 $0.00003 |
| 목소리 | `edge-tts` | 무료(비공식) |
| | `azure-tts` (월 50만 자 무료), `melotts` (로컬), `rvc` (로컬, edge-tts + RVC 변환) | 무료 |
| | `typecast`, `elevenlabs`, `openai-tts` | 유료 |

RVC 목소리 설치(별도 venv·Applio 추론 코드·모델 파일)는 [`docs/operations.md`](docs/operations.md#배포운영)의 "RVC 목소리" 항목을 본다.
**RVC 모델 파일(`.pth`·`.index`)은 이 저장소에 없고 앞으로도 올리지 않는다.** 테토 음원은 비상업·개인 범위 규약이라 학습 결과물을 배포할 수 없다. 모델은 각자 준비한다.

## 개인정보와 녹음 고지

남의 대화를 듣는 봇이라 다음을 지킨다. 실제로 쓸 때는 **같은 채널 사람들에게 미리 알리고 동의를 받는다.**

- 입장할 때 채팅 고지와 음성 인사로 "듣고 있다"를 알리고, 상태 메시지로 계속 표시한다.
- **원본 음성은 저장하지 않는다.** 받아쓴 글은 메모리에 최근 15분만 두고 버린다.
- 부를 때만 최근 대화 몇 분이 두뇌 공급자(Anthropic·Groq 등)로 간다.
- 누구나 `/무시해줘`로 빠질 수 있고, 그 사람의 기억도 지운다.
- 기본값으로 주인이 채널에 없으면 듣지 않는다(`require_owner_present`).

한국 통신비밀보호법상 대화에 참여하지 않은 제3자의 녹음·청취는 처벌 대상이다. 자세한 근거는 [`docs/background.md`](docs/background.md#6-법정책--녹음-고지-필수).

## 저장소 구조

| 경로 | 내용 |
|---|---|
| [`bot/`](bot/) | 본체(Rust). `src/voice.rs` 수신·호출, `convo.rs` 호출어, `llm.rs` 두뇌·판단, `web.rs`·`web/` 웹 콘솔, `commands.rs` 슬래시 명령 |
| [`sidecar/`](sidecar/) | 받아쓰기·목소리 사이드카(Python). `melo_worker.py`·`rvc_worker.py`는 별도 venv 작업 프로세스 |
| [`deploy/`](deploy/) | Windows 재기동 스크립트(`start.ps1`)·키 입력 스크립트(`set-key.ps1`) |
| [`poc/`](poc/) | 시작 전 검증 — 음성 수신(DAVE), 받아쓰기 벤치마크, 두뇌 지연 |
| [`docs/`](docs/) | 조사 보고서, [배경](docs/background.md), [운영 기록](docs/operations.md) |
| [`SPEC.md`](SPEC.md) | 요구사항(`F-*`·`N-*`)과 버전 기록 — 동작의 기준 |
| [`assets/`](assets/) | 캐릭터 아바타 |

## 개발

```powershell
cd bot
cargo test            # 설정 검증·호출어·판단 등 단위 테스트
cargo build --release
```

- 동작을 바꾸면 [`SPEC.md`](SPEC.md)의 해당 요구사항과 버전 기록도 고친다. 요구사항 ID는 코드 주석에서 그대로 인용한다.
- 받아쓰기·판단 기준처럼 숫자를 바꿀 때는 근거가 된 측정([`poc/`](poc/), [`docs/judge-stt-research-2026-10-16.md`](docs/judge-stt-research-2026-10-16.md))을 먼저 확인한다.
- PowerShell 스크립트(`deploy/*.ps1`)는 **UTF-8 BOM**으로 저장한다(Windows PowerShell 5.1이 BOM 없는 한글을 깨뜨린다).

## 알려진 제약

- 디스코드 음성 수신은 비공식이다. 2026-03부터 의무인 DAVE(종단간 암호화)는 songbird 0.6이 처리한다.
- edge-tts는 비공식 API라 막히거나 가끔 끊긴다. 그래서 목소리는 체인으로 두고 실패하면 다음 공급자로 넘어간다.
- 받아쓰기가 짧은 이름을 엉뚱하게 적는 일이 있다("필장"·"약정"). "헤이"를 붙여 부르거나 이모지 호출을 쓰면 낫다.
- GTX 1060 기준: 받아쓰기 +1.1GB, RVC +0.7GB VRAM. RVC는 문장당 약 2초 더 걸린다.

## 기여

이슈·PR 환영합니다. 큰 변경은 이슈로 먼저 이야기해 주세요.

- 비밀값(토큰·키)과 실제 대화 기록, 다른 사람의 디스코드 ID는 커밋하지 마세요.
- 음성 모델 파일(`.pth`·`.index`)은 받지 않습니다.

## 라이선스

아직 정하지 않았습니다. 라이선스가 붙기 전까지는 코드 저작권이 작성자에게 있습니다.
캐릭터 이미지(`assets/`)와 캐릭터 설정은 별도 허락 없이 재배포하지 마세요.

## 감사의 말

- [serenity](https://github.com/serenity-rs/serenity) · [songbird](https://github.com/serenity-rs/songbird) — 디스코드 게이트웨이·음성(DAVE 수신)
- [faster-whisper](https://github.com/SYSTRAN/faster-whisper) — 로컬 받아쓰기
- [edge-tts](https://github.com/rany2/edge-tts) · [MeloTTS](https://github.com/myshell-ai/MeloTTS) · [Applio](https://github.com/IAHispano/Applio) — 목소리
- 카사네 테토(重音テト) — 테토 캐릭터는 팬 캐릭터이며 원작자와 관계없다
