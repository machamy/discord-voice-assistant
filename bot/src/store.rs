//! `data/state.json` — 명령·웹 콘솔로 바꾸는 값. 대화 내용은 절대 넣지 않는다(SPEC N-06).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use crate::config::{Character, Judge, Limits, Voice};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub guilds: BTreeMap<u64, GuildState>,
    /// 웹 콘솔에서 바꾼 전역 값(없으면 config.toml) — SPEC F-43, F-44
    pub stt_chain: Option<Vec<String>>,
    pub tts_chain: Option<Vec<String>>,
    pub limits: Option<Limits>,
    /// 끈 LLM 프리셋 — SPEC F-44
    pub disabled_presets: BTreeSet<String>,
    /// 날짜(KST, YYYY-MM-DD) → 사용량 — SPEC F-33, F-43
    pub usage: BTreeMap<String, DayUsage>,
    /// 웹 콘솔 호출어 사전 — 캐릭터 → config 호출어에 더할 받아쓰기 변형
    pub wake_extra: BTreeMap<String, Vec<String>>,
    /// 위험 별칭 — 평소에도 쓰는 말로 받아써진 이름(실장·약정). 헤이·부르는 꼴·맨 앞 질문일 때만 호출
    pub wake_risky: BTreeMap<String, Vec<String>>,
    /// 받아쓰기 힌트(로컬 whisper hotwords) — 웹 콘솔에서 켬. 짧은 소리에 호출어를 지어내므로 min_ms 이상 발화에만
    pub stt_hint: SttHint,
    /// 웹 콘솔에서 추가한 캐릭터 — config.toml 캐릭터에 더해진다(같은 이름은 못 씀)
    pub custom_characters: BTreeMap<String, Character>,
    /// 판단 모델 설정(없으면 config [judge])
    pub judge: Option<Judge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttHint {
    pub on: bool,
    pub words: String,
    pub min_ms: u64,
}

