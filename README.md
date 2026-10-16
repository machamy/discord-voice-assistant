# discord-voice-assistant (아짱·클짱)

디스코드 음성 채널에 상주하면서 대화를 받아쓰기로 기억해 두고, 누가 **"아짱" / "클짱"** 을 부르면
그때까지의 대화 맥락으로 **음성으로 대답**하는 봇. 특정 사람의 음성은 무시한다.

- 상태: **1차 구현 배포 (2026-10-13)** — 보조 PC에서 상시 실행 중. 듣기·받아쓰기·목소리(edge-tts·MeloTTS 대체)·명령·웹 콘솔 동작 확인.
  **Codex 교차검증 PASS**(gpt-5.6-sol xhigh, SPEC 기반 완료조건 15개 전부 통과 — 10회차, 1~9회차 지적은 전부 수정·재배포).
  **두뇌: Anthropic 키 등록·실측 완료(2026-10-13)** — haiku·sonnet·opus가 봇 코드 경로로 대답, 캐시 쓰기→읽기 확인,
  스트리밍으로 말 도착 중앙값 약 1.3초(추정 응답 지연 ≈ 2.9초, [`poc/llm-latency/`](poc/llm-latency/README.md)). Groq 키 등록(2026-10-14) — `groq-free` 실측 말 도착 0.3~0.7초. OpenAI 키는 아직 없음 → 아짱(luna)은 체인 규칙대로
  `groq-free`(무료)로 대답. 모델·받아쓰기 조사: [`docs/research-2026-10.html`](docs/research-2026-10.html).
  **모델·명령어·받아쓰기 개선 검수(2026-10-14)**: Codex 교차검증 PASS(gpt-5.6-sol xhigh, 완료조건 14개 — 16회차) + 서브에이전트 리뷰 OK(명령·두뇌 두 영역).
  **헤이 호출·`/설정 호출방식`·groq-fast 체인 제외(2026-10-14)**: Codex PASS(완료조건 9개 — 2회차) + 서브에이전트 OK, 보조 PC 배포.
  **2차 기능(2026-10-16)**: 판단 모델(애매한 호출을 Groq 무료 모델에 YES/NO, 실측 0.25~0.68초)·말투 선택(성격/존댓말)·캐릭터 추가(웹 콘솔)·
  디스코드 로그인(코드 완료, 클라이언트 비밀 등록 대기)·다국어 edge 목소리·Azure·Typecast(키 대기). 서브에이전트 리뷰 1회 + **Codex 교차검증 PASS**
  (gpt-5.6-sol xhigh, 완료조건 13개 — 32회차, 1~31회차 지적은 전부 수정·재배포: 판단 결과·무시·잊어줘·채널 이동 사이 경합을 호출 번호·세대·질문 줄로 막음). 보조 PC 배포.
  **판단 전용 모델(2026-10-16)**: OpenRouter 판단 모델 22종을 실측해 기본 판단 모델을 `jev`(Jev 1.14)로 바꿈 — 처음 보는 문장·깨진 받아쓰기 50개 중 49
  (Groq는 32/40, 연달아 부르면 무료 한도로 실패), 0.41초, 회당 약 $0.00003. 봇 경유 재측정 48/50·0.36초. 일반 문장 72·실제 대화 350줄 재측정으로 기준 0.8(일반 문장 오탐 3/72), 판단 요청 원문 보기 추가. Codex 교차검증 PASS(gpt-5.6-sol xhigh, 완료조건 14개 — 5회차, 1·3·4회차 지적 전부 수정). 보조 PC 배포. 조사: [`docs/judge-stt-research-2026-10-16.md`](docs/judge-stt-research-2026-10-16.md).
- 스펙: [`SPEC.md`](SPEC.md) · 측정: [`poc/stt-bench/`](poc/stt-bench/README.md)(Zeroth CER 6.1%, FLEURS 5.1%, 지연 ~0.9초) · [`poc/voice-recv/`](poc/voice-recv/README.md)(DAVE 수신 통과)
- 실행 위치: 보조 PC `DESKTOP-Bed`(192.168.0.30) `C:\Users\macham\voicebot\` — 음악봇과 같은 호스트, **다른 봇 계정·다른 프로세스**.
- 포트: 웹 콘솔 `8694`(2026-10-15부터 방화벽 규칙 `voicebot-console-8694`가 NAS 터널 `192.168.0.9`만 허용 — LAN 평문 접속 차단, 외부 `https://voicebot.macham.net` — Cloudflare Access 이메일 OTP 뒤 + 디스코드 로그인(주인·허용 목록), 준비 전엔 콘솔 비밀번호),
  사이드카 `8695`(localhost 전용).
