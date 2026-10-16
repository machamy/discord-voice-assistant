# PoC 2 — faster-whisper 한국어 받아쓰기 측정 (보조 PC GTX 1060)

- 상태: **공개 데이터셋 측정 완료 (2026-10-12)** — 속도·VRAM 통과, 실제 잡담 녹음(`record`) 측정만 남음. 아래 "결과"
- 질문: large-v3-turbo int8이 GTX 1060에서 **짧은 발화를 1초 안에** 받아쓰나? 한국어 정확도는 쓸 만한가?

## 보조 PC 환경 (2026-10-12 읽기 전용 확인)

| 항목 | 값 | 의미 |
|---|---|---|
| GPU | GTX 1060 6GB, Pascal **CC 6.1**, WDDM | CTranslate2 GPU **int8 가능**(CC 6.1+). `float16`·`int8_float16`은 CC 7.0+라 **불가** → `compute_type="int8"` 고정 |
| VRAM | 6144MiB 중 1209MiB 사용(데스크톱 앱들) | turbo int8 약 1.5GB 추가 → 여유 있음. 단 게임 실행 중엔 경합 |
| 드라이버 | 582.66 (CUDA 13.0 드라이버) | R580 계열 — NVIDIA가 Pascal을 지원하는 **마지막 드라이버 계열**. 업데이트할 이유 없음. CUDA 12 런타임과 하위 호환 |
| CUDA 툴킷 | `v13.0` 폴더만 있고 PATH에 nvcc 없음 | 안 쓴다. CUDA 13은 Pascal 지원을 뺐다. **pip `nvidia-*-cu12` DLL을 venv 안에** 두고 쓴다 |
| CPU / RAM | Ryzen 7 3700X 8C16T / 32GB(여유 약 11GB) | CPU 폴백도 측정 가능 |
| Python | PATH의 `python` = msys64 3.12.7(쓰지 않음), `py` 3.13 (`C:\Program Files\Python313`). scoop `uv`는 shim만 있고 `apps\uv\0.8.12`가 비어 있어 **동작 안 함**(손대지 않음) | 3.13 표준 `venv` + `pip`. 새 Python 설치 불필요 |
| ffmpeg | 없음 | faster-whisper는 PyAV로 디코드해서 필요 없음 |
| 디스크 | C: 171GB 여유 | 설치 총 약 3GB |
| 음악봇 | `mc-musicbot.exe`(pid 20180) — `Desktop\musicbot-portable-20260601-2005\bot-mk2\` 에서 데스크톱 프로세스로 실행 | **건드리지 않는다.** 측정 전후 pid·CPU만 확인 |
| 용도 | Steam·Epic·Battle.net·Overwolf 상주 → **게임용 PC이기도 함** | 게임 중 GPU 경합이 실제 운영 리스크. 측정 D(CPU 폴백)가 그래서 필요 |

CTranslate2 4.8.2(2026-08-31, 최신) Windows 휠은 CUDA 12.8 + cuDNN 9.10으로 빌드된다(`python/tools/prepare_build_environment_windows.sh`).
**설치 후 확인(2026-10-12)**: `get_cuda_device_count()=1`, CUDA 지원 타입 `{int8, int8_float32, float32}` →
GTX 1060에서 GPU int8 정상 동작. turbo 첫 로드 41.5초(다운로드 포함, 이후 약 4초), 워밍업 약 1~1.5초.

⚠️ **1초 무음에 "다음 영상에서 만나요."를 지어냈다**(no_speech_prob ≈ 0). Whisper의 알려진 환각이다.
→ 봇에서는 **앞단 VAD로 말한 구간만** 넘기고, 이런 상투 문구(유튜브 엔딩 멘트류)는 받아쓰기 결과에서 거른다.
안 그러면 호출어 오탐과 대화 버퍼 오염의 원인이 된다.

## 설치 (2026-10-12 승인·완료)

전부 `C:\Users\macham\voice-poc\` 한 폴더 안. 시스템 Python·PATH·드라이버·음악봇 폴더는 손대지 않는다.
모델 캐시도 `HF_HOME`으로 같은 폴더에 넣어서 **치울 땐 폴더 하나만 지우면 된다.**

```powershell
# (PC) 스크립트 전송 — aux-server SSH는 PowerShell 도구로. PS 5.1 파이프는 바이너리를 깨므로 tar 파이프 대신 scp
ssh aux-server "New-Item -ItemType Directory -Force C:\Users\macham\voice-poc | Out-Null"
scp -r poc\stt-bench aux-server:C:/Users/macham/voice-poc/

