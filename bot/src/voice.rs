//! 음성 세션: 수신 → 거르기 → 발화 자르기 → 받아쓰기 → 호출어 → 대답(말·글) → 재생.
//! SPEC F-02~F-05, F-10~F-13, F-20~F-25, N-01, N-06.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serenity::all::{ChannelId, CreateMessage, GuildId, UserId};
use serenity::async_trait;
use songbird::events::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, TrackEvent};
use songbird::tracks::TrackHandle;
use songbird::Call;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::app::AppRef;
use crate::convo::{Buffer, Wake};
use crate::{llm, sidecar};

pub struct Participant {
    pub name: String,
    pub bot: bool,
    pub roles: Vec<u64>,
    pub fetched: Instant,
    allowed: Option<(u64, bool)>,
}

struct Seg {
    user: u64,
    pcm: Vec<i16>,
    abs_sum: u64,
    silence_ticks: u32,
}

struct Utterance {
    user: u64,
    pcm: Vec<i16>,
    ended: Instant,
}

pub struct Job {
    pub char_key: String,
    pub user: u64,
    pub caller: String,
    pub question: String,
    pub t0: Instant,
}

struct Playing {
    key: (u64, String),
    handle: TrackHandle,
}

#[derive(Default, Clone, serde::Serialize)]
pub struct SessionStats {
    pub utterances: u64,
    pub dropped_noise: u64,
    pub stt_filtered: u64,
    pub stt_failed: u64,
    pub wakes: u64,
    pub answers: u64,
    pub answer_failed: u64,
    /// 수신 진단(받아쓰기 상황) — 소리가 든 20ms 묶음 수, 누구 소리인지 모름(SSRC 미매핑), 거르기(무시·안 듣는 중)로 버림
    pub rx_frames: u64,
    pub rx_unknown: u64,
    pub rx_blocked: u64,
}

pub struct Session {
    pub guild: GuildId,
    pub channel: Mutex<ChannelId>,
    pub call: Arc<tokio::sync::Mutex<Call>>,
    pub buffer: Mutex<Buffer>,
    pub participants: Mutex<HashMap<u64, Participant>>,
    pub listening: AtomicBool,
    pub started: Instant,
    pub stats: Mutex<SessionStats>,
    ssrc_user: Mutex<HashMap<u32, u64>>,
    segs: Mutex<HashMap<u32, Seg>>,
    utt_tx: mpsc::Sender<Utterance>,
    job_tx: mpsc::Sender<Job>,
    current: Mutex<Option<Playing>>,
    pending: Mutex<HashMap<u64, (String, Instant)>>,
    wake: RwLock<Wake>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    fetching: Mutex<HashSet<u64>>,
}

/// config 호출어(첫 번째가 대표 이름) + 웹 콘솔 호출어 사전
pub fn build_wake(app: &AppRef) -> Wake {
    let cfg = app.cfg();
    let extra = app.store.lock().unwrap().state.wake_extra.clone();
    let list: Vec<(String, Vec<String>)> = cfg
        .characters
        .iter()
        .map(|(k, c)| {
            let mut w = c.wake.clone();
            for x in extra.get(k).into_iter().flatten() {
                if !w.contains(x) {
                    w.push(x.clone());
                }
            }
            (k.clone(), w)
        })
        .collect();
    Wake::new(&list)
}

impl Session {
    fn keep(&self, app: &AppRef) -> Duration {
        Duration::from_secs(app.listen_opts(self.guild.get()).1 * 60)
    }

    /// 이 사람 소리를 받아도 되나 — 봇 → 본인 거부 → 유저 무시 → 역할 무시(SPEC F-10 판정 순서)
    fn allowed(&self, app: &AppRef, user: u64) -> Option<bool> {
        let (ver, verdict) = {
            let st = app.store.lock().unwrap();
            let ver = st.version;
            let mut parts = self.participants.lock().unwrap();
            let p = parts.get_mut(&user)?;
            if let Some((v, a)) = p.allowed {
                if v == ver {
                    return Some(a);
                }
            }
            let g = st.state.guilds.get(&self.guild.get());
            let ok = !p.bot
                && !g.is_some_and(|g| {
                    g.self_ignored.contains(&user)
                        || g.ignore_users.contains(&user)
                        || p.roles.iter().any(|r| g.ignore_roles.contains(r))
                });
            p.allowed = Some((ver, ok));
            (ver, ok)
        };
        let _ = ver;
        Some(verdict)
    }

    pub fn reset_allowed(&self) {
        for p in self.participants.lock().unwrap().values_mut() {
            p.allowed = None;
        }
    }

    pub fn clear_audio(&self) {
        self.segs.lock().unwrap().clear();
        self.pending.lock().unwrap().clear();
    }

    pub fn reload_wake(&self, app: &AppRef) {
        *self.wake.write().unwrap() = build_wake(app);
    }

    pub fn current_users(&self) -> Vec<u64> {
        self.ssrc_user.lock().unwrap().values().copied().collect()
    }

