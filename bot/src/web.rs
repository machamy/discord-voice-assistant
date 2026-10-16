//! 웹 콘솔 — SPEC §8 (F-40~F-46, N-13, N-14). 주인 전용. 디스코드 로그인(OAuth2 identify, 주인·허용 목록만),
//! 디스코드 로그인을 준비하지 않았으면 비밀번호 로그인(`CONSOLE_PASSWORD`).
//! 외부 주소는 Cloudflare Access 뒤에만 둔다. 웹 태스크가 죽어도 음성은 계속 돈다(별도 태스크).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, State};
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

/// 로그인 확인 — (디스코드 유저 ID(비밀번호 로그인이면 0), 표시 이름)
fn authed(app: &AppRef, headers: &HeaderMap) -> Result<(u64, String), (StatusCode, String)> {
    let Some(t) = token(headers) else { return Err(err(StatusCode::UNAUTHORIZED, "로그인 필요")) };
    let mut tokens = app.web_tokens.lock().unwrap();
    tokens.retain(|_, (at, _, _)| at.elapsed() < SESSION_TTL);
    match tokens.get(&t) {
        // 허용 목록에서 빠진 디스코드 계정, 비밀번호 로그인을 끈 뒤의 비밀번호 세션은 바로 끊는다
        Some((_, uid, _)) if (*uid != 0 && !discord_allowed(app, *uid)) || (*uid == 0 && !password_allowed(app)) => {
            tokens.remove(&t);
            Err(err(StatusCode::UNAUTHORIZED, "로그인 필요"))
        }
        Some((_, uid, name)) => Ok((*uid, name.clone())),
        None => Err(err(StatusCode::UNAUTHORIZED, "로그인 필요")),
    }
}

fn discord_allowed(app: &AppRef, uid: u64) -> bool {
    let cfg = app.cfg();
    uid == cfg.owner_id || cfg.web.allowed_users.contains(&uid)
}

/// 디스코드 로그인 설정 — (클라이언트 ID, 비밀, 돌아올 주소). 하나라도 없으면 None
fn oauth(app: &AppRef) -> Option<(String, String, String)> {
    let public = app.cfg().web.public_url.trim().trim_end_matches('/').to_string();
    if !public.starts_with("https://") && !public.starts_with("http://") {
        return None;
    }
    let secret = std::env::var("DISCORD_CLIENT_SECRET").ok().filter(|s| !s.trim().is_empty())?;
    // 클라이언트 ID = 봇 애플리케이션 ID(봇 유저 ID와 같다). .env에 따로 두면 그걸
    let id = std::env::var("DISCORD_CLIENT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| app.ctx().map(|c| c.cache.current_user().id.get().to_string()))?;
    Some((id.trim().to_string(), secret.trim().to_string(), format!("{public}/auth/discord/callback")))
}

/// 디스코드 로그인을 쓰기로 했나(비밀·주소가 있음) — 클라이언트 ID는 봇이 붙은 뒤에야 알 수 있어 따로 본다
fn oauth_configured(app: &AppRef) -> bool {
    let public = app.cfg().web.public_url.trim().to_string();
    (public.starts_with("https://") || public.starts_with("http://")) && std::env::var("DISCORD_CLIENT_SECRET").is_ok_and(|s| !s.trim().is_empty())
}

/// 비밀번호 로그인을 받나 — 설정값, 없으면 디스코드 로그인을 쓰지 않을 때만(시작 직후 클라이언트 ID를 모르는 사이에도 열리지 않게)
fn password_allowed(app: &AppRef) -> bool {
    let has_pw = std::env::var("CONSOLE_PASSWORD").is_ok_and(|p| !p.is_empty());
    has_pw && app.cfg().web.password_login.unwrap_or(!oauth_configured(app))
}

/// HTTPS 터널(voicebot.macham.net)로 온 요청이면 Secure. LAN 평문 http에선 붙이면 쿠키가 안 붙어 로그인이 안 된다
fn secure_flag(headers: &HeaderMap, addr: SocketAddr) -> &'static str {
    let via_tunnel = addr.ip().to_string() == TUNNEL_PEER && headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    if via_tunnel { "; Secure" } else { "" }
}

fn random_hex(n: usize) -> Result<String, (StatusCode, String)> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// 세션을 새로 만들고 쿠키 값을 돌려준다
fn issue_session(app: &AppRef, uid: u64, name: &str, secure: &str) -> Result<String, (StatusCode, String)> {
    let t = random_hex(32)?;
    {
        let mut tokens = app.web_tokens.lock().unwrap();
        tokens.retain(|_, (at, _, _)| at.elapsed() < SESSION_TTL);
        // 오래된 세션부터 밀어낸다
        while tokens.len() >= MAX_SESSIONS {
            let oldest = tokens.iter().min_by_key(|(_, (at, _, _))| *at).map(|(k, _)| k.clone()).unwrap();
            tokens.remove(&oldest);
        }
        tokens.insert(t.clone(), (Instant::now(), uid, name.to_string()));
    }
    Ok(format!("{COOKIE}={t}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}"))
}

