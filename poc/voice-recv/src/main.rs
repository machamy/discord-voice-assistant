//! PoC 1 — songbird 0.6 `receive`로 사람별 음성이 오는지, DAVE 복호화가 얼마나 새는지 잰다.
//!
//! 음성은 저장하지 않는다. 사람별로 "말한 틱 수 / 손실 틱 / RTP 순번 빠짐 / 평균 음량"만 센다.
//! songbird는 복호화 못 한 DAVE 패킷을 조용히 버리므로(udp_rx/mod.rs), 손실은 RTP 순번 빠짐으로 잡는다.
//!
//! 슬래시 명령: `/들어와`(명령한 사람의 음성 채널로) · `/나가`(최종 통계 후 퇴장) · `/상태`(지금까지 통계, 나만 보기)
//!
//! 명령은 봇이 들어가 있는 모든 서버에 서버 단위로 등록한다(새로 초대되면 그때 등록).
//!
//! 환경변수(.env 가능): DISCORD_TOKEN, GUILD_ID(자동 입장할 서버, 선택),
//!   VOICE_CHANNEL_ID(있으면 시작하자마자 자동 입장), DURATION_SECS(입장 후 자동 퇴장, 기본 600, 0=끄기), NOTICE(기본 1)

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{
    ChannelId, CommandInteraction, Context, CreateCommand, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, EventHandler, GatewayIntents, GuildId, Interaction, Ready,
};
use serenity::async_trait;
use serenity::Client;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::{Config, SerenityInit};

#[derive(Default, Debug)]
struct SsrcStats {
    user: Option<u64>,
    ticks: u64,          // 이 SSRC가 speaking에 들어온 틱(20ms) 수
    lost: u64,           // packet == None (지터 버퍼가 손실로 판정)
    seq_gaps: u64,       // RTP 순번이 건너뛴 패킷 수 (복호화 실패로 버려진 것 포함)
    last_seq: Option<u16>,
    samples: u64,
    abs_sum: u64,        // 평균 음량 계산용 (복호화가 깨지면 무음/잡음으로 티가 남)
    no_pcm: u64,         // 패킷은 있는데 decoded_voice 없음
}

#[derive(Default)]
struct State {
    ssrc: HashMap<u32, SsrcStats>,
    users: HashMap<u32, u64>,
    unmapped_ticks: u64,
    disconnects: u64,
}

#[derive(Clone)]
struct Receiver {
    st: Arc<Mutex<State>>,
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let mut st = self.st.lock().unwrap();
        match ctx {
            EventContext::SpeakingStateUpdate(sp) => {
                if let Some(uid) = sp.user_id {
                    st.users.insert(sp.ssrc, uid.0);
                    st.ssrc.entry(sp.ssrc).or_default().user = Some(uid.0);
                }
            }
            EventContext::VoiceTick(tick) => {
                for (ssrc, data) in &tick.speaking {
                    let user = st.users.get(ssrc).copied();
                    if user.is_none() {
                        st.unmapped_ticks += 1;
                    }
                    let s = st.ssrc.entry(*ssrc).or_default();
                    s.user = s.user.or(user);
                    s.ticks += 1;
                    match &data.packet {
                        None => s.lost += 1,
                        Some(p) => {
                            let seq: u16 = p.rtp().get_sequence().0.0;
                            if let Some(prev) = s.last_seq {
                                let d = seq.wrapping_sub(prev);
                                if d > 1 && d < 1000 {
                                    s.seq_gaps += u64::from(d - 1);
                                }
                            }
                            s.last_seq = Some(seq);
                        }
                    }
                    match &data.decoded_voice {
                        Some(pcm) => {
                            s.samples += pcm.len() as u64;
                            s.abs_sum += pcm.iter().map(|x| x.unsigned_abs() as u64).sum::<u64>();
                        }
                        None => s.no_pcm += 1,
                    }
                }
            }
            EventContext::ClientDisconnect(_) => st.disconnects += 1,
            _ => {}
        }
        None
    }
}

fn report(st: &State, elapsed: Duration) -> String {
    let mut out = format!(
        "{:.0}초 경과 · SSRC {}개 · 매핑 안 된 틱 {} · 퇴장 {}\n",
        elapsed.as_secs_f32(),
        st.ssrc.len(),
        st.unmapped_ticks,
        st.disconnects
    );
    for (ssrc, s) in &st.ssrc {
        let got = s.ticks - s.lost;
        let loss = if s.ticks + s.seq_gaps > 0 {
            100.0 * (s.lost + s.seq_gaps) as f64 / (s.ticks + s.seq_gaps) as f64
        } else {
            0.0
        };
        let level = if s.samples > 0 { s.abs_sum / s.samples } else { 0 };
        out += &format!(
            "  user={} ssrc={} 말한 {:.1}s · 받은패킷 {} · 손실 {} · 순번빠짐 {} → 손실률 {:.2}% · PCM없음 {} · 평균음량 {}\n",
            s.user.map(|u| u.to_string()).unwrap_or_else(|| "?".into()),
            ssrc,
            s.ticks as f64 * 0.02,
            got,
            s.lost,
            s.seq_gaps,
            loss,
            s.no_pcm,
            level
        );
    }
    out
}

