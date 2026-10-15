//! 웹 콘솔 — SPEC §8 (F-40~F-46, N-13, N-14). 주인 전용, 비밀번호 로그인(`CONSOLE_PASSWORD`).
//! 외부 주소는 Cloudflare Access 뒤에만 둔다. 웹 태스크가 죽어도 음성은 계속 돈다(별도 태스크).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use serenity::all::{ChannelId, ChannelType, GuildId};

use crate::app::AppRef;
use crate::config::Voice;
use crate::{sidecar, voice};

const INDEX: &str = include_str!("../web/index.html");
const COOKIE: &str = "vb_session";

type R = Result<Response, (StatusCode, String)>;

fn err(code: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (code, msg.into())
}

/// 디스코드 ID는 0이 될 수 없다(serenity *Id::new(0)은 패닉) — 웹 입력은 여기서 거른다
fn nz(v: u64) -> Result<u64, (StatusCode, String)> {
    if v == 0 { Err(err(StatusCode::BAD_REQUEST, "ID는 0일 수 없어요")) } else { Ok(v) }
}

fn parse_id(s: &str) -> Result<u64, (StatusCode, String)> {
    nz(s.trim().parse().map_err(|_| err(StatusCode::BAD_REQUEST, "ID는 숫자"))?)
}

fn token(headers: &HeaderMap) -> Option<String> {
    let c = headers.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').find_map(|kv| kv.trim().strip_prefix(&format!("{COOKIE}=")).map(String::from))
}

const SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
const MAX_SESSIONS: usize = 20;

fn authed(app: &AppRef, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let Some(t) = token(headers) else { return Err(err(StatusCode::UNAUTHORIZED, "로그인 필요")) };
    let mut tokens = app.web_tokens.lock().unwrap();
    tokens.retain(|_, at| at.elapsed() < SESSION_TTL);
    if tokens.contains_key(&t) {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "로그인 필요"))
    }
}

/// SHA-256 다이제스트(고정 32바이트)끼리 비교 — 입력 길이와 상관없이 같은 횟수로 비교한다
fn ct_eq(a: &str, b: &str) -> bool {
    use sha2::{Digest, Sha256};
    let (x, y) = (Sha256::digest(a.as_bytes()), Sha256::digest(b.as_bytes()));
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

/// NAS 터널(cloudflared, 192.168.0.9)에서 온 요청만 cf-connecting-ip를 믿는다.
/// LAN에서 직접 온 요청은 헤더를 마음대로 넣을 수 있으니 소켓 주소를 쓴다(잠금 우회 방지)
const TUNNEL_PEER: &str = "192.168.0.9";

fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    let peer = addr.ip().to_string();
    if peer == TUNNEL_PEER {
        if let Some(v) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()) {
            return format!("cf:{v}");
        }
    }
    peer
}

#[derive(Deserialize)]
struct Login {
    password: String,
}

async fn login(State(app): State<AppRef>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(b): Json<Login>) -> R {
    let Ok(pw) = std::env::var("CONSOLE_PASSWORD") else {
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "CONSOLE_PASSWORD가 .env에 없어 콘솔을 잠가 뒀어요"));
    };
    let ip = client_ip(&headers, addr);
    {
        let mut f = app.login_fail.lock().unwrap();
        // 10분 지난 실패 기록은 버리고, 출발지가 너무 많으면 오래된 것부터 정리
        f.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(600));
        while f.len() > 1000 {
            let oldest = f.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone()).unwrap();
            f.remove(&oldest);
        }
        if let Some((n, at)) = f.get(&ip) {
            if *n >= 10 && at.elapsed() < Duration::from_secs(600) {
                return Err(err(StatusCode::TOO_MANY_REQUESTS, "실패가 많아 10분간 잠겼어요"));
            }
        }
        if !ct_eq(&b.password, &pw) || pw.is_empty() {
            let e = f.entry(ip.clone()).or_insert((0, Instant::now()));
            if e.1.elapsed() > Duration::from_secs(600) {
                *e = (0, Instant::now());
            }
            e.0 += 1;
            e.1 = Instant::now();
            tracing::warn!("콘솔 로그인 실패 ip={ip}");
            return Err(err(StatusCode::UNAUTHORIZED, "비밀번호가 틀렸어요"));
        }
        f.remove(&ip);
    }
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let t: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    {
        let mut tokens = app.web_tokens.lock().unwrap();
        tokens.retain(|_, at| at.elapsed() < SESSION_TTL);
        // 오래된 세션부터 밀어낸다
        while tokens.len() >= MAX_SESSIONS {
            let oldest = tokens.iter().min_by_key(|(_, at)| **at).map(|(k, _)| k.clone()).unwrap();
            tokens.remove(&oldest);
        }
        tokens.insert(t.clone(), Instant::now());
    }
    tracing::info!("콘솔 로그인 ip={ip}");
    // HTTPS 터널(voicebot.macham.net)로 온 요청이면 Secure. LAN 평문 http에선 붙이면 쿠키가 안 붙어 로그인이 안 된다
    let via_tunnel = addr.ip().to_string() == TUNNEL_PEER
        && headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let secure = if via_tunnel { "; Secure" } else { "" };
    let cookie = format!("{COOKIE}={t}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}");
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response())
}