/// 실패 횟수 확인·기록 — 10분에 10번 넘게 틀리면 잠근다
fn fail_locked(app: &AppRef, ip: &str) -> bool {
    let mut f = app.login_fail.lock().unwrap();
    f.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(600));
    f.get(ip).is_some_and(|(n, at)| *n >= 10 && at.elapsed() < Duration::from_secs(600))
}

fn note_fail(app: &AppRef, ip: &str) {
    let mut f = app.login_fail.lock().unwrap();
    while f.len() > 1000 {
        let oldest = f.iter().min_by_key(|(_, (_, at))| *at).map(|(k, _)| k.clone()).unwrap();
        f.remove(&oldest);
    }
    let e = f.entry(ip.to_string()).or_insert((0, Instant::now()));
    if e.1.elapsed() > Duration::from_secs(600) {
        *e = (0, Instant::now());
    }
    e.0 += 1;
    e.1 = Instant::now();
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
    if !password_allowed(&app) {
        return Err(err(StatusCode::FORBIDDEN, "비밀번호 로그인은 꺼져 있어요 — 디스코드로 로그인해 주세요"));
    }
    let pw = std::env::var("CONSOLE_PASSWORD").unwrap_or_default();
    let ip = client_ip(&headers, addr);
    if fail_locked(&app, &ip) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, "실패가 많아 10분간 잠겼어요"));
    }
    if !ct_eq(&b.password, &pw) || pw.is_empty() {
        note_fail(&app, &ip);
        tracing::warn!("콘솔 로그인 실패 ip={ip}");
        return Err(err(StatusCode::UNAUTHORIZED, "비밀번호가 틀렸어요"));
    }
    app.login_fail.lock().unwrap().remove(&ip);
    tracing::info!("콘솔 로그인(비밀번호) ip={ip}");
    let cookie = issue_session(&app, 0, "비밀번호", secure_flag(&headers, addr))?;
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response())
}

/// 로그인 화면이 고를 방법 — 로그인 없이 볼 수 있다(비밀 없음)
async fn login_info(State(app): State<AppRef>) -> R {
    Ok(Json(json!({"discord": oauth_configured(&app), "password": password_allowed(&app)})).into_response())
}

const OAUTH_COOKIE: &str = "vb_oauth";

fn page(msg: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>보이스봇 로그인</title>\
         <body style='font-family:sans-serif;padding:2em;background:#1f1e1d;color:#eee'><p>{}</p><p><a style='color:#e8a17f' href='/'>콘솔로 돌아가기</a></p>",
        msg.replace('&', "&amp;").replace('<', "&lt;")
    );
    (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

/// 디스코드 로그인 시작 — state를 쿠키와 서버에 같이 두고 디스코드 인증 화면으로 보낸다
async fn discord_start(State(app): State<AppRef>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap) -> R {
    let Some((id, _, redirect)) = oauth(&app) else { return Ok(page("디스코드 로그인이 아직 준비되지 않았어요(DISCORD_CLIENT_SECRET·[web] public_url, 봇 접속 대기).")) };
    let ip = client_ip(&headers, addr);
    let state = random_hex(16)?;
    {
        let mut s = app.oauth_states.lock().unwrap();
        s.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        // 출발지 하나가 진행 중 로그인을 몰아 만들어 남의 로그인을 밀어내지 못하게 — 출발지당 5개, 전체 200개
        if s.values().filter(|(_, i)| *i == ip).count() >= 5 {
            return Ok(page("로그인 시도가 너무 많아요. 잠시 뒤에 다시 해 주세요."));
        }
        while s.len() >= 200 {
            let oldest = s.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()).unwrap();
            s.remove(&oldest);
        }
        s.insert(state.clone(), (Instant::now(), ip));
    }
    let url = reqwest::Url::parse_with_params(
        "https://discord.com/oauth2/authorize",
        &[("client_id", id.as_str()), ("response_type", "code"), ("redirect_uri", redirect.as_str()), ("scope", "identify"), ("state", state.as_str())],
    )
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // 디스코드에서 돌아오는 요청은 다른 사이트에서 오는 이동이라 SameSite=Lax여야 쿠키가 붙는다
    let cookie = format!("{OAUTH_COOKIE}={state}; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=600{}", secure_flag(&headers, addr));
    Ok((StatusCode::FOUND, [(header::LOCATION, url.to_string()), (header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_string())]).into_response())
}