    /// 지금 이 음성 채널에 있는 사람(봇 자신 제외) — 아직 말하지 않은 사람 포함(상태 표시용, SPEC F-40)
    pub fn channel_members(&self, app: &AppRef) -> Vec<(u64, String)> {
        let Some(ctx) = app.ctx() else { return vec![] };
        let me = ctx.cache.current_user().id;
        let ch = *self.channel.lock().unwrap();
        let mut out: Vec<(u64, String)> = ctx
            .cache
            .guild(self.guild)
            .map(|g| {
                g.voice_states
                    .iter()
                    .filter(|(u, v)| v.channel_id == Some(ch) && **u != me)
                    .map(|(u, v)| {
                        let name = v.member.as_ref().map(|m| m.display_name().to_string()).unwrap_or_else(|| u.get().to_string());
                        (u.get(), name)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let parts = self.participants.lock().unwrap();
        for (u, name) in out.iter_mut() {
            if let Some(p) = parts.get(u) {
                *name = p.name.clone();
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    fn enqueue(&self, job: Job) {
        // 같은 사람이 같은 캐릭터를 다시 부르면 지금 대답을 끊는다 — SPEC F-24
        if let Some(p) = self.current.lock().unwrap().as_ref() {
            if p.key == (job.user, job.char_key.clone()) {
                let _ = p.handle.stop();
            }
        }
        self.stats.lock().unwrap().wakes += 1;
        // 대답 대기열은 8개까지 — 넘치면 이번 호출은 버린다(메모리 무한 증가 방지)
        if self.job_tx.try_send(job).is_err() {
            tracing::warn!("대답 대기열이 가득 차 호출 하나를 버림");
        }
    }
}

// ------------------------------------------------------------------ 수신

#[derive(Clone)]
struct Receiver {
    app: AppRef,
    sess: Arc<Session>,
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(sp) => {
                if let Some(uid) = sp.user_id {
                    if self.sess.ssrc_user.lock().unwrap().insert(sp.ssrc, uid.0).is_none() {
                        tracing::info!("말하는 사람 연결 ssrc={} user={}", sp.ssrc, uid.0);
                    }
                    ensure_participant(&self.app, &self.sess, uid.0);
                }
            }
            EventContext::VoiceTick(tick) => self.on_tick(tick),
            EventContext::ClientDisconnect(d) => {
                let u = d.user_id.0;
                let mut map = self.sess.ssrc_user.lock().unwrap();
                let ssrcs: Vec<u32> = map.iter().filter(|(_, v)| **v == u).map(|(k, _)| *k).collect();
                let mut segs = self.sess.segs.lock().unwrap();
                for s in ssrcs {
                    map.remove(&s);
                    segs.remove(&s);
                }
            }
            _ => {}
        }
        None
    }
}

impl Receiver {
    fn on_tick(&self, tick: &songbird::events::context_data::VoiceTick) {
        let app = &self.app;
        let sess = &self.sess;
        let lc = app.cfg().listen.clone();
        let listening = sess.listening.load(Ordering::Relaxed);
        let map = sess.ssrc_user.lock().unwrap().clone();
        let mut finals: Vec<Seg> = vec![];
        let (mut frames, mut unknown, mut blocked) = (0u64, 0u64, 0u64);
        {
            let mut segs = sess.segs.lock().unwrap();
            for (ssrc, data) in &tick.speaking {
                frames += 1;
                let Some(&user) = map.get(ssrc) else {
                    unknown += 1;
                    continue;
                };
                // 거르기는 수신 단계에서 — 받아쓰기도 저장도 안 한다(SPEC F-05, F-10, F-13)
                if sess.allowed(app, user) != Some(true) || !listening {
                    blocked += 1;
                    segs.remove(ssrc);
                    continue;
                }
                let Some(pcm) = &data.decoded_voice else { continue };
                if pcm.is_empty() {
                    continue;
                }
                let seg = segs.entry(*ssrc).or_insert_with(|| Seg { user, pcm: Vec::with_capacity(16000 * 4), abs_sum: 0, silence_ticks: 0 });
                seg.pcm.extend_from_slice(pcm);
                seg.abs_sum += pcm.iter().map(|x| x.unsigned_abs() as u64).sum::<u64>();
                seg.silence_ticks = 0;
                if seg.pcm.len() as u64 >= lc.max_utterance_ms * 16 {
                    finals.push(segs.remove(ssrc).unwrap());
                }
            }
            let need = (lc.silence_ms / 20).max(1) as u32;
            let quiet: Vec<u32> = segs.keys().filter(|s| !tick.speaking.contains_key(s)).copied().collect();
            for s in quiet {
                let seg = segs.get_mut(&s).unwrap();
                seg.silence_ticks += 1;
                if seg.silence_ticks >= need {
                    finals.push(segs.remove(&s).unwrap());
                }
            }
        }
        if frames > 0 {
            let mut st = sess.stats.lock().unwrap();
            st.rx_frames += frames;
            st.rx_unknown += unknown;
            st.rx_blocked += blocked;
        }
        for seg in finals {
            let n = seg.pcm.len() as u64;
            let level = if n > 0 { seg.abs_sum / n } else { 0 };
            if n < lc.min_utterance_ms * 16 || level < lc.min_level as u64 {
                sess.stats.lock().unwrap().dropped_noise += 1;
                // 판정과 기록을 버퍼 잠금 안에서 — 조용해서 끝난 조각은 그새 무시·주인 퇴장이 있었을 수 있다
                log_stt(app, sess, seg.user, "noise", n / 16, 0, "", 0);
                continue;
            }
            let ended = Instant::now() - Duration::from_millis(seg.silence_ticks as u64 * 20);
            sess.stats.lock().unwrap().utterances += 1;
            if sess.utt_tx.try_send(Utterance { user: seg.user, pcm: seg.pcm, ended }).is_err() {
                tracing::warn!("받아쓰기 대기열이 가득 차 발화 하나를 버림");
            }
        }
    }
}

/// 멤버 정보(봇 여부·역할·이름)를 가져온다. 캐시 → 멤버 조회 API(특권 인텐트 없이) — SPEC F-05, F-10
fn ensure_participant(app: &AppRef, sess: &Arc<Session>, user: u64) {
    if sess.participants.lock().unwrap().contains_key(&user) || !sess.fetching.lock().unwrap().insert(user) {
        return;
    }
    let (app, sess) = (app.clone(), sess.clone());
    tokio::spawn(async move {
        let r = fetch_member(&app, sess.guild, user).await;
        sess.fetching.lock().unwrap().remove(&user);
        match r {
            Ok(p) => {
                sess.participants.lock().unwrap().insert(user, p);
            }
            Err(e) => tracing::warn!("멤버 정보 실패 user={user}: {e}"),
        }
    });
}

async fn fetch_member(app: &AppRef, guild: GuildId, user: u64) -> Result<Participant> {
    let ctx = app.ctx().ok_or_else(|| anyhow!("디스코드 미접속"))?;
    let cached = ctx.cache.guild(guild).and_then(|g| {
        g.members.get(&UserId::new(user)).cloned().or_else(|| g.voice_states.get(&UserId::new(user)).and_then(|v| v.member.clone()))
    });
    let m = match cached {
        Some(m) => m,
        None => ctx.http.get_member(guild, UserId::new(user)).await?,
    };
    Ok(Participant {
        name: m.display_name().to_string(),
        bot: m.user.bot,
        roles: m.roles.iter().map(|r| r.get()).collect(),
        fetched: Instant::now(),
        allowed: None,
    })
}

/// 역할 변화를 반영하려고 주기적으로 API로 다시 읽는다 — SPEC F-10 "5분마다"
async fn refresh_members(app: AppRef, sess: Arc<Session>) {
    loop {
        let secs = app.cfg().listen.member_refresh_secs.max(30);
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let users: Vec<u64> = sess.participants.lock().unwrap().keys().copied().collect();
        let Some(ctx) = app.ctx() else { continue };
        for u in users {
            if let Ok(m) = ctx.http.get_member(sess.guild, UserId::new(u)).await {
                if let Some(p) = sess.participants.lock().unwrap().get_mut(&u) {
                    p.name = m.display_name().to_string();
                    p.bot = m.user.bot;
                    p.roles = m.roles.iter().map(|r| r.get()).collect();
                    p.fetched = Instant::now();
                    p.allowed = None;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

/// 조용한 채널에서도 기억 시간이 지나면 지운다 — SPEC N-06
async fn prune_loop(app: AppRef, sess: Arc<Session>) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let keep = sess.keep(&app);
        sess.buffer.lock().unwrap().prune(keep);
        let now = Instant::now();
        sess.pending.lock().unwrap().retain(|_, (_, until)| *until > now);
    }
}

/// 지금 듣는 대상인지와 제외 이유 — 상태 표시용(SPEC F-40)
pub fn exclusion(app: &AppRef, sess: &Session, user: u64) -> Option<&'static str> {
    let st = app.store.lock().unwrap();
    let parts = sess.participants.lock().unwrap();
    let p = parts.get(&user)?;
    if p.bot {
        return Some("봇");
    }
    let g = st.state.guilds.get(&sess.guild.get());
    if g.is_some_and(|g| g.self_ignored.contains(&user)) {
        return Some("스스로 거부");
    }
    if g.is_some_and(|g| g.ignore_users.contains(&user)) {
        return Some("유저 무시");
    }
    if g.is_some_and(|g| p.roles.iter().any(|r| g.ignore_roles.contains(r))) {
        return Some("역할 무시");
    }
    None
}

// ------------------------------------------------------------------ 받아쓰기

/// 받아쓰기 기록에 남길 이름 — 참가자 캐시, 없으면 ID
fn speaker_name(sess: &Session, user: u64) -> String {
    sess.participants.lock().unwrap().get(&user).map(|p| p.name.clone()).unwrap_or_else(|| user.to_string())
}

/// 받아쓰기 기록(웹 콘솔). 기다리는 사이 무시·주인 퇴장이 있었으면 남기지 않는다 — 판정과 기록을 버퍼 잠금 안에서 한 번에
#[allow(clippy::too_many_arguments)]
fn log_stt(app: &AppRef, sess: &Session, user: u64, kind: &'static str, audio_ms: u64, stt_ms: u64, provider: &str, chars: usize) {
    let name = speaker_name(sess, user);
    let mut b = sess.buffer.lock().unwrap();
    if sess.allowed(app, user) == Some(true) && sess.listening.load(Ordering::Relaxed) {
        b.log_stt(user, &name, kind, audio_ms, stt_ms, provider, chars, None);
    }
}

async fn stt_worker(app: AppRef, sess: Arc<Session>, mut rx: mpsc::Receiver<Utterance>) {
    while let Some(u) = rx.recv().await {
        let audio_ms = u.pcm.len() as u64 / 16;
        if !app.sidecar.lock().unwrap().ok {
            sess.stats.lock().unwrap().stt_failed += 1;
            log_stt(&app, &sess, u.user, "failed", audio_ms, 0, "사이드카 꺼짐", 0);
            continue; // "잠깐 못 들어요" 상태 — 상태 메시지로 알린다(SPEC N-09)
        }
        // 기다리는 사이 무시 목록에 들어갔으면 버린다(판정 순서: 사용자 필터 → 주인 부재)
        if sess.allowed(&app, u.user) != Some(true) || !sess.listening.load(Ordering::Relaxed) {
            continue;
        }
        match sidecar::stt(&app, &u.pcm).await {
            Ok(r) => {
                if let Some(p) = &r.provider {
                    app.last_used.lock().unwrap().stt = p.clone();
                    tracing::debug!("stt {p} {}ms", r.ms);
                }
                let provider = r.provider.as_deref().unwrap_or("");
                if r.filtered || r.text.trim().is_empty() {
                    sess.stats.lock().unwrap().stt_filtered += 1;
                    log_stt(&app, &sess, u.user, "filtered", audio_ms, r.ms, provider, 0);
                    continue;
                }
                // 받아쓰는 사이 /무시해줘·무시 목록·주인 퇴장이 있었으면 버린다(지운 기억에 다시 들어가지 않게)
                if sess.allowed(&app, u.user) != Some(true) || !sess.listening.load(Ordering::Relaxed) {
                    continue;
                }
                let text = r.text.trim();
                if let Some(wake) = handle_text(&app, &sess, u.user, text, u.ended) {
                    let name = speaker_name(&sess, u.user);
                    let mut b = sess.buffer.lock().unwrap();
                    if sess.allowed(&app, u.user) == Some(true) && sess.listening.load(Ordering::Relaxed) {
                        b.log_stt(u.user, &name, "ok", audio_ms, r.ms, provider, text.chars().count(), wake);
                    }
                }
            }
            Err(e) => {
                sess.stats.lock().unwrap().stt_failed += 1;
                log_stt(&app, &sess, u.user, "failed", audio_ms, 0, "", 0);
                tracing::warn!("받아쓰기 실패: {e}");
            }
        }
    }
}

/// 버퍼에 넣고 호출어를 본다. None = 버림(그새 무시됨), Some(부른 캐릭터 — 없으면 None)
fn handle_text(app: &AppRef, sess: &Arc<Session>, user: u64, text: &str, ended: Instant) -> Option<Option<String>> {
    let name = sess
        .participants
        .lock()
        .unwrap()
        .get(&user)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| user.to_string());
    {
        // 최종 검사와 저장을 버퍼 잠금 안에서 한 번에 — /무시해줘·무시 목록·주인 퇴장은 저장소·listening을 먼저 바꾼 뒤
        // 이 잠금을 잡고 버퍼를 지우므로, 여기서 통과했으면 지우기 전, 실패했으면 버린다(TOCTOU 방지)
        let mut b = sess.buffer.lock().unwrap();
        if sess.allowed(app, user) != Some(true) || !sess.listening.load(Ordering::Relaxed) {
            return None;
        }
        b.push(user, &name, text);
        b.prune(sess.keep(app));
    }
    let hey_only = app.hey_only(sess.guild.get());
    let hit = sess.wake.read().unwrap().find(text, hey_only);
    // 호출어만 말하고 끊었던 사람의 다음 발화 = 질문 — SPEC F-21
    let pend = sess.pending.lock().unwrap().remove(&user);
    if let (None, Some((ch, until))) = (&hit, pend) {
        if Instant::now() <= until {
            sess.enqueue(Job { char_key: ch.clone(), user, caller: name, question: text.to_string(), t0: ended });
            return Some(Some(ch));
        }
    }
    let Some(h) = hit else { return Some(None) };
    let ch = h.character.clone();
    if !h.rest.is_empty() {
        sess.enqueue(Job { char_key: h.character, user, caller: name, question: h.rest, t0: ended });
    } else {
        let secs = app.cfg().listen.followup_secs.clamp(1, 120);
        let until = Instant::now().checked_add(Duration::from_secs(secs)).unwrap_or_else(Instant::now);
        sess.pending.lock().unwrap().insert(user, (h.character, until));
    }
    Some(Some(ch))
}

// ------------------------------------------------------------------ 대답

struct EndNotify(Mutex<Option<oneshot::Sender<()>>>);

#[async_trait]
impl VoiceEventHandler for EndNotify {
    async fn act(&self, _: &EventContext<'_>) -> Option<Event> {
        if let Some(tx) = self.0.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Some(Event::Cancel)
    }
}

/// WAV를 재생하고 끝날 때까지 기다린다
async fn play(app: &AppRef, sess: &Arc<Session>, wav: Vec<u8>, key: (u64, String)) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    // 서버별 음량(`/설정 음량`, 100% = 원래 크기)
    let vol = app.volume(sess.guild.get()) as f32 / 100.0;
    let track = songbird::tracks::Track::from(songbird::input::Input::from(wav)).volume(vol);
    let handle = {
        let mut call = sess.call.lock().await;
        call.play(track)
    };
    let n = Arc::new(EndNotify(Mutex::new(Some(tx))));
    handle.add_event(Event::Track(TrackEvent::End), EndNotifyRef(n.clone()))?;
    handle.add_event(Event::Track(TrackEvent::Error), EndNotifyRef(n))?;
    let id = handle.uuid();
    // 대답(부른 사람이 있는 재생)만 "진행 중 대답"으로 기록한다 — 고지·미리 듣기가 덮어쓰지 않게(SPEC F-24)
    let is_answer = key.0 != 0;
    if is_answer {
        *sess.current.lock().unwrap() = Some(Playing { key, handle });
    }
    let _ = tokio::time::timeout(Duration::from_secs(120), rx).await;
    if is_answer {
        let mut cur = sess.current.lock().unwrap();
        if cur.as_ref().is_some_and(|p| p.handle.uuid() == id) {
            *cur = None;
        }
    }
    Ok(())
}

struct EndNotifyRef(Arc<EndNotify>);

#[async_trait]
impl VoiceEventHandler for EndNotifyRef {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.0.act(ctx).await
    }
}

pub async fn speak(app: &AppRef, sess: &Arc<Session>, char_key: &str, text: &str, key: (u64, String)) -> Result<String> {
    let ch = app.character(sess.guild.get(), char_key).ok_or_else(|| anyhow!("캐릭터 {char_key} 없음"))?;
    let chain = sidecar::voice_chain(app, &ch.voice);
    let (wav, provider) = sidecar::tts(app, text, &chain).await?;
    app.last_used.lock().unwrap().tts = provider.clone();
    play(app, sess, wav, key).await?;
    Ok(provider)
}

/// 글을 올릴 채널 — 지정 채널(/채널지정·/들어와 친 곳), 없으면 듣는 음성 채널의 채팅(SPEC F-12, F-23)
pub fn text_channel(app: &AppRef, guild: GuildId) -> Option<ChannelId> {
    app.store
        .lock()
        .unwrap()
        .state
        .guilds
        .get(&guild.get())
        .and_then(|g| g.text_channel)
        .filter(|c| *c != 0) // 손으로 고친 state.json 대비(ChannelId::new(0)은 패닉)
        .map(ChannelId::new)
}

async fn notice_channel(app: &AppRef, guild: GuildId, voice: ChannelId) -> ChannelId {
    text_channel(app, guild).unwrap_or(voice)
}

/// 2,000자 제한에 맞춰 나눠 올린다 — SPEC F-23.
/// 멘션은 `mention` 한 사람만 실제 알림으로 — 답변 글 속 @everyone·역할 멘션이 퍼지지 않게
pub async fn post_to(app: &AppRef, ch: ChannelId, content: &str, mention: Option<u64>) -> bool {
    let Some(ctx) = app.ctx() else { return false };
    let mut chunk = String::new();
    let mut parts = vec![];
    for line in content.split_inclusive('\n') {
        if chunk.chars().count() + line.chars().count() > 1900 && !chunk.is_empty() {
            parts.push(std::mem::take(&mut chunk));
        }
        if line.chars().count() > 1900 {
            let cs: Vec<char> = line.chars().collect();
            for c in cs.chunks(1900) {
                parts.push(c.iter().collect());
            }
        } else {
            chunk.push_str(line);
        }
    }
    if !chunk.is_empty() {
        parts.push(chunk);
    }
    for p in parts {
        let mut am = serenity::all::CreateAllowedMentions::new().everyone(false).all_roles(false).all_users(false);
        if let Some(u) = mention {
            am = am.users([UserId::new(u)]);
        }
        if let Err(e) = ch.send_message(&ctx.http, CreateMessage::new().content(p).allowed_mentions(am)).await {
            tracing::warn!("텍스트 채널 게시 실패 {ch}: {e}");
            return false;
        }
    }
    true
}

pub async fn post(app: &AppRef, ch: ChannelId, content: &str) -> bool {
    post_to(app, ch, content, None).await
}

async fn answer_worker(app: AppRef, sess: Arc<Session>, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        let g = sess.guild.get();
        let Some(ch) = app.character(g, &job.char_key) else { continue };
        let lines = sess.buffer.lock().unwrap().snapshot(sess.keep(&app));
        let mut trace_users: Vec<u64> = lines.iter().map(|l| l.user).filter(|&u| u != 0).collect();
        trace_users.push(job.user);
        trace_users.sort_unstable();
        trace_users.dedup();
        // 지정 채널이 없으면(웹으로 입장 등) 듣는 음성 채널 채팅에 — 글 답이 버려지지 않게(SPEC F-23)
        let voice_ch = *sess.channel.lock().unwrap();
        let tc = Some(text_channel(&app, sess.guild).unwrap_or(voice_ch));
        let key = (job.user, job.char_key.clone());
        // 음성 대답을 껐으면(`/설정 음성대답 끄기`) 말은 만들지 않고 글로만 올린다
        let voice = app.voice_reply(g);
        // 스트리밍: <말>이 오는 즉시 음성을 시작하고, 긴 <글>은 다 받은 뒤 올린다(SPEC N-01)
        let (stx, srx) = oneshot::channel::<String>();
        let llm_fut = llm::answer(&app, g, &job.char_key, &ch, &job.caller, &job.question, &lines, voice.then_some(stx));
        let early = async {
            match srx.await {
                Ok(s) => {
                    let t = Instant::now();
                    let r = speak_measured(&app, &sess, &job, &s, key.clone()).await;
                    Some((r, t))
                }
                Err(_) => None, // 형식이 어긋나 미리 못 받음 — 끝난 뒤 말한다
            }
        };
        let (res, spoke) = tokio::join!(
            async {
                let r = llm_fut.await;
                // 글은 대답이 끝나는 즉시(음성 재생과 겹쳐서)
                // 기억 도구 — 글을 올리기 전에 적용해서 결과를 글 끝에 작게 붙인다(SPEC F-37)
                let notes = match &r {
                    Ok(a) if !a.memory.is_empty() => apply_memory(&app, g, job.user, &job.caller, &a.memory),
                    _ => vec![],
                };
                if let (Ok(a), Some(tc)) = (&r, tc) {
                    // <글>이 비었으면 <말>을 올린다 — 음성 대답을 껐을 때 대답이 사라지지 않게
                    let text = if a.text.trim().is_empty() { &a.speech } else { &a.text };
                    let mut body = format!("<@{}> {} **{}**: {}", job.user, ch.emoji, ch.name, text);
                    if !notes.is_empty() {
                        body.push_str(&format!("
-# 📝 {}", notes.join(" · ")));
                    }
                    let (app2, caller) = (app.clone(), job.user);
                    tokio::spawn(async move { post_to(&app2, tc, &body, Some(caller)).await });
                }
                r
            },
            early
        );
        match res {
            Ok(a) => {
                sess.stats.lock().unwrap().answers += 1;
                // 입출력 원문(웹 콘솔) — 그새 부른 사람이 무시됐으면 남기지 않는다(판정과 기록을 버퍼 잠금 안에서)
                {
                    let mut b = sess.buffer.lock().unwrap();
                    if sess.allowed(&app, job.user) == Some(true) {
                        b.log_llm(crate::convo::LlmTrace {
                            time: crate::convo::now_kst(),
                            at: Instant::now(),
                            users: trace_users.clone(),
                            character: ch.name.clone(),
                            caller: job.caller.clone(),
                            preset: a.preset.clone(),
                            ms: a.ms,
                            usd: a.usd,
                            tokens: a.tokens,
                            prompt: a.prompt.clone(),
                            raw: a.raw.clone(),
                        });
                    }
                }
                // 캐릭터 말도 기록에 남긴다(두 캐릭터가 같은 기록을 본다 — SPEC §3)
                // 글이 비었으면 말을 기억한다(채팅에 올린 것과 같게)
                let said = if a.text.trim().is_empty() { &a.speech } else { &a.text };
                let mem: String = said.chars().take(300).collect();
                sess.buffer.lock().unwrap().push(0, &ch.name, &mem);
                if a.capped {
                    notify_cap_once(&app, &sess, tc).await;
                }
                if !voice {
                    tracing::info!("대답(글만) llm={}ms preset={} usd={:.5}", a.ms, a.preset, a.usd);
                    continue;
                }
                // 글은 위에서 올렸다(부른 사람 멘션 + 긴 답 — SPEC F-23). 말: 미리 못 했으면 지금
                let (r, t_tts) = match spoke {
                    Some((r, t)) => (r, t),
                    None => {
                        let t = Instant::now();
                        (speak_measured(&app, &sess, &job, &a.speech, key).await, t)
                    }
                };
                if let Err(e) = r {
                    tracing::warn!("음성 대답 실패: {e}");
                } else {
                    tracing::info!(
                        "대답 llm={}ms 말도착={:?}ms tts+재생={}ms preset={} usd={:.5}",
                        a.ms, a.speech_ms, t_tts.elapsed().as_millis(), a.preset, a.usd
                    );
                }
            }
            Err(e) => {
                sess.stats.lock().unwrap().answer_failed += 1;
                let msg = e.to_string();
                if spoke.is_some() {
                    // 말은 이미 나갔다(두뇌가 끊긴 경우는 llm이 받은 데까지 Ok로 돌려주므로 보통 오지 않는다) — 실패 음성은 덧붙이지 않는다
                    tracing::warn!("대답 실패(말은 나감): {msg}");
                } else if msg.starts_with("CAP") {
                    if let Some(tc) = tc {
                        post_to(&app, tc, &format!("<@{}> {} 오늘은 여기까지! 유료 한도를 다 써서 내일 다시 불러 줘.", job.user, ch.emoji), Some(job.user)).await;
                    }
                    if voice {
                        let _ = speak(&app, &sess, &job.char_key, "오늘은 여기까지야. 내일 다시 불러 줘.", key).await;
                    }
                } else {
                    tracing::warn!("대답 실패: {msg}");
                    if let Some(tc) = tc {
                        post_to(&app, tc, &format!("<@{}> {} {}: 지금은 대답을 못 하겠어요. (두뇌 연결 실패 — 웹 콘솔 상태 확인)", job.user, ch.emoji, ch.name), Some(job.user)).await;
                    }
                }
            }
        }
    }
}

/// 기억 도구 적용 — 기억은 서버별, 최대 MEMORY_MAX_ITEMS개(넘치면 오래된 것부터). 돌려주는 건 글 끝에 붙일 안내
pub fn apply_memory(app: &AppRef, guild: u64, user: u64, by: &str, ops: &[llm::MemOp]) -> Vec<String> {
    let mut notes = Vec::new();
    {
        let mut st = app.store.lock().unwrap();
        let mems = &mut st.guild(guild).memories;
        for op in ops {
            match op {
                llm::MemOp::Remember(t) => {
                    if mems.iter().any(|m| &m.text == t) {
                        continue;
                    }
                    while mems.len() >= llm::MEMORY_MAX_ITEMS {
                        mems.remove(0);
                    }
                    mems.push(crate::store::Memory { text: t.clone(), by: by.chars().take(32).collect(), user, date: crate::store::today() });
                    notes.push(format!("기억했어: {t}"));
                }
                llm::MemOp::Forget(k) => {
                    // 한 글자 낱말로 엉뚱한 기억까지 지우지 않게 두 글자 이상만
                    if k.chars().count() < 2 {
                        continue;
                    }
                    let kl = k.to_lowercase();
                    let before = mems.len();
                    mems.retain(|m| !m.text.to_lowercase().contains(&kl));
                    let n = before - mems.len();
                    if n > 0 {
                        notes.push(format!("잊었어: {k} ({n}개)"));
                    }
                }
            }
        }
    }
    if !notes.is_empty() {
        app.save_store();
        tracing::info!("기억 도구 guild={guild} {}건", notes.len());
    }
    notes
}

/// 재생 시작 시점에 지연을 잰다(말 끝남 → 첫 소리) — SPEC N-01
async fn speak_measured(app: &AppRef, sess: &Arc<Session>, job: &Job, text: &str, key: (u64, String)) -> Result<()> {
    let ch = app.character(sess.guild.get(), &job.char_key).ok_or_else(|| anyhow!("캐릭터 없음"))?;
    let chain = sidecar::voice_chain(app, &ch.voice);
    let (wav, provider) = sidecar::tts(app, text, &chain).await?;
    app.last_used.lock().unwrap().tts = provider;
    app.record_latency(job.t0.elapsed().as_millis() as u64);
    play(app, sess, wav, key).await
}

async fn notify_cap_once(app: &AppRef, sess: &Arc<Session>, tc: Option<ChannelId>) {
    let already = app.store.lock().unwrap().day().cap_notified.contains(&sess.guild.get());
    if already {
        return;
    }
    let Some(tc) = tc else { return };
    // 실제로 올렸을 때만 "안내함"으로 기록 — 못 올렸으면 다음 대답 때 다시 시도
    if post(app, tc, "💸 오늘 유료 두뇌 한도에 닿아서 지금부터는 무료 모델로 대답할게요.").await {
        app.store.lock().unwrap().day().cap_notified.insert(sess.guild.get());
        app.save_store();
    }
}

// ------------------------------------------------------------------ 입장·퇴장

pub fn owner_in(app: &AppRef, guild: GuildId, channel: ChannelId) -> bool {
    let Some(ctx) = app.ctx() else { return false };
    let owner = UserId::new(app.cfg().owner_id);
    ctx.cache
        .guild(guild)
        .and_then(|g| g.voice_states.get(&owner).and_then(|v| v.channel_id))
        == Some(channel)
}

pub fn should_listen(app: &AppRef, guild: GuildId, channel: ChannelId) -> bool {
    !app.listen_opts(guild.get()).0 || owner_in(app, guild, channel)
}

pub async fn join(app: &AppRef, guild: GuildId, channel: ChannelId, text: Option<ChannelId>) -> Result<Arc<Session>> {
    if let Some(t) = text {
        // 지정 채널이 없을 때만 /들어와 친 채널을 기본값으로 — SPEC F-23
        let mut st = app.store.lock().unwrap();
        let g = st.guild(guild.get());
        if g.text_channel.is_none() {
            g.text_channel = Some(t.get());
            drop(st);
            app.save_store();
        }
    }
    let existing = app.sessions.lock().await.get(&guild.get()).cloned();
    if let Some(s) = existing {
        if *s.channel.lock().unwrap() != channel {
            app.songbird.join(guild, channel).await.map_err(|e| anyhow!("이동 실패: {e:?}"))?;
            *s.channel.lock().unwrap() = channel;
            s.listening.store(should_listen(app, guild, channel), Ordering::Relaxed);
            s.clear_audio();
            s.buffer.lock().unwrap().clear();
            // 새 채널 사람들에게도 고지 — SPEC F-12
            let nc = notice_channel(app, guild, channel).await;
            post(app, nc, &format!("{}\n(듣는 채널: <#{channel}>)", app.cfg().notice.text)).await;
            spawn_voice_notice(app, &s);
            spawn_unmapped_check(app, &s, nc);
            crate::presence::refresh(app).await;
        }
        return Ok(s);
    }
    let call = app.songbird.join(guild, channel).await.map_err(|e| anyhow!("입장 실패: {e:?}"))?;
    let (utt_tx, utt_rx) = mpsc::channel(32);
    let (job_tx, job_rx) = mpsc::channel(8);
    let sess = Arc::new(Session {
        guild,
        channel: Mutex::new(channel),
        call: call.clone(),
        buffer: Mutex::new(Buffer::default()),
        participants: Mutex::new(HashMap::new()),
        listening: AtomicBool::new(should_listen(app, guild, channel)),
        started: Instant::now(),
        stats: Mutex::new(SessionStats::default()),
        ssrc_user: Mutex::new(HashMap::new()),
        segs: Mutex::new(HashMap::new()),
        utt_tx,
        job_tx,
        current: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        wake: RwLock::new(build_wake(app)),
        tasks: Mutex::new(vec![]),
        fetching: Mutex::new(HashSet::new()),
    });
    {
        let mut c = call.lock().await;
        let rx = Receiver { app: app.clone(), sess: sess.clone() };
        c.add_global_event(CoreEvent::SpeakingStateUpdate.into(), rx.clone());
        c.add_global_event(CoreEvent::VoiceTick.into(), rx.clone());
        c.add_global_event(CoreEvent::ClientDisconnect.into(), rx);
    }
    // 이미 채널에 있는 사람들 정보를 미리
    if let Some(ctx) = app.ctx() {
        let ids: Vec<u64> = ctx
            .cache
            .guild(guild)
            .map(|g| g.voice_states.iter().filter(|(_, v)| v.channel_id == Some(channel)).map(|(u, _)| u.get()).collect())
            .unwrap_or_default();
        for u in ids {
            ensure_participant(app, &sess, u);
        }
    }
    let t1 = tokio::spawn(stt_worker(app.clone(), sess.clone(), utt_rx));
    let t2 = tokio::spawn(answer_worker(app.clone(), sess.clone(), job_rx));
    let t3 = tokio::spawn(refresh_members(app.clone(), sess.clone()));
    let t4 = tokio::spawn(prune_loop(app.clone(), sess.clone()));
    sess.tasks.lock().unwrap().extend([t1, t2, t3, t4]);
    app.sessions.lock().await.insert(guild.get(), sess.clone());
    tracing::info!("입장 guild={guild} channel={channel} listening={}", sess.listening.load(Ordering::Relaxed));

    // 녹음 고지 — 채팅 + 음성 1회(SPEC F-12)
    let notice = app.cfg().notice.clone();
    let nc = notice_channel(app, guild, channel).await;
    post(app, nc, &format!("{}\n(듣는 채널: <#{channel}>)", notice.text)).await;
    spawn_voice_notice(app, &sess);
    spawn_unmapped_check(app, &sess, nc);
    crate::presence::refresh(app).await;
    Ok(sess)
}

/// 음성 녹음 고지 1회 — 입장·채널 이동 때(SPEC F-12)
/// 디스코드가 "이 소리는 누구"(Speaking) 신호를 안 보내면 아무도 못 알아듣는다 — 2026-10-15 실제 발생.
/// 그때는 누가 채널에 새로 들어오면 신호가 한꺼번에 왔다. 입장 20초 뒤에도 소리가 전부 "누구인지 모름"이면 한 번 안내한다
fn spawn_unmapped_check(app: &AppRef, sess: &Arc<Session>, nc: ChannelId) {
    let (app2, sess2) = (app.clone(), sess.clone());
    let t = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let (frames, unknown) = {
            let st = sess2.stats.lock().unwrap();
            (st.rx_frames, st.rx_unknown)
        };
        let mapped = !sess2.ssrc_user.lock().unwrap().is_empty();
        if frames >= 100 && unknown == frames && !mapped {
            tracing::warn!("말하는 사람 연결 없음 frames={frames} — 안내");
            post(&app2, nc, "🔇 디스코드가 아직 누가 말하는지 알려 주지 않아서 못 알아듣고 있어요. 한 분이 음성 채널을 **나갔다 다시 들어오면** 들려요.").await;
        }
    });
    sess.tasks.lock().unwrap().push(t);
}

fn spawn_voice_notice(app: &AppRef, sess: &Arc<Session>) {
    let (app2, sess2) = (app.clone(), sess.clone());
    let cfg = app.cfg();
    let first_char = cfg.characters.keys().next().cloned().unwrap_or_default();
    // 인사를 껐으면 말하지 않는다(`/설정 인사말`) — 채팅 고지는 이미 올렸다
    let Some(text) = app.greeting(sess.guild.get()) else { return };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        // 사이드카 예열·재시작 중이면 실패한다 — 최대 2분 동안 다시 시도(고지는 반드시 한 번)
        for attempt in 1..=12 {
            match speak(&app2, &sess2, &first_char, &text, (0, String::new())).await {
                Ok(_) => return,
                Err(e) => {
                    tracing::warn!("음성 고지 실패({attempt}/12): {e}");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    });
}

pub async fn leave(app: &AppRef, guild: GuildId) -> bool {
    let s = app.sessions.lock().await.remove(&guild.get());
    let Some(s) = s else { return false };
    for t in s.tasks.lock().unwrap().drain(..) {
        t.abort();
    }
    s.buffer.lock().unwrap().clear();
    s.clear_audio();
    let _ = app.songbird.remove(guild).await;
    tracing::info!("퇴장 guild={guild}");
    crate::presence::refresh(app).await;
    true
}

/// 주인 입·퇴장 따라 듣기 켜고 끄기 — SPEC F-13
/// 듣는 채널에 누가 들어오고 나간 것을 대화 기록에 남긴다 — 두뇌가 "방금 누가 왔는지" 알게(2026-10-15 요구).
/// 봇·무시 대상(유저·역할·스스로 거부)은 남기지 않고, 안 듣는 중이면 남기지 않는다. 버퍼 규칙(기억 시간·지우기)을 그대로 따른다
pub async fn note_presence(app: &AppRef, guild: GuildId, user: UserId, old: Option<ChannelId>, new: Option<ChannelId>) {
    if old == new {
        return; // 음소거·화면 공유 같은 상태 변경
    }
    let Some(s) = app.sessions.lock().await.get(&guild.get()).cloned() else { return };
    let ch = *s.channel.lock().unwrap();
    let joined = new == Some(ch);
    if !joined && old != Some(ch) {
        return;
    }
    let Some(ctx) = app.ctx() else { return };
    if user == ctx.cache.current_user().id {
        return;
    }
    // 이름·봇 여부·역할은 캐시에서(특권 인텐트 없이 음성 상태에 멤버가 같이 온다)
    let info = ctx.cache.guild(guild).and_then(|g| g.members.get(&user).map(|m| (m.display_name().to_string(), m.user.bot, m.roles.iter().map(|r| r.get()).collect::<Vec<_>>())));
    let Some((name, bot, roles)) = info else { return };
    if bot {
        return;
    }
    let u = user.get();
    let ignored = {
        let st = app.store.lock().unwrap();
        st.state.guilds.get(&guild.get()).is_some_and(|g| {
            g.ignore_users.contains(&u) || g.self_ignored.contains(&u) || roles.iter().any(|r| g.ignore_roles.contains(r))
        })
    };
    if ignored {
        return;
    }
    let text = if joined { format!("{name}이(가) 음성 채널에 들어왔다") } else { format!("{name}이(가) 음성 채널에서 나갔다") };
    let mut b = s.buffer.lock().unwrap();
    if s.listening.load(Ordering::Relaxed) {
        b.push(u, "알림", &text);
        b.prune(s.keep(app));
    }
}

pub async fn on_voice_state(app: &AppRef, guild: GuildId, user: UserId, new_channel: Option<ChannelId>) {
    let Some(s) = app.sessions.lock().await.get(&guild.get()).cloned() else { return };
    let bot_id = app.ctx().map(|c| c.cache.current_user().id);
    if Some(user) == bot_id && new_channel.is_none() {
        // 누가 봇을 끊었다
        leave(app, guild).await;
        return;
    }
    if user.get() != app.cfg().owner_id {
        return;
    }
    apply_listen(app, &s).await;
}

/// 주인 위치·설정에 맞춰 듣기 상태를 다시 맞춘다(설정을 바꿨을 때도 부른다)
pub async fn apply_listen(app: &AppRef, s: &Arc<Session>) {
    let guild = s.guild;
    let ch = *s.channel.lock().unwrap();
    let now = should_listen(app, guild, ch);
    let was = s.listening.swap(now, Ordering::Relaxed);
    let nc = notice_channel(app, guild, ch).await;
    if was && !now {
        s.clear_audio();
        s.buffer.lock().unwrap().clear();
        post(app, nc, "⏸️ 주인이 나가서 듣기를 멈추고 기억을 지웠어요. 돌아오면 다시 들을게요.").await;
    } else if !was && now {
        post(app, nc, "▶️ 주인이 돌아와서 다시 들을게요.").await;
    }
    crate::presence::refresh(app).await;
}