async fn logout(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    if let Some(t) = token(&headers) {
        app.web_tokens.lock().unwrap().remove(&t);
    }
    Ok(([(header::SET_COOKIE, format!("{COOKIE}=; Path=/; Max-Age=0"))], Json(json!({"ok": true}))).into_response())
}

// ------------------------------------------------------------------ 상태 F-40

async fn status(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    let ctx = app.ctx();
    let mut guilds = vec![];
    if let Some(ctx) = &ctx {
        for gid in ctx.cache.guilds() {
            let Some(g) = ctx.cache.guild(gid).map(|g| (g.name.clone(), voice_channels(&g), text_channels(&g), roles(&g))) else { continue };
            guilds.push(json!({"id": gid.get().to_string(), "name": g.0, "voice_channels": g.1, "text_channels": g.2, "roles": g.3}));
        }
    }
    let mut sessions = vec![];
    for (gid, s) in app.sessions.lock().await.iter() {
        // 채널에 있는 사람 전원 — 무시·봇이면 제외 이유를 같이(SPEC F-40)
        let users: Vec<Value> = s
            .channel_members(&app)
            .into_iter()
            .map(|(u, name)| {
                let excluded = voice::exclusion(&app, s, u);
                json!({"id": u.to_string(), "name": name, "excluded": excluded})
            })
            .collect();
        sessions.push(json!({
            "guild": gid.to_string(),
            "channel": s.channel.lock().unwrap().get().to_string(),
            "listening": s.listening.load(Ordering::Relaxed),
            "minutes": s.started.elapsed().as_secs() / 60,
            "users": users,
            "stats": *s.stats.lock().unwrap(),
            "buffer_lines": s.buffer.lock().unwrap().len(),
        }));
    }
    let (n, p50, p95) = app.latency_stats();
    Ok(Json(json!({
        "bot": ctx.as_ref().map(|c| c.cache.current_user().name.clone()),
        "uptime_min": app.started.elapsed().as_secs() / 60,
        "guilds": guilds,
        "sessions": sessions,
        "sidecar": *app.sidecar.lock().unwrap(),
        "last_used": *app.last_used.lock().unwrap(),
        "latency": {"n": n, "p50_ms": p50, "p95_ms": p95},
    }))
    .into_response())
}