/// 디스코드에서 돌아옴 — state 확인 → 코드로 토큰 → 내 정보(identify) → 주인·허용 목록이면 세션
async fn discord_callback(State(app): State<AppRef>, ConnectInfo(addr): ConnectInfo<SocketAddr>, headers: HeaderMap, Query(q): Query<BTreeMap<String, String>>) -> R {
    let ip = client_ip(&headers, addr);
    if fail_locked(&app, &ip) {
        return Ok(page("실패가 많아 10분간 잠겼어요."));
    }
    let Some((id, secret, redirect)) = oauth(&app) else { return Ok(page("디스코드 로그인이 아직 준비되지 않았어요.")) };
    // state부터 확인한다 — 취소(error) 콜백도 state가 맞을 때만 "취소"로 보고, 아니면 실패로 센다
    let Some(state) = q.get("state") else {
        note_fail(&app, &ip);
        return Ok(page("잘못된 요청이에요."));
    };
    let cookie_state = headers
        .get(header::COOKIE)
        .and_then(|c| c.to_str().ok())
        .and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix(&format!("{OAUTH_COOKIE}=")).map(String::from)));
    // 쿠키부터 맞춰 본다 — 쿠키가 다른 잘못된 콜백이 정상 로그인의 state를 써 버리지 않게
    let cookie_ok = cookie_state.as_deref().is_some_and(|c| ct_eq(c, state));
    let known = cookie_ok && app.oauth_states.lock().unwrap().remove(state).is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(600));
    if !known {
        note_fail(&app, &ip);
        return Ok(page("로그인 확인값이 맞지 않아요. 처음부터 다시 해 주세요."));
    }
    if let Some(e) = q.get("error") {
        // 취소·거절도 실패로 센다(반복 시도로 잠금을 피하지 못하게)
        note_fail(&app, &ip);
        return Ok(page(&format!("디스코드 로그인이 취소됐어요({}).", e.chars().take(40).collect::<String>())));
    }
    let Some(code) = q.get("code") else {
        note_fail(&app, &ip);
        return Ok(page("잘못된 요청이에요."));
    };
    // 폼 본문 — reqwest의 form 기능 없이 URL 인코딩만 빌려 쓴다
    let form = reqwest::Url::parse_with_params("http://x/", &[("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect.as_str())])
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .query()
        .unwrap_or_default()
        .to_string();
    let tok: Value = match app
        .http
        .post("https://discord.com/api/v10/oauth2/token")
        .basic_auth(&id, Some(&secret))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        Ok(r) => {
            tracing::warn!("디스코드 로그인 토큰 교환 실패 HTTP {}", r.status());
            return Ok(page("디스코드 로그인에 실패했어요(토큰 교환)."));
        }
        Err(e) => {
            tracing::warn!("디스코드 로그인 토큰 교환 실패: {e}");
            return Ok(page("디스코드에 연결하지 못했어요."));
        }
    };
    let Some(access) = tok["access_token"].as_str() else { return Ok(page("디스코드 로그인에 실패했어요(토큰 없음).")) };
    let me: Value = match app.http.get("https://discord.com/api/v10/users/@me").bearer_auth(access).timeout(Duration::from_secs(15)).send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(Value::Null),
        _ => return Ok(page("디스코드 계정 정보를 받지 못했어요.")),
    };
    let uid: u64 = me["id"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
    let name = me["global_name"].as_str().or(me["username"].as_str()).unwrap_or("?").chars().take(40).collect::<String>();
    if uid == 0 || !discord_allowed(&app, uid) {
        note_fail(&app, &ip);
        tracing::warn!("콘솔 디스코드 로그인 거절 uid={uid} ip={ip}");
        return Ok(page("이 디스코드 계정은 콘솔을 쓸 수 없어요(주인·허용 목록만)."));
    }
    app.login_fail.lock().unwrap().remove(&ip);
    tracing::info!("콘솔 로그인(디스코드) uid={uid} ip={ip}");
    let secure = secure_flag(&headers, addr);
    let session = issue_session(&app, uid, &name, secure)?;
    let mut h = HeaderMap::new();
    h.insert(header::LOCATION, header::HeaderValue::from_static("/"));
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    h.append(header::SET_COOKIE, session.parse().map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "쿠키"))?);
    h.append(header::SET_COOKIE, format!("{OAUTH_COOKIE}=; Path=/auth; Max-Age=0").parse().map_err(|_| err(StatusCode::INTERNAL_SERVER_ERROR, "쿠키"))?);
    Ok((StatusCode::FOUND, h).into_response())
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
    let (me_id, me_name) = authed(&app, &headers)?;
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
        "me": {"id": me_id.to_string(), "name": me_name},
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
        "presets": cfg.llm.presets.iter().map(|(k, p)| (k.clone(), json!({"model": p.model, "free": p.free, "owner_only": p.owner_only, "has_key": p.has_key(), "trains_on_input": p.trains_on_input, "decision": p.is_decision()}))).collect::<BTreeMap<_, _>>(),
        "ignore_users": ids(&gs.ignore_users),
        "ignore_roles": ids(&gs.ignore_roles),
        "self_ignored": ids(&gs.self_ignored),
        "text_channel": gs.text_channel.map(|c| c.to_string()),
        "require_owner_present": req_owner,
        "buffer_minutes": minutes,
        "hey_only": app.hey_only(g),
        "wake_extra": app.store.lock().unwrap().state.wake_extra.clone(),
        "wake_risky": app.store.lock().unwrap().state.wake_risky.clone(),
        "voice_reply": app.voice_reply(g),
        "greeting_on": gs.greeting_on != Some(false),
        "greeting": gs.greeting.clone().unwrap_or_default(),
        "greeting_default": cfg.notice.voice,
        "volume": app.volume(g),
        "names": names,
        "styles": crate::config::STYLES,
        // 웹 콘솔에서 추가한 캐릭터(원본 — 고치기·지우기용)
        "custom_characters": app.store.lock().unwrap().state.custom_characters.clone(),
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
    /// "성격" / "존댓말"
    style: Option<String>,
    persona_polite: Option<String>,
}

