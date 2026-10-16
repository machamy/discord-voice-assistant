//! 봇 전체가 같이 쓰는 상태.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serenity::all::Context;
use songbird::Songbird;

use crate::config::{Character, Config, Limits};
use crate::store::Store;
use crate::voice::Session;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SidecarHealth {
    pub ok: bool,
    pub owned_pid: Option<u32>,
    pub restarts: u32,
    pub last_error: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LastUsed {
    pub stt: String,
    pub llm: String,
    pub tts: String,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RateInfo {
    pub remaining_requests: Option<String>,
    pub remaining_tokens: Option<String>,
    pub at: String,
}

pub struct App {
    pub cfg_path: PathBuf,
    /// 실제로 쓰는 설정 = config.toml + 웹 콘솔에서 추가한 캐릭터(refresh_cfg)
    pub cfg: RwLock<Arc<Config>>,
    /// config.toml 그대로
    pub cfg_file: RwLock<Arc<Config>>,
    pub store: Mutex<Store>,
    pub sessions: tokio::sync::Mutex<HashMap<u64, Arc<Session>>>,
    pub http: reqwest::Client,
    pub ctx: Mutex<Option<Context>>,
    pub songbird: Arc<Songbird>,
    pub sidecar: Mutex<SidecarHealth>,
    pub ratelimits: Mutex<HashMap<String, RateInfo>>,
    /// 말 끝남 → 첫 소리(ms) 최근 200개 — SPEC N-01
    pub latency: Mutex<VecDeque<u64>>,
    pub last_used: Mutex<LastUsed>,
    /// 콘솔 세션 토큰 → (발급 시각, 로그인한 사람 — 비밀번호 로그인이면 0)
    pub web_tokens: Mutex<HashMap<String, (Instant, u64, String)>>,
    /// 디스코드 로그인 진행 중인 state → 발급 시각
    pub oauth_states: Mutex<HashMap<String, (Instant, String)>>,
    pub login_fail: Mutex<HashMap<String, (u32, Instant)>>,
    pub started: Instant,
}

impl App {
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    /// config.toml 설정에 웹 콘솔 캐릭터를 얹어 cfg를 다시 만든다 — 파일과 이름이 겹치거나 검증에 걸리는 캐릭터는 건너뛴다
    pub fn refresh_cfg(&self) {
        let mut c = (*self.cfg_file.read().unwrap().clone()).clone();
        let (custom, extra, risky) = {
            let st = self.store.lock().unwrap();
            (st.state.custom_characters.clone(), st.state.wake_extra.clone(), st.state.wake_risky.clone())
        };
        let norm = |w: &str| w.split_whitespace().collect::<String>();
        let fe = |e: &str| e.chars().filter(|&x| x != '\u{fe0f}').collect::<String>();
        // 캐릭터의 모든 호출어(기본·추가·위험 별칭), 띄어쓰기 없이
        let all_words = |k: &str, base: &[String]| -> Vec<String> {
            base.iter().chain(extra.get(k).into_iter().flatten()).chain(risky.get(k).into_iter().flatten()).map(|w| norm(w)).collect()
        };
        for (k, ch) in custom {
            if c.characters.contains_key(&k) || c.characters.len() >= 25 {
                tracing::warn!("웹 콘솔 캐릭터 {k}: config.toml과 이름이 겹치거나 25명을 넘어 건너뜀");
                continue;
            }
            if let Err(e) = crate::config::validate_character(&k, &ch, &c.llm.presets) {
                tracing::warn!("웹 콘솔 캐릭터 건너뜀: {e}");
                continue;
            }
            // 추가할 때 한 검사를 설정을 다시 읽은 뒤에도 — 학습 등급 두뇌, 앞선 캐릭터와 겹치는 호출어·이모지
            let trains = std::iter::once(&ch.brain).chain(ch.fallback.iter()).any(|p| c.llm.presets.get(p).is_some_and(|p| p.trains_on_input));
            let mine = all_words(&k, &ch.wake);
            let wake_clash = c.characters.iter().any(|(ok, o)| all_words(ok, &o.wake).iter().any(|w| mine.contains(w)));
            let emoji_clash = !fe(&ch.emoji).is_empty() && c.characters.values().any(|o| fe(&o.emoji) == fe(&ch.emoji));
            if trains || wake_clash || emoji_clash {
                tracing::warn!("웹 콘솔 캐릭터 {k} 건너뜀: {}", if trains { "입력을 학습에 쓰는 두뇌" } else if wake_clash { "다른 캐릭터와 겹치는 호출어" } else { "다른 캐릭터와 겹치는 이모지" });
                continue;
            }
            c.characters.insert(k, ch);
        }
        *self.cfg.write().unwrap() = Arc::new(c);
    }

    /// 웹 콘솔에서 추가한 캐릭터인가(지울 수 있나)
    pub fn is_custom(&self, key: &str) -> bool {
        // config.toml에 같은 이름이 새로 생겨 건너뛰는 웹 캐릭터도 지울 수 있게 — 저장소에 있으면 웹 캐릭터
        self.store.lock().unwrap().state.custom_characters.contains_key(key)
    }

    /// 판단 모델 설정 — 웹 콘솔 값 우선, 검증에 걸리면 config
    pub fn judge(&self) -> crate::config::Judge {
        let cfg = self.cfg();
        let (j, disabled) = {
            let st = self.store.lock().unwrap();
            (st.state.judge.clone(), st.state.disabled_presets.clone())
        };
        // 콘솔에서 꺼 둔 설정은 그대로 끔(프리셋이 사라져도 config 기본값으로 다시 켜지지 않게). 켜 둔 설정이 틀리면 끈다
        let mut j = match j {
            Some(j) if !j.on => j,
            Some(j) if j.validate(&cfg).is_ok() => j,
            Some(mut j) => {
                // 켜 둔 설정이 틀려졌다(프리셋 삭제 등) — 끈 상태로 저장해 두어, 나중에 프리셋이 돌아와도 저절로 켜지지 않게
                tracing::warn!("판단 모델 설정({})이 맞지 않아 끔", j.preset);
                j.on = false;
                self.store.lock().unwrap().state.judge = Some(j.clone());
                self.save_store();
                j
            }
            None => cfg.judge.clone(),
        };
        // 웹 콘솔에서 끈 프리셋은 판단에도 쓰지 않는다(원래 규칙대로)
        if disabled.contains(&j.preset) {
            j.on = false;
        }
        j
    }

    pub fn ctx(&self) -> Option<Context> {
        self.ctx.lock().unwrap().clone()
    }

    pub fn stt_chain(&self) -> Vec<String> {
        let st = self.store.lock().unwrap();
        st.state.stt_chain.clone().unwrap_or_else(|| self.cfg().stt.chain.clone())
    }

    pub fn tts_chain(&self) -> Vec<String> {
        let st = self.store.lock().unwrap();
        st.state.tts_chain.clone().unwrap_or_else(|| self.cfg().tts.chain.clone())
    }

    pub fn limits(&self) -> Limits {
        let st = self.store.lock().unwrap();
        st.state.limits.clone().unwrap_or_else(|| self.cfg().limits.clone())
    }

    /// config.toml의 캐릭터에 서버별 덮어쓰기를 얹은 실제 값
    pub fn character(&self, guild: u64, key: &str) -> Option<Character> {
        let cfg = self.cfg();
        let mut c = cfg.characters.get(key)?.clone();
        let st = self.store.lock().unwrap();
        if let Some(o) = st.state.guilds.get(&guild).and_then(|g| g.characters.get(key)) {
            if let Some(b) = &o.brain {
                if cfg.llm.presets.get(b).is_some_and(|p| !p.is_decision()) {
                    c.brain = b.clone();
                }
            }
            if let Some(f) = &o.fallback {
                c.fallback = Some(f.clone()).filter(|f| !f.is_empty() && cfg.llm.presets.get(f).is_some_and(|p| !p.is_decision()));
            }
            if let Some(p) = &o.persona {
                c.persona = p.clone();
            }
            if let Some(v) = &o.voice {
                c.voice = v.clone();
            }
            if let Some(s) = o.style.as_ref().filter(|s| crate::config::STYLES.contains(&s.as_str())) {
                c.style = s.clone();
            }
            if let Some(p) = &o.persona_polite {
                c.persona_polite = p.clone();
            }
        }
        Some(c)
    }

    /// (주인 없으면 안 듣기, 기억 시간(분)) — 서버별 덮어쓰기 우선
    pub fn listen_opts(&self, guild: u64) -> (bool, u64) {
        let cfg = self.cfg();
        let st = self.store.lock().unwrap();
        let g = st.state.guilds.get(&guild);
        (
            g.and_then(|g| g.require_owner_present).unwrap_or(cfg.listen.require_owner_present),
            g.and_then(|g| g.buffer_minutes).unwrap_or(cfg.listen.buffer_minutes).clamp(1, 120),
        )
    }

    /// 헤이를 붙여야만 호출되나 — 서버별 덮어쓰기 우선
    pub fn hey_only(&self, guild: u64) -> bool {
        let cfg = self.cfg();
        let st = self.store.lock().unwrap();
        st.state.guilds.get(&guild).and_then(|g| g.hey_only).unwrap_or(cfg.listen.hey_only)
    }

    /// 음성으로도 대답하나(false면 글로만) — 서버별 덮어쓰기 우선
    pub fn voice_reply(&self, guild: u64) -> bool {
        let cfg = self.cfg();
        let st = self.store.lock().unwrap();
        st.state.guilds.get(&guild).and_then(|g| g.voice_reply).unwrap_or(cfg.listen.voice_reply)
    }

    /// 음성 대답 음량(%) — 서버별 덮어쓰기 우선, 0~200
    pub fn volume(&self, guild: u64) -> u32 {
        let cfg = self.cfg();
        let st = self.store.lock().unwrap();
        st.state.guilds.get(&guild).and_then(|g| g.volume).unwrap_or(cfg.listen.volume).min(200)
    }

    /// 입장 음성 인사 — 꺼져 있으면 None
    pub fn greeting(&self, guild: u64) -> Option<String> {
        let cfg = self.cfg();
        let st = self.store.lock().unwrap();
        let g = st.state.guilds.get(&guild);
        if g.and_then(|g| g.greeting_on) == Some(false) {
            return None;
        }
        Some(g.and_then(|g| g.greeting.clone()).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| cfg.notice.voice.clone()))
    }

    pub fn record_latency(&self, ms: u64) {
        let mut l = self.latency.lock().unwrap();
        l.push_back(ms);
        while l.len() > 200 {
            l.pop_front();
        }
    }

    pub fn latency_stats(&self) -> (usize, Option<u64>, Option<u64>) {
        let mut v: Vec<u64> = self.latency.lock().unwrap().iter().copied().collect();
        if v.is_empty() {
            return (0, None, None);
        }
        v.sort_unstable();
        let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        (v.len(), Some(p(0.5)), Some(p(0.95)))
    }

    pub fn save_store(&self) {
        if let Err(e) = self.store.lock().unwrap().save() {
            tracing::error!("state.json 저장 실패: {e}");
        }
    }
}

pub type AppRef = Arc<App>;