fn voice_channels(g: &serenity::all::Guild) -> Vec<Value> {
    let mut v: Vec<_> = g
        .channels
        .values()
        .filter(|c| matches!(c.kind, ChannelType::Voice | ChannelType::Stage))
        .map(|c| (c.position, json!({"id": c.id.get().to_string(), "name": c.name})))
        .collect();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

fn text_channels(g: &serenity::all::Guild) -> Vec<Value> {
    let mut v: Vec<_> = g
        .channels
        .values()
        .filter(|c| c.kind == ChannelType::Text)
        .map(|c| (c.position, json!({"id": c.id.get().to_string(), "name": c.name})))
        .collect();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

fn roles(g: &serenity::all::Guild) -> Vec<Value> {
    let mut v: Vec<_> = g
        .roles
        .values()
        .filter(|r| r.id.get() != g.id.get())
        .map(|r| (std::cmp::Reverse(r.position), json!({"id": r.id.get().to_string(), "name": r.name})))
        .collect();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

// ------------------------------------------------------------------ 서버 설정 F-41, F-42

async fn guild_get(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let cfg = app.cfg();
    let chars: BTreeMap<String, Value> = cfg
        .characters
        .keys()
        .filter_map(|k| app.character(g, k).map(|c| (k.clone(), serde_json::to_value(c).unwrap())))
        .collect();
    let gs = app.store.lock().unwrap().state.guilds.get(&g).cloned().unwrap_or_default();
    let (req_owner, minutes) = app.listen_opts(g);
    // 이름 붙이기: 캐시에 있는 멤버만(특권 인텐트 없이)
    let names = app.ctx().and_then(|c| {
        c.cache.guild(GuildId::new(g)).map(|gu| {
            let mut m: BTreeMap<String, String> = BTreeMap::new();
            for (u, mem) in &gu.members {
                m.insert(u.get().to_string(), mem.display_name().to_string());
            }
            for (u, vs) in &gu.voice_states {
                if let Some(mem) = &vs.member {
                    m.insert(u.get().to_string(), mem.display_name().to_string());
                }
            }
            m
        })
    });
    let ids = |s: &std::collections::BTreeSet<u64>| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(Json(json!({
        "characters": chars,
        "presets": cfg.llm.presets.iter().map(|(k, p)| (k.clone(), json!({"model": p.model, "free": p.free, "owner_only": p.owner_only, "has_key": p.has_key(), "trains_on_input": p.trains_on_input}))).collect::<BTreeMap<_, _>>(),
        "ignore_users": ids(&gs.ignore_users),
        "ignore_roles": ids(&gs.ignore_roles),
        "self_ignored": ids(&gs.self_ignored),
        "text_channel": gs.text_channel.map(|c| c.to_string()),
        "require_owner_present": req_owner,
        "buffer_minutes": minutes,
        "hey_only": app.hey_only(g),
        "wake_extra": app.store.lock().unwrap().state.wake_extra.clone(),
        "voice_reply": app.voice_reply(g),
        "greeting_on": gs.greeting_on != Some(false),
        "greeting": gs.greeting.clone().unwrap_or_default(),
        "greeting_default": cfg.notice.voice,
        "volume": app.volume(g),
        "names": names,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct CharPatch {
    brain: Option<String>,
    /// "" 이면 대체 없음
    fallback: Option<String>,
    persona: Option<String>,
    voice: Option<BTreeMap<String, Voice>>,
    reset: Option<bool>,
}

async fn char_set(State(app): State<AppRef>, headers: HeaderMap, Path((g, key)): Path<(u64, String)>, Json(b): Json<CharPatch>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let cfg = app.cfg();
    if !cfg.characters.contains_key(&key) {
        return Err(err(StatusCode::NOT_FOUND, "캐릭터 없음"));
    }
    for p in b.brain.iter().chain(b.fallback.iter().filter(|f| !f.is_empty())) {
        if !cfg.llm.presets.contains_key(p) {
            return Err(err(StatusCode::BAD_REQUEST, format!("프리셋 {p} 없음")));
        }
    }
    if b.persona.as_ref().is_some_and(|p| p.chars().count() > 1500) {
        return Err(err(StatusCode::BAD_REQUEST, "말투는 1500자 이내"));
    }
    if let Some(v) = &b.voice {
        if v.len() > crate::config::TTS_PROVIDERS.len() {
            return Err(err(StatusCode::BAD_REQUEST, "목소리 항목이 너무 많음"));
        }
        for (prov, voice) in v {
            crate::config::validate_voice(prov, voice).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        }
    }
    {
        let mut st = app.store.lock().unwrap();
        let gs = st.guild(g);
        if b.reset == Some(true) {
            gs.characters.remove(&key);
        } else {
            let o = gs.characters.entry(key.clone()).or_default();
            if let Some(x) = b.brain {
                o.brain = Some(x);
            }
            if let Some(x) = b.fallback {
                o.fallback = Some(x); // ""는 "대체 없음"
            }
            if let Some(x) = b.persona {
                o.persona = if x.trim().is_empty() { None } else { Some(x) };
            }
            if let Some(x) = b.voice {
                o.voice = Some(x);
            }
        }
    }
    app.save_store();
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct GuildPatch {
    text_channel: Option<String>,
    require_owner_present: Option<bool>,
    hey_only: Option<bool>,
    voice_reply: Option<bool>,
    volume: Option<u32>,
    greeting_on: Option<bool>,
    /// "" 이면 기본값
    greeting: Option<String>,
    buffer_minutes: Option<u64>,
}

async fn guild_set(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>, Json(b): Json<GuildPatch>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    // 텍스트 채널은 이 서버의 일반 텍스트 채널이어야 한다
    let text_channel: Option<Option<u64>> = match b.text_channel.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(t) => {
            let id: u64 = parse_id(t)?;
            let ok = app.ctx().is_some_and(|c| {
                c.cache.guild(GuildId::new(g)).is_some_and(|gu| gu.channels.get(&ChannelId::new(id)).is_some_and(|ch| ch.kind == ChannelType::Text))
            });
            if !ok {
                return Err(err(StatusCode::BAD_REQUEST, "이 서버의 텍스트 채널이 아니에요"));
            }
            Some(Some(id))
        }
    };
    {
        let mut st = app.store.lock().unwrap();
        let gs = st.guild(g);
        if let Some(t) = text_channel {
            gs.text_channel = t;
        }
        if let Some(r) = b.require_owner_present {
            gs.require_owner_present = Some(r);
        }
        if let Some(h) = b.hey_only {
            gs.hey_only = Some(h);
        }
        if let Some(v) = b.voice_reply {
            gs.voice_reply = Some(v);
        }
        if let Some(v) = b.volume {
            gs.volume = Some(v.min(200));
        }
        if let Some(v) = b.greeting_on {
            gs.greeting_on = Some(v);
        }
        if let Some(t) = b.greeting {
            let t = t.trim();
            gs.greeting = (!t.is_empty()).then(|| t.chars().take(200).collect());
        }
        if let Some(m) = b.buffer_minutes {
            gs.buffer_minutes = Some(m.clamp(1, 120));
        }
    }
    app.save_store();
    // 잠금을 먼저 놓는다 — apply_listen이 sessions를 다시 잠근다
    let s = app.sessions.lock().await.get(&g).cloned();
    if let Some(s) = s {
        voice::apply_listen(&app, &s).await;
    }
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct WakePatch {
    character: String,
    words: Vec<String>,
}

/// 호출어 사전(전역) — config 호출어에 더할 받아쓰기 변형. 바로 모든 세션에 반영
async fn wake_set(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<WakePatch>) -> R {
    authed(&app, &headers)?;
    let cfg = app.cfg();
    let Some(ch) = cfg.characters.get(&b.character) else {
        return Err(err(StatusCode::BAD_REQUEST, "없는 캐릭터예요"));
    };
    let mut words: Vec<String> = Vec::new();
    for w in b.words.iter().map(|w| w.trim()).filter(|w| !w.is_empty()) {
        if w.chars().count() > 20 || w.chars().any(char::is_whitespace) {
            return Err(err(StatusCode::BAD_REQUEST, "호출어는 띄어쓰기 없이 20자 이내"));
        }
        if !ch.wake.iter().any(|x| x == w) && !words.iter().any(|x| x == w) {
            words.push(w.to_string());
        }
    }
    if words.len() > 30 {
        return Err(err(StatusCode::BAD_REQUEST, "추가 호출어는 30개까지"));
    }
    let mut all = ch.wake.clone();
    all.extend(words.iter().cloned());
    if crate::convo::Wake::compile(&all).is_err() {
        return Err(err(StatusCode::BAD_REQUEST, "호출어로 쓸 수 없는 글자가 있어요"));
    }
    {
        let mut st = app.store.lock().unwrap();
        if words.is_empty() {
            st.state.wake_extra.remove(&b.character);
        } else {
            st.state.wake_extra.insert(b.character.clone(), words.clone());
        }
    }
    app.save_store();
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for s in sessions {
        s.reload_wake(&app);
    }
    Ok(Json(json!({"ok": true, "words": words})).into_response())
}

#[derive(Deserialize)]
struct IgnorePatch {
    kind: String,
    id: String,
    add: bool,
}

async fn ignore_set(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>, Json(b): Json<IgnorePatch>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let id: u64 = parse_id(&b.id)?;
    // 추가할 때는 이 서버의 역할·멤버인지 확인(해제는 이미 저장된 값이라 그대로)
    if b.add {
        let Some(ctx) = app.ctx() else { return Err(err(StatusCode::SERVICE_UNAVAILABLE, "디스코드 미접속")) };
        match b.kind.as_str() {
            "role" => {
                let ok = ctx.cache.guild(GuildId::new(g)).is_some_and(|gu| gu.roles.contains_key(&serenity::all::RoleId::new(id)));
                if !ok {
                    return Err(err(StatusCode::BAD_REQUEST, "이 서버의 역할이 아니에요"));
                }
            }
            "user" | "self" => {
                if ctx.http.get_member(GuildId::new(g), serenity::all::UserId::new(id)).await.is_err() {
                    return Err(err(StatusCode::BAD_REQUEST, "이 서버의 멤버가 아니에요"));
                }
            }
            _ => {}
        }
    }
    {
        let mut st = app.store.lock().unwrap();
        let gs = st.guild(g);
        let set = match b.kind.as_str() {
            "user" => &mut gs.ignore_users,
            "role" => &mut gs.ignore_roles,
            "self" => &mut gs.self_ignored,
            _ => return Err(err(StatusCode::BAD_REQUEST, "kind는 user|role|self")),
        };
        if b.add {
            set.insert(id);
        } else {
            set.remove(&id);
        }
        if b.add && b.kind != "role" {
            st.guild(g).memories.retain(|m| m.user != id);
        }
        st.bump();
    }
    app.save_store();
    let s = app.sessions.lock().await.get(&g).cloned();
    if let Some(s) = s {
        s.reset_allowed();
        if b.add && b.kind != "role" {
            s.buffer.lock().unwrap().remove_user(id);
        }
    }
    Ok(Json(json!({"ok": true})).into_response())
}

/// 이름으로 멤버 찾기 — 멤버 검색 API(특권 인텐트 불필요)
async fn member_search(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>, axum::extract::Query(q): axum::extract::Query<BTreeMap<String, String>>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let Some(ctx) = app.ctx() else { return Err(err(StatusCode::SERVICE_UNAVAILABLE, "디스코드 미접속")) };
    let query = q.get("q").cloned().unwrap_or_default();
    if query.trim().is_empty() {
        return Ok(Json(json!({"members": []})).into_response());
    }
    let ms = GuildId::new(g)
        .search_members(&ctx.http, &query, Some(20))
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let v: Vec<Value> = ms
        .iter()
        .map(|m| json!({"id": m.user.id.get().to_string(), "name": m.display_name(), "username": m.user.name, "bot": m.user.bot}))
        .collect();
    Ok(Json(json!({"members": v})).into_response())
}

// ------------------------------------------------------------------ 사용량·공급자 F-43, F-44

async fn usage(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    let lim = app.limits();
    let st = app.store.lock().unwrap();
    let days: Vec<(&String, &crate::store::DayUsage)> = st.state.usage.iter().rev().take(31).collect();
    let month = st.month_usd();
    Ok(Json(json!({
        "today": crate::store::today(),
        "days": days.into_iter().map(|(k, v)| json!({"date": k, "usage": v})).collect::<Vec<_>>(),
        "month": {"paid_calls": month.0, "free_calls": month.1, "usd": month.2},
        "limits": lim,
        "ratelimits": *app.ratelimits.lock().unwrap(),
    }))
    .into_response())
}

#[derive(Deserialize)]
struct LimitPatch {
    daily_calls: Option<u32>,
    daily_usd: Option<f64>,
}

async fn limits_set(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<LimitPatch>) -> R {
    authed(&app, &headers)?;
    let mut lim = app.limits();
    if let Some(c) = b.daily_calls {
        lim.daily_calls = c;
    }
    if let Some(u) = b.daily_usd {
        lim.daily_usd = u.max(0.0);
    }
    app.store.lock().unwrap().state.limits = Some(lim);
    app.save_store();
    Ok(Json(json!({"ok": true})).into_response())
}

async fn providers(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    let cfg = app.cfg();
    let disabled = app.store.lock().unwrap().state.disabled_presets.clone();
    let keys = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GROQ_API_KEY", "GEMINI_API_KEY", "DEEPGRAM_API_KEY", "AZURE_SPEECH_KEY", "ELEVENLABS_API_KEY"];
    let key_state: BTreeMap<&str, bool> = keys.iter().map(|k| (*k, std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false))).collect();
    Ok(Json(json!({
        "stt_chain": app.stt_chain(),
        "stt_hint": app.store.lock().unwrap().state.stt_hint.clone(),
        "tts_chain": app.tts_chain(),
        "stt_default": cfg.stt.chain,
        "tts_default": cfg.tts.chain,
        "stt_known": ["local-whisper:cuda:large-v3-turbo", "local-whisper:cpu:small", "local-whisper:cpu:medium", "groq-whisper", "groq-whisper-turbo", "deepgram", "openai-transcribe"],
        "tts_known": ["edge-tts", "azure-tts", "melotts", "elevenlabs", "openai-tts"],
        "presets": cfg.llm.presets.iter().map(|(k, p)| (k.clone(), json!({"provider": p.provider, "model": p.model, "free": p.free, "owner_only": p.owner_only, "trains_on_input": p.trains_on_input, "has_key": p.has_key(), "key_env": p.key_env(), "disabled": disabled.contains(k), "effort": p.reasoning_effort, "price_in": p.price_in, "price_out": p.price_out}))).collect::<BTreeMap<_, _>>(),
        "free_chain": cfg.llm.free_chain,
        "keys": key_state, // 값은 절대 내보내지 않는다 — SPEC N-10
    }))
    .into_response())
}

#[derive(Deserialize)]
struct ProviderPatch {
    stt_chain: Option<Vec<String>>,
    tts_chain: Option<Vec<String>>,
    reset_chains: Option<bool>,
    preset: Option<String>,
    disabled: Option<bool>,
    stt_hint: Option<crate::store::SttHint>,
}

async fn providers_set(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<ProviderPatch>) -> R {
    authed(&app, &headers)?;
    if let Some(c) = &b.stt_chain {
        if c.is_empty() || c.len() > 6 || !c.iter().all(|x| crate::config::stt_provider_ok(x)) {
            return Err(err(StatusCode::BAD_REQUEST, "받아쓰기 체인에 모르는 공급자가 있어요"));
        }
    }
    if let Some(c) = &b.tts_chain {
        const TTS: [&str; 5] = ["edge-tts", "azure-tts", "melotts", "elevenlabs", "openai-tts"];
        if c.is_empty() || c.len() > 6 || !c.iter().all(|x| TTS.contains(&x.as_str())) {
            return Err(err(StatusCode::BAD_REQUEST, "목소리 체인에 모르는 공급자가 있어요"));
        }
    }
    if let Some(p) = &b.preset {
        if !app.cfg().llm.presets.contains_key(p) {
            return Err(err(StatusCode::BAD_REQUEST, "없는 프리셋"));
        }
    }
    {
        let mut st = app.store.lock().unwrap();
        if b.reset_chains == Some(true) {
            st.state.stt_chain = None;
            st.state.tts_chain = None;
        }
        if let Some(c) = b.stt_chain.filter(|c| !c.is_empty()) {
            st.state.stt_chain = Some(c);
        }
        if let Some(c) = b.tts_chain.filter(|c| !c.is_empty()) {
            st.state.tts_chain = Some(c);
        }
        if let Some(mut h) = b.stt_hint {
            h.words = h.words.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect();
            h.min_ms = h.min_ms.clamp(0, 10_000);
            st.state.stt_hint = h;
        }
        if let (Some(p), Some(d)) = (b.preset, b.disabled) {
            if d {
                st.state.disabled_presets.insert(p);
            } else {
                st.state.disabled_presets.remove(&p);
            }
        }
    }
    app.save_store();
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct LlmTestReq {
    preset: String,
    question: Option<String>,
    fill: Option<bool>,
}

/// 두뇌 시험 — 프리셋 하나를 실제로 불러 지연·토큰·형식을 본다(유료면 하루 상한에 잡힘). F-35
async fn llm_test(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<LlmTestReq>) -> R {
    authed(&app, &headers)?;
    if !app.cfg().llm.presets.contains_key(&b.preset) {
        return Err(err(StatusCode::BAD_REQUEST, "없는 프리셋"));
    }
    let q = b.question.map(|q| q.trim().to_string()).filter(|q| !q.is_empty()).unwrap_or_else(|| "오늘 저녁 메뉴 하나만 추천해 줘.".into());
    if q.chars().count() > 300 {
        return Err(err(StatusCode::BAD_REQUEST, "질문은 300자까지"));
    }
    let a = crate::llm::probe(&app, &b.preset, &q, b.fill.unwrap_or(false))
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({
        "preset": a.preset, "ms": a.ms, "speech_ms": a.speech_ms, "usd": a.usd, "format_ok": a.format_ok,
        "tokens": {"input": a.tokens[0], "output": a.tokens[1], "cache_read": a.tokens[2], "cache_write": a.tokens[3]},
        "speech": a.speech, "text": a.text,
    }))
    .into_response())
}

// ------------------------------------------------------------------ 대화 F-45, 조작 F-46

async fn buffer(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let s = app.sessions.lock().await.get(&g).cloned();
    let (lines, events, stats, listening) = match s {
        Some(s) => {
            let keep = Duration::from_secs(app.listen_opts(g).1 * 60);
            let (l, e) = {
                let mut b = s.buffer.lock().unwrap();
                (b.snapshot(keep), b.events(keep))
            };
            let st = s.stats.lock().unwrap().clone();
            (l, e, Some(st), s.listening.load(std::sync::atomic::Ordering::Relaxed))
        }
        None => (vec![], vec![], None, false),
    };
    // 화면 표시만 — 저장·내려받기 없음(SPEC N-06). 받아쓰기 기록에는 글 내용이 없다
    let side = app.sidecar.lock().unwrap().ok;
    let last = app.last_used.lock().unwrap().stt.clone();
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"lines": lines, "events": events, "stats": stats, "listening": listening, "sidecar_ok": side, "stt_chain": app.stt_chain(), "stt_last": last})),
    )
        .into_response())
}

/// 두뇌 입출력 원문(최근 20건, 메모리에만) — 화면 표시만, 저장·내려받기 없음
async fn llm_log(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let s = app.sessions.lock().await.get(&g).cloned();
    let traces = match s {
        Some(s) => {
            let keep = Duration::from_secs(app.listen_opts(g).1 * 60);
            s.buffer.lock().unwrap().traces(keep)
        }
        None => vec![],
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"traces": traces}))).into_response())
}