async fn char_set(State(app): State<AppRef>, headers: HeaderMap, Path((g, key)): Path<(u64, String)>, Json(b): Json<CharPatch>) -> R {
    authed(&app, &headers)?;
    let g = nz(g)?;
    let cfg = app.cfg();
    if !cfg.characters.contains_key(&key) {
        return Err(err(StatusCode::NOT_FOUND, "캐릭터 없음"));
    }
    for p in b.brain.iter().chain(b.fallback.iter().filter(|f| !f.is_empty())) {
        match cfg.llm.presets.get(p) {
            None => return Err(err(StatusCode::BAD_REQUEST, format!("프리셋 {p} 없음"))),
            Some(x) if x.is_decision() => return Err(err(StatusCode::BAD_REQUEST, format!("{p}는 판단 전용이라 두뇌로 못 써요"))),
            _ => {}
        }
    }
    if b.persona.as_ref().is_some_and(|p| p.chars().count() > 3000) || b.persona_polite.as_ref().is_some_and(|p| p.chars().count() > 3000) {
        return Err(err(StatusCode::BAD_REQUEST, "말투는 3000자 이내"));
    }
    if b.style.as_ref().is_some_and(|s| !crate::config::STYLES.contains(&s.as_str())) {
        return Err(err(StatusCode::BAD_REQUEST, "말투 종류는 성격·존댓말"));
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
                // 비었거나 config 기본값과 같으면 덮어쓰기로 남기지 않는다 — 기본 말투를 고치면 그대로 따라가게
                let default = cfg.characters.get(&key).map(|c| c.persona.trim().to_string()).unwrap_or_default();
                o.persona = if x.trim().is_empty() || x.trim() == default { None } else { Some(x) };
            }
            if let Some(x) = b.voice {
                o.voice = Some(x);
            }
            if let Some(x) = b.style {
                o.style = Some(x);
            }
            if let Some(x) = b.persona_polite {
                let default = cfg.characters.get(&key).map(|c| c.persona_polite.trim().to_string()).unwrap_or_default();
                o.persona_polite = if x.trim().is_empty() || x.trim() == default { None } else { Some(x) };
            }
        }
    }
    app.save_store();
    Ok(Json(json!({"ok": true})).into_response())
}

// ------------------------------------------------------------------ 캐릭터 추가·삭제(전역) F-56

#[derive(Deserialize)]
struct CharNew {
    key: String,
    name: Option<String>,
    emoji: Option<String>,
    wake: Vec<String>,
    brain: String,
    fallback: Option<String>,
    persona: String,
    persona_polite: Option<String>,
    style: Option<String>,
    voice: Option<BTreeMap<String, Voice>>,
}

