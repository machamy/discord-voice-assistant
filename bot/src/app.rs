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
    pub cfg: RwLock<Arc<Config>>,
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
    /// 콘솔 세션 토큰 → 발급 시각(만료·개수 제한)
    pub web_tokens: Mutex<HashMap<String, Instant>>,
    pub login_fail: Mutex<HashMap<String, (u32, Instant)>>,
    pub started: Instant,
}

impl App {
    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
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
                if cfg.llm.presets.contains_key(b) {
                    c.brain = b.clone();
                }
            }
            if let Some(f) = &o.fallback {
                c.fallback = Some(f.clone()).filter(|f| !f.is_empty() && cfg.llm.presets.contains_key(f));
            }
            if let Some(p) = &o.persona {
                c.persona = p.clone();
            }
            if let Some(v) = &o.voice {
                c.voice = v.clone();
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