async fn memories_get(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let mems = app.store.lock().unwrap().state.guilds.get(&g).map(|x| x.memories.clone()).unwrap_or_default();
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"memories": mems}))).into_response())
}

#[derive(Deserialize)]
struct MemPatch {
    /// 지울 번호(0부터), 없으면 add
    remove: Option<usize>,
    add: Option<String>,
    clear: Option<bool>,
}

/// 장기 기억 고치기 — 지우기·추가·전부 지우기(SPEC F-37)
async fn memories_set(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>, Json(b): Json<MemPatch>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    {
        let mut st = app.store.lock().unwrap();
        let mems = &mut st.guild(g).memories;
        if b.clear == Some(true) {
            mems.clear();
        } else if let Some(i) = b.remove {
            if i >= mems.len() {
                return Err(err(StatusCode::BAD_REQUEST, "없는 기억이에요"));
            }
            mems.remove(i);
        } else if let Some(t) = b.add {
            let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(crate::llm::MEMORY_MAX_CHARS).collect();
            if t.is_empty() || t.contains('<') {
                return Err(err(StatusCode::BAD_REQUEST, "내용을 확인해 주세요"));
            }
            while mems.len() >= crate::llm::MEMORY_MAX_ITEMS {
                mems.remove(0);
            }
            mems.push(crate::store::Memory { text: t, by: "웹 콘솔".into(), user: 0, date: crate::store::today() });
        }
    }
    app.save_store();
    Ok(Json(json!({"ok": true})).into_response())
}