/// 캐릭터 추가·고치기 — config.toml 캐릭터 이름은 못 쓴다(그건 파일·서버별 설정으로)
async fn char_upsert(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<CharNew>) -> R {
    authed(&app, &headers)?;
    let key = b.key.trim().to_string();
    let file = app.cfg_file.read().unwrap().clone();
    if file.characters.contains_key(&key) {
        return Err(err(StatusCode::BAD_REQUEST, "config.toml에 있는 캐릭터예요 — 서버별 설정에서 바꿔 주세요"));
    }
    let cfg = app.cfg();
    let is_new = !cfg.characters.contains_key(&key);
    // 비교 대상: 지금 쓰는 캐릭터 + 저장돼 있지만 설정 다시 읽기에서 건너뛴 웹 캐릭터(다시 유효해질 수 있다)
    let skipped: Vec<(String, crate::config::Character)> = {
        let st = app.store.lock().unwrap();
        // 건너뛴 웹 캐릭터 + config에 같은 키가 생겨 가려진 웹 원본
        st.state.custom_characters.iter().filter(|(k, _)| **k != key && (!cfg.characters.contains_key(*k) || file.characters.contains_key(*k))).map(|(k, c)| (k.clone(), c.clone())).collect()
    };
    let others: Vec<(&String, &crate::config::Character)> =
        cfg.characters.iter().filter(|(k, _)| **k != key).chain(skipped.iter().map(|(k, c)| (k, c))).collect();
    // 표시 이름도 다른 캐릭터의 이름·키와 겹치면 안 된다(누가 누군지 헷갈림)
    let display = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&key).to_string();
    if let Some((k, _)) = others.iter().find(|(k, c)| c.name == display || **k == display) {
        return Err(err(StatusCode::BAD_REQUEST, format!("이름 '{display}'은(는) {k}가 이미 써요")));
    }
    if is_new && cfg.characters.len() >= 25 {
        return Err(err(StatusCode::BAD_REQUEST, "캐릭터는 25명까지(디스코드 선택지 한도)"));
    }
    let norm = |w: &String| w.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut wake: Vec<String> = vec![];
    for w in b.wake.iter().map(norm).filter(|w| !w.is_empty()) {
        if !wake.contains(&w) {
            wake.push(w);
        }
    }
    // 다른 캐릭터의 호출어(기본·추가·위험 별칭)와 겹치면 누가 불렸는지 모른다
    // 겹침 검사(wake_owner)는 건너뛴 웹 캐릭터의 기본·추가·위험 별칭까지 본다
    // 이 캐릭터에 남아 있는 추가 호출어·위험 별칭도 같이(설정 다시 읽기에서 건너뛰던 캐릭터를 다시 저장할 때)
    let own_extra: Vec<String> = {
        let st = app.store.lock().unwrap();
        st.state.wake_extra.get(&key).into_iter().flatten().chain(st.state.wake_risky.get(&key).into_iter().flatten()).cloned().collect()
    };
    for w in wake.iter().chain(own_extra.iter()) {
        let owner = voice::wake_owner(&app, w, &key);
        if let Some(k) = owner {
            return Err(err(StatusCode::BAD_REQUEST, format!("호출어 '{w}'은(는) {k}가 이미 써요")));
        }
    }
    // 남의 대화를 학습에 쓰는 무료 등급은 캐릭터 두뇌·대체 두뇌로 고르지 않는다(SPEC N-12 — config에서 주인이 직접만)
    for name in std::iter::once(&b.brain).chain(b.fallback.iter().filter(|f| !f.is_empty())) {
        let p = cfg.llm.presets.get(name).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("없는 두뇌 프리셋 {name}")))?;
        if p.trains_on_input {
            return Err(err(StatusCode::BAD_REQUEST, "입력을 학습에 쓰는 무료 등급은 웹에서 캐릭터 두뇌로 못 골라요"));
        }
        if p.is_decision() {
            return Err(err(StatusCode::BAD_REQUEST, format!("{name}는 판단 전용이라 두뇌로 못 써요")));
        }
    }
    let ch = crate::config::Character {
        name: b.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| key.clone()),
        emoji: b.emoji.unwrap_or_default().trim().to_string(),
        wake,
        brain: b.brain,
        fallback: b.fallback.filter(|f| !f.is_empty()),
        persona: b.persona,
        reminder: String::new(),
        persona_polite: b.persona_polite.unwrap_or_default(),
        reminder_polite: String::new(),
        style: b.style.unwrap_or_else(|| crate::config::STYLES[0].into()),
        voice: b.voice.unwrap_or_default(),
    };
    if ch.persona.chars().count() > 3000 || ch.persona_polite.chars().count() > 3000 {
        return Err(err(StatusCode::BAD_REQUEST, "말투는 3000자 이내"));
    }
    if ch.voice.len() > crate::config::TTS_PROVIDERS.len() {
        return Err(err(StatusCode::BAD_REQUEST, "목소리 항목이 너무 많음"));
    }
    crate::config::validate_character(&key, &ch, &cfg.llm.presets).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    // 이모지 호출이 겹치면 첫 캐릭터만 불린다 — 호출 쪽과 같이 변형 선택자(U+FE0F)는 빼고 비교
    let clean = |e: &str| e.chars().filter(|&c| c != '\u{fe0f}').collect::<String>();
    if !clean(&ch.emoji).is_empty() {
        if let Some((k, _)) = others.iter().find(|(_, c)| clean(&c.emoji) == clean(&ch.emoji)) {
            return Err(err(StatusCode::BAD_REQUEST, format!("이모지 {}는 {k}가 이미 써요", ch.emoji)));
        }
    }
    app.store.lock().unwrap().state.custom_characters.insert(key.clone(), ch);
    app.save_store();
    app.refresh_cfg();
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for s in sessions {
        s.reload_wake(&app);
    }
    tracing::info!("캐릭터 {} {key}", if is_new { "추가" } else { "고침" });
    Ok(Json(json!({"ok": true, "new": is_new})).into_response())
}

async fn char_delete(State(app): State<AppRef>, headers: HeaderMap, Path(key): Path<String>) -> R {
    authed(&app, &headers)?;
    if !app.is_custom(&key) {
        return Err(err(StatusCode::BAD_REQUEST, "웹 콘솔에서 추가한 캐릭터만 지울 수 있어요"));
    }
    // config.toml에 같은 이름이 새로 생겨 건너뛰던 웹 캐릭터면 웹 쪽 원본만 지운다(config 캐릭터의 호출어·서버 설정은 그대로)
    let in_file = app.cfg_file.read().unwrap().characters.contains_key(&key);
    {
        let mut st = app.store.lock().unwrap();
        st.state.custom_characters.remove(&key);
        if !in_file {
            st.state.wake_extra.remove(&key);
            st.state.wake_risky.remove(&key);
            for gs in st.state.guilds.values_mut() {
                gs.characters.remove(&key);
            }
        }
    }
    app.save_store();
    app.refresh_cfg();
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for s in sessions {
        s.reload_wake(&app);
    }
    tracing::info!("캐릭터 삭제 {key}");
    Ok(Json(json!({"ok": true})).into_response())
}

// ------------------------------------------------------------------ 판단 모델 F-54

