# 운영 기록 (주인 배포 환경)

이 문서는 저장소 주인이 실제로 돌리는 환경(보조 PC 한 대)의 배포 절차·상태 이력·장애 메모다.
처음 설치하는 법은 [README](../README.md#빠른-시작)를 본다.

## 상태 이력

- 상태: **1차 구현 배포 (2026-10-13)** — 보조 PC에서 상시 실행 중. 듣기·받아쓰기·목소리(edge-tts·MeloTTS 대체)·명령·웹 콘솔 동작 확인.
  **Codex 교차검증 PASS**(gpt-5.6-sol xhigh, SPEC 기반 완료조건 15개 전부 통과 — 10회차, 1~9회차 지적은 전부 수정·재배포).
  **두뇌: Anthropic 키 등록·실측 완료(2026-10-13)** — haiku·sonnet·opus가 봇 코드 경로로 대답, 캐시 쓰기→읽기 확인,
  스트리밍으로 말 도착 중앙값 약 1.3초(추정 응답 지연 ≈ 2.9초, [`poc/llm-latency/`](../poc/llm-latency/README.md)). Groq 키 등록(2026-10-14) — `groq-free` 실측 말 도착 0.3~0.7초. OpenAI 키는 아직 없음 → 아짱(luna)은 체인 규칙대로
  `groq-free`(무료)로 대답. 모델·받아쓰기 조사: [`docs/research-2026-10.html`](research-2026-10.html).
  **모델·명령어·받아쓰기 개선 검수(2026-10-14)**: Codex 교차검증 PASS(gpt-5.6-sol xhigh, 완료조건 14개 — 16회차) + 서브에이전트 리뷰 OK(명령·두뇌 두 영역).
  **헤이 호출·`/설정 호출방식`·groq-fast 체인 제외(2026-10-14)**: Codex PASS(완료조건 9개 — 2회차) + 서브에이전트 OK, 보조 PC 배포.
  **2차 기능(2026-10-16)**: 판단 모델(애매한 호출을 Groq 무료 모델에 YES/NO, 실측 0.25~0.68초)·말투 선택(성격/존댓말)·캐릭터 추가(웹 콘솔)·
  디스코드 로그인(코드 완료, 클라이언트 비밀 등록 대기)·다국어 edge 목소리·Azure·Typecast(키 대기). 서브에이전트 리뷰 1회 + **Codex 교차검증 PASS**
  (gpt-5.6-sol xhigh, 완료조건 13개 — 32회차, 1~31회차 지적은 전부 수정·재배포: 판단 결과·무시·잊어줘·채널 이동 사이 경합을 호출 번호·세대·질문 줄로 막음). 보조 PC 배포.
  **판단 전용 모델(2026-10-16)**: OpenRouter 판단 모델 22종을 실측해 기본 판단 모델을 `jev`(Jev 1.14)로 바꿈 — 처음 보는 문장·깨진 받아쓰기 50개 중 49
  (Groq는 32/40, 연달아 부르면 무료 한도로 실패), 0.41초, 회당 약 $0.00003. 봇 경유 재측정 48/50·0.36초. 일반 문장 72·실제 대화 350줄 재측정으로 기준 0.8(일반 문장 오탐 3/72), 판단 요청 원문 보기 추가. Codex 교차검증 PASS(gpt-5.6-sol xhigh, 완료조건 14개 — 5회차, 1·3·4회차 지적 전부 수정). 보조 PC 배포.
  **테토·RVC 목소리·긴 호출어(2026-10-17)**: 테토 캐릭터(RVC 목소리, GTX 1060에서 문장당 약 2초), 클로드짱·아스트라짱·코덱스짱·지피티짱 호출어. Codex 교차검증 PASS(완료조건 10개 — 4회차, 1~3회차 지적 전부 수정). 보조 PC 배포. 조사: [`docs/judge-stt-research-2026-10-16.md`](judge-stt-research-2026-10-16.md).
- 스펙: [`SPEC.md`](../SPEC.md) · 측정: [`poc/stt-bench/`](../poc/stt-bench/README.md)(Zeroth CER 6.1%, FLEURS 5.1%, 지연 ~0.9초) · [`poc/voice-recv/`](../poc/voice-recv/README.md)(DAVE 수신 통과)
- 실행 위치: 보조 PC `DESKTOP-Bed`(192.168.0.30) `C:\Users\macham\voicebot\` — 음악봇과 같은 호스트, **다른 봇 계정·다른 프로세스**.
- 포트: 웹 콘솔 `8694`(2026-10-15부터 방화벽 규칙 `voicebot-console-8694`가 NAS 터널 `192.168.0.9`만 허용 — LAN 평문 접속 차단, 외부 `https://voicebot.macham.net` — Cloudflare Access 이메일 OTP 뒤 + 디스코드 로그인(주인·허용 목록), 준비 전엔 콘솔 비밀번호),
  사이드카 `8695`(localhost 전용).
- 디스코드 앱: `클짱아짱`(id 1559203564205768876), 친구 서버·개발용 서버 2곳. 아바타 클짱 v1.

## 주인 환경 폴더

| 폴더 | 내용 |
|---|---|
| [`bot/`](../bot/) | 본체(Rust, serenity + songbird 0.6 receive). 수신·거르기·발화 자르기·호출어·두뇌 체인·재생 큐·명령·웹 콘솔 |
| [`sidecar/`](../sidecar/app.py) | Python. 받아쓰기 체인(faster-whisper GPU turbo → CPU small → 클라우드), 목소리 체인(edge-tts → azure → melotts → …). 봇이 띄우고 감시 |
| [`deploy/start.ps1`](../deploy/start.ps1) | 보조 PC에서 봇·사이드카 재기동(SSH가 끊겨도 살게 `Win32_Process`). 웹 콘솔 "재시작"도 이걸 부른다 |
| `bot/config.example.toml` | 설정 예시(비밀 없음). 보조 PC에는 `config.toml`로 복사돼 있다 |

보조 PC 폴더(`C:\Users\macham\voicebot\`): `voicebot.exe`, `config.toml`, `.env`(토큰·키·`CONSOLE_PASSWORD`, 본인만 읽기),
`data/state.json`(무시 목록·서버별 설정·사용량 — 대화 내용 없음), `data/voicebot.log`, `data/sidecar.log`, `sidecar/`(venv).

## 배포·운영

```powershell
# 배포(PC): 빌드 → 복사 → 재기동
cd bot; cargo build --release; cd ..
scp bot\target\release\voicebot.exe deploy\start.ps1 aux-server:C:/Users/macham/voicebot/
scp sidecar\app.py sidecar\rvc_worker.py aux-server:C:/Users/macham/voicebot/sidecar/
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
- 페르소나: 캐릭터 카드 구조 + 대답 직전 말투 상기(`[characters.X] reminder`). 웹 콘솔 → 공급자 → 두뇌 시험에서 캐릭터를 골라 말투를 시험한다. 조사·개선 보고서: [`docs/improvement-research-2026-10-15.html`](improvement-research-2026-10-15.html).
- 장기 기억: 음성으로 "클짱, ○○ 기억해" → `/기억 보기`·웹 콘솔 대화 탭(SPEC F-37). 두뇌 입출력 원문(보낸 프롬프트·받은 응답)도 대화 탭에서 본다.
- 입장 인사: `/설정 인사말`·웹 콘솔 이 서버. 소리를 사람에게 못 붙이면(디스코드 Speaking 신호 없음) 채팅에 "나갔다 들어와 주세요"를 안내한다.
- 공개 저장소: https://github.com/machamy/discord-voice-assistant (2026-10-15, 친구 ID·서버 이름을 가린 스냅샷 커밋 하나로 올림 — 로컬 이력은 비공개 유지)
- 비용 보기: 웹 콘솔 → 사용량 → **비용 요약**(오늘·7일·이번 달·31일, 프리셋별 대답 두뇌/판단 모델 호출·토큰·금액·비중, 캐릭터별 금액 — 캐릭터별은 2026-10-17부터).
  로컬 받아쓰기·edge·RVC는 무료, 유료 키 TTS/STT(Typecast 등)는 각 서비스 대시보드에서.
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
- RVC 목소리(SPEC F-60·F-61, 테토): 보조 PC `rvc-venv`(Python 3.11) + `rvc\applio`(Applio 3.6.5 추론 부분) + `models\rvc\teto\`(`.pth`·`.index`).
  설치(보조 PC `C:\Users\macham\voicebot`에서):
  ① `py -3.11 -m venv rvc-venv`
  ② `rvc-venv\Scripts\python -m pip install torch==2.11.0 torchaudio==2.11.0 --index-url https://download.pytorch.org/whl/cu126`(cu128은 GTX 1060 sm_61 미지원일 수 있음 — `torch.cuda.get_arch_list()`에 sm_61 확인)
  ③ `rvc-venv\Scripts\python -m pip install numpy requests tqdm wget faiss-cpu librosa==0.11.0 scipy soundfile noisereduce pedalboard stftpitchshift soxr torchcrepe==0.0.24 torchfcpe==0.0.4 einops transformers==5.13.1 psutil`
  ④ Applio 코드: `git clone https://github.com/IAHispano/Applio rvc\applio` → `git -C rvc\applio checkout 55fe0b976a6990bb75261c32ccecf6bfca3198f1`(v3.6.5),
     `rvc\applio\assets\config_template.json`을 `config.json`으로 복사.
  ⑤ 추론 자산: `rvc\applio\rvc\models\predictors\rmvpe.pt`·`rvc\applio\rvc\models\embedders\contentvec\`(Applio를 한 번 실행하면 받는 파일 — 이번에는 학습한 PC의 Applio에서
     `rvc\models\pretraineds`만 빼고 복사했다). ⑥ 모델: `models\rvc\<이름>\*.pth`(+`*.index`).
  사이드카가 시작할 때 모델을 미리 올린다. 웹 콘솔 캐릭터 카드 **RVC 목소리**에서 모델·높낮이(반음)를 고른다. 문장당 약 2초 더 걸린다.
  **테토 음원 규약**: 비상업·개인(営利目的でない同人) 범위에서만 쓴다. 음성 합성 AI 학습은 개인으로 즐기는 범위만, 학습 결과물·음원 배포와 판매는 금지.
  그래서 **모델 파일(`.pth`·`.index`)은 절대 커밋·공유하지 않는다**. 원본 백업은 본인 PC `D:\02_Workspace\voice-models\teto`.
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
