//! 음성 세션: 수신 → 거르기 → 발화 자르기 → 받아쓰기 → 호출어 → 대답(말·글) → 재생.
//! SPEC F-02~F-05, F-10~F-13, F-20~F-25, N-01, N-06.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
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
    /// 말이 끝난 때의 세대 — 받아쓰는 사이 채널을 옮겼으면 이전 채널 말로 보고 버린다
    epoch: u64,
}

pub struct Job {
    /// 세션 안 호출 번호 — 같은 사람·캐릭터의 새 호출이 오면 옛 대답 음성을 버리는 기준
    pub id: u64,
    pub char_key: String,
    pub user: u64,
    pub caller: String,
    pub question: String,
    /// 말이 끝난 시각(지연 기준점)
    pub t0: Instant,
    /// 부른 순간의 대화(질문 줄 제외) — 대기하는 동안 나온 대화가 섞이지 않게
    pub lines: Vec<crate::convo::Line>,
    /// 호출 판정 근거
    pub via: &'static str,
    pub stt_wait_ms: u64,
    pub stt_ms: u64,
    /// 판단 모델에 걸린 시간(물었을 때만)
    pub judge_ms: Option<u64>,
    pub queued: Instant,
    /// 부른 때의 세대 — 채널 이동·듣기 중지 전에 부른 대답은 그 뒤에 만들어져도 재생하지 않는다
    pub epoch: u64,
    /// 질문이 된 발화의 버퍼 줄 번호 — 그새 지워졌으면(잊어줘·무시) 대답하지 않는다
    pub line_seq: Option<u64>,
}

/// 재생 대기열 한 칸 — 대답 처리기는 음성을 만들어 넣기만 하고 다음 호출로 넘어간다(재생 끝까지 기다리지 않음)
struct Audio {
    wav: Vec<u8>,
    key: (u64, String),
    job: Option<u64>,
    tl: Option<Arc<Mutex<Timeline>>>,
    at: Instant,
    /// 넣을 때의 세대 — 채널 이동·듣기 중지(clear_audio)로 세대가 바뀌면 옛 음성은 재생하지 않는다
    epoch: u64,
    /// 이 사람에게 하는 말(이모지 확인 등) — 재생 직전에 그새 무시됐으면 버린다
    who: Option<u64>,
    /// 질문 발화의 버퍼 줄 — 재생 직전에 지워졌으면(잊어줘·무시) 버린다
    line: Option<u64>,
}

/// 호출 하나의 단계별 지연(웹 콘솔 "호출별 지연") — 이름 등 개인정보는 담지 않는다
#[derive(Clone, Default, serde::Serialize)]
pub struct Timeline {
    pub time: String,
    pub character: String,
    pub via: String,
    pub preset: String,
    /// 말 끝 → 받아쓰기 시작
    pub stt_wait_ms: u64,
    pub stt_ms: u64,
    /// 판단 모델(물었을 때만)
    pub judge_ms: Option<u64>,
    /// 호출 → 대답 처리 시작
    pub answer_wait_ms: u64,
    /// 대답 처리 시작 → 두뇌 첫 문장(말)
    pub llm_speech_ms: Option<u64>,
    pub llm_ms: u64,
    pub tts_ms: u64,
    /// 음성 준비 → 재생 시작(앞 대답 재생 대기)
    pub play_wait_ms: u64,
    /// 말 끝 → 재생 시작(첫 소리)
    pub total_ms: u64,
    pub result: String,
    #[serde(skip)]
    parts: u8,
    /// 말 끝 시각(첫 소리 지연 기준점)
    #[serde(skip)]
    t0: Option<Instant>,
}

/// 대답 프롬프트에 넣는 최근 대화 길이(분) — 버퍼는 기억 시간만큼 두되 두뇌엔 최근 것만
const PROMPT_MINUTES: u64 = 5;

/// 같은 호출의 오래된 대답 음성은 버린다(말 끝에서 이만큼 지나면)
const STALE_AUDIO: Duration = Duration::from_secs(45);

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
    /// 잡음 버림 사유 — 짧아서 / 작아서 / 받아쓰기 대기열이 가득 차서
    pub dropped_short: u64,
    pub dropped_quiet: u64,
    pub dropped_queue: u64,
    /// 새 호출이나 오래돼서 버린 대답 음성
    pub stale_audio: u64,
    /// 판단 모델 — 부른 것 / 아님 / 실패(원래 규칙대로)
    pub judge_yes: u64,
    pub judge_no: u64,
    pub judge_fail: u64,
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
    audio_tx: mpsc::Sender<Audio>,
    next_job: std::sync::atomic::AtomicU64,
    latest: Mutex<HashMap<(u64, String), u64>>,
    pub timelines: Mutex<std::collections::VecDeque<Timeline>>,
    emoji_last: Mutex<HashMap<u64, Instant>>,
    epoch: std::sync::atomic::AtomicU64,
    current: Mutex<Option<Playing>>,
    pending: Mutex<HashMap<u64, (String, Instant)>>,
    /// 이름만 들려 판단 모델을 기다리는 사람 → (호출 번호, 캐릭터, 그사이 나온 다음 말 — 부른 것으로 판정되면 질문으로, 다음 말 마감)
    judging: Mutex<HashMap<u64, (u64, String, Option<Job>, Instant)>>,
    /// 사람별 호출 번호 — 부를 때마다(이름·헤이·이어서·이모지) 올린다. 판단 모델 결과가 늦게 오면 그사이 새 호출이 있었는지 본다
    call_seq: Mutex<HashMap<u64, u64>>,
    wake: RwLock<Wake>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    fetching: Mutex<HashSet<u64>>,
}

/// 추가 호출어(전역) 바꾸기 — 웹 콘솔·`/설정 호출어` 공통. 부르는 이름("클로드")이든 받아쓰기 변형("실장")이든 된다.
/// 띄어쓰기가 있는 말도 되고(받아쓰기 띄어쓰기는 무시), 바로 모든 세션에 반영한다
pub async fn set_wake_extra(app: &AppRef, character: &str, words: &[String]) -> std::result::Result<Vec<String>, String> {
    set_wake_words(app, character, words, false).await
}

/// 위험 별칭(전역) 바꾸기 — set_wake_extra와 같은 검증
pub async fn set_wake_risky(app: &AppRef, character: &str, words: &[String]) -> std::result::Result<Vec<String>, String> {
    set_wake_words(app, character, words, true).await
}

async fn set_wake_words(app: &AppRef, character: &str, words: &[String], risky: bool) -> std::result::Result<Vec<String>, String> {
    let out = check_wake_words(app, character, words)?;
    // 같은 낱말이 추가 호출어와 위험 별칭 양쪽에 있으면 일반 패턴이 먼저 걸려 위험 제한이 무력화된다
    {
        let st = app.store.lock().unwrap();
        let other = if risky { &st.state.wake_extra } else { &st.state.wake_risky };
        let ns = |w: &str| w.split_whitespace().collect::<String>();
        if let Some(w) = other.get(character).and_then(|o| out.iter().find(|w| o.iter().any(|x| ns(x) == ns(w)))) {
            return Err(format!("'{w}'은(는) {}에 이미 있어요 — 한쪽만 두세요", if risky { "추가 호출어" } else { "위험 별칭" }));
        }
    }
    {
        let mut st = app.store.lock().unwrap();
        let map = if risky { &mut st.state.wake_risky } else { &mut st.state.wake_extra };
        if out.is_empty() {
            map.remove(character);
        } else {
            map.insert(character.to_string(), out.clone());
        }
    }
    app.save_store();
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    for s in sessions {
        s.reload_wake(app);
    }
    Ok(out)
}