async fn judge_get(State(app): State<AppRef>, headers: HeaderMap) -> R {
    authed(&app, &headers)?;
    let cfg = app.cfg();
    let disabled = app.store.lock().unwrap().state.disabled_presets.clone();
    let presets: Vec<Value> = cfg
        .llm
        .presets
        .iter()
        // 학습 안 함·콘솔에서 안 끈 것 중 무료 OpenAI 호환이거나 판단 전용인 것만 고를 수 있게
        .filter(|(k, p)| !p.trains_on_input && (p.is_decision() || (p.free && p.provider != "anthropic")) && !disabled.contains(*k))
        .map(|(k, p)| {
            json!({"name": k, "model": p.model, "has_key": p.has_key(), "decision": p.is_decision(), "free": p.free,
                   "price_in": p.price_in, "threshold": p.judge_threshold, "note": p.judge_note})
        })
        .collect();
    Ok(Json(json!({"judge": app.judge(), "default": cfg.judge, "presets": presets, "scopes": crate::config::JUDGE_SCOPES})).into_response())
}

#[derive(Deserialize)]
struct JudgePatch {
    judge: Option<crate::config::Judge>,
    reset: Option<bool>,
}

async fn judge_set(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<JudgePatch>) -> R {
    authed(&app, &headers)?;
    if b.reset == Some(true) {
        app.store.lock().unwrap().state.judge = None;
    } else if let Some(j) = b.judge {
        j.validate(&app.cfg()).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        // 켜서 저장할 때만 — 끄는 설정은 꺼진 프리셋이어도 저장할 수 있다
        if j.on && app.store.lock().unwrap().state.disabled_presets.contains(&j.preset) {
            return Err(err(StatusCode::BAD_REQUEST, format!("{}는 콘솔에서 꺼져 있어요 — 먼저 켜 주세요", j.preset)));
        }
        app.store.lock().unwrap().state.judge = Some(j);
    }
    app.save_store();
    Ok(Json(json!({"ok": true, "judge": app.judge()})).into_response())
}

#[derive(Deserialize)]
struct JudgeTest {
    character: String,
    text: String,
    /// 앞 대화(줄마다 "이름: 말")
    context: Option<String>,
    /// 저장하지 않고 이 프리셋·기준으로 시험(비우면 지금 설정)
    preset: Option<String>,
    threshold: Option<f64>,
    /// 모델을 부르지 않고 보낼 요청 원문만(비용 없음)
    dry: Option<bool>,
}

/// 판단 모델 시험 — 무료 프리셋은 비용 없음, 판단 전용 유료는 회당 0.0001달러 아래(하루 사용액에 더함)
async fn judge_test(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<JudgeTest>) -> R {
    authed(&app, &headers)?;
    let cfg = app.cfg();
    let ch = cfg.characters.get(&b.character).ok_or_else(|| err(StatusCode::NOT_FOUND, "캐릭터 없음"))?;
    let text = b.text.trim();
    if text.is_empty() || text.chars().count() > 300 || b.context.as_ref().is_some_and(|c| c.chars().count() > 2000) {
        return Err(err(StatusCode::BAD_REQUEST, "발화는 1~300자, 앞 대화는 2000자까지"));
    }
    // 앞 대화를 기록 줄 모양으로
    let now = Instant::now();
    let lines: Vec<crate::convo::Line> = b
        .context
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .enumerate()
        .map(|(i, l)| {
            let (s, t) = l.split_once(':').map(|(a, b)| (a.trim(), b.trim())).unwrap_or(("친구", l.trim()));
            crate::convo::Line { seq: i as u64, time: "00:00:00".into(), at: now, user: 1, speaker: s.to_string(), text: t.to_string() }
        })
        .collect();
    let wake = crate::voice::build_wake(&app);
    let hit = wake.find(text, false);
    // 규칙이 고른 캐릭터와 시험할 캐릭터가 같을 때만 그 낱말을 넘긴다(다르면 캐릭터 이름 그대로)
    let word = hit.as_ref().filter(|h| h.character == b.character).map(|h| h.word.clone()).unwrap_or_else(|| ch.name.clone());
    let names: Vec<String> = cfg.characters.values().map(|c| c.name.clone()).collect();
    let variants = crate::voice::name_variants(&app, &b.character);
    let jp = crate::llm::judge_prompt(&names, &ch.name, &variants, &word, &lines, "시험", text);
    let mut jd = app.judge();
    // 저장하지 않고 고른 모델·기준으로 — 기준만 바꿔도 적용하고, 바꾼 조합은 저장 때와 같은 검사를 거친다
    let pick = b.preset.filter(|p| !p.is_empty());
    if pick.is_some() || b.threshold.is_some() {
        if let Some(p) = pick {
            jd.preset = p;
        }
        jd.threshold = b.threshold;
        jd.on = true;
        jd.validate(&cfg).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    }
    // 보내는 요청 원문 — 실제 호출과 같은 함수로 만든다(키는 헤더라 여기 없음)
    let preset = cfg.llm.presets.get(&jd.preset).ok_or_else(|| err(StatusCode::BAD_REQUEST, "없는 프리셋"))?;
    // 원문만 보기도 실제 시험과 같은 조건 — 콘솔에서 끈 프리셋은 안 됨
    if app.store.lock().unwrap().state.disabled_presets.contains(&jd.preset) {
        return Err(err(StatusCode::BAD_REQUEST, format!("{}는 콘솔에서 꺼져 있어요", jd.preset)));
    }
    let (url, body) = crate::llm::judge_request(preset, &jp);
    let request = json!({"url": url, "body": body});
    let rule = hit.map(|h| json!({"character": h.character, "via": h.via, "word": h.word}));
    if b.dry == Some(true) {
        return Ok(Json(json!({"dry": true, "preset": jd.preset, "threshold": preset.is_decision().then(|| jd.threshold_for(preset)), "request": request, "rule": rule})).into_response());
    }
    let v = crate::llm::judge(&app, &jd, &jd.preset, &jp, Duration::from_millis(jd.timeout_ms))
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({
        "call": v.call, "ms": v.ms, "raw": v.raw, "preset": jd.preset, "p": v.p, "threshold": v.threshold,
        "rule": rule, "request": request,
    }))
    .into_response())
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
    words: Option<Vec<String>>,
    /// 위험 별칭(실장·약정처럼 평소 쓰는 말)
    risky: Option<Vec<String>>,
}