struct Session {
    st: Arc<Mutex<State>>,
    start: Instant,
    reporter: tokio::task::JoinHandle<()>,
}

struct Handler {
    guild: Option<GuildId>, // 자동 입장용(VOICE_CHANNEL_ID와 같이 쓸 때만)
    auto_channel: Option<ChannelId>,
    duration: Option<Duration>,
    notice: bool,
    sessions: Arc<tokio::sync::Mutex<HashMap<GuildId, Session>>>,
}

impl Handler {
    /// 채널에 들어가 수신 집계를 시작한다. 이미 들어가 있으면 채널만 옮기고 집계는 이어간다.
    async fn join(&self, ctx: &Context, guild: GuildId, channel: ChannelId) -> Result<(), String> {
        let manager = songbird::get(ctx).await.expect("songbird 미등록");
        let call = manager.join(guild, channel).await.map_err(|e| format!("입장 실패: {e:?}"))?;
        println!("입장: 서버 {guild} 채널 {channel}");

        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&guild) {
            return Ok(());
        }
        let st = Arc::new(Mutex::new(State::default()));
        {
            let mut h = call.lock().await;
            let rx = Receiver { st: st.clone() };
            h.add_global_event(CoreEvent::SpeakingStateUpdate.into(), rx.clone());
            h.add_global_event(CoreEvent::VoiceTick.into(), rx.clone());
            h.add_global_event(CoreEvent::ClientDisconnect.into(), rx);
        }
        if self.notice {
            let _ = channel
                .say(&ctx.http, "🎙️ [테스트] 음성 수신 테스트 중이에요. 음성은 저장하지 않고 사람별 수신 통계만 세요. `/나가`로 내보낼 수 있어요.")
                .await;
        }

        let start = Instant::now();
        let (st2, ctx2, sessions2, duration) = (st.clone(), ctx.clone(), self.sessions.clone(), self.duration);
        let reporter = tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(30));
            iv.tick().await;
            loop {
                iv.tick().await;
                let text = report(&st2.lock().unwrap(), start.elapsed());
                println!("--- {text}");
                if duration.is_some_and(|d| start.elapsed() >= d) {
                    println!("자동 퇴장(DURATION_SECS)");
                    if let Some(s) = sessions2.lock().await.remove(&guild) {
                        let text = report(&s.st.lock().unwrap(), s.start.elapsed());
                        println!("=== 최종\n{text}");
                    }
                    let _ = channel.say(&ctx2.http, "⏱️ 테스트 시간이 끝나서 나가요.").await;
                    let _ = songbird::get(&ctx2).await.unwrap().remove(guild).await;
                    return;
                }
            }
        });
        sessions.insert(guild, Session { st, start, reporter });
        Ok(())
    }

    async fn leave(&self, ctx: &Context, guild: GuildId) -> Option<String> {
        let s = self.sessions.lock().await.remove(&guild)?;
        s.reporter.abort();
        let text = report(&s.st.lock().unwrap(), s.start.elapsed());
        println!("=== 최종\n{text}");
        let _ = songbird::get(ctx).await.unwrap().remove(guild).await;
        Some(text)
    }

    async fn on_command(&self, ctx: &Context, cmd: &CommandInteraction) {
        println!("명령 /{} from {} ({}) guild={:?} channel={}", cmd.data.name, cmd.user.name, cmd.user.id, cmd.guild_id, cmd.channel_id);
        let Some(guild) = cmd.guild_id else {
            reply(ctx, cmd, "서버 채널에서 써 주세요.", true).await;
            return;
        };
        match cmd.data.name.as_str() {
            "들어와" => {
                // 캐시 참조는 await 전에 놓는다
                let cached = ctx
                    .cache
                    .guild(guild)
                    .and_then(|g| g.voice_states.get(&cmd.user.id).and_then(|vs| vs.channel_id));
                // 캐시에 없으면(봇 시작 전부터 채널에 있던 경우 등) API로 다시 묻는다
                let channel = match cached {
                    Some(c) => Some(c),
                    None => match ctx.http.get_user_voice_state(guild, cmd.user.id).await {
                        Ok(vs) => vs.channel_id,
                        Err(e) => {
                            println!("  음성 상태 조회 실패: {e}");
                            None
                        }
                    },
                };
                println!("  음성 채널: 캐시={cached:?} 최종={channel:?}");
                let Some(channel) = channel else {
                    reply(ctx, cmd, "먼저 음성 채널에 들어가 있어 주세요.", true).await;
                    return;
                };
                if let Err(e) = cmd.defer(&ctx.http).await {
                    println!("  defer 실패: {e}");
                }
                let msg = match self.join(ctx, guild, channel).await {
                    Ok(()) => format!("<#{channel}> 에 들어왔어요."),
                    Err(e) => {
                        println!("  {e}");
                        e
                    }
                };
                if let Err(e) = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(msg)).await {
                    println!("  응답 수정 실패: {e}");
                }
            }
            "나가" => match self.leave(ctx, guild).await {
                Some(text) => reply(ctx, cmd, &format!("나갈게요. 수신 통계:\n```\n{}```", clip(&text)), false).await,
                None => reply(ctx, cmd, "지금 음성 채널에 없어요.", true).await,
            },
            "상태" => {
                let text = {
                    let sessions = self.sessions.lock().await;
                    match sessions.get(&guild) {
                        Some(s) => format!("```\n{}```", clip(&report(&s.st.lock().unwrap(), s.start.elapsed()))),
                        None => "지금 음성 채널에 없어요.".into(),
                    }
                };
                reply(ctx, cmd, &text, true).await;
            }
            _ => {}
        }
    }
}