/// 호출어 목록 검증만(저장 안 함) — 웹에서 추가·위험 두 목록을 함께 바꿀 때 둘 다 먼저 검증해 일부만 바뀌지 않게
pub fn check_wake_words(app: &AppRef, character: &str, words: &[String]) -> std::result::Result<Vec<String>, String> {
    let cfg = app.cfg();
    let Some(ch) = cfg.characters.get(character) else { return Err("없는 캐릭터예요".into()) };
    let mut out: Vec<String> = Vec::new();
    for w in words {
        let w = w.split_whitespace().collect::<Vec<_>>().join(" ");
        if w.is_empty() {
            continue;
        }
        if w.chars().count() > 20 || !w.chars().any(char::is_alphanumeric) {
            return Err("호출어는 20자 이내 글자로".into());
        }
        let ns = |x: &str| x.split_whitespace().collect::<String>();
        if ch.wake.iter().any(|b| ns(b) == ns(&w)) {
            continue; // 기본 호출어(config)와 같으면(띄어쓰기 무시) 따로 둘 필요 없음
        }
        if let Some(other) = wake_owner(app, &w, character) {
            return Err(format!("'{w}'은(는) {other}의 호출어예요 — 누가 불렸는지 모르게 돼요"));
        }
        if !out.iter().any(|o| ns(o) == ns(&w)) {
            out.push(w);
        }
    }
    if out.len() > 30 {
        return Err("추가 호출어는 30개까지".into());
    }
    let mut all = ch.wake.clone();
    all.extend(out.iter().cloned());
    if crate::convo::Wake::compile(&all).is_err() {
        return Err("호출어로 쓸 수 없는 글자가 있어요".into());
    }
    Ok(out)
}