# (보조 PC) — 원격 셸이 PowerShell이라 여러 줄 명령은 -EncodedCommand로 보내는 게 안전하다
cd C:\Users\macham\voice-poc\stt-bench
$env:PIP_CACHE_DIR = "C:\Users\macham\voice-poc\pip-cache"
& "C:\Program Files\Python313\python.exe" -m venv .venv
.venv\Scripts\python -m pip install -r requirements.txt   # cuBLAS·cuDNN이 대부분
$env:HF_HOME = "C:\Users\macham\voice-poc\hf"
.venv\Scripts\python -c "import ctranslate2; print(ctranslate2.get_cuda_device_count(), ctranslate2.get_supported_compute_types('cuda'))"
.venv\Scripts\python prep_zeroth.py data\zeroth           # 60MB 다운로드 → 457발화
```

모델(`large-v3-turbo` → `mobiuslabsgmbh/faster-whisper-large-v3-turbo`, 약 1.6GB)은 첫 실행 때 받는다.
**비용 드는 API 호출 없음.** 정리: `Remove-Item -Recurse C:\Users\macham\voice-poc`.

## 측정 세트

| 세트 | 내용 | 용도 |
|---|---|---|
| `zeroth` | Zeroth-Korean test 457발화, 낭독 뉴스, 16kHz, CC BY 4.0 | 속도 + 정확도 **상한선**(낭독이라 잡담보다 쉬움) |
| `record` | 주인이 직접 녹음한 잡담 30~50문장, 디스코드와 같은 마이크. 그중 10~15문장에 "아짱/클짱" 포함 | 실제 판단 기준 + PoC 3 호출어 예비 측정 |

`record`는 휴대폰/PC 녹음기로 문장마다 파일 하나, 정답 텍스트는 `manifest.jsonl`에 적는다
(`{"audio": "record/01.wav", "text": "클짱 오늘 저녁 뭐 먹을까"}`). 호출어 없는 문장이 오탐 측정용이라 섞여 있어야 한다.
가능하면 디스코드 채널에서 실제로 말한 것을 OBS 등으로 녹음하면 더 좋다(녹음 시 참여자 동의).

## 실행 매트릭스

```powershell
$m = "data\zeroth\manifest.jsonl"
.venv\Scripts\python bench.py $m --out out\A.jsonl                               # A 기본: GPU int8 beam1
.venv\Scripts\python bench.py $m --opus --out out\B.jsonl                        # B 디스코드 Opus 64kbps 왕복
.venv\Scripts\python bench.py $m --beam-size 5 --out out\C.jsonl                 # C 정확도↑ 대신 지연 얼마나?
.venv\Scripts\python bench.py $m --device cpu --cpu-threads 4 --limit 100 --out out\D.jsonl   # D CPU 폴백(게임 중 대비)
.venv\Scripts\python bench.py record\manifest.jsonl --opus --out out\R.jsonl     # R 실제 잡담 + 호출어
.venv\Scripts\python bench.py record\manifest.jsonl --opus --initial-prompt "아짱, 클짱" --out out\R1.jsonl   # R1 initial_prompt 효과(오탐 주의)
```

A가 지연 기준에 못 미치면 `--model medium`, `--model small`로 같은 측정을 반복해 속도·정확도 교환을 본다.
음악봇 영향: 실행 전후 `Get-Process mc-musicbot`로 pid가 그대로인지, 측정 중 CPU 사용률을 같이 기록한다.

## 통과 기준 (제안)

| 지표 | 기준 | 근거 |
|---|---|---|
| 2~5초 발화 지연 p95 | **≤ 1.0초** | [배경 문서](../../docs/background.md) 응답 지연 예산(받아쓰기 0.3~1초) |
| RTF | ≤ 0.2 | 여러 명이 동시에 말해도 밀리지 않게 |
| CER (zeroth) | ≤ 8% | 낭독체 상한선 |
| CER (record, Opus) | ≤ 15% | LLM이 맥락으로 보정할 수 있는 수준 |
| 호출어 인식 | ≥ 90%, 오탐 ≤ 2% | 못 알아들으면 쓸모없고, 오탐이면 API 비용 + 끼어들기 |
| 추가 VRAM | ≤ 2.5GB | 데스크톱·게임 몫 남기기 |

결과는 이 파일 아래 "결과" 절과 [`docs/background.md`](../../docs/background.md)의 PoC 순서에 적는다.

## 결과 (2026-10-12, Zeroth-Korean test)

| 측정 | 설정 | 발화 | CER | 지연 p50 / p95 | RTF | 비고 |
|---|---|---|---|---|---|---|
| A | GPU int8 beam1 | 457 | **5.23%** | 0.88 / 0.93초 (5~10초 발화) · 0.96 / 1.05초 (10초+) | 0.100 | 기준 |
| B2 | A + Opus 64kbps 왕복 | 457 | 6.07% | 0.87 / 0.93 · 0.95 / 1.03 | 0.097 | 디스코드 코덱 손실 ≈ CER +0.8%p. (첫 B는 꼬리 손실 버그로 음성 5초 짧았음 — 수정 후 CER 동일) |
| C | GPU int8 **beam5** | 457 | 5.61% | 0.94 / 1.03 · 1.06 / 1.19 | 0.108 | 더 느리고 더 부정확 → **beam1 유지** |
| D | **CPU** turbo int8 4스레드 | 100 | 4.34%* | 9.75 / 10.46초 | 0.981 | **실시간 불가**. CPU 폴백으로 못 씀 |
| Dsmall | CPU **small** int8 8스레드 | 50 | 8.12%* | 2.30 / 2.51 · 2.54 / 2.82 | 0.246 | CPU 폴백 후보(느리지만 동작) |
| S15 | B + 앞 1.5초만 | 150 | — | 0.78 / 0.96 | — | 짧은 발화 |
| S3 | B + 앞 3초만 | 150 | — | 0.81 / 0.82 | — | 짧은 발화 |
| S15np / S3np | 위와 같음, `initial_prompt` 없음 | 150 / 150 | — | 0.77 / 0.79 · 0.80 / 0.82 | — | 호출어 오탐 **0/150** |

| F | FLEURS 한국어 test(위키 문장 낭독, 다양한 녹음 환경) + Opus | 382 | **5.06%** | 0.88 / 0.93 · 0.94 / 1.05 (2~5초 n=5: 0.84 / 0.85) | 0.074 | 두 번째 공개 데이터셋(2026-10-13). Zeroth와 같은 수준 |
| H1 | F + `--hotwords "클짱 아짱"` | 382 | 4.92% | 0.94 / 1.06 (10초+) | 0.075 | 호출어 오탐 **4/382**(F는 0) → 안 씀 |
| H2 | F, 앞 1초만 | 382 | — | — | — | 호출어 오탐 0/382(기준선) |
| H3 | H2 + `--hotwords "클짱 아짱"` | 382 | — | — | — | 호출어 오탐 **271/382** — 잘린 음성을 "아짱"으로 지어냄 |
| H4 | H2 + `--hotwords "헤이 클짱 헤이 아짱"` (2026-10-15) | 294 | — | — | — | 호출어 오탐 **184/294(63%)** — "헤이 아짱" 136·"아짱" 44 |
| H5 | F, 앞 1.5초만 + 같은 힌트 | 328 | — | — | — | 호출어 오탐 **83/328(25%)** → 힌트는 1.5초 넘는 발화에도 위험. 웹 콘솔 옵션은 기본 끔 유지 |
| G1 | F, 모델 `ghost613/faster-whisper-large-v3-turbo-korean` | 382 | **12.57%** | 0.87 / 0.91 · 0.93 / 1.04 | 0.074 | Zeroth로만 학습한 미세조정판 — 과적합, 탈락 |

\* D·Dsmall은 앞 100·50발화만이라 A와 직접 비교 불가.

판정:
- **N-02 통과** — 2~5초 발화 p95 0.82초(≤ 1.0초). 단 **발화 길이와 무관하게 약 0.8초가 바닥**이다(Whisper가 30초 창으로 패딩). 짧게 잘라도 더 빨라지지 않는다.
- **CER 상한선(낭독체) 통과** — 6.07%(Opus 포함) ≤ 8%. 실제 잡담(`record`) 측정은 녹음 대기.
- **VRAM** 추가 약 1.1GB(1209 → 2347MiB) ≤ 2.5GB. 통과.
- **CPU 폴백**: turbo는 불가(RTF 0.98), small이면 발화당 약 2.5초. 게임 중에는 대답이 2초쯤 늦어지는 걸 감수하는 구성.
- 측정 내내 음악봇(pid 20180) 그대로, CPU 증가 속도도 평소와 같음(약 0.05코어).

⚠️ **`initial_prompt="아짱, 클짱"`이 호출어를 지어낸다.** 잘린/불분명한 음성에서 "클짱", "아참, 클짱",
"아짱, 클짱처럼…"을 만들어 냈다(S15 3/150, S3 1/150 오탐). 프롬프트를 빼면 0/150.
→ **호출어 힌트를 initial_prompt로 주지 않는다.** 인식률은 PoC 3에서 실제 녹음으로 재고, 부족하면
faster-whisper `hotwords`나 받아쓰기 후 퍼지 매칭으로 보완한다(SPEC F-20).