/// 호출어 사전(전역) — config 호출어에 더할 받아쓰기 변형. 바로 모든 세션에 반영
async fn wake_set(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<WakePatch>) -> R {
    authed(&app, &headers)?;
    // 두 목록을 먼저 다 검증한다 — 하나만 저장되고 400이 나가는 일이 없게
    for w in [&b.words, &b.risky].into_iter().flatten() {
        voice::check_wake_words(&app, &b.character, w).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
    }
    // 두 목록을 같이 바꿀 땐 새 목록끼리 겹치지 않는지 먼저(저장 순서 때문에 한쪽만 바뀌지 않게)
    if let (Some(a), Some(r)) = (&b.words, &b.risky) {
        // 호출어 패턴은 띄어쓰기를 무시하므로 비교도 공백을 다 빼고
        let norm = |w: &String| w.split_whitespace().collect::<String>();
        if let Some(x) = a.iter().map(norm).find(|x| r.iter().map(norm).any(|y| &y == x)) {
            return Err(err(StatusCode::BAD_REQUEST, &format!("'{x}'이(가) 추가 호출어와 위험 별칭 양쪽에 있어요 — 한쪽만 두세요")));
        }
        // 저장은 위험 별칭을 먼저 비우고(겹침 검사가 옛 목록에 걸리지 않게) 순서대로
        voice::set_wake_risky(&app, &b.character, &[]).await.map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
    }
    let mut res = json!({"ok": true});
    if let Some(w) = &b.words {
        match voice::set_wake_extra(&app, &b.character, w).await {
            Ok(words) => res["words"] = json!(words),
            Err(e) => return Err(err(StatusCode::BAD_REQUEST, &e)),
        }
    }
    if let Some(w) = &b.risky {
        match voice::set_wake_risky(&app, &b.character, w).await {
            Ok(words) => res["risky"] = json!(words),
            Err(e) => return Err(err(StatusCode::BAD_REQUEST, &e)),
        }
    }
    Ok(Json(res).into_response())
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
            s.forget_calls(id);
        }
        // 역할 무시를 더하면 이 서버의 호출 대기(다음 말·판단)를 모두 무효로
        if b.add && b.kind == "role" {
            s.forget_all_calls();
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
    // 비용 요약에서 프리셋을 대답 두뇌·판단 모델로 나누고 모델 이름을 보여 주려고(값만, 키 없음)
    let cfg = app.cfg();
    let presets: BTreeMap<String, Value> = cfg
        .llm
        .presets
        .iter()
        .map(|(k, p)| (k.clone(), json!({"model": p.model, "provider": p.provider, "decision": p.is_decision(), "free": p.free, "price_in": p.price_in, "price_out": p.price_out})))
        .collect();
    Ok(Json(json!({
        "presets": presets,
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
    let keys = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GROQ_API_KEY", "GEMINI_API_KEY", "DEEPGRAM_API_KEY", "AZURE_SPEECH_KEY", "ELEVENLABS_API_KEY", "TYPECAST_API_KEY", "OPENROUTER_API_KEY", "DISCORD_CLIENT_SECRET"];
    let key_state: BTreeMap<&str, bool> = keys.iter().map(|k| (*k, std::env::var(k).map(|v| !v.trim().is_empty()).unwrap_or(false))).collect();
    Ok(Json(json!({
        "stt_chain": app.stt_chain(),
        "stt_hint": app.store.lock().unwrap().state.stt_hint.clone(),
        "tts_chain": app.tts_chain(),
        "stt_default": cfg.stt.chain,
        "tts_default": cfg.tts.chain,
        "stt_known": ["local-whisper:cuda:large-v3-turbo", "local-whisper:cpu:small", "local-whisper:cpu:medium", "groq-whisper", "groq-whisper-turbo", "deepgram", "openai-transcribe"],
        "tts_known": crate::config::TTS_PROVIDERS,
        "presets": cfg.llm.presets.iter().map(|(k, p)| (k.clone(), json!({"provider": p.provider, "model": p.model, "free": p.free, "owner_only": p.owner_only, "trains_on_input": p.trains_on_input, "has_key": p.has_key(), "key_env": p.key_env(), "disabled": disabled.contains(k), "effort": p.reasoning_effort, "price_in": p.price_in, "price_out": p.price_out, "decision": p.is_decision()}))).collect::<BTreeMap<_, _>>(),
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
        if c.is_empty() || c.len() > 6 || !c.iter().all(|x| crate::config::TTS_PROVIDERS.contains(&x.as_str())) {
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
    character: Option<String>,
    style: Option<String>,
}

/// 두뇌 시험 — 프리셋 하나를 실제로 불러 지연·토큰·형식을 본다(유료면 하루 상한에 잡힘). F-35
async fn llm_test(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<LlmTestReq>) -> R {
    authed(&app, &headers)?;
    match app.cfg().llm.presets.get(&b.preset) {
        None => return Err(err(StatusCode::BAD_REQUEST, "없는 프리셋")),
        Some(p) if p.is_decision() => return Err(err(StatusCode::BAD_REQUEST, "판단 전용 모델은 판단 모델 카드에서 시험해요")),
        _ => {}
    }
    let q = b.question.map(|q| q.trim().to_string()).filter(|q| !q.is_empty()).unwrap_or_else(|| "오늘 저녁 메뉴 하나만 추천해 줘.".into());
    if q.chars().count() > 300 {
        return Err(err(StatusCode::BAD_REQUEST, "질문은 300자까지"));
    }
    let a = crate::llm::probe(&app, &b.preset, &q, b.fill.unwrap_or(false), b.character.as_deref(), b.style.as_deref())
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
    let (lines, events, stats, listening, timelines) = match s {
        Some(s) => {
            let keep = Duration::from_secs(app.listen_opts(g).1 * 60);
            let (l, e) = {
                let mut b = s.buffer.lock().unwrap();
                (b.snapshot(keep), b.events(keep))
            };
            let st = s.stats.lock().unwrap().clone();
            let tl: Vec<crate::voice::Timeline> = s.timelines.lock().unwrap().iter().rev().cloned().collect();
            (l, e, Some(st), s.listening.load(std::sync::atomic::Ordering::Relaxed), tl)
        }
        None => (vec![], vec![], None, false, vec![]),
    };
    // 화면 표시만 — 저장·내려받기 없음(SPEC N-06). 받아쓰기 기록에는 글 내용이 없다
    let side = app.sidecar.lock().unwrap().ok;
    let last = app.last_used.lock().unwrap().stt.clone();
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"lines": lines, "events": events, "stats": stats, "listening": listening, "sidecar_ok": side, "stt_chain": app.stt_chain(), "stt_last": last, "timelines": timelines})),
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
        s.forget_all_calls();
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
    /// 저장하지 않고 들어 볼 목소리(없으면 저장된 것)
    voice: Option<std::collections::BTreeMap<String, crate::config::Voice>>,
    /// 이 공급자로만(빠른 설정은 무료 edge만 — 유료 캐릭터 목소리가 끼어들지 않게)
    only: Option<String>,
}

/// 목소리 미리 듣기 — 브라우저에서 재생할 WAV를 돌려준다(F-41)
async fn tts_preview(State(app): State<AppRef>, headers: HeaderMap, Json(b): Json<PreviewReq>) -> R {
    authed(&app, &headers)?;
    let g = b.guild.and_then(|g| g.parse::<u64>().ok()).unwrap_or(0); // 0은 "서버 설정 없음"으로만 쓰고 ID 생성에 안 쓴다
    let ch = app.character(g, &b.character).ok_or_else(|| err(StatusCode::NOT_FOUND, "캐릭터 없음"))?;
    let text = b.text.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| format!("안녕, 나는 {}이야. 이 목소리 어때?", ch.name));
    let voice = match b.voice {
        Some(v) => {
            for (prov, x) in &v {
                crate::config::validate_voice(prov, x).map_err(|e| err(StatusCode::BAD_REQUEST, &e))?;
            }
            v
        }
        None => ch.voice.clone(),
    };
    let mut chain = sidecar::voice_chain(&app, &voice);
    if let Some(p) = &b.only {
        if !crate::config::TTS_PROVIDERS.contains(&p.as_str()) {
            return Err(err(StatusCode::BAD_REQUEST, "모르는 공급자"));
        }
        let v = voice.get(p).cloned().unwrap_or_default();
        chain = vec![sidecar::TtsVoice { provider: p.clone(), voice: v.voice, rate: v.rate, pitch: v.pitch }];
    }
    let (wav, provider) = sidecar::tts(&app, &text, &chain).await.map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(([(header::CONTENT_TYPE, "audio/wav".to_string()), (header::HeaderName::from_static("x-provider"), provider)], wav).into_response())
}

async fn voices(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<BTreeMap<String, String>>) -> R {
    authed(&app, &headers)?;
    let provider = q.get("provider").map(String::as_str).unwrap_or("edge-tts");
    if !crate::config::TTS_PROVIDERS.contains(&provider) {
        return Err(err(StatusCode::BAD_REQUEST, "모르는 공급자"));
    }
    let v = sidecar::voices(&app, provider).await.map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
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
        .route("/api/login-info", get(login_info))
        .route("/auth/discord", get(discord_start))
        .route("/auth/discord/callback", get(discord_callback))
        .route("/api/characters", post(char_upsert))
        .route("/api/characters/{key}/delete", post(char_delete))
        .route("/api/judge", get(judge_get).post(judge_set))
        .route("/api/judge-test", post(judge_test))
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
