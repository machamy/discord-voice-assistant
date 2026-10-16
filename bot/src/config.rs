//! `config.toml` — 비밀이 아닌 설정. 키는 `.env`에만 둔다(SPEC N-10).
//! 공급자 교체는 이 파일만 바꾸고 `/설정다시읽기`로 반영한다(SPEC N-11).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// 주인 디스코드 유저 ID (상위 프리셋·무시 목록·공급자 명령 권한, F-13 기준)
    pub owner_id: u64,
    #[serde(default)]
    pub listen: Listen,
    pub stt: Chain,
    pub tts: Chain,
    pub llm: Llm,
    pub characters: BTreeMap<String, Character>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub sidecar: Sidecar,
    #[serde(default)]
    pub notice: Notice,
    #[serde(default)]
    pub judge: Judge,
}

/// 판단 모델 — 이름이 애매하게 들렸을 때(비슷한 말·위험 별칭) "정말 캐릭터를 부른 말인가"를 작은 무료 모델에 묻는다(SPEC F-54)
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Judge {
    pub on: bool,
    /// 무료 프리셋만(판단마다 호출되므로)
    pub preset: String,
    /// "애매할 때" = 비슷한 말·위험 별칭만, "이름도" = 헤이 없이 이름만 부른 것까지(헤이·이어서·이모지는 항상 통과)
    pub scope: String,
    /// 이 시간 안에 답이 없으면 판단 없이 원래 규칙대로
    pub timeout_ms: u64,
    /// 판단 전용 모델(decisions)이 "부름" 확률을 이 값 이상으로 내면 부른 것으로. 비우면 프리셋 추천값(judge_threshold)
    pub threshold: Option<f64>,
}

impl Default for Judge {
    fn default() -> Self {
        Self { on: true, preset: "jev".into(), scope: "애매할 때".into(), timeout_ms: 2500, threshold: None }
    }
}

pub const JUDGE_SCOPES: [&str; 2] = ["애매할 때", "이름도"];

impl Judge {
    pub fn validate(&self, cfg: &Config) -> std::result::Result<(), String> {
        if !JUDGE_SCOPES.contains(&self.scope.as_str()) {
            return Err(format!("판단 범위는 {} 중 하나", JUDGE_SCOPES.join("·")));
        }
        if !(300..=10_000).contains(&self.timeout_ms) {
            return Err("판단 대기 시간은 300~10000ms".into());
        }
        if self.threshold.is_some_and(|t| !(0.05..=0.99).contains(&t)) {
            return Err("부름 기준 확률은 0.05~0.99".into());
        }
        // 꺼 두었으면 프리셋은 따지지 않는다(나중에 켤 때 검사)
        if !self.on {
            return Ok(());
        }
        match cfg.llm.presets.get(&self.preset) {
            None => Err(format!("판단 모델 프리셋 '{}' 없음", self.preset)),
            Some(p) if p.trains_on_input => Err(format!("판단 모델에 입력을 학습에 쓰는 등급('{}')은 못 씀", self.preset)),
            // 판단 전용 모델은 유료여도 된다(회당 0.0001달러 아래, 하루 사용액에 더해지고 상한에 닿으면 안 부름)
            Some(p) if p.is_decision() => Ok(()),
            Some(p) if !p.free => Err(format!("판단 모델은 무료 프리셋이나 판단 전용 프리셋만('{}'는 유료)", self.preset)),
            Some(p) if p.provider == "anthropic" => Err("판단 모델은 OpenAI 호환 프리셋만".into()),
            _ => Ok(()),
        }
    }