/// config 호출어(첫 번째가 대표 이름) + 웹 콘솔 호출어 사전
pub fn build_wake(app: &AppRef) -> Wake {
    let cfg = app.cfg();
    let (extra, risky) = {
        let st = app.store.lock().unwrap();
        (st.state.wake_extra.clone(), st.state.wake_risky.clone())
    };
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
    let risky: Vec<(String, Vec<String>)> = risky.into_iter().filter(|(k, _)| cfg.characters.contains_key(k)).collect();
    Wake::new(&list).with_risky(&risky)
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
        // 판단 결과 적용(judge_then_call)과 한 경계로 — 세대를 올리는 사이 늦은 결과가 끼어들지 않게(잠금 순서: call_seq → 나머지)
        let _calls = self.call_seq.lock().unwrap();
        // 세대를 먼저 올린다 — 받는 쪽(on_tick)이 segs 잠금 안에서 세대를 읽으므로, 지운 뒤 새로 모인 소리는 새 세대로 붙는다
        self.epoch.fetch_add(1, Ordering::Relaxed);
        self.segs.lock().unwrap().clear();
        self.pending.lock().unwrap().clear();
        self.judging.lock().unwrap().clear();
        // 재생 중인 대답을 멈춘다(대기열의 옛 음성은 위에서 올린 세대로 무효) — 채널 이동·듣기 중지 뒤 이전 대답이 나오지 않게
        if let Some(p) = self.current.lock().unwrap().as_ref() {
            let _ = p.handle.stop();
        }
    }

    /// 이 호출이 아직 유효한가 — 부른 사람 허용·듣는 중·같은 세대·질문 줄이 남아 있음. buf: 이미 쥔 버퍼(그 안에서 판정과 기록을 한 번에)
    fn call_alive(&self, app: &AppRef, user: u64, epoch: u64, line: Option<u64>, buf: &crate::convo::Buffer) -> bool {
        self.allowed(app, user) != Some(false)
            && self.listening.load(Ordering::Relaxed)
            && self.epoch.load(Ordering::Relaxed) == epoch
            && line.is_none_or(|s| buf.has_line(s))
    }

    /// 이 사람의 호출 대기(다음 말 대기·판단 대기)를 없애고 호출 번호를 올린다 — 무시·본인 거부 때. 늦게 온 판단 결과도 무효가 된다
    pub fn forget_calls(&self, user: u64) {
        let mut calls = self.call_seq.lock().unwrap();
        *calls.entry(user).or_insert(0) += 1;
        self.pending.lock().unwrap().remove(&user);
        self.judging.lock().unwrap().remove(&user);
    }

    /// 모두의 호출 대기를 없앤다 — /잊어줘
    pub fn forget_all_calls(&self) {
        let mut calls = self.call_seq.lock().unwrap();
        for n in calls.values_mut() {
            *n += 1;
        }
        self.pending.lock().unwrap().clear();
        self.judging.lock().unwrap().clear();
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

    fn enqueue(&self, mut job: Job) {
        job.id = self.next_job.fetch_add(1, Ordering::Relaxed) + 1;
        let (latest_key, latest_id) = ((job.user, job.char_key.clone()), job.id);
        self.stats.lock().unwrap().wakes += 1;
        let tl = Timeline {
            time: crate::convo::now_kst(),
            character: job.char_key.clone(),
            via: job.via.to_string(),
            stt_wait_ms: job.stt_wait_ms,
            stt_ms: job.stt_ms,
            judge_ms: job.judge_ms,
            ..Default::default()
        };
        // 재생 시작(play)과 같은 current 잠금 안에서 — "지금 재생 중인 것 끊기 + 새 호출 등록"과
        // "재생 등록 + 새 호출 확인"이 서로 끼어들지 못하게(잠금 순서: current → latest)
        let sent = {
            let cur = self.current.lock().unwrap();
            // 대답 대기열은 8개까지 — 넘치면 이번 호출은 버린다(메모리 무한 증가 방지)
            let sent = self.job_tx.try_send(job).is_ok();
            if sent {
                // 같은 사람이 같은 캐릭터를 다시 부르면 지금 대답을 끊는다 — SPEC F-24
                if let Some(p) = cur.as_ref() {
                    if p.key == latest_key {
                        let _ = p.handle.stop();
                    }
                }
                // 대기 중인 옛 대답 음성은 재생 작업이 latest를 보고 버린다 — 실제로 들어간 호출만
                self.latest.lock().unwrap().insert(latest_key, latest_id);
            }
            sent
        };
        if !sent {
            tracing::warn!("대답 대기열이 가득 차 호출 하나를 버림");
            let tl = Arc::new(Mutex::new(tl));
            finish_part(self, &tl, |t| set_result(t, "대답 대기열 가득"));
            finish_part(self, &tl, |_| {});
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
        let tick_epoch;
        {
            let mut segs = sess.segs.lock().unwrap();
            // 세대는 segs 잠금 안에서 — clear_audio가 세대를 올리고 이 잠금으로 조각을 지우므로, 여기 남은 조각은 이 세대의 것
            tick_epoch = sess.epoch.load(Ordering::Relaxed);
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
                let short = n < lc.min_utterance_ms * 16;
                {
                    let mut st = sess.stats.lock().unwrap();
                    st.dropped_noise += 1;
                    if short {
                        st.dropped_short += 1;
                    } else {
                        st.dropped_quiet += 1;
                    }
                }
                // 판정과 기록을 버퍼 잠금 안에서 — 조용해서 끝난 조각은 그새 무시·주인 퇴장이 있었을 수 있다
                log_stt_at(app, sess, seg.user, if short { "short" } else { "quiet" }, n / 16, 0, "", tick_epoch);
                continue;
            }
            let ended = Instant::now() - Duration::from_millis(seg.silence_ticks as u64 * 20);
            sess.stats.lock().unwrap().utterances += 1;
            let (user, audio_ms) = (seg.user, n / 16);
            if sess.utt_tx.try_send(Utterance { user: seg.user, pcm: seg.pcm, ended, epoch: tick_epoch }).is_err() {
                tracing::warn!("받아쓰기 대기열이 가득 차 발화 하나를 버림");
                sess.stats.lock().unwrap().dropped_queue += 1;
                log_stt_at(app, sess, user, "queue", audio_ms, 0, "", tick_epoch);
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
        // 마감은 다음 말이 "끝난" 시각으로 판정하므로(받아쓰기가 늦게 끝나도) 마감 뒤 30초 여유를 두고 지운다
        let now = Instant::now();
        sess.pending.lock().unwrap().retain(|_, (_, until)| until.checked_add(Duration::from_secs(30)).is_some_and(|u| u > now));
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
/// 받아쓰기 기록(진단) — 그 소리를 모은 세대가 지금과 같을 때만, 버퍼 잠금 안에서 판정과 기록을 한 번에
/// (clear_audio는 세대를 먼저 올리므로 그 뒤의 이전 채널 소리는 여기서 걸러진다)
#[allow(clippy::too_many_arguments)]
fn log_stt_at(app: &AppRef, sess: &Session, user: u64, kind: &'static str, audio_ms: u64, stt_ms: u64, provider: &str, epoch: u64) {
    let name = speaker_name(sess, user);
    let mut b = sess.buffer.lock().unwrap();
    if sess.allowed(app, user) == Some(true) && sess.listening.load(Ordering::Relaxed) && sess.epoch.load(Ordering::Relaxed) == epoch {
        b.log_stt(user, &name, kind, audio_ms, stt_ms, provider, 0, None);
    }
}

async fn stt_worker(app: AppRef, sess: Arc<Session>, mut rx: mpsc::Receiver<Utterance>) {
    while let Some(u) = rx.recv().await {
        // 이전 채널(세대)의 말이면 받아쓰지도 기록하지도 않는다
        let same_epoch = |s: &Session| s.epoch.load(Ordering::Relaxed) == u.epoch;
        if !same_epoch(&sess) {
            continue;
        }
        let audio_ms = u.pcm.len() as u64 / 16;
        let stt_wait_ms = u.ended.elapsed().as_millis() as u64;
        if !app.sidecar.lock().unwrap().ok {
            sess.stats.lock().unwrap().stt_failed += 1;
            log_stt_at(&app, &sess, u.user, "failed", audio_ms, 0, "사이드카 꺼짐", u.epoch);
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
                if !same_epoch(&sess) {
                    continue;
                }
                if r.filtered || r.text.trim().is_empty() {
                    sess.stats.lock().unwrap().stt_filtered += 1;
                    log_stt_at(&app, &sess, u.user, "filtered", audio_ms, r.ms, provider, u.epoch);
                    continue;
                }
                // 받아쓰는 사이 /무시해줘·무시 목록·주인 퇴장이 있었으면 버린다(지운 기억에 다시 들어가지 않게)
                if sess.allowed(&app, u.user) != Some(true) || !sess.listening.load(Ordering::Relaxed) {
                    continue;
                }
                let text = r.text.trim();
                if let Some((wake, judge, line_seq)) = handle_text(&app, &sess, u.user, text, u.ended, u.epoch, stt_wait_ms, r.ms) {
                    let name = speaker_name(&sess, u.user);
                    let (event, judge) = {
                        let mut b = sess.buffer.lock().unwrap();
                        // 그새 이 발화가 지워졌으면(잊어줘·무시) 판단도 기록도 하지 않는다
                        let alive = b.has_line(line_seq) && sess.epoch.load(Ordering::Relaxed) == u.epoch;
                        let judge = judge.filter(|_| alive);
                        let ev = (alive && sess.allowed(&app, u.user) == Some(true) && sess.listening.load(Ordering::Relaxed))
                            .then(|| b.log_stt(u.user, &name, "ok", audio_ms, r.ms, provider, text.chars().count(), wake));
                        (ev, judge)
                    };
                    // 판단 모델은 기록을 남긴 뒤에 — 결과를 그 줄에 적는다
                    if let Some(j) = judge {
                        tokio::spawn(judge_then_call(app.clone(), sess.clone(), j, event));
                    }
                }
            }
            Err(e) => {
                if !same_epoch(&sess) {
                    continue;
                }
                sess.stats.lock().unwrap().stt_failed += 1;
                log_stt_at(&app, &sess, u.user, "failed", audio_ms, 0, "", u.epoch);
                tracing::warn!("받아쓰기 실패: {e}");
            }
        }
    }
}

/// 판단 모델에 물을 호출 — 받아쓰기 기록을 남긴 뒤 띄운다
struct JudgeReq {
    job: Job,
    hit: crate::convo::WakeHit,
    text: String,
    jd: crate::config::Judge,
    /// 물을 때의 세대·이 사람의 호출 번호 — 그새 채널을 옮겼거나 같은 사람이 새로 불렀으면 결과를 버린다
    epoch: u64,
    seq: u64,
    /// 그 발화의 버퍼 줄 번호 — 결과가 왔을 때 지워졌으면(무시·잊어줘 등, 그 뒤 다시 듣기여도) 버린다
    line_seq: u64,
}

/// 버퍼에 넣고 호출어를 본다. None = 버림(그새 무시됨), Some((부른 캐릭터 표시, 판단 모델에 물을 것))
/// epoch: 그 말이 끝난 때의 세대(채널 이동 전 말은 버린다)
#[allow(clippy::too_many_arguments)]
fn handle_text(app: &AppRef, sess: &Arc<Session>, user: u64, text: &str, ended: Instant, epoch: u64, stt_wait_ms: u64, stt_ms: u64) -> Option<(Option<String>, Option<JudgeReq>, u64)> {
    let name = sess
        .participants
        .lock()
        .unwrap()
        .get(&user)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| user.to_string());
    let line_seq = {
        // 최종 검사와 저장을 버퍼 잠금 안에서 한 번에 — /무시해줘·무시 목록·주인 퇴장은 저장소·listening을 먼저 바꾼 뒤
        // 이 잠금을 잡고 버퍼를 지우므로, 여기서 통과했으면 지우기 전, 실패했으면 버린다(TOCTOU 방지)
        let mut b = sess.buffer.lock().unwrap();
        if sess.allowed(app, user) != Some(true) || !sess.listening.load(Ordering::Relaxed) || sess.epoch.load(Ordering::Relaxed) != epoch {
            return None;
        }
        let seq = b.push(user, &name, text);
        b.prune(sess.keep(app));
        seq
    };
    let hey_only = app.hey_only(sess.guild.get());
    let hit = sess.wake.read().unwrap().find(text, hey_only);
    // 여기서부터 호출 결정(다음 말 대기·판단 대기·호출 번호)은 호출 번호 잠금 안에서 — 늦게 온 판단 결과 적용(judge_then_call)과
    // 서로 끼어들지 못하게(잠금 순서: call_seq → buffer·pending·judging·나머지)
    let mut calls = sess.call_seq.lock().unwrap();
    // 버퍼에 넣은 뒤 이 잠금을 잡기 전에 무시·잊어줘가 끼었으면(이 발화가 지워짐) 호출로 쓰지 않는다.
    // 잠금을 잡은 뒤의 무시는 forget_calls가 이 잠금을 기다렸다가 여기서 만든 대기를 지운다
    // 채널 이동·듣기 중지(clear_audio — 같은 잠금으로 세대를 올리고 대기를 지움)가 먼저 끝났어도 여기서 걸러진다
    if !sess.buffer.lock().unwrap().has_line(line_seq)
        || sess.allowed(app, user) != Some(true)
        || sess.epoch.load(Ordering::Relaxed) != epoch
        || !sess.listening.load(Ordering::Relaxed)
    {
        return None;
    }
    let mut bump = || {
        let n = calls.entry(user).or_insert(0);
        *n += 1;
        *n
    };
    // 호출어만 말하고 끊었던 사람의 다음 발화 = 질문 — SPEC F-21
    let pend = sess.pending.lock().unwrap().remove(&user);
    // 부른 순간의 대화를 고정한다 — 질문 줄(방금 넣은 이 발화)은 빼고
    let snapshot = || {
        let mut v = sess.buffer.lock().unwrap().snapshot(sess.keep(app));
        if let Some(pos) = v.iter().rposition(|l| l.user == user && l.text == text) {
            v.remove(pos);
        }
        v
    };
    let job = |char_key: String, question: String, via: &'static str, lines| Job {
        id: 0,
        char_key,
        user,
        caller: name.clone(),
        question,
        t0: ended,
        lines,
        via,
        stt_wait_ms,
        stt_ms,
        judge_ms: None,
        queued: Instant::now(),
        epoch,
        line_seq: Some(line_seq),
    };
    if let (None, Some((ch, until))) = (&hit, pend) {
        // 다음 말이 마감 안에 끝났으면(받아쓰기가 늦게 끝나도) 질문으로
        if ended <= until {
            bump();
            sess.enqueue(job(ch.clone(), text.to_string(), "이어서", snapshot()));
            return Some((Some(format!("{ch}(이어서)")), None, line_seq));
        }
    }
    let Some(h) = hit else {
        // 이름만 부르고 판단 모델을 기다리는 사이에 나온 다음 말 — 맡겨 두었다가 부른 것으로 판정되면 질문으로
        let mut jg = sess.judging.lock().unwrap();
        if let Some((_, ch, held, until)) = jg.get_mut(&user) {
            // 다음 말 마감(이름이 끝난 시각 + followup_secs) 안에 나온 말만
            if held.is_none() && ended <= *until {
                *held = Some(job(ch.clone(), text.to_string(), "이어서", snapshot()));
                return Some((Some(format!("{ch}(이어서·판단 기다림)")), None, line_seq));
            }
        }
        return Some((None, None, line_seq));
    };
    let ch = h.character.clone();
    let seq = bump();
    // 판단 모델 — 애매하게 들린 호출은 "정말 부른 말인가"를 물어본 뒤에 대답한다(SPEC F-54)
    let jd = app.judge();
    let ask = jd.on && (matches!(h.via, "비슷한 말" | "위험 별칭") || (h.via == "이름" && jd.scope == "이름도"));
    if ask {
        let note = format!("{ch}({}·판단 중)", h.via);
        // 이 사람의 앞선 판단 대기는 새 호출로 무효(맡겨 둔 말도 이번 것으로)
        if h.rest.is_empty() {
            let secs = app.cfg().listen.followup_secs.clamp(1, 120);
            let until = ended.checked_add(Duration::from_secs(secs)).unwrap_or_else(Instant::now);
            sess.judging.lock().unwrap().insert(user, (seq, ch.clone(), None, until));
        } else {
            sess.judging.lock().unwrap().remove(&user);
        }
        let req = JudgeReq {
            job: job(h.character.clone(), h.rest.clone(), h.via, snapshot()),
            text: text.to_string(),
            hit: h,
            jd,
            epoch,
            seq,
            line_seq,
        };
        return Some((Some(note), Some(req), line_seq));
    }
    let wake_note = format!("{ch}({})", h.via);
    let uncertain = matches!(h.via, "비슷한 말" | "위험 별칭");
    sess.judging.lock().unwrap().remove(&user);
    accept_call(app, sess, job(h.character.clone(), h.rest.clone(), h.via, snapshot()), uncertain, ended);
    Some((Some(wake_note), None, line_seq))
}

/// 호출로 받아들인다 — 뒤에 말이 붙었으면 바로 질문으로, 이름만이면 다음 말을 질문으로 기다린다(SPEC F-21).
/// uncertain: 불확실한 판정(비슷한 말·위험 별칭, 판단 모델 확인 없음)으로 이름만 들렸으면 다음 말을 질문으로 받지 않는다 —
/// 이름 오인 한 번이 다음 정상 발화까지 오호출로 번진다(2026-10-15 검토)
/// from: 다음 말을 기다리는 시간의 기준(판단 모델을 거쳤으면 말이 끝난 시각 — 판단에 걸린 시간만큼 늘어나지 않게)
fn accept_call(app: &AppRef, sess: &Arc<Session>, job: Job, uncertain: bool, from: Instant) {
    if !job.question.is_empty() {
        sess.enqueue(job);
    } else if !uncertain {
        let secs = app.cfg().listen.followup_secs.clamp(1, 120);
        let until = from.checked_add(Duration::from_secs(secs)).unwrap_or_else(Instant::now);
        sess.pending.lock().unwrap().insert(job.user, (job.char_key, until));
    }
}

/// 이 낱말을 이미 호출어(기본·추가·위험 별칭)로 쓰는 다른 캐릭터 — 띄어쓰기는 무시하고 비교
pub fn wake_owner(app: &AppRef, word: &str, except: &str) -> Option<String> {
    let norm = |w: &str| w.split_whitespace().collect::<String>();
    let w = norm(word);
    let cfg = app.cfg();
    let st = app.store.lock().unwrap();
    // 지금 쓰는 캐릭터 + 설정 다시 읽기에서 건너뛴 웹 캐릭터(다시 살아날 수 있다)
    let file = app.cfg_file.read().unwrap().clone();
    let mut chars: Vec<(&String, &Vec<String>)> = cfg.characters.iter().map(|(k, c)| (k, &c.wake)).collect();
    // + 건너뛴 웹 캐릭터, config에 같은 키가 생겨 가려진 웹 원본(config 쪽이 사라지면 다시 살아난다)
    chars.extend(st.state.custom_characters.iter().filter(|(k, _)| !cfg.characters.contains_key(*k) || file.characters.contains_key(*k)).map(|(k, c)| (k, &c.wake)));
    chars
        .into_iter()
        .filter(|(k, _)| k.as_str() != except)
        .find(|(k, base)| {
            base.iter()
                .chain(st.state.wake_extra.get(*k).into_iter().flatten())
                .chain(st.state.wake_risky.get(*k).into_iter().flatten())
                .any(|x| norm(x) == w)
        })
        .map(|(k, _)| k.clone())
}

/// 판단 모델에 같이 보낼 이름 변형 — 호출어(config)·위험 별칭·추가 호출어
pub fn name_variants(app: &AppRef, key: &str) -> Vec<String> {
    let cfg = app.cfg();
    let mut v: Vec<String> = cfg.characters.get(key).map(|c| c.wake.clone()).unwrap_or_default();
    let st = app.store.lock().unwrap();
    for m in [&st.state.wake_risky, &st.state.wake_extra] {
        for w in m.get(key).into_iter().flatten() {
            if !v.contains(w) {
                v.push(w.clone());
            }
        }
    }
    v
}

/// 판단 모델에 묻고 결과대로 — 부른 것이면 호출, 아니면 기록만. 실패·시간 초과면 원래 규칙대로(판단 없이).
/// event: 그 발화의 받아쓰기 기록 번호(결과를 거기에 적는다)
async fn judge_then_call(app: AppRef, sess: Arc<Session>, req: JudgeReq, event: Option<u64>) {
    let JudgeReq { mut job, hit: h, text, jd, epoch, seq, line_seq } = req;
    let cfg = app.cfg();
    let names: Vec<String> = cfg.characters.values().map(|c| c.name.clone()).collect();
    let ch_name = cfg.characters.get(&h.character).map(|c| c.name.clone()).unwrap_or_else(|| h.character.clone());
    // 최근 2분 중 마지막 6줄만 — 짧을수록 빠르다
    let recent: Vec<crate::convo::Line> = {
        let cut = job.lines.iter().rposition(|l| l.at.elapsed() > Duration::from_secs(120)).map_or(0, |i| i + 1);
        let tail = &job.lines[cut..];
        tail[tail.len().saturating_sub(6)..].to_vec()
    };
    let variants = name_variants(&app, &h.character);
    let (system, user) = llm::judge_prompt(&names, &ch_name, &variants, &h.word, &recent, &job.caller, &text);
    let t = Instant::now();
    let r = llm::judge(&app, &jd.preset, &system, &user, Duration::from_millis(jd.timeout_ms)).await;
    let ms = t.elapsed().as_millis() as u64;
    let verdict = match &r {
        Ok(v) => Some(v.call),
        Err(e) => {
            tracing::warn!("판단 모델 실패 — 원래 규칙대로: {e}");
            None
        }
    };
    job.judge_ms = Some(ms);
    let ch = &h.character;
    // 확인부터 적용까지 호출 번호 잠금을 쥔다 — 새 호출(handle_text)·채널 이동(clear_audio)이 그 사이에 끼지 못하게
    let calls = sess.call_seq.lock().unwrap();
    // 그새 무시됐거나 듣기를 멈췄거나 채널을 옮겼거나 그 발화가 지워졌으면 버린다. 버퍼 잠금도 적용까지 쥔다 —
    // 무시·잊어줘는 저장소를 먼저 바꾸고 이 잠금을 잡아 지우므로, 여기를 통과했으면 지우기 전이다(잠금 순서: call_seq → buffer → 나머지)
    let mut buf = sess.buffer.lock().unwrap();
    // 맡겨 둔 다음 말(이름만 부른 경우) — 이 판단의 것일 때만 꺼낸다(그새 새 판단이 시작됐으면 그쪽 몫). 두 잠금 안이라 그 뒤 끝난 말은
    // handle_text가 이 결과(다음 말 대기)를 보고 처리한다
    let held = {
        let mut jg = sess.judging.lock().unwrap();
        if jg.get(&job.user).is_some_and(|(s, _, _, _)| *s == seq) { jg.remove(&job.user).and_then(|(_, _, j, _)| j) } else { None }
    };
    if sess.allowed(&app, job.user) != Some(true)
        || !sess.listening.load(Ordering::Relaxed)
        || sess.epoch.load(Ordering::Relaxed) != epoch
        || !buf.has_line(line_seq)
    {
        return;
    }
    let mut set_note = |n: String| {
        if let Some(id) = event {
            buf.update_wake(id, Some(n));
        }
    };
    // 같은 사람이 그새 새로 불렀으면(헤이로 다시 부름·다른 캐릭터·이어서·이모지) 늦게 온 이 결과로 그 호출을 끊거나 덮지 않는다
    if calls.get(&job.user).copied().unwrap_or(0) != seq {
        set_note(format!("{ch}({}·새 호출이 먼저)", h.via));
        return;
    }
    let (note, call, uncertain) = match verdict {
        Some(true) => {
            sess.stats.lock().unwrap().judge_yes += 1;
            (format!("{ch}({}·판단: 부름)", h.via), true, false)
        }
        Some(false) => {
            sess.stats.lock().unwrap().judge_no += 1;
            (format!("{ch}({}·판단: 아님)", h.via), false, false)
        }
        None => {
            sess.stats.lock().unwrap().judge_fail += 1;
            let uncertain = matches!(h.via, "비슷한 말" | "위험 별칭");
            (format!("{ch}({}·판단 실패)", h.via), true, uncertain)
        }
    };
    tracing::info!("판단 모델 {} {}ms: {}", jd.preset, ms, note);
    set_note(note);
    if call {
        let t0 = job.t0;
        match held {
            // 판단을 기다리는 사이 나온 다음 말이 질문 — 확실히 부른 것일 때만(실패면 원래 규칙대로 버림)
            Some(mut q) if !uncertain => {
                q.judge_ms = Some(ms);
                sess.enqueue(q);
            }
            _ => accept_call(&app, &sess, job, uncertain, t0),
        }
    } else {
        // 부르지 않은 것으로 본 호출도 지연 기록에 남긴다(판단이 너무 깐깐한지 볼 수 있게)
        let tl = Arc::new(Mutex::new(Timeline {
            time: crate::convo::now_kst(),
            character: job.char_key.clone(),
            via: job.via.to_string(),
            stt_wait_ms: job.stt_wait_ms,
            stt_ms: job.stt_ms,
            judge_ms: job.judge_ms,
            ..Default::default()
        }));
        finish_part(&sess, &tl, |t| set_result(t, "판단: 부른 게 아님"));
        finish_part(&sess, &tl, |_| {});
    }
    drop(buf);
    drop(calls);
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

/// 재생 작업 — 대기열의 음성을 차례로 재생한다. 새 호출로 밀린 대답·오래된 대답은 버린다
async fn player(app: AppRef, sess: Arc<Session>, mut rx: mpsc::Receiver<Audio>) {
    while let Some(a) = rx.recv().await {
        // 대답 음성은 재생 직전에 부른 사람이 무시됐는지도 본다(대기하는 사이 /무시해줘 등)
        let superseded = a.epoch != sess.epoch.load(Ordering::Relaxed)
            || a.line.is_some_and(|s| !sess.buffer.lock().unwrap().has_line(s))
            || a.job.is_some_and(|id| sess.latest.lock().unwrap().get(&a.key).is_some_and(|&l| l > id))
            || [Some(a.key.0).filter(|&u| u != 0), a.who].into_iter().flatten().any(|u| {
                let roles: Vec<u64> = app
                    .ctx()
                    .and_then(|c| c.cache.guild(sess.guild).and_then(|gu| gu.members.get(&UserId::new(u)).map(|m| m.roles.iter().map(|r| r.get()).collect())))
                    .unwrap_or_default();
                let listed = app.store.lock().unwrap().state.guilds.get(&sess.guild.get()).is_some_and(|g| {
                    g.ignore_users.contains(&u) || g.self_ignored.contains(&u) || roles.iter().any(|r| g.ignore_roles.contains(r))
                });
                listed || sess.allowed(&app, u) == Some(false)
            });
        let stale = a.tl.as_ref().is_some_and(|t| t.lock().unwrap().t0.is_some_and(|t0| t0.elapsed() > STALE_AUDIO));
        if superseded || stale {
            sess.stats.lock().unwrap().stale_audio += 1;
            if let Some(tl) = &a.tl {
                // 재생하지 않았다는 사실이 우선 — 대답 처리기가 먼저 "대답"을 적었어도 덮는다
                finish_part(&sess, tl, |t| t.result = if superseded { "새 호출로 취소".into() } else { "오래돼서 버림".into() });
            }
            continue;
        }
        if let Some(tl) = &a.tl {
            let wait = a.at.elapsed().as_millis() as u64;
            let mut total = 0;
            finish_part(&sess, tl, |t| {
                t.play_wait_ms = wait;
                total = t.t0.map(|t0| t0.elapsed().as_millis() as u64).unwrap_or(0);
                t.total_ms = total;
            });
            if total > 0 {
                app.record_latency(total);
            }
        }
        let _ = play(&app, &sess, a.wav, a.key, a.job, a.epoch).await;
    }
}

/// 결과는 먼저 정해진 것을 지킨다 — 재생 작업이 "새 호출로 취소"라고 적었으면 대답 처리기가 "대답"으로 덮지 않게
fn set_result(t: &mut Timeline, r: &str) {
    if t.result.is_empty() {
        t.result = r.to_string();
    }
}

/// 타임라인 조각을 채운다 — 대답 처리기와 재생 작업이 각각 한 번씩, 둘 다 끝나면 기록·로그
fn finish_part(sess: &Session, tl: &Arc<Mutex<Timeline>>, f: impl FnOnce(&mut Timeline)) {
    let done = {
        let mut t = tl.lock().unwrap();
        f(&mut t);
        t.parts += 1;
        (t.parts == 2).then(|| t.clone())
    };
    if let Some(t) = done {
        tracing::info!(
            "호출 {}({}) 받아쓰기대기={} 받아쓰기={} 대답대기={} 첫문장={:?} 두뇌={} tts={} 재생대기={} 첫소리={}ms {} {}",
            t.character, t.via, t.stt_wait_ms, t.stt_ms, t.answer_wait_ms, t.llm_speech_ms, t.llm_ms, t.tts_ms, t.play_wait_ms, t.total_ms, t.preset, t.result
        );
        let mut q = sess.timelines.lock().unwrap();
        q.push_back(t);
        while q.len() > 30 {
            q.pop_front();
        }
    }
}

/// WAV를 재생하고 끝날 때까지 기다린다(재생 작업 안에서만 부른다)
async fn play(app: &AppRef, sess: &Arc<Session>, wav: Vec<u8>, key: (u64, String), job: Option<u64>, epoch: u64) -> Result<()> {
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
        // enqueue와 같은 current 잠금 안에서 등록·확인 — 그새 같은 사람·캐릭터의 새 호출이 들어왔거나 채널을 옮겼으면 바로 멈춘다.
        // clear_audio는 세대를 올린 뒤 이 잠금을 잡고 멈추므로, 등록이 먼저면 거기서 멈추고 나중이면 여기서 세대가 바뀐 걸 본다
        let mut cur = sess.current.lock().unwrap();
        let superseded = job.is_some_and(|id| sess.latest.lock().unwrap().get(&key).is_some_and(|&l| l > id));
        if superseded || sess.epoch.load(Ordering::Relaxed) != epoch {
            let _ = handle.stop();
        }
        *cur = Some(Playing { key, handle });
    } else if sess.epoch.load(Ordering::Relaxed) != epoch {
        let _ = handle.stop();
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
    speak_at(app, sess, char_key, text, key, None, None, None).await
}

/// at: 이 세대에 속한 말(호출에 딸린 안내) — 그새 채널을 옮겼으면 말하지 않는다. who: 이 사람에게 하는 말 — 그새 무시됐으면 말하지 않는다
/// line: 이 말이 딸린 질문 발화의 버퍼 줄 — 합성하는 사이 지워졌으면(잊어줘·무시) 말하지 않고, 재생 직전에도 본다
#[allow(clippy::too_many_arguments)]
async fn speak_at(app: &AppRef, sess: &Arc<Session>, char_key: &str, text: &str, key: (u64, String), at: Option<u64>, who: Option<u64>, line: Option<u64>) -> Result<String> {
    // 이 사람에게 하는 말이면 무시·듣기 확인 — 참가자 정보가 아직 없어도(이모지만 누른 사람) 저장소의 유저 무시·본인 거부는 바로 본다
    let blocked = |u: u64| {
        // 참가자 정보가 없으면 캐시의 멤버 역할로 역할 무시도 본다
        let roles: Vec<u64> = app
            .ctx()
            .and_then(|c| c.cache.guild(sess.guild).and_then(|gu| gu.members.get(&UserId::new(u)).map(|m| m.roles.iter().map(|r| r.get()).collect())))
            .unwrap_or_default();
        let listed = {
            let st = app.store.lock().unwrap();
            st.state.guilds.get(&sess.guild.get()).is_some_and(|g| {
                g.ignore_users.contains(&u) || g.self_ignored.contains(&u) || roles.iter().any(|r| g.ignore_roles.contains(r))
            })
        };
        listed || sess.allowed(app, u) == Some(false) || !sess.listening.load(Ordering::Relaxed)
    };
    // 합성 전에도 — 채널을 옮겼거나 무시됐으면 (유료일 수 있는) TTS를 부르지 않는다
    if at.is_some_and(|e| e != sess.epoch.load(Ordering::Relaxed)) {
        bail!("채널을 옮겨 말하지 않음");
    }
    let gone = || line.is_some_and(|s| !sess.buffer.lock().unwrap().has_line(s));
    if who.is_some_and(blocked) || gone() {
        bail!("부른 사람이 무시됐거나 듣기를 멈춤");
    }
    let ch = app.character(sess.guild.get(), char_key).ok_or_else(|| anyhow!("캐릭터 {char_key} 없음"))?;
    let chain = sidecar::voice_chain(app, &ch.voice);
    let (wav, provider) = sidecar::tts(app, text, &chain).await?;
    app.last_used.lock().unwrap().tts = provider.clone();
    // 재생 대기열로 — 대답과 겹쳐 동시에 나오지 않게
    let epoch = at.unwrap_or_else(|| sess.epoch.load(Ordering::Relaxed));
    if epoch != sess.epoch.load(Ordering::Relaxed) {
        bail!("채널을 옮겨 말하지 않음");
    }
    if who.is_some_and(blocked) || gone() {
        bail!("부른 사람이 무시됐거나 듣기를 멈춤");
    }
    sess.audio_tx.try_send(Audio { wav, key, job: None, tl: None, at: Instant::now(), epoch, who, line }).map_err(|_| anyhow!("재생 대기열에 못 넣음"))?;
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
    while let Some(mut job) = rx.recv().await {
        let g = sess.guild.get();
        // 기다리는 사이 부른 사람이 무시됐거나(본인 거부 포함) 듣기를 멈췄으면 대답하지 않는다 — 어느 경로로 들어온 호출이든 마지막 관문.
        // 그사이 무시된 다른 사람의 말도 고정해 둔 대화에서 뺀다
        let gone = job.line_seq.is_some_and(|s| !sess.buffer.lock().unwrap().has_line(s));
        if gone || sess.allowed(&app, job.user) == Some(false) || !sess.listening.load(Ordering::Relaxed) || job.epoch != sess.epoch.load(Ordering::Relaxed) {
            let tl = Arc::new(Mutex::new(Timeline {
                time: crate::convo::now_kst(),
                character: job.char_key.clone(),
                via: job.via.to_string(),
                stt_wait_ms: job.stt_wait_ms,
                stt_ms: job.stt_ms,
                judge_ms: job.judge_ms,
                answer_wait_ms: job.queued.elapsed().as_millis() as u64,
                ..Default::default()
            }));
            finish_part(&sess, &tl, |t| set_result(t, "무시·안 들음으로 버림"));
            finish_part(&sess, &tl, |_| {});
            continue;
        }
        job.lines.retain(|l| l.user == 0 || sess.allowed(&app, l.user) != Some(false));
        let Some(ch) = app.character(g, &job.char_key) else {
            // 대기하는 사이 설정에서 캐릭터가 빠졌다 — 기록은 남긴다
            let tl = Arc::new(Mutex::new(Timeline {
                time: crate::convo::now_kst(),
                character: job.char_key.clone(),
                via: job.via.to_string(),
                stt_wait_ms: job.stt_wait_ms,
                stt_ms: job.stt_ms,
                judge_ms: job.judge_ms,
                answer_wait_ms: job.queued.elapsed().as_millis() as u64,
                ..Default::default()
            }));
            finish_part(&sess, &tl, |t| set_result(t, "캐릭터 없음"));
            finish_part(&sess, &tl, |_| {});
            continue;
        };
        let t_llm = Instant::now();
        let tl = Arc::new(Mutex::new(Timeline {
            time: crate::convo::now_kst(),
            character: ch.name.clone(),
            via: job.via.to_string(),
            stt_wait_ms: job.stt_wait_ms,
            stt_ms: job.stt_ms,
            judge_ms: job.judge_ms,
            answer_wait_ms: job.queued.elapsed().as_millis() as u64,
            t0: Some(job.t0),
            ..Default::default()
        }));
        // 부른 순간 고정한 대화 중 최근 PROMPT_MINUTES분만 — 기록이 길면 첫 문장이 늦고 무료 두뇌 분당 토큰 한도에 걸린다
        let window = Duration::from_secs(PROMPT_MINUTES * 60);
        let lines: Vec<crate::convo::Line> = job.lines.iter().filter(|l| l.at.elapsed() <= window).cloned().collect();
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
                Ok(s) => Some(speak_queued(&app, &sess, &job, &s, key.clone(), &tl).await),
                Err(_) => None, // 형식이 어긋나 미리 못 받음 — 끝난 뒤 말한다
            }
        };
        let (res, spoke) = tokio::join!(
            async {
                let r = llm_fut.await;
                // 두뇌 시간은 여기서 — 미리 보낸 말의 TTS가 병렬로 돌아도 섞이지 않게
                tl.lock().unwrap().llm_ms = t_llm.elapsed().as_millis() as u64;
                // 글은 대답이 끝나는 즉시(음성 재생과 겹쳐서)
                // 대답을 만드는 사이 부른 사람이 무시됐거나 듣기를 멈췄거나 채널을 옮겼으면 기억도 남기지 않고 글도 올리지 않는다
                // 기억 도구 — 글을 올리기 전에 적용해서 결과를 글 끝에 작게 붙인다(SPEC F-37). 판정과 적용을 버퍼 잠금 안에서 한 번에
                // (잊어줘·무시는 이 잠금으로 지운다)
                let (still, notes) = {
                    let b = sess.buffer.lock().unwrap();
                    let still = sess.call_alive(&app, job.user, job.epoch, job.line_seq, &b);
                    let notes = match &r {
                        Ok(a) if still && !a.memory.is_empty() && sess.allowed(&app, job.user) == Some(true) => apply_memory(&app, g, job.user, &job.caller, &a.memory),
                        _ => vec![],
                    };
                    (still, notes)
                };
                if let (Ok(a), Some(tc), true) = (&r, tc, still) {
                    // <글>이 비었으면 <말>을 올린다 — 음성 대답을 껐을 때 대답이 사라지지 않게
                    let text = if a.text.trim().is_empty() { &a.speech } else { &a.text };
                    // 글은 서버에서도 길이를 자른다(프롬프트는 800자 이내로 시키지만 모델이 넘길 때가 있다)
                    let text: String = if text.chars().count() > 1000 { text.chars().take(999).collect::<String>() + "…" } else { text.to_string() };
                    let mut body = format!("<@{}> {} **{}**: {}", job.user, ch.emoji, ch.name, text);
                    if !notes.is_empty() {
                        body.push_str(&format!("
-# 📝 {}", notes.join(" · ")));
                    }
                    let (app2, sess2, caller, ep, line) = (app.clone(), sess.clone(), job.user, job.epoch, job.line_seq);
                    tokio::spawn(async move {
                        // 올리기 직전 한 번 더 — 그새 무시·듣기 중지·채널 이동·잊어줘면 올리지 않는다
                        let ok = sess2.call_alive(&app2, caller, ep, line, &sess2.buffer.lock().unwrap());
                        if ok {
                            post_to(&app2, tc, &body, Some(caller)).await;
                        }
                    });
                }
                r
            },
            early
        );
        // 출력 전 마지막 확인 — 그새 무시·듣기 중지·채널 이동이면 기록·안내·음성 없이 끝낸다
        let still = sess.allowed(&app, job.user) != Some(false)
            && sess.listening.load(Ordering::Relaxed)
            && job.epoch == sess.epoch.load(Ordering::Relaxed)
            && job.line_seq.is_none_or(|s| sess.buffer.lock().unwrap().has_line(s));
        if !still {
            finish_part(&sess, &tl, |t| set_result(t, "무시·안 들음으로 버림"));
            if !matches!(spoke, Some(Ok(()))) {
                finish_part(&sess, &tl, |_| {});
            }
            continue;
        }
        match res {
            Ok(a) => {
                sess.stats.lock().unwrap().answers += 1;
                // 입출력 원문(웹 콘솔) — 그새 부른 사람이 무시됐으면 남기지 않는다(판정과 기록을 버퍼 잠금 안에서)
                {
                    let mut b = sess.buffer.lock().unwrap();
                    // 부른 사람이 허용이고, 프롬프트에 든 사람 중 그새 무시된 사람이 없을 때만(SPEC F-11)
                    if sess.allowed(&app, job.user) == Some(true)
                        && sess.listening.load(Ordering::Relaxed)
                        && sess.epoch.load(Ordering::Relaxed) == job.epoch
                        && trace_users.iter().all(|&u| sess.allowed(&app, u) != Some(false))
                    {
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
                // 짧은 <말>만 기록한다 — 긴 <글>(목록형 정리)을 넣으면 다음 호출에서 그 정리를 또 반복했다(2026-10-15 실사용).
                // 말이 비었으면 남기지 않는다
                if !a.speech.trim().is_empty() {
                    let mem: String = a.speech.chars().take(150).collect();
                    // 기록도 버퍼 잠금 안에서 확인하고 — 그새 무시·듣기 중지·채널 이동이면 남기지 않는다
                    let mut b = sess.buffer.lock().unwrap();
                    if sess.call_alive(&app, job.user, job.epoch, job.line_seq, &b) {
                        b.push(0, &ch.name, &mem);
                    }
                }
                if a.capped {
                    notify_cap_once(&app, &sess, tc).await;
                }
                {
                    let mut t = tl.lock().unwrap();
                    t.preset = a.preset.clone();
                    t.llm_speech_ms = a.speech_ms;
                }
                if !voice {
                    // 음성이 없으면 재생 조각까지 여기서 채운다
                    finish_part(&sess, &tl, |t| t.result = "글만".into());
                    finish_part(&sess, &tl, |_| {});
                    continue;
                }
                // 글은 위에서 올렸다(부른 사람 멘션 + 긴 답 — SPEC F-23). 말: 미리 못 했으면 지금(재생 대기열에 넣고 바로 다음 호출로)
                let r = match spoke {
                    Some(r) => r,
                    None => speak_queued(&app, &sess, &job, &a.speech, key, &tl).await,
                };
                match r {
                    Ok(()) => finish_part(&sess, &tl, |t| set_result(t, if a.cut { "끊김(말은 나감)" } else { "대답" })),
                    Err(e) => {
                        tracing::warn!("음성 대답 실패: {e}");
                        finish_part(&sess, &tl, |t| t.result = "음성 실패".into());
                        finish_part(&sess, &tl, |_| {});
                    }
                }
            }
            Err(e) => {
                sess.stats.lock().unwrap().answer_failed += 1;
                let msg = e.to_string();
                // 미리 보낸 말이 실제로 재생 대기열에 들어갔을 때만 "말은 나감" — TTS가 실패했으면 재생 조각도 여기서 채운다
                let spoken = matches!(spoke, Some(Ok(())));
                if spoken {
                    finish_part(&sess, &tl, |t| set_result(t, "끊김(말은 나감)"));
                } else {
                    finish_part(&sess, &tl, |t| set_result(t, "두뇌 실패"));
                    finish_part(&sess, &tl, |_| {});
                }
                if spoken {
                    // 말은 이미 나갔다(두뇌가 끊긴 경우는 llm이 받은 데까지 Ok로 돌려주므로 보통 오지 않는다) — 실패 음성은 덧붙이지 않는다
                    tracing::warn!("대답 실패(말은 나감): {msg}");
                } else if msg.starts_with("CAP") {
                    // 안내 글도 올리기 직전에 — 그새 무시·듣기 중지·채널 이동이면 올리지 않는다
                    let ok_now = sess.call_alive(&app, job.user, job.epoch, job.line_seq, &sess.buffer.lock().unwrap());
                    if let (Some(tc), true) = (tc, ok_now) {
                        post_to(&app, tc, &format!("<@{}> {} 오늘은 여기까지! 유료 한도를 다 써서 내일 다시 불러 줘.", job.user, ch.emoji), Some(job.user)).await;
                    }
                    if voice && sess.call_alive(&app, job.user, job.epoch, job.line_seq, &sess.buffer.lock().unwrap()) {
                        let _ = speak_at(&app, &sess, &job.char_key, "오늘은 여기까지야. 내일 다시 불러 줘.", key, Some(job.epoch), Some(job.user), job.line_seq).await;
                    }
                } else {
                    tracing::warn!("대답 실패: {msg}");
                    let ok_now = sess.call_alive(&app, job.user, job.epoch, job.line_seq, &sess.buffer.lock().unwrap());
                    if let (Some(tc), true) = (tc, ok_now) {
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
    let is_owner = user == app.cfg().owner_id;
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
                    // 음성 "잊어"는 내가 부탁한 기억만(주인은 전부) — /기억 지우기와 같은 규칙
                    let kl = k.to_lowercase();
                    let before = mems.len();
                    mems.retain(|m| !(m.text.to_lowercase().contains(&kl) && (is_owner || m.user == user)));
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

/// TTS를 만들어 재생 대기열에 넣는다(재생 끝까지 기다리지 않음). 첫 소리 지연은 재생 작업이 재생 시작 때 잰다 — SPEC N-01
async fn speak_queued(app: &AppRef, sess: &Arc<Session>, job: &Job, text: &str, key: (u64, String), tl: &Arc<Mutex<Timeline>>) -> Result<()> {
    // 대답을 만드는 사이 부른 사람이 무시됐거나 듣기를 멈췄으면 말하지 않는다
    let still = || sess.call_alive(app, job.user, job.epoch, job.line_seq, &sess.buffer.lock().unwrap());
    if !still() {
        bail!("부른 사람이 무시됐거나 듣기를 멈춤");
    }
    let ch = app.character(sess.guild.get(), &job.char_key).ok_or_else(|| anyhow!("캐릭터 없음"))?;
    let chain = sidecar::voice_chain(app, &ch.voice);
    let t = Instant::now();
    let res = sidecar::tts(app, text, &chain).await;
    // 실패해도 걸린 시간은 남긴다
    tl.lock().unwrap().tts_ms = t.elapsed().as_millis() as u64;
    let (wav, provider) = res?;
    app.last_used.lock().unwrap().tts = provider;
    if !still() {
        bail!("부른 사람이 무시됐거나 듣기를 멈춤");
    }
    // 기다리지 않고 넣는다 — 재생 대기열이 가득 차면(앞 대답이 길게 쌓임) 대답 처리기가 재생 끝을 기다리게 되므로 이번 음성은 버린다
    sess.audio_tx
        .try_send(Audio { wav, key, job: Some(job.id), tl: Some(tl.clone()), at: Instant::now(), epoch: job.epoch, who: Some(job.user), line: job.line_seq })
        .map_err(|e| anyhow!("재생 대기열에 못 넣음: {}", if matches!(e, mpsc::error::TrySendError::Full(_)) { "가득 참" } else { "재생 작업 없음" }))?;
    Ok(())
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
            // 옮기기 전에 먼저 무효화 — 이전 채널의 늦은 판단 결과·대기 음성이 옮기는 사이에 끼어들지 않게
            s.clear_audio();
            app.songbird.join(guild, channel).await.map_err(|e| anyhow!("이동 실패: {e:?}"))?;
            *s.channel.lock().unwrap() = channel;
            app.store.lock().unwrap().guild(guild.get()).voice_channel = Some(channel.get());
            app.save_store();
            s.listening.store(should_listen(app, guild, channel), Ordering::Relaxed);
            s.clear_audio();
            s.buffer.lock().unwrap().clear();
            // 새 채널 사람들에게도 고지 — SPEC F-12
            let nc = notice_channel(app, guild, channel).await;
            post(app, nc, &format!("{}\n(듣는 채널: <#{channel}>)", app.cfg().notice.text)).await;
            spawn_voice_notice(app, &s);
            crate::presence::refresh(app).await;
        }
        return Ok(s);
    }
    // 수신기를 접속 "전에" 단다 — 디스코드는 접속 핸드셰이크 도중에 "이 SSRC는 누구"(Speaking)를 보내는데,
    // 접속이 끝난 뒤에 달면 그걸 놓쳐 아무도 못 알아듣는다(2026-10-15 실제 발생: 입장할 때마다 전부 "누구인지 모름")
    let call = app.songbird.get_or_insert(guild);
    let (utt_tx, utt_rx) = mpsc::channel(32);
    let (job_tx, job_rx) = mpsc::channel(8);
    let (audio_tx, audio_rx) = mpsc::channel(16);
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
        audio_tx,
        next_job: std::sync::atomic::AtomicU64::new(0),
        latest: Mutex::new(HashMap::new()),
        timelines: Mutex::new(std::collections::VecDeque::new()),
        emoji_last: Mutex::new(HashMap::new()),
        epoch: std::sync::atomic::AtomicU64::new(0),
        current: Mutex::new(None),
        pending: Mutex::new(HashMap::new()),
        judging: Mutex::new(HashMap::new()),
        call_seq: Mutex::new(HashMap::new()),
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
    let joined = {
        let stage = call.lock().await.join(channel).await;
        match stage {
            Ok(j) => j.await,
            Err(e) => Err(e),
        }
    };
    if let Err(e) = joined {
        // 실패한 Call은 지운다 — 남기면 다음 입장 때 수신기가 겹쳐 붙는다
        let _ = app.songbird.remove(guild).await;
        return Err(anyhow!("입장 실패: {e:?}"));
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
    let t5 = tokio::spawn(player(app.clone(), sess.clone(), audio_rx));
    sess.tasks.lock().unwrap().extend([t1, t2, t3, t4, t5]);
    app.sessions.lock().await.insert(guild.get(), sess.clone());
    app.store.lock().unwrap().guild(guild.get()).voice_channel = Some(channel.get());
    app.save_store();
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
    // 인사는 config.toml의 첫 캐릭터 목소리로(웹에서 추가한 캐릭터 이름 순서에 밀리지 않게)
    let first_char = app.cfg_file.read().unwrap().characters.keys().next().cloned().unwrap_or_else(|| cfg.characters.keys().next().cloned().unwrap_or_default());
    // 인사를 껐으면 말하지 않는다(`/설정 인사말`) — 채팅 고지는 이미 올렸다
    let Some(text) = app.greeting(sess.guild.get()) else { return };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        // 사이드카 예열·재시작 중이면 실패한다 — 최대 2분 동안 다시 시도(고지는 반드시 한 번)
        for attempt in 1..=12 {
            // 그새 나갔으면(세션이 바뀌었으면) 그만
            let alive = app2.sessions.lock().await.get(&sess2.guild.get()).is_some_and(|s| Arc::ptr_eq(s, &sess2));
            if !alive {
                return;
            }
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
    app.store.lock().unwrap().guild(guild.get()).voice_channel = None;
    app.save_store();
    tracing::info!("퇴장 guild={guild}");
    crate::presence::refresh(app).await;
    true
}

/// 주인 입·퇴장 따라 듣기 켜고 끄기 — SPEC F-13
/// 이모지로 부르기 — 음성 채널에서 캐릭터 이모지(🧡 클짱, 🩵 아짱)로 반응하면 그 캐릭터가 "응?" 하고
/// 그 사람의 다음 말을 질문으로 받는다. 받아쓰기가 이름을 못 알아들어도 확실하게 부르는 방법(2026-10-15 요구: 딸깍 호출)
pub async fn on_voice_effect(app: &AppRef, v: &serde_json::Value) {
    let id = |k: &str| v.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<u64>().ok()).filter(|&n| n != 0);
    let (Some(g), Some(c), Some(u)) = (id("guild_id"), id("channel_id"), id("user_id")) else { return };
    let Some(emoji) = v.get("emoji").and_then(|e| e.get("name")).and_then(|x| x.as_str()) else { return };
    let Some(s) = app.sessions.lock().await.get(&g).cloned() else { return };
    let epoch0 = s.epoch.load(Ordering::Relaxed);
    if *s.channel.lock().unwrap() != ChannelId::new(c) || !s.listening.load(Ordering::Relaxed) {
        return;
    }
    // 이모지 변형 선택자(U+FE0F)는 빼고 비교
    let clean = |e: &str| e.chars().filter(|&ch| ch != '\u{fe0f}').collect::<String>();
    let cfg = app.cfg();
    let Some((key, ch)) = cfg.characters.iter().find(|(_, ch)| !ch.emoji.is_empty() && clean(&ch.emoji) == clean(emoji)) else { return };
    // 무시 대상은 부를 수 없다 — 아직 말을 안 해 참가자 정보가 없으면 캐시의 멤버 정보(봇·역할)와 저장소 목록으로 판정
    ensure_participant(app, &s, u);
    // 아직 말을 안 해 참가자 정보가 없으면 캐시의 멤버 정보로 — 잠금 안 재확인에도 같은 판정을 쓴다
    let member = app.ctx().and_then(|c| {
        c.cache.guild(GuildId::new(g)).and_then(|gu| gu.members.get(&UserId::new(u)).map(|m| (m.user.bot, m.roles.iter().map(|r| r.get()).collect::<Vec<_>>())))
    });
    let allowed_now = |s: &Session| -> bool {
        match s.allowed(app, u) {
            Some(a) => a,
            None => {
                let Some((bot, roles)) = &member else { return false };
                let st = app.store.lock().unwrap();
                !bot && !st.state.guilds.get(&g).is_some_and(|gs| {
                    gs.ignore_users.contains(&u) || gs.self_ignored.contains(&u) || roles.iter().any(|r| gs.ignore_roles.contains(r))
                })
            }
        }
    };
    if !allowed_now(&s) {
        return;
    }
    // 연타 방지 — 사람마다 5초에 한 번(대답 대기열이 "응?"으로 차지 않게)
    {
        let mut last = s.emoji_last.lock().unwrap();
        if last.get(&u).is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        last.insert(u, Instant::now());
    }
    let secs = cfg.listen.followup_secs.clamp(1, 120) + 4;
    let until = Instant::now().checked_add(Duration::from_secs(secs)).unwrap_or_else(Instant::now);
    // 새 호출 — 기다리던 판단 결과는 이걸 덮지 않는다(호출 번호 잠금 안에서 한 번에)
    {
        let mut calls = s.call_seq.lock().unwrap();
        // 잠금 안에서 다시 — 그새 채널을 옮겼거나 무시·듣기 중지가 끝났으면 대기를 만들지 않는다
        if s.epoch.load(Ordering::Relaxed) != epoch0 || !s.listening.load(Ordering::Relaxed) || !allowed_now(&s) {
            return;
        }
        *calls.entry(u).or_insert(0) += 1;
        s.judging.lock().unwrap().remove(&u);
        s.pending.lock().unwrap().insert(u, (key.clone(), until));
    }
    let name = speaker_name(&s, u);
    {
        // 기록도 버퍼 잠금 안에서 다시 확인하고 — 그새 무시·듣기 중지·채널 이동이면 남기지 않는다
        let mut b = s.buffer.lock().unwrap();
        if s.epoch.load(Ordering::Relaxed) == epoch0 && s.listening.load(Ordering::Relaxed) && allowed_now(&s) {
            b.log_stt(u, &name, "emoji", 0, 0, "", 0, Some(format!("{}(이모지)", ch.name)));
        }
    }
    tracing::info!("이모지 호출 {} guild={g}", ch.name);
    let ack = if key == "클짱" { "응? 불렀어?" } else { "왜, 말해 봐." };
    let (app2, s2, k2) = (app.clone(), s.clone(), key.clone());
    tokio::spawn(async move {
        if let Err(e) = speak_at(&app2, &s2, &k2, ack, (0, String::new()), Some(epoch0), Some(u), None).await {
            tracing::warn!("이모지 호출 대답 실패: {e}");
        }
    });
}

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