async fn forget(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let s = app.sessions.lock().await.get(&g).cloned();
    if let Some(s) = s {
        s.buffer.lock().unwrap().clear();
    }
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct JoinReq {
    channel: String,
}

async fn join(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>, Json(b): Json<JoinReq>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let ch: u64 = parse_id(&b.channel)?;
    let ok = app.ctx().is_some_and(|c| {
        c.cache
            .guild(GuildId::new(g))
            .is_some_and(|gu| gu.channels.get(&ChannelId::new(ch)).is_some_and(|x| matches!(x.kind, ChannelType::Voice | ChannelType::Stage)))
    });
    if !ok {
        return Err(err(StatusCode::BAD_REQUEST, "이 서버의 음성 채널이 아니에요"));
    }
    voice::join(&app, GuildId::new(g), ChannelId::new(ch), None)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn leave(State(app): State<AppRef>, headers: HeaderMap, Path(g): Path<u64>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    voice::leave(&app, GuildId::new(g)).await;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn reload(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    crate::reload(&app).await.map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Some(c) = app.ctx() {
        crate::register_all(&app, &c).await;
    }
    Ok(Json(json!({"ok": true})).into_response())
}

async fn restart(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        crate::restart_self();
    });
    Ok(Json(json!({"ok": true, "msg": "재시작합니다"})).into_response())
}

#[derive(Deserialize)]
struct PreviewReq {
    character: String,
    guild: Option<String>,
    text: Option<String>,
}

/// 목소리 미리 듣기 — 브라우저에서 재생할 WAV를 돌려준다(F-41)
async fn tts_preview(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<PreviewReq>) -> R {
    authed(&app, &headers)?;
    let g = b.guild.and_then(|g| g.parse::<u64>().ok()).unwrap_or(0); // 0은 "서버 설정 없음"으로만 쓰고 ID 생성에 안 쓴다
    let ch = app.character(g, &b.character).ok_or_else(|| err(StatusCode::NOT_FOUND, "캐릭터 없음"))?;
    let text = b.text.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| format!("안녕, 나는 {}이야. 이 목소리 어때?", ch.name));
    let chain = sidecar::voice_chain(&app, &ch.voice);
    let (wav, provider) = sidecar::tts(&app, &text, &chain).await.map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "audio/wav".to_string()), (header::HeaderName::from_static("x-provider"), provider)], wav).into_response())
}

