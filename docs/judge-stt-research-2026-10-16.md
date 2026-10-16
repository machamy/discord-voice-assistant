# 판단 모델·받아쓰기 모델 조사 (2026-10-16)

## 1. 판단 모델 — OpenRouter 판단 전용 모델 실측

판단 모델은 이름이 애매하게 들린 호출이 정말 캐릭터를 부른 것인지 가려낸다(SPEC F-54·F-59).
OpenRouter Decisions API(`POST https://openrouter.ai/api/alpha/decisions`)는 글을 만들지 않고, 정해 둔 답의 확률만 돌려준다.
2026-10-16 기준 판단 전용 모델은 25개(별칭 포함)이고, 가격은 입력 100만 토큰당 $0~0.15, 출력은 무료다.

**측정 방법**
- 보조 PC에서 직접 호출했다. 질문 방식은 noul(예/아니오 확률)이다.
- 1차: 기존 17문장으로 22종을 돌렸다. 기준 문구에 시험 사례가 섞여 있어 너무 쉬웠다(대부분 17/17).
- 2차: 처음 보는 문장 26개(부름 12, 아님 14)로 상위 13종을 돌렸다.
- 3차: 받아쓰기가 깨진 문장 24개로 상위 10종을 돌렸다. 띄어쓰기 깨짐("실장 이 오늘 안 나왔 대"), 오타("외 안대"), 군말("어 그 뭐냐"), 목록에 없는 이름 변형("시장", "약장")을 넣었다.
- "최적 기준"은 같은 50문장에서 가장 잘 나오는 기준값을 고른 것이라 조금 낙관적이다. AUC(부름 문장의 확률이 아님 문장보다 높은 비율)가 기준값과 무관한 지표다.

| 모델 | 50문장 AUC | 기준 0.5 | 최적 기준(값) | 깨진 받아쓰기 24 | 중앙 지연 | 회당 비용 |
|---|---|---|---|---|---|---|
| `typesafe/jev-1.14` | 0.99 | 43/50 | 49/50 (0.67) | 24/24 | 411ms | $0.000027 |
| `liquid/d1-ultra` | 0.98 | 40/50 | 48/50 (0.90) | 22/24 | 506ms | $0.000015 |
| `respan/span-02` | 0.98 | 46/50 | 47/50 (0.65) | 22/24 | 472ms | $0.000004 |
| `cloudflare/clef` | 0.96 | 36/50 | 47/50 (0.88) | 22/24 | 604ms | $0.000018 |
| `cloudflare/clef-omni` | 0.94 | 35/50 | 47/50 (0.69) | 21/24 | 332ms | $0.000070 |
| `liquid/d1` | 0.98 | 39/50 | 46/50 (0.98) | 22/24 | 544ms | $0.000007 |
| `microsoft/microsoft-decision-1` | 0.97 | 41/50 | 46/50 (0.95) | 22/24 | 618ms | $0.000014 |
| `openai/gpt-6-luna-decisions` | 0.94 | 38/50 | 43/50 (0.96) | 20/24 | 426ms | $0.000049 |
| `inception/mercury-decide` | 0.91 | 36/50 | 43/50 (0.99) | 20/24 | 476ms | $0.000007 |
| `inception/mercury-decide:free` | 0.90 | 37/50 | 43/50 (0.99) | 20/24 | 254ms | $0.000000 |

1차(17문장, 기준 문구에 사례가 섞여 쉬움)에서만 본 모델:
- `perplexity/pplx-decider-v1.1-27b`: 기준 0.5에서 15/17, 중앙 580ms
- `scaledown/classify-1`: 기준 0.5에서 9/17, 중앙 584ms
- `nace-ai/drex-v2.0`: 기준 0.5에서 12/17, 중앙 918ms
- `nace-ai/drex-v1.5`: 기준 0.5에서 8/17, 중앙 779ms
- `upstage/solar-decide-flash`: 기준 0.5에서 8/17, 중앙 561ms
- `cloudflare/clef-flash`: 기준 0.5에서 10/17, 중앙 822ms
- `togethercomputer/tev1-4b-experimental`: 기준 0.5에서 10/17, 중앙 477ms
- `upstage/solar-decide`: 기준 0.5에서 9/17, 중앙 531ms
- `respan/span-01`: 기준 0.5에서 16/17, 중앙 508ms
- `respan/span-01-lite:free`: 기준 0.5에서 16/17, 중앙 286ms
- `jaredpalmer/kev-4b`: 기준 0.5에서 14/17, 중앙 745ms
- `typesafe/jev-1.13`: 기준 0.5에서 17/17, 중앙 440ms


2차에서 탈락한 모델: `respan/span-01-lite:free`(26개 중 9개가 오류), `respan/span-01`(AUC 0.70), `togethercomputer/tev1-4b-experimental`(AUC 0.85).

**질문 방식 비교** — 같은 50문장, 상위 10종
- "부름/안부름"(noul, 규칙이 고른 캐릭터를 불렀는지만 묻기)이 대체로 더 정확했다.
- "클짱/아짱/안부름 중 고르기"(choice)는 `liquid/d1` 44/50, `liquid/d1-ultra` 39/50, `jev-1.14` 37/50이었고, 나머지는 27~34였다.
- 그래서 noul 방식을 쓴다. 어느 캐릭터인지는 원래대로 호출어 규칙이 정한다.