async fn register_commands(ctx: &Context, guild: GuildId) {
    let cmds = vec![
        CreateCommand::new("들어와").description("내가 있는 음성 채널로 클짱·아짱을 부른다"),
        CreateCommand::new("나가").description("음성 채널에서 내보낸다"),
        CreateCommand::new("상태").description("지금까지의 음성 수신 통계(나만 보기)"),
    ];
    match guild.set_commands(&ctx.http, cmds).await {
        Ok(c) => println!("슬래시 명령 {}개 등록: 서버 {guild}", c.len()),
        Err(e) => eprintln!("슬래시 명령 등록 실패(서버 {guild}): {e:?}"),
    }
}

fn clip(s: &str) -> String {
    // 디스코드 메시지 2000자 제한
    s.chars().take(1800).collect()
}

async fn reply(ctx: &Context, cmd: &CommandInteraction, text: &str, ephemeral: bool) {
    let msg = CreateInteractionResponseMessage::new().content(text).ephemeral(ephemeral);
    if let Err(e) = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await {
        println!("  응답 실패: {e}");
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("접속: {} (서버 {}개)", ready.user.name, ready.guilds.len());
        // 봇이 들어가 있는 모든 서버에 서버 단위로 등록한다(즉시 반영. 전역 등록은 반영이 늦을 수 있다)
        for g in &ready.guilds {
            register_commands(&ctx, g.id).await;
        }
        if let (Some(guild), Some(ch)) = (self.guild, self.auto_channel) {
            if let Err(e) = self.join(&ctx, guild, ch).await {
                eprintln!("{e}");
            }
        }
    }

    // 새로 초대된 서버에도 바로 명령을 단다
    async fn guild_create(&self, ctx: Context, guild: serenity::all::Guild, is_new: Option<bool>) {
        println!("서버: {} ({}) new={is_new:?}", guild.name, guild.id);
        if is_new == Some(true) {
            register_commands(&ctx, guild.id).await;
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        println!("interaction 수신: kind={:?}", interaction.kind());
        if let Interaction::Command(cmd) = interaction {
            self.on_command(&ctx, &cmd).await;
        }
    }
}

fn env_u64(k: &str) -> Option<u64> {
    env::var(k).ok().filter(|v| !v.is_empty()).map(|v| v.parse().unwrap_or_else(|_| panic!("{k} 숫자 아님")))
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "warn,songbird=info".into()))
        .init();

    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN 필요");
    let handler = Handler {
        guild: env_u64("GUILD_ID").map(GuildId::new),
        auto_channel: env_u64("VOICE_CHANNEL_ID").map(ChannelId::new),
        duration: match env_u64("DURATION_SECS").unwrap_or(600) {
            0 => None,
            n => Some(Duration::from_secs(n)),
        },
        notice: env::var("NOTICE").map(|v| v != "0").unwrap_or(true),
        sessions: Default::default(),
    };
    // 봇 본체도 16kHz mono로 받으면 Whisper에 바로 넘길 수 있다. PoC에서도 같은 설정으로 잰다.
    let cfg = Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(Channels::Mono, SampleRate::Hz16000)));
    // GUILD_VOICE_STATES: 명령한 사람이 어느 음성 채널에 있는지 캐시로 안다(특권 인텐트 아님)
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_VOICE_STATES;
    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .register_songbird_from_config(cfg)
        .await
        .expect("클라이언트 생성 실패");
    let shard = client.shard_manager.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("종료(Ctrl+C)");
        shard.shutdown_all().await;
    });
    if let Err(e) = client.start().await {
        eprintln!("클라이언트 오류: {e:?}");
    }
}
