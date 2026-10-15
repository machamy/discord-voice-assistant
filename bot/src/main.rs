//! 아짱·클짱 — 디스코드 음성 채널에서 대화를 듣고, 호출어에 음성(짧게)·텍스트(길게)로 대답하는 봇.
//! 스펙: SPEC.md. 실행 폴더에 `config.toml`, `.env`, `data/`, `sidecar/`가 있어야 한다.

mod app;
mod commands;
mod config;
mod convo;
mod llm;
mod presence;
mod sidecar;
mod store;
mod voice;
mod web;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::Result;
use serenity::all::{Context, EventHandler, GatewayIntents, Guild, Interaction, Ready, VoiceState};
use serenity::Client;
use serenity::async_trait;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::{SerenityInit, Songbird};

use crate::app::{App, AppRef};
use crate::config::Config;
use crate::store::Store;

/// config.toml 다시 읽기 — 형식이 틀리면 기존 설정 유지(SPEC N-11)
pub async fn reload(app: &AppRef) -> Result<()> {
    let c = Config::load(&app.cfg_path)?;
    warn_trains_on_input(&c);
    let old = app.cfg();
    {
        // 설정 파일에서 바뀐 항목은 파일이 이긴다 — 콘솔에서 저장해 둔 덮어쓰기를 지운다(N-11)
        let mut st = app.store.lock().unwrap();
        if old.stt.chain != c.stt.chain {
            st.state.stt_chain = None;
        }
        if old.tts.chain != c.tts.chain {
            st.state.tts_chain = None;
        }
        if old.limits.daily_calls != c.limits.daily_calls || old.limits.daily_usd != c.limits.daily_usd {
            st.state.limits = None;
        }
    }
    app.save_store();
    *app.cfg.write().unwrap() = Arc::new(c);
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for s in sessions {
        s.reload_wake(app);
        s.reset_allowed();
        // require_owner_present 등 바뀐 정책을 바로 적용
        voice::apply_listen(app, &s).await;
    }
    tracing::info!("config.toml 다시 읽음");
    Ok(())
}

fn warn_trains_on_input(c: &Config) {
    // SPEC N-12: 입력을 학습에 쓰는 무료 등급을 캐릭터 두뇌로 쓰면 경고
    for (k, ch) in &c.characters {
        for p in std::iter::once(&ch.brain).chain(ch.fallback.iter()) {
            if c.llm.presets.get(p).is_some_and(|p| p.trains_on_input) {
                tracing::warn!("⚠️ 캐릭터 {k}의 프리셋 {p}는 입력을 학습에 쓰는 무료 등급이다 — 남의 대화가 넘어간다");
            }
        }
    }
}

pub async fn register_all(app: &AppRef, ctx: &Context) {
    for g in ctx.cache.guilds() {
        commands::register(app, ctx, g).await;
    }
}

/// 웹 콘솔 "재시작"(SPEC F-46): 실행 폴더의 start.ps1을 띄운다. 그 스크립트가 이 프로세스와 사이드카를 끄고
/// 로그를 붙여 새로 띄운다(배포 때와 같은 경로). start.ps1이 없으면 같은 실행 파일을 직접 다시 띄운다.
pub fn restart_self() {
    let script = std::path::Path::new("start.ps1");
    let mut cmd = if script.exists() {
        let mut c = std::process::Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "start.ps1"]);
        c
    } else {
        match std::env::current_exe() {
            Ok(exe) => std::process::Command::new(exe),
            Err(e) => {
                tracing::error!("재시작 실패: {e}");
                return;
            }
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    match cmd.spawn() {
        Ok(c) => tracing::info!("재시작 요청 pid={}", c.id()),
        Err(e) => {
            tracing::error!("재시작 실패: {e}");
            return;
        }
    }
    if !script.exists() {
        std::process::exit(0);
    }
    // start.ps1이 곧 이 프로세스를 끈다
}

struct Handler {
    app: AppRef,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!("접속: {} (서버 {}개)", ready.user.name, ready.guilds.len());
        *self.app.ctx.lock().unwrap() = Some(ctx.clone());
        for g in &ready.guilds {
            commands::register(&self.app, &ctx, g.id).await;
        }
        tokio::spawn(commands::preload_voices(self.app.clone()));
        presence::refresh(&self.app).await;
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        tracing::info!("서버: {} ({}) new={is_new:?}", guild.name, guild.id);
        if is_new == Some(true) {
            commands::register(&self.app, &ctx, guild.id).await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => commands::handle(&self.app, &ctx, &cmd).await,
            Interaction::Autocomplete(ac) => commands::autocomplete(&self.app, &ctx, &ac).await,
            _ => {}
        }
    }

    async fn voice_state_update(&self, _ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        if let Some(g) = new.guild_id {
            voice::note_presence(&self.app, g, new.user_id, old.and_then(|o| o.channel_id), new.channel_id).await;
            voice::on_voice_state(&self.app, g, new.user_id, new.channel_id).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    sidecar::no_inherit_std_handles();
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info,serenity=warn,songbird=warn,tracing::span=warn".into()))
        .with_ansi(false)
        .init();

    let cfg_path = PathBuf::from(std::env::var("VOICEBOT_CONFIG").unwrap_or_else(|_| "config.toml".into()));
    let cfg = Config::load(&cfg_path)?;
    warn_trains_on_input(&cfg);
    let token = std::env::var("DISCORD_TOKEN").map_err(|_| anyhow::anyhow!("DISCORD_TOKEN이 .env에 없다"))?;
    std::fs::create_dir_all("data")?;

    // 받는 소리는 16kHz mono로 바로 디코드 — Whisper 입력 형식(PoC 1)
    let sb_cfg = songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz16000)));
    let songbird = Songbird::serenity_from_config(sb_cfg);

    let app: AppRef = Arc::new(App {
        cfg_path,
        cfg: RwLock::new(Arc::new(cfg)),
        store: Mutex::new(Store::load(PathBuf::from("data/state.json"))),
        sessions: tokio::sync::Mutex::new(HashMap::new()),
        http: reqwest::Client::builder().user_agent("voicebot/0.1").build()?,
        ctx: Mutex::new(None),
        songbird: songbird.clone(),
        sidecar: Mutex::new(Default::default()),
        ratelimits: Mutex::new(HashMap::new()),
        latency: Mutex::new(VecDeque::new()),
        last_used: Mutex::new(Default::default()),
        web_tokens: Mutex::new(HashMap::new()),
        login_fail: Mutex::new(HashMap::new()),
        started: Instant::now(),
    });

    tokio::spawn(sidecar::supervise(app.clone()));
    // 웹 콘솔은 별도 태스크 — 죽어도 음성은 계속(SPEC N-14)
    tokio::spawn(web::serve(app.clone()));

    // GUILD_VOICE_STATES: 누가 어느 채널에 있는지(특권 아님). 멤버 목록 특권 인텐트는 쓰지 않는다
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { app: app.clone() })
        .register_songbird_with(songbird)
        .await?;
    let shard = client.shard_manager.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("종료(Ctrl+C)");
        shard.shutdown_all().await;
    });
    // 끊겨도 serenity가 다시 붙는다(SPEC N-09)
    client.start().await?;
    Ok(())
}