    /// 실제로 쓸 부름 기준 확률 — 직접 정한 값 > 프리셋 추천값 > 0.5
    pub fn threshold_for(&self, p: &Preset) -> f64 {
        self.threshold.or(p.judge_threshold).unwrap_or(0.5)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Listen {
    /// 대화 버퍼 보관 시간(분) — SPEC N-06
    pub buffer_minutes: u64,
    /// 주인이 같은 채널에 없으면 듣지 않는다 — SPEC F-13
    pub require_owner_present: bool,
    /// true면 "헤이 클짱"처럼 헤이를 붙여 불러야만 대답한다(이름만 부르면 무시) — 서버별로 `/설정 호출방식`
    pub hey_only: bool,
    /// false면 음성으로 대답하지 않고 글로만 올린다 — 서버별로 `/설정 음성대답`
    pub voice_reply: bool,
    /// 음성 대답 음량(%, 0~200) — 서버별로 `/설정 음량`
    pub volume: u32,
    /// 이만큼 조용하면 발화 끝으로 본다(ms) — SPEC F-02
    pub silence_ms: u64,
    pub min_utterance_ms: u64,
    pub max_utterance_ms: u64,
    /// 평균 절대 진폭(16bit)이 이보다 작으면 잡음으로 버린다
    pub min_level: u32,
    /// 호출어만 말하고 멈췄을 때 다음 발화를 질문으로 기다리는 시간(초) — SPEC F-21
    pub followup_secs: u64,
    /// 멤버 역할 갱신 주기(초) — SPEC F-10
    pub member_refresh_secs: u64,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            buffer_minutes: 15,
            require_owner_present: true,
            hey_only: false,
            voice_reply: true,
            volume: 100,
            silence_ms: 600,
            min_utterance_ms: 400,
            max_utterance_ms: 20_000,
            min_level: 120,
            followup_secs: 8,
            member_refresh_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chain {
    pub chain: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Llm {
    pub presets: BTreeMap<String, Preset>,
    /// 캐릭터의 두뇌·대체 두뇌가 모두 실패하면 차례로 시도할 무료 프리셋(SPEC F-32) — 무료 한도가 찬 경우 대비
    #[serde(default)]
    pub free_chain: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Preset {
    /// "anthropic" | "openai" | "openai-compatible"
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// 키 환경변수 이름. 비우면 공급자 기본값(ANTHROPIC_API_KEY / OPENAI_API_KEY)
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// 주인만 고를 수 있음 — SPEC F-31
    #[serde(default)]
    pub owner_only: bool,
    /// 무료 공급자(하루 상한에 안 잡힘) — SPEC F-32
    #[serde(default)]
    pub free: bool,
    /// 입력을 학습에 쓰는 무료 등급 — SPEC N-12. 켜면 경고
    #[serde(default)]
    pub trains_on_input: bool,
    /// 1M 토큰당 달러
    #[serde(default)]
    pub price_in: f64,
    #[serde(default)]
    pub price_out: f64,
    /// OpenAI 호환 스트리밍에 사용량 청크를 요청(stream_options.include_usage). 거부하는 서버만 false
    #[serde(default = "yes")]
    pub stream_usage: bool,
    /// 캐시 읽기 1M 토큰당 USD(없으면 price_in의 0.1배)
    #[serde(default)]
    pub price_cache_read: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 생각(추론) 정도. Anthropic은 output_config.effort(기본 low), OpenAI 호환은 reasoning_effort(없으면 안 보냄).
    /// 모델마다 받는 값이 다르다 — 예: gpt-6-luna "none", gpt-6.1-sol "low", Groq gpt-oss "low", Gemini 3.x "minimal"
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// OpenAI 호환 요청 본문에 그대로 더할 값(예: Groq include_reasoning = false)
    #[serde(default)]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
    /// 판단 모델로 쓸 때 추천 부름 기준 확률(decisions 공급자) — SPEC F-59
    #[serde(default)]
    pub judge_threshold: Option<f64>,
    /// 판단 모델 고르기 화면에 같이 보여 줄 실측 메모(정확도·속도)
    #[serde(default)]
    pub judge_note: Option<String>,
}

fn yes() -> bool {
    true
}

pub const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 모델마다 받는 effort가 다르다(2026-10 문서 기준) — 400을 미리 막는다. 모르는 모델은 통과
pub fn effort_fits_model(model: &str, e: &str) -> std::result::Result<(), String> {
    let m = model.rsplit('/').next().unwrap_or(model);
    if m.starts_with("gpt-6") {
        if e == "minimal" {
            return Err("GPT-6 계열은 minimal을 받지 않음".into());
        }
        // none은 Luna와 구형 gpt-6-sol만
        if e == "none" && !(m.starts_with("gpt-6-luna") || m == "gpt-6-sol") {
            return Err(format!("{m}는 reasoning_effort none을 받지 않음(low 이상)"));
        }
    }
    if m.starts_with("gpt-oss") && !matches!(e, "low" | "medium" | "high") {
        return Err("gpt-oss는 low·medium·high만".into());
    }
    if m.starts_with("gemini-3") && !matches!(e, "minimal" | "low" | "medium" | "high") {
        return Err("Gemini 3.x는 minimal·low·medium·high만(생각을 끌 수 없음)".into());
    }
    Ok(())
}

impl Preset {
    pub fn key_env(&self) -> String {
        if let Some(k) = &self.api_key_env {
            return k.clone();
        }
        match self.provider.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY".into(),
            "decisions" => "OPENROUTER_API_KEY".into(),
            _ => "OPENAI_API_KEY".into(),
        }
    }
    /// 판단 전용 모델(OpenRouter Decisions API) — 글을 만들지 않고 정한 답의 확률만 낸다. 대답 두뇌로는 못 쓴다
    pub fn is_decision(&self) -> bool {
        self.provider == "decisions"
    }
    pub fn has_key(&self) -> bool {
        // 로컬 Ollama 같은 키 없는 공급자는 api_key_env = "" 로 표시
        if self.api_key_env.as_deref() == Some("") {
            return true;
        }
        std::env::var(self.key_env()).map(|v| !v.trim().is_empty()).unwrap_or(false)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Character {
    /// 화면·텍스트에 쓰는 이름
    pub name: String,
    #[serde(default)]
    pub emoji: String,
    /// 호출어와 받아쓰기 변형 — SPEC F-20
    pub wake: Vec<String>,
    pub brain: String,
    #[serde(default)]
    pub fallback: Option<String>,
    /// 말투(시스템 프롬프트) — SPEC §3. 캐릭터 카드 구조(정체성·성격·말투·관계·내가 아는 나·예시·금지)
    pub persona: String,
    /// 질문 바로 뒤에 붙이는 한 줄 말투 상기 — 대화가 길어져도 말투가 흐려지지 않게(캐시 지점 뒤라 비용 거의 없음)
    #[serde(default)]
    pub reminder: String,
    /// 존댓말 버전 페르소나 — 서버별로 `/설정 말투선택`에서 "존댓말"을 고르면 이걸 쓴다(비우면 성격 페르소나 + 존댓말 지시)
    #[serde(default)]
    pub persona_polite: String,
    #[serde(default)]
    pub reminder_polite: String,
    /// 기본 말투: "성격"(캐릭터 카드, 반말) 또는 "존댓말"
    #[serde(default = "default_style")]
    pub style: String,
    /// 공급자별 목소리 — 키는 TTS 공급자 이름
    #[serde(default)]
    pub voice: BTreeMap<String, Voice>,
}

fn default_style() -> String {
    STYLES[0].into()
}

/// 말투 종류 — 성격(캐릭터 카드 그대로) / 존댓말(공손한 해요체)
pub const STYLES: [&str; 2] = ["성격", "존댓말"];

impl Character {
    pub fn polite(&self) -> bool {
        self.style == STYLES[1]
    }

    /// 지금 말투에 맞는 페르소나
    pub fn active_persona(&self) -> String {
        if !self.polite() {
            return self.persona.trim().to_string();
        }
        if !self.persona_polite.trim().is_empty() {
            return self.persona_polite.trim().to_string();
        }
        format!(
            "{}

[말투] 위 설정의 성격과 관계는 그대로 두되, 반말 대신 상냥하고 공손한 존댓말(해요체)로만 말한다. 비꼬거나 놀리는 말은 하지 않는다.",
            self.persona.trim()
        )
    }

    pub fn active_reminder(&self) -> String {
        let r = if self.polite() { &self.reminder_polite } else { &self.reminder };
        if !r.trim().is_empty() {
            r.trim().to_string()
        } else if self.polite() {
            "(존댓말로, <말>부터 시작)".into()
        } else {
            "(<말>부터 시작)".into()
        }
    }
}

/// 캐릭터 한 명 검증 — config.toml과 웹 콘솔에서 추가한 캐릭터 공통
pub fn validate_character(k: &str, ch: &Character, presets: &BTreeMap<String, Preset>) -> std::result::Result<(), String> {
    if k.trim().is_empty() || k.chars().count() > 25 || k.trim() != k {
        return Err(format!("캐릭터 이름 '{k}'은 1~25자(앞뒤 공백 없이)"));
    }
    if ch.name.trim().is_empty() || ch.name.chars().count() > 25 {
        return Err(format!("캐릭터 {k}: 표시 이름은 1~25자"));
    }
    if ch.emoji.chars().count() > 8 {
        return Err(format!("캐릭터 {k}: 이모지는 8자 이내"));
    }
    match presets.get(&ch.brain) {
        None => return Err(format!("캐릭터 {k}: 두뇌 프리셋 '{}' 없음", ch.brain)),
        Some(p) if p.is_decision() => return Err(format!("캐릭터 {k}: '{}'는 판단 전용이라 두뇌로 못 씀", ch.brain)),
        _ => {}
    }
    if let Some(f) = &ch.fallback {
        match presets.get(f) {
            None => return Err(format!("캐릭터 {k}: 대체 프리셋 '{f}' 없음")),
            Some(p) if p.is_decision() => return Err(format!("캐릭터 {k}: '{f}'는 판단 전용이라 대체 두뇌로 못 씀")),
            _ => {}
        }
    }
    if ch.wake.is_empty() || ch.wake.len() > 50 || ch.wake.iter().any(|w| w.trim().is_empty() || w.chars().count() > 20) {
        return Err(format!("캐릭터 {k}: 호출어는 1~50개, 각 1~20자"));
    }
    crate::convo::Wake::compile(&ch.wake).map_err(|e| format!("캐릭터 {k}: 호출어 정규식 오류: {e}"))?;
    for (prov, v) in &ch.voice {
        validate_voice(prov, v).map_err(|e| format!("캐릭터 {k}: {e}"))?;
    }
    if !STYLES.contains(&ch.style.as_str()) {
        return Err(format!("캐릭터 {k}: 말투(style)는 {} 중 하나", STYLES.join("·")));
    }
    for (what, t, max) in [("persona", &ch.persona, 6000), ("persona_polite", &ch.persona_polite, 6000), ("reminder", &ch.reminder, 300), ("reminder_polite", &ch.reminder_polite, 300)] {
        if t.chars().count() > max {
            return Err(format!("캐릭터 {k}: {what}는 {max}자 이내"));
        }
    }
    if ch.persona.trim().is_empty() {
        return Err(format!("캐릭터 {k}: persona(말투)가 비었음"));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Voice {
    #[serde(default)]
    pub voice: String,
    #[serde(default = "default_rate")]
    pub rate: String,
    #[serde(default = "default_pitch")]
    pub pitch: String,
}

fn default_rate() -> String {
    "+0%".into()
}
fn default_pitch() -> String {
    "+0Hz".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Limits {
    /// 하루 유료 호출 수 상한 — SPEC F-32
    pub daily_calls: u32,
    /// 하루 유료 예상 금액 상한(USD)
    pub daily_usd: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { daily_calls: 200, daily_usd: 1.0 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Web {
    pub bind: String,
    /// 콘솔 바깥 주소(디스코드 로그인 돌아올 곳 = public_url + /auth/discord/callback). 비우면 디스코드 로그인 안 씀
    pub public_url: String,
    /// 디스코드 로그인을 허락할 사람(주인은 항상)
    pub allowed_users: Vec<u64>,
    /// 비밀번호 로그인(CONSOLE_PASSWORD) — 비우면 디스코드 로그인이 준비 안 됐을 때만 연다
    pub password_login: Option<bool>,
}

impl Default for Web {
    fn default() -> Self {
        Self { bind: "0.0.0.0:8694".into(), public_url: String::new(), allowed_users: vec![], password_login: None }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Sidecar {
    pub autostart: bool,
    pub python: String,
    pub script: String,
    pub port: u16,
    /// 시작할 때 미리 올릴 STT 모델
    pub preload: String,
}

impl Default for Sidecar {
    fn default() -> Self {
        Self {
            autostart: true,
            python: "sidecar/.venv/Scripts/python.exe".into(),
            script: "sidecar/app.py".into(),
            port: 8695,
            preload: "local-whisper:cuda:large-v3-turbo".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Notice {
    /// 입장 시 채팅 고지 — SPEC F-12
    pub text: String,
    /// 입장 시 음성 고지(짧게)
    pub voice: String,
    /// 상태 메시지
    pub status: String,
}

impl Default for Notice {
    fn default() -> Self {
        Self {
            text: "🎙️ 아짱·클짱이 대화를 듣고 있어요. 음성은 저장하지 않고, 받아쓴 글은 최근 15분만 기억했다가 지워요. 부르면 최근 대화 몇 분이 대답을 만드는 AI 회사(Anthropic·Groq 등)로 보내져요. 듣지 않길 원하면 `/무시해줘`".into(),
            voice: "아짱 클짱이 대화를 듣고 있어요. 원하지 않으면 무시해줘 명령을 써 주세요.".into(),
            status: "🎙️ 듣는 중 · /무시해줘".into(),
        }
    }
}

/// STT 체인 항목 형식 — 웹 콘솔과 같은 규칙
pub fn stt_provider_ok(x: &str) -> bool {
    if matches!(x, "groq-whisper" | "groq-whisper-turbo" | "deepgram" | "openai-transcribe") {
        return true;
    }
    let mut it = x.splitn(3, ':');
    // 모델: faster-whisper 이름(large-v3-turbo) 또는 Hugging Face 저장소(owner/name) — 경로 이동(..)은 막는다
    matches!((it.next(), it.next(), it.next()), (Some("local-whisper"), Some("cuda" | "cpu"), Some(m))
        if m.matches('/').count() <= 1
            && m.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
            // 각 부분은 영문·숫자로 시작 — `.`·`..`·`./x` 같은 경로 조각을 막는다(HF owner/name 또는 모델 이름만)
            && m.split('/').all(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())))
}

pub const TTS_PROVIDERS: [&str; 6] = ["edge-tts", "azure-tts", "melotts", "elevenlabs", "openai-tts", "typecast"];

/// 목소리 설정 검증 — 웹·명령·설정 파일 공통(SSML·요청에 들어가는 값이라 형식을 좁힌다)
pub fn validate_voice(provider: &str, v: &Voice) -> std::result::Result<(), String> {
    if !TTS_PROVIDERS.contains(&provider) {
        return Err(format!("모르는 TTS 공급자 '{provider}'"));
    }
    let name_ok = v.voice.len() <= 100 && v.voice.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    if !name_ok {
        return Err(format!("목소리 이름 형식 오류: '{}'", v.voice));
    }
    let num = |s: &str, unit: &str, lo: i64, hi: i64| -> bool {
        s.strip_suffix(unit)
            .and_then(|n| n.parse::<i64>().ok())
            .is_some_and(|n| (lo..=hi).contains(&n) && (s.starts_with('+') || s.starts_with('-')))
    };
    if !num(&v.rate, "%", -50, 100) {
        return Err(format!("속도 형식 오류: '{}' (예: +10%)", v.rate));
    }
    if !num(&v.pitch, "Hz", -50, 50) {
        return Err(format!("높낮이 형식 오류: '{}' (예: -5Hz)", v.pitch));
    }
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("{} 읽기 실패", path.display()))?;
        let mut c: Config = toml::from_str(&s).with_context(|| format!("{} 형식 오류", path.display()))?;
        // 판단 모델 설정이 틀렸다고 봇이 안 뜨면 안 된다 — 끄고 알린다([judge]가 없고 groq-fast도 없는 옛 설정 등)
        if let Err(e) = c.judge.validate(&c) {
            tracing::warn!("[judge] {e} — 판단 모델을 끄고(기본값) 시작");
            c.judge = Judge { on: false, ..Judge::default() };
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.owner_id == 0 {
            bail!("owner_id는 0일 수 없다(디스코드 유저 ID)");
        }
        for (k, ch) in &self.characters {
            validate_character(k, ch, &self.llm.presets).map_err(|e| anyhow::anyhow!(e))?;
        }
        self.judge.validate(self).map_err(|e| anyhow::anyhow!("[judge] {e}"))?;
        let l = &self.listen;
        let rng = |v: u64, lo: u64, hi: u64, name: &str| -> Result<()> {
            if (lo..=hi).contains(&v) { Ok(()) } else { bail!("[listen] {name}은(는) {lo}~{hi}") }
        };
        rng(l.buffer_minutes, 1, 120, "buffer_minutes")?;
        rng(l.silence_ms, 100, 5000, "silence_ms")?;
        rng(l.min_utterance_ms, 100, 10_000, "min_utterance_ms")?;
        rng(l.max_utterance_ms, 1000, 60_000, "max_utterance_ms")?;
        rng(l.min_level as u64, 0, 32_767, "min_level")?;
        rng(l.followup_secs, 1, 120, "followup_secs")?;
        rng(l.member_refresh_secs, 30, 86_400, "member_refresh_secs")?;
        rng(l.volume as u64, 0, 200, "volume")?;
        if l.min_utterance_ms >= l.max_utterance_ms {
            bail!("[listen] min_utterance_ms < max_utterance_ms 이어야 한다");
        }
        if !(self.limits.daily_usd.is_finite() && self.limits.daily_usd >= 0.0) {
            bail!("[limits] daily_usd는 0 이상");
        }
        if self.stt.chain.is_empty() || self.stt.chain.len() > 6 || !self.stt.chain.iter().all(|x| stt_provider_ok(x)) {
            bail!("[stt] chain: 1~6개, local-whisper:<cuda|cpu>:<모델 또는 owner/name> / groq-whisper / groq-whisper-turbo / deepgram / openai-transcribe 중에서");
        }
        if self.tts.chain.is_empty() || self.tts.chain.len() > 6 || !self.tts.chain.iter().all(|x| TTS_PROVIDERS.contains(&x.as_str())) {
            bail!("[tts] chain: 1~6개, {} 중에서", TTS_PROVIDERS.join("/"));
        }
        if self.characters.is_empty() || self.characters.len() > 25 {
            bail!("캐릭터는 1~25개(디스코드 선택지 한도)");
        }
        for (k, p) in &self.llm.presets {
            // 명령 자동완성 값·웹 콘솔 id로 쓰인다 — 짧은 영문·숫자·-_. 만
            if k.is_empty() || k.len() > 32 || !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
                bail!("프리셋 이름 '{k}': 1~32자 영문·숫자·-_.");
            }
            if p.model.trim().is_empty() || p.model.len() > 100 {
                bail!("프리셋 {k}: model은 1~100자");
            }
            // 가격이 음수·무한이면 예상 금액 예약이 깨져 상한이 무력화된다
            if !(p.price_in.is_finite() && p.price_in >= 0.0 && p.price_out.is_finite() && p.price_out >= 0.0) {
                bail!("프리셋 {k}: price_in·price_out은 0 이상의 유한한 값");
            }
            if p.price_cache_read.is_some_and(|c| !(c.is_finite() && c >= 0.0)) {
                bail!("프리셋 {k}: price_cache_read는 0 이상의 유한한 값");
            }
            if p.max_tokens.is_some_and(|m| !(1..=8192).contains(&m)) {
                bail!("프리셋 {k}: max_tokens는 1~8192");
            }
            if !matches!(p.provider.as_str(), "anthropic" | "openai" | "openai-compatible" | "decisions") {
                bail!("프리셋 {k}: 알 수 없는 공급자 '{}'", p.provider);
            }
            if p.judge_threshold.is_some_and(|t| !(0.05..=0.99).contains(&t)) {
                bail!("프리셋 {k}: judge_threshold는 0.05~0.99");
            }
            if p.judge_note.as_ref().is_some_and(|n| n.chars().count() > 200) {
                bail!("프리셋 {k}: judge_note는 200자 이내");
            }
            if p.is_decision() && (p.reasoning_effort.is_some() || p.extra.is_some()) {
                bail!("프리셋 {k}: decisions 공급자는 reasoning_effort·extra를 받지 않음");
            }
            if p.provider == "openai-compatible" && p.base_url.is_none() {
                bail!("프리셋 {k}: openai-compatible은 base_url 필요");
            }
            if let Some(e) = &p.reasoning_effort {
                if !EFFORTS.contains(&e.as_str()) {
                    bail!("프리셋 {k}: reasoning_effort는 {} 중 하나", EFFORTS.join("·"));
                }
                if p.provider == "anthropic" && matches!(e.as_str(), "none" | "minimal") {
                    bail!("프리셋 {k}: Anthropic effort는 low 이상");
                }
                effort_fits_model(&p.model, e).map_err(|m| anyhow::anyhow!("프리셋 {k}: {m}"))?;
            }
            if let Some(x) = &p.extra {
                // 봇이 직접 정하는 필드는 덮어쓰지 못하게(effort 검증·스트리밍 사용량 수집·응답 개수)
                const RESERVED: [&str; 12] = [
                    "model", "messages", "system", "max_tokens", "max_completion_tokens", "stream", "stream_options",
                    "reasoning_effort", "reasoning", "n", "tools", "tool_choice",
                ];
                if p.provider == "anthropic" {
                    bail!("프리셋 {k}: extra는 OpenAI 호환 공급자에만");
                }
                if let Some(bad) = x.keys().find(|k| RESERVED.contains(&k.as_str())) {
                    bail!("프리셋 {k}: extra에 '{bad}'는 넣을 수 없음");
                }
            }
        }
        if self.llm.free_chain.len() > 6 {
            bail!("free_chain은 6개까지");
        }
        for f in &self.llm.free_chain {
            match self.llm.presets.get(f) {
                None => bail!("free_chain: 프리셋 '{f}' 없음"),
                Some(p) if !p.free => bail!("free_chain: '{f}'는 무료 프리셋이 아님"),
                Some(p) if p.is_decision() => bail!("free_chain: '{f}'는 판단 전용이라 대답 두뇌로 못 씀"),
                // 학습에 쓰이는 무료 등급은 주인이 캐릭터 두뇌로 직접 고를 때만(SPEC N-12) — 자동 대체로는 안 씀
                Some(p) if p.trains_on_input => bail!("free_chain: '{f}'는 입력을 학습에 쓰는 등급이라 자동 대체에 넣을 수 없음"),
                _ => {}
            }
        }
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 저장소의 예시 설정이 실제로 읽히고 검증을 통과해야 한다(배포본 config.toml의 원본)
    #[test]
    fn example_config_is_valid() {
        let c: Config = toml::from_str(include_str!("../config.example.toml")).expect("형식");
        c.validate().expect("검증");
        assert!(c.llm.free_chain.iter().all(|f| c.llm.presets[f].free));
        assert_eq!(c.llm.presets["luna"].reasoning_effort.as_deref(), Some("none"));
        assert!(c.llm.presets["groq-free"].extra.as_ref().is_some_and(|x| x["include_reasoning"] == false));
    }

    #[test]
    fn styles_and_judge() {
        let c: Config = toml::from_str(include_str!("../config.example.toml")).expect("형식");
        // 두 캐릭터 모두 존댓말 페르소나가 있고, 말투를 바꾸면 그쪽을 쓴다
        for ch in c.characters.values() {
            assert!(!ch.persona_polite.trim().is_empty());
            let mut p = ch.clone();
            assert_eq!(p.active_persona(), ch.persona.trim());
            p.style = "존댓말".into();
            assert_eq!(p.active_persona(), ch.persona_polite.trim());
            assert!(p.active_reminder().contains("존댓말"));
            // 존댓말 글이 없으면 성격 글 + 존댓말 지시
            p.persona_polite.clear();
            assert!(p.active_persona().starts_with(ch.persona.trim()) && p.active_persona().contains("존댓말"));
        }
        assert!(c.judge.validate(&c).is_ok());
        let mut j = c.judge.clone();
        j.preset = "haiku".into();
        assert!(j.validate(&c).is_err(), "유료 프리셋은 판단 모델로 못 씀");
        j.preset = "gemini-free".into();
        assert!(j.validate(&c).is_err(), "학습 등급은 못 씀");
        // 웹 콘솔 캐릭터 검증: 이름 공백·말투 종류·빈 페르소나
        let mut x = c.characters["클짱"].clone();
        assert!(validate_character("젬짱", &x, &c.llm.presets).is_ok());
        assert!(validate_character(" 젬짱", &x, &c.llm.presets).is_err());
        x.style = "반말".into();
        assert!(validate_character("젬짱", &x, &c.llm.presets).is_err());
        x.style = "성격".into();
        x.persona = " ".into();
        assert!(validate_character("젬짱", &x, &c.llm.presets).is_err());
    }

    #[test]
    fn decision_presets() {
        let c: Config = toml::from_str(include_str!("../config.example.toml")).expect("형식");
        c.validate().expect("예시 config 검증");
        // 기본 판단 모델은 판단 전용(유료지만 허용), 기준 확률은 프리셋 추천값
        assert_eq!(c.judge.preset, "jev");
        assert!(c.judge.validate(&c).is_ok());
        assert_eq!(Judge::default().preset, "jev", "[judge]를 생략해도 같은 기본값");
        assert_eq!(c.judge.threshold_for(&c.llm.presets["jev"]), 0.65);
        let mut j = c.judge.clone();
        j.threshold = Some(0.8);
        assert_eq!(j.threshold_for(&c.llm.presets["jev"]), 0.8);
        j.threshold = Some(1.0);
        assert!(j.validate(&c).is_err(), "기준 확률 범위");
        j.threshold = None;
        j.preset = "groq-fast".into();
        assert!(j.validate(&c).is_ok(), "무료 OpenAI 호환도 여전히 됨");
        assert_eq!(j.threshold_for(&c.llm.presets["groq-fast"]), 0.5);
        // 판단 전용은 대답 두뇌·대체 두뇌·무료 체인으로 못 씀
        let mut x = c.characters["클짱"].clone();
        x.brain = "jev".into();
        assert!(validate_character("젬짱", &x, &c.llm.presets).is_err());
        x.brain = "haiku".into();
        x.fallback = Some("mercury-free".into());
        assert!(validate_character("젬짱", &x, &c.llm.presets).is_err());
        let mut c2 = c.clone();
        c2.llm.free_chain = vec!["mercury-free".into()];
        assert!(c2.validate().is_err());
        // 판단 전용에 reasoning_effort·extra 금지, 키 이름 기본값
        let mut c3 = c.clone();
        c3.llm.presets.get_mut("jev").unwrap().reasoning_effort = Some("low".into());
        assert!(c3.validate().is_err());
        let mut p = c.llm.presets["jev"].clone();
        p.api_key_env = None;
        assert_eq!(p.key_env(), "OPENROUTER_API_KEY");
    }

    #[test]
    fn effort_rules() {
        assert!(effort_fits_model("gpt-6-luna", "none").is_ok());
        assert!(effort_fits_model("gpt-6.1-sol", "none").is_err());
        assert!(effort_fits_model("gpt-6-astra", "none").is_err());
        assert!(effort_fits_model("gpt-6-luna", "minimal").is_err());
        assert!(effort_fits_model("openai/gpt-oss-120b", "low").is_ok());
        assert!(effort_fits_model("openai/gpt-oss-120b", "none").is_err());
        assert!(effort_fits_model("gemini-3.5-flash-lite", "minimal").is_ok());
        assert!(effort_fits_model("gemini-3.5-flash-lite", "none").is_err());
        assert!(effort_fits_model("qwen3.5:4b", "none").is_ok());
        assert!(effort_fits_model("gemini-3.5-flash-lite", "max").is_err());
        assert!(effort_fits_model("gemini-3.5-flash-lite", "xhigh").is_err());
        assert!(effort_fits_model("claude-haiku-5-5", "high").is_ok());
        // Anthropic 5.5는 low~max 전부 유효(생각 끄기만 high 이하) — 봇은 xhigh·max에서 thinking을 안 보낸다
        assert!(effort_fits_model("claude-haiku-5-5", "max").is_ok());
        assert!(effort_fits_model("claude-opus-5-5", "xhigh").is_ok());
    }

    #[test]
    fn stt_names() {
        assert!(stt_provider_ok("local-whisper:cuda:large-v3-turbo"));
        assert!(stt_provider_ok("local-whisper:cuda:ghost613/faster-whisper-large-v3-turbo-korean"));
        assert!(stt_provider_ok("deepgram"));
        assert!(!stt_provider_ok("local-whisper:cuda:../x"));
        assert!(!stt_provider_ok("local-whisper:cuda:/abs"));
        assert!(!stt_provider_ok("local-whisper:cuda:a/b/c"));
        assert!(!stt_provider_ok("local-whisper:cuda:./model"));
        assert!(!stt_provider_ok("local-whisper:cuda:./"));
        assert!(!stt_provider_ok("local-whisper:cuda:.hidden"));
        assert!(!stt_provider_ok("local-whisper:cuda:"));
        assert!(!stt_provider_ok("local-whisper:gpu:x"));
    }
}