**기준선 — 지금까지 쓰던 Groq `groq-fast`**(gpt-oss-20b, YES/NO, 봇 경유)
- 결과: 처음 보는 문장 18/21, 깨진 받아쓰기 14/19, 중앙 약 0.42초.
- 3초 간격으로 불러도 45번 중 10번이 HTTP 502로 실패했다. 무료 한도로 추정한다. 실패하면 원래 규칙대로 대답하므로, 사실상 판단을 건너뛴다.

**결정**
- 기본 판단 모델을 `jev`(`typesafe/jev-1.14`)로, 부름 기준 확률을 0.65로 정했다.
- 봇 경유 재측정에서 50개 중 48개를 맞혔고(깨진 받아쓰기 24/24), 중앙 0.36초였다. 실제 비용은 50번에 $0.0014였다.
- 틀린 2개는 "실장님이 오늘 회식 쏜대"(0.67), "실장님 커피 사 오래"(0.70)로, 기준을 0.72로 올리면 잡힌다. 다만 그만큼 부름 쪽을 놓칠 수 있다.
- 무료 대안은 `mercury-free`(43/50, 3인칭 언급을 자주 부름으로 봄)와 `groq-fast`다.

## 2. 받아쓰기(STT) 모델 조사 — 짧은 한국어 발화

서브에이전트가 공개 자료로 조사했다. 유료 API는 호출하지 않았다. FLEURS는 낭독체라, 친구끼리의 대화·은어 품질은 따로 실측해야 한다.

| 서비스 | 한국어 품질 근거 | 가격/시간 | 이름 바이어싱 | 메모 |
|---|---|---|---|---|
| ElevenLabs Scribe v2 | FLEURS-ko WER 7.43(15개 중 1위) | $0.22 + keyterm $0.05 | keyterm 최대 1,000개 | 짧은 클립 지연은 미측정 |
| OpenAI gpt-transcribe | 직전 4o-transcribe WER 9.04 | 약 $0.27 | `keywords` + `prompt` | 직전 세대 평균 4초 — 1초 목표에 불리 |
| Deepgram Nova-3(현재 폴백) | WER 19.00(하위권) | $0.26~0.31 | keyterm 500토큰 | 한국어가 약함 |
| Groq Whisper(현재 폴백) | v3 WER 13.66 | turbo $0.04, 요청당 최소 10초 과금 | `prompt` 224토큰 | 빠름, 무료 한도 있음 |
| Naver CLOVA Speech 단문 | 독립 수치 없음 | 단가 미확인 | `boostings` — 키워드 3자 이상 | "클짱"(2자)은 그대로 못 넣음 → "클짱아"처럼 |
| RTZR(리턴제로) | 공개 수치 없음 | ₩1,000, 무료 600분 | `keywords` | 일반 API는 비동기라 부적합, 스트리밍만 후보 |
| Soniox | FLEURS-ko 9.62(독립) | $0.10~0.12 | custom context | |
| Google Chirp 3 | 이전 Chirp 2가 10.96 | 약 $0.96 | speech adaptation | |
| Mistral Voxtral Transcribe 2 | 수치 없음 | $0.18 | context biasing | 실시간 모델은 오픈웨이트 |
| 로컬 faster-whisper large-v3-turbo(현재) | 낭독체 CER 2.22% | 무료 | `hotwords`, `initial_prompt` | `hotwords`는 매 구간에 들어감 |
| 로컬 한국어 파인튜닝 Whisper | turbo-ko WER 24→16 | 무료 | 같음 | `ghost613/faster-whisper-large-v3-turbo-korean`(CT2) |
| 로컬 Qwen3-ASR 1.7B | 커뮤니티 ko 파인튜닝 CER 5.37(자가 보고) | 무료 | `context` 인자 | Windows는 transformers 백엔드로 |
| 로컬 Nemotron 3.5 ASR Streaming 0.6B | FLEURS-ko CER 6.19 | 무료 | 핫워드 없음 | 스트리밍 |
| Parakeet·Canary | 한국어 없음 | | | |

OpenRouter에도 받아쓰기 API(`/api/v1/audio/transcriptions`, 26개 모델)가 있다. 다만 키워드는 공급자별 옵션으로 그대로 넘겨야 하고 따로 검증되지 않아, 바이어싱이 핵심인 이 용도에는 직접 API가 더 확실하다.

**추천(이름 오인식 해결 기준)**
1. 지금 쓰는 로컬 faster-whisper에 `hotwords`("클짱 아짱")를 넣고, 남는 오인식은 호출어 자리에서만 치환한다. 비용은 0이다. 다음 단계로 한국어 파인튜닝 CT2 모델과 A/B 시험한다.
2. 클라우드 폴백을 Deepgram에서 ElevenLabs Scribe v2 + keyterms로 바꾼다. 한국어 1위이고 시간당 약 $0.27이다.
3. Naver CLOVA Speech 단문 + boostings(3자 이상 변형으로 등록)를 쓴다.

**미검증 항목**
- CLOVA·RTZR의 독립 한국어 CER, CLOVA 단가
- Scribe v2 가격 차이(공식 $0.22 vs OpenRouter 환산 약 $0.11)
- 모든 엔진의 1~10초 클립 실측 지연

출처는 조사 원문의 링크다: verli.app 언어별 STT 정확도(FLEURS-ko 200클립), elevenlabs 요금·keyterm 문서, developers.openai.com, deepgram 문서, ncloud CLOVA Speech 단문 문서, developers.rtzr.ai, soniox, mistral.ai Voxtral Transcribe 2, huggingface(Qwen3-ASR, nemotron-3.5-asr, ghost613 CT2), openrouter.ai/docs/guides/overview/multimodal/stt.