impl Default for SttHint {
    fn default() -> Self {
        Self { on: false, words: "헤이 클짱 헤이 아짱".into(), min_ms: 1500 }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuildState {
    /// 유저 무시 — SPEC F-10
    pub ignore_users: BTreeSet<u64>,
    /// 역할 무시 — SPEC F-10
    pub ignore_roles: BTreeSet<u64>,
    /// 본인 거부 — SPEC F-11
    pub self_ignored: BTreeSet<u64>,
    /// 캐릭터별 덮어쓰기 — SPEC F-30, F-41
    pub characters: BTreeMap<String, CharOverride>,
    /// 긴 답을 올릴 텍스트 채널 — SPEC F-23 (`/설정 채널`, 없으면 /들어와 친 곳)
    pub text_channel: Option<u64>,
    /// 서버별 덮어쓰기 — `/설정 주인없으면`, `/설정 기억시간`
    pub require_owner_present: Option<bool>,
    pub buffer_minutes: Option<u64>,
    /// `/설정 호출방식` — Some(true)면 헤이를 붙여야만 호출
    pub hey_only: Option<bool>,
    /// `/설정 음성대답` — Some(false)면 글로만 대답
    pub voice_reply: Option<bool>,
    /// `/설정 음량` — 0~200(%)
    pub volume: Option<u32>,
    /// 장기 기억(기억 도구) — SPEC F-37. 누가 부탁해서 남긴 것만, 서버당 최대 50개
    pub memories: Vec<Memory>,
    /// 입장 음성 인사 — `/설정 인사말`. 끄면 말하지 않는다(채팅 녹음 고지는 F-12라 항상 올림)
    pub greeting_on: Option<bool>,
    /// 인사 내용(없으면 config [notice] voice)
    pub greeting: Option<String>,
    /// 지금 들어가 있는 음성 채널 — 봇이 재시작되면 여기로 다시 들어간다(나가기·끊김이면 지움)
    pub voice_channel: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub text: String,
    /// 부탁한 사람(이름은 표시용, ID는 본인 거부 때 지우기용)
    pub by: String,
    pub user: u64,
    /// KST YYYY-MM-DD
    pub date: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CharOverride {
    pub brain: Option<String>,
    /// None = 덮어쓰기 없음, Some("") = 대체 없음(명시), Some(이름) = 그 프리셋
    pub fallback: Option<String>,
    pub persona: Option<String>,
    pub voice: Option<BTreeMap<String, Voice>>,
    /// 말투 선택 — "성격" / "존댓말"
    pub style: Option<String>,
    /// 존댓말 페르소나 덮어쓰기
    pub persona_polite: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DayUsage {
    pub paid_calls: u32,
    pub free_calls: u32,
    pub usd: f64,
    /// 프리셋 → 집계
    pub presets: BTreeMap<String, PresetUsage>,
    /// 캐릭터 → 호출 수
    pub characters: BTreeMap<String, u32>,
    /// 상한 도달 안내를 한 서버 — SPEC F-32 "한 번 알린다"
    pub cap_notified: BTreeSet<u64>,
    /// 진행 중인 유료 호출의 예상 비용(예약). 저장하지 않는다
    #[serde(skip)]
    pub reserved_usd: f64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetUsage {
    pub calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub usd: f64,
}

pub fn today() -> String {
    let kst = FixedOffset::east_opt(9 * 3600).unwrap();
    Utc::now().with_timezone(&kst).format("%Y-%m-%d").to_string()
}

pub struct Store {
    path: PathBuf,
    pub state: State,
    /// 무시 목록이 바뀔 때마다 올린다 — 수신 쪽 캐시 무효화용
    pub version: u64,
}

impl Store {
    /// 파일이 없으면 빈 상태. 읽기·파싱이 실패하면 원본을 `state.json.broken-<시각>`으로 옮겨 보존하고 빈 상태로 시작한다
    /// (그대로 두면 다음 저장이 무시 목록·설정·사용량을 덮어써 복구할 수 없다).
    pub fn load(path: PathBuf) -> Self {
        let state = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                Self::quarantine(&path, &e.to_string());
                State::default()
            }
            Ok(s) => match serde_json::from_str(&s) {
                Ok(st) => st,
                Err(e) => {
                    Self::quarantine(&path, &e.to_string());
                    State::default()
                }
            },
        };
        Self { path, state, version: 1 }
    }

    fn quarantine(path: &std::path::Path, why: &str) {
        let ts = Utc::now().format("%Y%m%d-%H%M%S");
        let dst = path.with_extension(format!("json.broken-{ts}"));
        match std::fs::rename(path, &dst) {
            Ok(()) => tracing::error!("state.json을 읽지 못해 {}로 옮기고 빈 상태로 시작: {why}", dst.display()),
            Err(e) => tracing::error!("state.json을 읽지 못했고 옮기지도 못함({e}): {why}"),
        }
    }

    /// 임시 파일에 쓰고 바꿔치기 — 쓰다가 죽어도 파일이 깨지지 않게
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.state)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn guild(&mut self, g: u64) -> &mut GuildState {
        self.state.guilds.entry(g).or_default()
    }

    pub fn bump(&mut self) {
        self.version += 1;
    }

    pub fn day(&mut self) -> &mut DayUsage {
        let d = today();
        // 90일 넘은 집계는 버린다
        while self.state.usage.len() > 90 {
            let first = self.state.usage.keys().next().cloned().unwrap();
            self.state.usage.remove(&first);
        }
        self.state.usage.entry(d).or_default()
    }

    /// 특정 날짜(KST "YYYY-MM-DD")의 집계 — 유료 예약을 그 날짜에 정산할 때(자정을 넘긴 호출)
    pub fn day_of(&mut self, key: &str) -> &mut DayUsage {
        self.state.usage.entry(key.to_string()).or_default()
    }

    pub fn month_usd(&self) -> (u32, u32, f64) {
        let m = &today()[..7];
        self.state
            .usage
            .iter()
            .filter(|(k, _)| k.starts_with(m))
            .fold((0, 0, 0.0), |a, (_, u)| (a.0 + u.paid_calls, a.1 + u.free_calls, a.2 + u.usd))
    }
}