- 디스코드 앱: `클짱아짱`(id 1559203564205768876), 친구 서버·개발용 서버 2곳. 아바타 클짱 v1.

## 구성

| 폴더 | 내용 |
|---|---|
| [`bot/`](bot/) | 본체(Rust, serenity + songbird 0.6 receive). 수신·거르기·발화 자르기·호출어·두뇌 체인·재생 큐·명령·웹 콘솔 |
| [`sidecar/`](sidecar/app.py) | Python. 받아쓰기 체인(faster-whisper GPU turbo → CPU small → 클라우드), 목소리 체인(edge-tts → azure → melotts → …). 봇이 띄우고 감시 |
| [`deploy/start.ps1`](deploy/start.ps1) | 보조 PC에서 봇·사이드카 재기동(SSH가 끊겨도 살게 `Win32_Process`). 웹 콘솔 "재시작"도 이걸 부른다 |
| `bot/config.example.toml` | 설정 예시(비밀 없음). 보조 PC에는 `config.toml`로 복사돼 있다 |

보조 PC 폴더(`C:\Users\macham\voicebot\`): `voicebot.exe`, `config.toml`, `.env`(토큰·키·`CONSOLE_PASSWORD`, 본인만 읽기),
`data/state.json`(무시 목록·서버별 설정·사용량 — 대화 내용 없음), `data/voicebot.log`, `data/sidecar.log`, `sidecar/`(venv).

## 쓰는 법 (디스코드)

음성 채널에 들어가서 `/들어와` → "클짱 오늘 저녁 뭐 먹지?"처럼 부르면, **음성으로 짧게** 답하고
**지정 채널에 부른 사람을 멘션해서 길게** 올린다. "클짱!"만 부르고 멈추면 다음 말을 질문으로 듣는다.

| 명령 | 누가 | 하는 일 |
|---|---|---|
| `/들어와` · `/나가` | 누구나 | 음성 채널 입장·퇴장 |
| `/무시해줘` · `/다시들어줘` | 본인 | 내 목소리 안 듣기(기억도 지움)·다시 듣기 |
| `/잊어줘` | 누구나 | 기억한 대화 전부 지우기 |
| `/기억 보기·지우기·전부지우기` | 보기는 누구나, 지우기는 본인 것(관리자는 전부), 전부지우기는 관리자 | 장기 기억(SPEC F-37) |
| `/상태` | 누구나 | 듣는 채널·사람·세션 통계·사이드카·응답 지연(나만 보기) |
| `/모델 캐릭터 프리셋` | 누구나(비싼 건 주인) | 캐릭터 두뇌 바꾸기(자동완성) |
| `/설정 보기` | 누구나 | 모든 설정을 카드 한 장으로 |
| `/설정 두뇌·대체두뇌·목소리·말투선택·말투·채널·주인없으면·호출방식·음성대답·음량·인사말·기억시간·초기화` | 주인·서버 관리 | 설정 바꾸기(목소리는 바꾸면 바로 들려줌) |
| `/설정 말투선택 캐릭터 성격·존댓말` | 주인·서버 관리 | 성격 있는 반말 / 상냥한 존댓말(SPEC F-55) |
| `/설정 호출어 캐릭터 [추가] [빼기]` | 주인 | 추가 호출어(모든 서버 공통, SPEC F-21b) |
| `/설정 한도` | 주인 | 하루 유료 호출·금액 상한 |
| `/채널지정` | 주인·서버 관리 | 긴 답 채널 |
| `/무시목록 유저추가·유저삭제·역할추가·역할삭제·보기` | 주인·서버 관리 | 무시 목록 |
| `/공급자` · `/설정다시읽기` | 주인 | 공급자·사용량 보기 / `config.toml` 다시 읽기 |

## 운영

```powershell
# 배포(PC): 빌드 → 복사 → 재기동
cd bot; cargo build --release; cd ..
scp bot\target\release\voicebot.exe deploy\start.ps1 aux-server:C:/Users/macham/voicebot/
scp sidecar\app.py aux-server:C:/Users/macham/voicebot/sidecar/
ssh aux-server "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\macham\voicebot\start.ps1"

# 상태
ssh aux-server "Get-Content C:\Users\macham\voicebot\data\voicebot.log -Tail 30"
```

- `start.ps1`은 **UTF-8 BOM**으로 저장해야 한다(PowerShell 5.1이 BOM 없는 한글을 CP949로 읽어 구문이 깨짐 — 2026-10-13 실제 발생).
- 사이드카 패키지: `sidecar\.venv`에 `pip install -r sidecar\requirements.txt`(PoC 2와 같은 CUDA 12 조합).
- 키 추가: **본인 PC PowerShell 창**에서 `powershell -File deploy\set-key.ps1`(숨김 입력 → 보조 PC `.env`에 UTF-8로 반영,
  같은 이름은 교체) → 웹 콘솔 "재시작". 값은 화면·대화 기록·로그에 남지 않는다.
  (2026-10-13: `"KEY=…" | ssh … more >> .env` 방식은 PowerShell 5.1이 UTF-16으로 보내 `.env`가 깨졌다 — 쓰지 말 것.)
- 자동 시작: 보조 PC 작업 스케줄러 **`VoiceBot`**(로그온 1분 뒤 `start.ps1`, 음악봇 `MusicBot Portable`과 같은 방식). 2026-10-14 재부팅 뒤 안 떠서 2026-10-15 등록.
- 받아쓰기 상황: 웹 콘솔 → 대화 → **받아쓰기 상황**(발화별 결과·지연·공급자·호출, 2초 자동 갱신. 글 내용은 아래 대화 칸에만).
- 음성 대답 끄기·음량: `/설정 음성대답`·`/설정 음량` 또는 웹 콘솔 → 이 서버.
- 이모지 호출: 음성 채널에서 🧡(클짱)·🩵(아짱) 반응 → "응?" 하고 다음 말을 질문으로 받는다(받아쓰기가 이름을 못 알아들어도 됨).
- 호출별 지연: 웹 콘솔 → 대화 → 받아쓰기 상황 → **호출별 지연**(받아쓰기 대기·받아쓰기·대답 대기·두뇌 첫 문장·TTS·재생 대기·첫 소리).
- 재시작 뒤 자동 재입장: 마지막 음성 채널을 저장해 두었다가 봇이 다시 뜨면 들어간다.
- 위험 별칭: `/설정 호출어 캐릭터 추가:실장 위험:true` 또는 웹 콘솔 → 위험 별칭. 헤이·"실장아"·맨 앞 질문일 때만 호출.
- 페르소나: 캐릭터 카드 구조 + 대답 직전 말투 상기(`[characters.X] reminder`). 웹 콘솔 → 공급자 → 두뇌 시험에서 캐릭터를 골라 말투를 시험한다. 조사·개선 보고서: [`docs/improvement-research-2026-10-15.html`](docs/improvement-research-2026-10-15.html).
- 장기 기억: 음성으로 "클짱, ○○ 기억해" → `/기억 보기`·웹 콘솔 대화 탭(SPEC F-37). 두뇌 입출력 원문(보낸 프롬프트·받은 응답)도 대화 탭에서 본다.
- 입장 인사: `/설정 인사말`·웹 콘솔 이 서버. 소리를 사람에게 못 붙이면(디스코드 Speaking 신호 없음) 채팅에 "나갔다 들어와 주세요"를 안내한다.
- 공개 저장소: https://github.com/machamy/discord-voice-assistant (2026-10-15, 친구 ID·서버 이름을 가린 스냅샷 커밋 하나로 올림 — 로컬 이력은 비공개 유지)
- 두뇌 확인: 웹 콘솔 → 공급자 → 두뇌 프리셋 표의 **시험** 버튼(말 도착·전체 지연, 토큰·캐시, 형식). 유료는 하루 상한에 잡힌다.
- 판단 모델(SPEC F-54·F-59): 비슷한 말·위험 별칭으로 들린 호출은 `jev`(OpenRouter 판단 전용, 키 `OPENROUTER_API_KEY`)에 "정말 부른 말인가"의 확률을 묻고,
  부름 기준 확률(기본 0.8) 이상이면 대답한다. 웹 콘솔 → 공급자 → **판단 모델**에서 켜기·모델(실측 표에서 고르기)·부름 기준 확률·범위("이름도"면 헤이 없는
  이름 호출까지 — 실측상 비추천)·대기 시간을 바꾸고, 저장 전에 문장을 넣어 시험한다. 시험 결과 아래 "보내는 요청 원문"에서 판단 모델에 실제로 보내는 프롬프트를 볼 수 있다("원문만 보기"는 모델을 안 부름). 결과는 대화 탭 받아쓰기 상황(📣 …·판단: 부름 93%/아님 12%)과 호출별 지연 "판단" 칸.
  판단 비용은 하루 사용액에 더해진다(상한에 닿으면 판단 없이 원래 규칙대로). 무료로 쓰려면 `groq-fast`나 `mercury-free`를 고른다.
- 말투 선택(SPEC F-55): `/설정 말투선택` 또는 웹 콘솔 캐릭터 카드. 존댓말 글은 `[characters.X] persona_polite`, 웹에서 서버별로 고칠 수 있다.
- 캐릭터 추가(SPEC F-56): 웹 콘솔 → 캐릭터 → 맨 아래 **캐릭터 추가**(이름·이모지·호출어·두뇌·말투·목소리). 바로 부를 수 있고 명령 자동완성에도 뜬다.
  웹에서 추가한 캐릭터만 고치기·삭제. 저장 위치는 `data/state.json`(`custom_characters`).
- 목소리(SPEC F-58): 캐릭터 카드 **빠른 설정** 버튼(선희 귀엽게·아기처럼·차분하게, 엠마 발랄·아바 상냥 등 다국어 — 누르면 미리 듣기, 마음에 들면 저장).
  쇼츠에서 많이 쓰는 **Typecast** 목소리는 `TYPECAST_API_KEY`(유료, 무료 3만 크레딧)를 넣으면 카드에 목록이 뜬다. Azure 키(`AZURE_SPEECH_KEY`·`AZURE_SPEECH_REGION`, 월 50만 자 무료)를 넣으면 카드의 **Azure 목소리** 칸에서 서현(아이)·지민·유진 음성도 고를 수 있다(비우면 edge와 같은 음성).
- 디스코드 로그인(SPEC F-57) 켜는 법: ① 디스코드 개발자 포털 → 앱 `클짱아짱` → OAuth2 → Redirects에 `https://voicebot.macham.net/auth/discord/callback` 추가
  ② 같은 화면의 Client Secret을 `deploy\set-key.ps1 DISCORD_CLIENT_SECRET`로 보조 PC `.env`에 넣기 ③ 웹 콘솔 재시작. 그 뒤로는 비밀번호 로그인이 꺼지고
  주인·`[web] allowed_users`만 들어온다.

## 알려진 문제·제약 (2026-10-13)

- **edge-tts 합성 주소 403**(2026-10-12부터, edge-tts 이슈 #490). 사이드카가 PR #491과 같은 방식으로
  `api.msedgeservices.com`에 합성한다. 정식 배포가 나오면 `EDGE_TTS_WSS_URL=legacy`로 되돌릴 수 있다.
- **MeloTTS(오프라인 대체)는 별도 venv(`melo-venv`, Python 3.11)의 작업 프로세스**로 돈다(첫 호출 약 20초, 이후 약 3초).
  Python 3.13에선 의존성 빌드가 실패했고, `python-mecab-ko`를 site-packages에 같이 넣으면 대소문자 구분 없는
  Windows에서 `mecab-python3`의 `MeCab` 폴더와 섞여 둘 다 깨진다 → `melo-venv\mecabko`에 따로 설치(설치법은 `sidecar/requirements.txt`).
- 웹 콘솔 외부 주소는 고정 IP(`192.168.0.30`)로 터널에 물려 있다. 보조 PC IP가 바뀌면 NAS live `config.yml`을 고쳐야 한다.
- 두뇌: Anthropic과 Groq 실측. `groq-free`(gpt-oss-120b)는 말 도착 0.3~0.7초·형식 정상(2026-10-14, 5회)이고, `groq-fast`(20b)는 형식 4/4 실패에 없는 사실을 지어내 무료 체인에서 뺐다. OpenAI(`luna`·`sol`·`astra`)·Gemini는 키가 생기면 두뇌 시험으로 확인.
- **xAI Grok ≠ Groq**: 무료 대체는 Groq(console.groq.com, 카드 없이 무료)다. xAI(Grok)는 유료라 쓰지 않는다.
- edge-tts는 가끔 한 번씩 끊긴다(2026-10-13 4회 중 1회) → 재시도하지 않고(2026-10-15, 8초×2 대기 제거) 바로 다음 공급자로, 실패한 공급자는 60초 동안 뒤로 미룬다. MeloTTS는 사이드카 시작 때 미리 올린다.

## 요구사항 (원문 2026-10-09)

> 대화하는거 녹음 하다가 아짱 클짱 부르면 대화 컨텍스트 기반으로 해주기.
> 근데 사람 목소리 다른거나 특정 사람 무시는 해주면 될듯

확정 (2026-10-10):
- **디스코드 음성 채널**에서 듣고, **음성(TTS)으로** 대답한다.
- 캐릭터 둘, 회사가 다르다:
  - **아짱 = 코덱스 아스트라** → OpenAI GPT-6 계열 (Codex에서 쓰는 Astra)
  - **클짱 = 클로드** → Anthropic Claude
- **모델은 캐릭터별로 고를 수 있게** 한다. 상위 모델(Opus·Astra)은 비싸서 기본값으로 두지 않는다.
- 두 캐릭터는 같은 대화 기록을 보고, 목소리(TTS 음성)를 서로 다르게 한다.

방침 (2026-10-12): **최대한 무료**, 그리고 STT·LLM·TTS를 **종류별로 공급자 교체 가능**하게(설정 파일만 바꿔서, 폴백 체인).
받아쓰기·목소리는 기본 0원(로컬·무료 등급). OpenAI·Anthropic은 API 무료 등급이 없어 캐릭터 기본 두뇌는
**최저가 유료(`claude-haiku-5-5`·`gpt-6-luna`, 1회 약 $0.001) + 상한·장애 시 Groq 무료 모델로 대체**(확정 2026-10-12).
상세는 [`SPEC.md`](SPEC.md) §6.

### 캐릭터별 모델 선택지 (1M 토큰당 입력/출력, 2026-10 기준)

| 등급 | 클짱 (Claude) | 아짱 (OpenAI) | 1회 호출 예상* |
|---|---|---|---|
| **싸고 빠름 = 기본값(2026-10-12)** | `claude-haiku-5-5` $0.10/$0.50 | `gpt-6-luna` $0.10/$0.50 | 약 $0.001 |
| 중간 | `claude-sonnet-5-5` $2/$10 | `gpt-6.1-sol` $2/$10 (2026-10-13 신형으로 교체) | 약 $0.014 |
| 비쌈 | `claude-opus-5-5` $4/$20 | `gpt-6-astra` $10/$50 | $0.03 / $0.07 |

\* 대화 기록 약 6천 토큰 입력 + 짧은 답 200토큰 기준. 대화 기록 앞부분은 프롬프트 캐시로 더 싸진다.
모델 ID·가격은 2026-10-13 공식 가격 페이지·Anthropic `/v1/models`로 확인했다([`docs/research-2026-10.html`](docs/research-2026-10.html)).
실측(대화 기록 짧을 때 입력 약 550토큰·출력 약 250토큰): haiku 약 $0.0002, sonnet 약 $0.004, opus 약 $0.008.

- 바꾸는 방법(안): 디스코드 슬래시 명령 `/모델 클짱 haiku`, `/모델 아짱 astra` — 서버별로 저장.
- 비용 안전장치: 하루 호출 수/금액 상한, 상위 모델은 주인만 고를 수 있게.

## 조사 결론

### 1. 디스코드에서 봇이 음성을 받을 수 있나 — 된다 (단, 비공식 기능)

- 2026-03-01부터 디스코드 음성은 종단간 암호화(DAVE)가 의무다. DAVE를 모르는 봇은 접속 자체가 안 된다
  (close code 4017).
- 봇도 DAVE 그룹의 정식 참여자라 복호화 키를 받는다. **음악봇이 쓰는 songbird 0.6.0은 수신 경로에서
  사람별로 DAVE 복호화를 한다** — 로컬 소스 `songbird-0.6.0/src/driver/tasks/udp_rx/mod.rs:225`
  `dave_session.decrypt(user_id, MediaType::AUDIO, body)`. (`receive` 기능 플래그 필요)
- discord.js(`@discordjs/voice`)도 `DAVESession.decrypt(packet, userId)`가 있다.
- ⚠️ 디스코드는 음성 수신을 **공식 문서화하지 않는다**. 디스코드 쪽 변경으로 언제든 깨질 수 있다.

### 2. "누가 말했는지" 구분 — 디스코드라서 거의 공짜

디스코드는 사람마다 음성 스트림(SSRC)을 따로 보내고, 그게 어느 유저 ID인지도 알려준다. 그래서
목소리로 사람을 맞히는 화자 분리·식별 모델이 **필요 없다**. "특정 사람 무시"는 **유저 ID 차단 목록**으로
수신 단계에서 버리면 끝이다(받아쓰기도 저장도 안 함). 다른 봇(음악봇 포함)의 소리도 같은 방식으로 뺀다.

### 3. 받아쓰기(STT) — 보조 PC GPU로 로컬 처리 권장

보조 PC 실측 환경(2026-10-12): GTX 1060 6GB(Pascal CC 6.1) · 드라이버 582.66 · Ryzen 7 3700X · RAM 32GB.
CC 6.1이라 GPU **int8은 되고 float16은 안 된다**. CUDA 13은 Pascal을 뺐으므로 CUDA 12 빌드(CTranslate2 4.8.x)로 고정한다.
상세와 측정 계획은 [`poc/stt-bench/README.md`](poc/stt-bench/README.md).

| 선택지 | 비용 | 비고 |
|---|---|---|
| **faster-whisper large-v3-turbo int8 (로컬)** | 0원 | VRAM 약 1.1GB 추가 → 보조 PC GTX 1060 6GB에 들어감. **실측 완료**(PoC 2): 지연 약 0.9초, CER 5~6% |
| Deepgram Nova-3 (`deepgram`, 일괄) | 분당 약 $0.0052, 가입 크레딧 $200 | 공식 문서에 한국어(`ko`) 지원. 2026-10-13 사이드카에 어댑터 추가(키 없어 꺼둠) |
| OpenAI gpt-4o-mini-transcribe (`openai-transcribe`) | 분당 $0.003 (공식 가격표, 2026-10-13) | 무료 없음. 키가 있으면 체인에 넣을 수 있음 |

상시 받아쓰기라 **클라우드는 말한 시간만큼 계속 과금**된다(하루 발화 3시간이면 Deepgram 분당 $0.0052 기준 월 약 $28).
로컬이면 0원이라 1순위. 사람별 스트림에 음성 구간 검출(VAD)을 걸어 **말한 구간만** 받아쓴다.

### 4. 호출어 "아짱 / 클짱"

이미 전부 받아쓰고 있으므로 별도 호출어 모델 없이 **받아쓰기 결과에서 찾는다**. Whisper가 "아장",
"클장", "크짱"처럼 적을 수 있어 변형을 같이 잡는다. "헤이 클짱"·"헤이 아짱"도 된다 — 받아쓰기가 "헤이클짱"으로 붙여 써도 걸린다.
이름만 짧게 부르면 앞부분이 잘려 인식이 나쁠 수 있어, 그럴 땐 "헤이"를 붙여 부르는 걸 권장한다.
- **부름과 언급 구분**(SPEC F-21a): 이름이 맨 앞·"클짱아"·문장 끝·헤이 뒤일 때만 대답한다. "아짱도 아니고"처럼 이야기 속 이름엔 끼어들지 않는다.
- **추가 호출어**(F-21b): `/설정 호출어 캐릭터 추가:클로드` 또는 웹 콘솔 → 캐릭터 → 추가 호출어. 부르는 이름(클로드·"오케이 클짱")이든 받아쓰기가 적은 변형(실장·휘장)이든 된다. 헤이 바로 뒤 말은 이름과 비슷하기만 해도 걸린다.
- 2026-10-15 실사용에서 이름이 "필장·휠장·글쩡·악장·약정"처럼 첫소리부터 바뀌어 적히기도 했다 — 사전·자모 거리로 다 못 잡는다. 단축키·버튼 호출이 다음 후보.
`/설정 호출방식 헤이만`(웹 콘솔 "이 서버 → 부르는 방식")으로 바꾸면 헤이를 붙인 호출만 받는다 — 대화 중 이름이 나와도 대답하지 않게 된다. Whisper `initial_prompt`·`hotwords`에 이름을 넣는 방법은
**기본으로 쓰지 않는다**(`hotwords`는 웹 콘솔 → 공급자의 선택 옵션, 기본 끔·긴 발화에만 — SPEC F-49) — 잘린 음성에서 호출어를 지어낸다(실측: 1초 조각 382개 중 271개, SPEC F-20). 호출 뒤에 붙은 말(같은 발화 + 바로 다음 발화)을 질문으로 본다.

### 5. 대답 — Claude + 한국어 TTS

- **LLM**: 부른 순간의 대화 중 최근 5분(질문 줄 제외)을 넣고 짧게 답한다(버퍼는 기억 시간만큼 두되 두뇌엔 최근 것만, 2026-10-15). 모델·가격은 위 "캐릭터별 모델 선택지" 표가 기준
  (기본 최저가 — 클짱 `claude-haiku-5-5`, 아짱 `gpt-6-luna`, 대체 Groq 무료). 대화 기록 앞부분은 프롬프트 캐시로 재사용한다.
- **TTS**: 무료 우선 — edge-tts(키 불필요, 비공식) → Azure Speech F0(월 50만 자 무료, 공식) → MeloTTS(로컬, MIT).
  비교용 유료: ElevenLabs Flash(1,000자당 $0.05, 첫 소리 약 75ms 주장), OpenAI gpt-4o-mini-tts(분당 약 $0.015 추정).
  모두 한국어 품질은 직접 들어봐야 한다.
- 응답 지연(SPEC N-01, 말 끝남 → 첫 소리. 침묵 감지 0.6초는 그 전이라 뺀다): 받아쓰기 약 0.9초 + 두뇌 말 도착 약 1.3초(스트리밍 실측 중앙값)
  + edge-tts 약 0.65초(실측) → **약 2.9초**(추정. 실채널 측정은 남음).

### 6. 법·정책 — 녹음 고지 필수

- 한국 통신비밀보호법: 대화 **당사자**의 녹음은 적법하지만, **대화에 참여하지 않은 제3자**가
  남의 대화를 녹음·청취하면 처벌 대상이다. 봇이 주인 없이 남들 대화만 듣는 상황은 위험하다.
- 그래서 설계 원칙:
  - 봇이 채널에 들어올 때 **"듣고 있어요" 안내**(채팅 + 음성), 상태 메시지로 상시 표시
  - **원본 음성은 저장하지 않는다.** 받아쓰기 텍스트도 메모리에 최근 N분만 두고 버린다
  - 본인 거부 명령(`/무시해줘`)으로 누구나 스스로 차단 목록에 들어갈 수 있게
  - 주인(마참)이 채널에 없으면 듣지 않는 옵션

## 구조 초안

```
디스코드 음성 채널
   │ 사람별 Opus 스트림 (DAVE 복호화: songbird 0.6 receive)
   ▼
[수신] 차단 목록·봇 제외 → Opus 디코드 → 사람별 VAD로 발화 단위 자르기
   ▼
[받아쓰기] faster-whisper (보조 PC GPU) → "닉네임: 문장" + 시각
   ▼
[대화 버퍼] 최근 15분(기억 시간), 메모리에만 — 두뇌엔 최근 5분
   │ 문장에 "아짱"/"클짱"?
   ▼
[대답] 클짱→Claude / 아짱→OpenAI (버퍼 + 질문, 캐릭터별 선택 모델) → 짧은 한국어 답 → TTS → 같은 채널에 재생 (+ 텍스트 채널에도)
```

## 정해야 할 것

1. ~~아짱과 클짱의 차이~~ → 아짱=OpenAI(GPT-6), 클짱=Claude로 확정
2. ~~**음악봇과 합칠지**~~ → **별도 봇으로 확정 (2026-10-12)**. 아래는 판단 근거.
   musicbot-mk2(Rust, songbird 0.6)에 붙이면 이미 채널에 있는 봇이 듣기만 추가하면 됨.
   따로 만들면 봇이 하나 더 들어오지만 음악봇 장애와 분리된다.
   - 확인(2026-10-12): musicbot-mk2 `mc-app`은 약 5만 줄, songbird `receive` 기능은 꺼져 있다. 보조 PC에선
     `mc-musicbot.exe`가 포터블 폴더에서 데스크톱 프로세스로 돈다. 받아쓰기는 어느 쪽이든 Python(faster-whisper)
     별도 프로세스가 된다.
   - **추천: 별도 봇**(Rust + songbird 0.6 `receive`, 음악봇의 DAVE·songbird 경험 재사용) + Python STT 사이드카.
     비공식 수신 경로가 깨져도 음악봇은 멀쩡하고, PoC 1 코드가 그대로 본체가 된다.
   - 디스코드 앱(봇)은 새로 만든다 — 주인이 Developer Portal에서 생성, 토큰은 보조 PC `.env`에만.
3. ~~LLM 기본 모델~~ → 캐릭터별 선택 가능, 기본은 최저가 등급(위 표, 2026-10-12 변경). OpenAI **API 키**가 필요하다
   (Codex CLI의 ChatGPT 로그인과는 별개).
4. TTS 목소리 — 샘플 비교로 정한다. 2026-10-12 무료 우선 방침에 따라 **edge-tts·MeloTTS부터** 만들고,
   ElevenLabs(무료 등급)·OpenAI(샘플 비용 $0.05 미만 승인됨)는 비교용.
6. ~~캐릭터 기본 두뇌~~ → 최저가 유료(haiku·luna) + Groq 무료 대체로 확정(2026-10-12).
5. ~~녹음 고지 문구~~ → 현재 고지(config `[notice] text`, 2026-10-15): 「🎙️ 아짱·클짱이 대화를 듣고 있어요. 음성은 저장하지 않고,
   받아쓴 글은 최근 15분만 기억했다가 지워요. 부르면 최근 대화 몇 분이 대답을 만드는 AI 회사(Anthropic·Groq 등)로 보내져요.
   듣지 않길 원하면 `/무시해줘`」 — 입장 시 채팅 고지(항상) + 음성 인사(`/설정 인사말`로 끌 수 있음), 상태 메시지로 상시 표시.

## 첫 검증(PoC) 순서

| # | 내용 | 상태 (2026-10-12) | 필요한 것 |
|---|---|---|---|
| 1 | songbird 0.6 `receive`로 채널에 들어가 사람별 PCM이 오는지 (DAVE 복호화 실패율) | **통과 (2026-10-13)** — 4명 매핑 100%, 손실률 0~2.4%. 음악봇 소리도 들어와서 봇 자동 제외 필요(SPEC F-05) → [`poc/voice-recv/`](poc/voice-recv/README.md) | — |
| 2 | 보조 PC GTX 1060에서 faster-whisper turbo int8 한국어 속도·정확도 | **공개 데이터셋 측정 완료** — 지연 p95 0.82초(2~5초 발화)·CER 6.1%(Opus 포함)·VRAM +1.1GB로 통과. CPU 폴백은 small 모델(약 2.5초). `initial_prompt`가 호출어를 지어내는 문제 발견 → 안 씀 → [`poc/stt-bench/`](poc/stt-bench/README.md) | 직접 녹음 30~50문장(실제 판단용) |
| 3 | 호출어 인식률(아짱/클짱 변형) | PoC 2의 녹음 세트로 예비 측정 가능 | 같음 |
| 4 | LLM + TTS를 붙여 실제 응답 지연, 무료 LLM 품질 비교 — **클짱(Claude) 먼저**(주력) | **일부 완료(2026-10-13)** — Anthropic 키로 haiku·sonnet·opus 실측(말 도착 중앙값 약 1.3초, 캐시 확인). 2026-10-14 Groq 실측(`groq-free` 형식 5/5·말 도착 0.3~0.7초, `groq-fast` 형식 0/4로 체인 제외). 실채널 20회는 남음 | OpenAI 키는 원하면 |

## 참고 링크

- [Discord: 모든 음성·영상 통화 종단간 암호화](https://discord.com/blog/every-voice-and-video-call-on-discord-is-now-end-to-end-encrypted)
- [Discord 개발자 문서: Voice Connections](https://docs.discord.com/developers/topics/voice-connections)
- [songbird CHANGELOG (0.6.0 DAVE 지원)](https://github.com/serenity-rs/songbird/blob/current/CHANGELOG.md)
- [@discordjs/voice DAVE 암호화](https://tessl.io/registry/tessl/npm-discordjs--voice/files/docs/dave-encryption.md)
- [faster-whisper 벤치마크 이슈](https://github.com/SYSTRAN/faster-whisper/issues/1030)
- [Deepgram Nova-3 가격](https://convertaudiototext.com/blog/deepgram-nova-3-explained)
- [GPT-6 Astra 출시 (2026-09-03)](https://www.igeeksblog.com/gpt-6-astra-chatgpt-codex/)
- [GPT-6 Sol·Luna 가격](https://aireiter.com/blog/gpt-6-sol-luna-api-pricing-cost-guide)
- [ElevenLabs API 가격](https://elevenlabs.io/ko/pricing/api)
- [통신비밀보호법 — 대화 녹음 적법성](https://www.lawtalk.co.kr/posts/110066)