async fn voices(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    let v = sidecar::voices(&app, "edge-tts").await.map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(v).into_response())
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX))
}

pub async fn serve(app: AppRef) {
    let bind = app.cfg().web.bind.clone();
    let router = Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/status", get(status))
        .route("/api/guild/{g}", get(guild_get).post(guild_set))
        .route("/api/guild/{g}/character/{key}", post(char_set))
        .route("/api/guild/{g}/ignore", post(ignore_set))
        .route("/api/guild/{g}/members", get(member_search))
        .route("/api/guild/{g}/buffer", get(buffer))
        .route("/api/guild/{g}/llm-log", get(llm_log))
        .route("/api/guild/{g}/memories", get(memories_get).post(memories_set))
        .route("/api/guild/{g}/forget", post(forget))
        .route("/api/guild/{g}/join", post(join))
        .route("/api/guild/{g}/leave", post(leave))
        .route("/api/usage", get(usage))
        .route("/api/limits", post(limits_set))
        .route("/api/providers", get(providers).post(providers_set))
        .route("/api/llm-test", post(llm_test))
        .route("/api/wake", post(wake_set))
        .route("/api/reload", post(reload))
        .route("/api/restart", post(restart))
        .route("/api/tts-preview", post(tts_preview))
        .route("/api/voices", get(voices))
        .with_state(app.clone());
    // 재시작 직후엔 이전 프로세스가 포트를 쥐고 있을 수 있다 — 잠깐 재시도
    for i in 0..30 {
        match tokio::net::TcpListener::bind(&bind).await {
            Ok(l) => {
                tracing::info!("웹 콘솔 http://{bind}");
                if let Err(e) = axum::serve(l, router.into_make_service_with_connect_info::<SocketAddr>()).await {
                    tracing::error!("웹 콘솔 중단: {e}");
                }
                return;
            }
            Err(e) => {
                if i == 29 {
                    tracing::error!("웹 콘솔 포트 {bind} 열기 실패: {e}");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
