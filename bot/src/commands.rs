//! 슬래시 명령 — SPEC §9. 설정은 `/설정`으로 보기·바꾸기(시중 봇처럼, SPEC F-36).
//! 권한: 주인 > 서버 관리 권한 > 누구나. 전역 값(한도·공급자·설정 파일)은 주인만.

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serenity::all::{
    AutocompleteChoice, ChannelId, ChannelType, CommandInteraction, CommandOptionType, Context,
    CreateAutocompleteResponse, CreateCommand, CreateCommandOption, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseMessage, EditInteractionResponse, GuildId, ResolvedOption, ResolvedValue,
};

use crate::app::AppRef;
use crate::config::Voice;
use crate::{llm, sidecar, voice};

const COLOR: u32 = 0xD97757;

// ------------------------------------------------------------------ 등록

/// 캐릭터는 자동완성으로 — 웹 콘솔에서 캐릭터를 더하거나 빼도 명령을 다시 등록할 필요가 없다
fn char_opt(_app: &AppRef) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::String, "캐릭터", "캐릭터(입력하면 목록이 뜸)").required(true).set_autocomplete(true)
}

/// 고른 캐릭터가 지금 있는지 — 자동완성이라 아무 글자나 들어올 수 있다
fn known_char(app: &AppRef, ch: &str) -> bool {
    app.cfg().characters.contains_key(ch)
}

const NO_CHAR: &str = "그런 캐릭터가 없어요. 목록에서 골라 주세요.";

fn sub(name: &str, desc: &str) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::SubCommand, name, desc)
}

pub fn definitions(app: &AppRef) -> Vec<CreateCommand> {
    let preset_opt = || {
        CreateCommandOption::new(CommandOptionType::String, "프리셋", "두뇌 프리셋(입력하면 목록이 뜸)")
            .required(true)
            .set_autocomplete(true)
    };
    let text_ch = || {
        CreateCommandOption::new(CommandOptionType::Channel, "채널", "텍스트 채널(비우면 지금 채널)")
            .channel_types(vec![ChannelType::Text])
    };
    vec![
        CreateCommand::new("도움말").description("아짱·클짱 쓰는 법과 명령 목록(나만 보기)"),
        CreateCommand::new("들어와").description("내가 있는 음성 채널로 아짱·클짱을 부른다"),
        CreateCommand::new("나가").description("음성 채널에서 내보낸다"),
        CreateCommand::new("무시해줘").description("내 목소리를 듣지 않게 한다(받아쓰기·기억 안 함)"),
        CreateCommand::new("다시들어줘").description("내 목소리를 다시 듣게 한다"),
        CreateCommand::new("잊어줘").description("지금까지 기억한 대화를 전부 지운다"),
        CreateCommand::new("기억")
            .description("아짱·클짱의 장기 기억(\"○○ 기억해\"라고 부탁한 것)")
            .add_option(sub("보기", "기억하고 있는 것(나만 보기)"))
            .add_option(sub("지우기", "번호로 하나 지우기(내가 부탁한 것, 관리자는 전부)").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Integer, "번호", "/기억 보기의 번호").min_int_value(1).max_int_value(50).required(true),
            ))
            .add_option(sub("전부지우기", "이 서버의 장기 기억을 전부 지우기(관리자)")),
        CreateCommand::new("상태").description("듣는 채널·사람·받아쓰기 상태(나만 보기)"),
        CreateCommand::new("공급자").description("[주인] 받아쓰기·두뇌·목소리 공급자와 오늘 사용량"),
        CreateCommand::new("설정다시읽기").description("[주인] config.toml 다시 읽기"),
        CreateCommand::new("채널지정").description("[관리] 긴 답을 올릴 텍스트 채널").add_option(text_ch()),
        CreateCommand::new("모델")
            .description("캐릭터 두뇌 바꾸기")
            .add_option(char_opt(app))
            .add_option(preset_opt()),
        CreateCommand::new("무시목록")
            .description("[관리] 유저·역할 무시 목록")
            .add_option(sub("유저추가", "이 사람 목소리를 무시").add_sub_option(
                CreateCommandOption::new(CommandOptionType::User, "유저", "무시할 사람").required(true),
            ))
            .add_option(sub("유저삭제", "유저 무시 해제").add_sub_option(
                CreateCommandOption::new(CommandOptionType::User, "유저", "해제할 사람").required(true),
            ))
            .add_option(sub("역할추가", "이 역할을 가진 사람 전부 무시").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Role, "역할", "무시할 역할").required(true),
            ))
            .add_option(sub("역할삭제", "역할 무시 해제").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Role, "역할", "해제할 역할").required(true),
            ))
            .add_option(sub("보기", "무시 목록 보기")),
        CreateCommand::new("설정")
            .description("설정 보기·바꾸기")
            .add_option(sub("보기", "지금 설정 한눈에 보기"))
            .add_option(sub("두뇌", "캐릭터 두뇌(LLM) 프리셋").add_sub_option(char_opt(app)).add_sub_option(preset_opt()))
            .add_option(
                sub("대체두뇌", "한도·실패 때 넘어갈 프리셋")
                    .add_sub_option(char_opt(app))
                    .add_sub_option(preset_opt().description("대체 프리셋('없음'이면 끔)")),
            )
            .add_option(
                sub("목소리", "캐릭터 목소리(무료 edge-tts 기준)")
                    .add_sub_option(char_opt(app))
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "목소리", "목소리 이름(입력하면 목록이 뜸)")
                            .required(true)
                            .set_autocomplete(true),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::Integer, "속도", "-50~100 (%) · 비우면 지금 값 유지").max_int_value(100),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::Integer, "높낮이", "-50~50 (Hz) · 비우면 지금 값 유지").max_int_value(50),
                    ),
            )
            .add_option(
                sub("말투선택", "캐릭터 말투 고르기 — 성격 있는 반말 / 공손한 존댓말")
                    .add_sub_option(char_opt(app))
                    .add_sub_option(
                        CreateCommandOption::new(CommandOptionType::String, "말투", "어떤 말투로 대답할지")
                            .add_string_choice("성격 (캐릭터 성격 그대로, 반말)", "성격")
                            .add_string_choice("존댓말 (상냥한 해요체)", "존댓말")
                            .required(true),
                    ),
            )
            .add_option(
                sub("말투", "캐릭터 말투 직접 쓰기(시스템 프롬프트 — 성격 말투에 적용)")
                    .add_sub_option(char_opt(app))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::String, "내용", "비우면 기본값으로").max_length(3000)),
            )
            .add_option(sub("채널", "긴 답을 올릴 텍스트 채널").add_sub_option(text_ch()))
            .add_option(
                sub("한도", "[주인] 하루 유료 호출·금액 상한")
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::Integer, "하루호출", "유료 호출 수").min_int_value(0))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::Number, "하루금액", "USD").min_number_value(0.0)),
            )
            .add_option(sub("주인없으면", "주인이 채널에 없을 때 듣지 않기").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Boolean, "안듣기", "켜면 주인이 없을 때 안 듣는다").required(true),
            ))
            .add_option(sub("호출방식", "이름만 불러도 되는지, \"헤이 클짱\"처럼 헤이를 붙여야만 하는지").add_sub_option(
                CreateCommandOption::new(CommandOptionType::String, "방식", "헤이만이면 이름만 부르는 말은 무시")
                    .add_string_choice("이름 (클짱·헤이 클짱 둘 다)", "이름")
                    .add_string_choice("헤이만 (헤이 클짱)", "헤이만")
                    .required(true),
            ))
            .add_option(sub("음성대답", "음성으로도 대답할지(끄면 글로만)").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Boolean, "켜기", "끄면 말하지 않고 글로만 대답").required(true),
            ))
            .add_option(sub("음량", "대답 목소리 크기(%)").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Integer, "퍼센트", "0~200, 기본 100").min_int_value(0).max_int_value(200).required(true),
            ))
            .add_option(
                sub("인사말", "들어올 때 음성 인사 켜기·끄기·내용")
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::Boolean, "켜기", "끄면 들어올 때 말하지 않음(채팅 고지는 그대로)").required(true))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::String, "내용", "비우면 지금 내용 유지, '기본'이면 기본값").max_length(200)),
            )
            .add_option(sub("기억시간", "대화를 기억하는 시간").add_sub_option(
                CreateCommandOption::new(CommandOptionType::Integer, "분", "1~120").min_int_value(1).max_int_value(120).required(true),
            ))
            .add_option(
                sub("호출어", "[주인] 부르는 말 추가·빼기(모든 서버 공통) — 비우면 목록")
                    .add_sub_option(char_opt(app))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::String, "추가", "예: 클로드, 오케이 클짱, 받아쓰기가 적은 변형").max_length(20))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::String, "빼기", "추가했던 호출어").max_length(20))
                    .add_sub_option(CreateCommandOption::new(CommandOptionType::Boolean, "위험", "실장·약정처럼 평소 쓰는 말이면 켬 — 헤이·부르는 꼴일 때만 호출")),
            )
            .add_option(sub("초기화", "캐릭터 설정을 config.toml 기본값으로").add_sub_option(char_opt(app))),
    ]
}

pub async fn register(app: &AppRef, ctx: &Context, guild: GuildId) {
    match guild.set_commands(&ctx.http, definitions(app)).await {
        Ok(c) => tracing::info!("슬래시 명령 {}개 등록: 서버 {guild}", c.len()),
        Err(e) => tracing::error!("슬래시 명령 등록 실패(서버 {guild}): {e:?}"),
    }
}

// ------------------------------------------------------------------ 공통

fn is_owner(app: &AppRef, cmd: &CommandInteraction) -> bool {
    cmd.user.id.get() == app.cfg().owner_id
}

fn can_manage(app: &AppRef, cmd: &CommandInteraction) -> bool {
    is_owner(app, cmd) || cmd.member.as_ref().and_then(|m| m.permissions).is_some_and(|p| p.manage_guild())
}

async fn reply(ctx: &Context, cmd: &CommandInteraction, text: impl Into<String>, ephemeral: bool) {
    let msg = CreateInteractionResponseMessage::new().content(text).ephemeral(ephemeral);
    if let Err(e) = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await {
        tracing::warn!("응답 실패: {e}");
    }
}

async fn reply_embed(ctx: &Context, cmd: &CommandInteraction, e: CreateEmbed, ephemeral: bool) {
    let msg = CreateInteractionResponseMessage::new().embed(e).ephemeral(ephemeral);
    if let Err(e) = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await {
        tracing::warn!("응답 실패: {e}");
    }
}

fn opt_str<'a>(opts: &'a [ResolvedOption<'a>], name: &str) -> Option<&'a str> {
    opts.iter().find(|o| o.name == name).and_then(|o| match &o.value {
        ResolvedValue::String(s) => Some(*s),
        ResolvedValue::Autocomplete { value, .. } => Some(*value),
        _ => None,
    })
}

fn opt_int(opts: &[ResolvedOption<'_>], name: &str) -> Option<i64> {
    opts.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Integer(i) => Some(i),
        _ => None,
    })
}

fn opt_num(opts: &[ResolvedOption<'_>], name: &str) -> Option<f64> {
    opts.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Number(n) => Some(n),
        _ => None,
    })
}

fn opt_bool(opts: &[ResolvedOption<'_>], name: &str) -> Option<bool> {
    opts.iter().find(|o| o.name == name).and_then(|o| match o.value {
        ResolvedValue::Boolean(b) => Some(b),
        _ => None,
    })
}

fn opt_channel(opts: &[ResolvedOption<'_>], name: &str) -> Option<ChannelId> {
    opts.iter().find(|o| o.name == name).and_then(|o| match &o.value {
        ResolvedValue::Channel(c) => Some(c.id),
        _ => None,
    })
}

const NO_PERM: &str = "이 설정은 주인이나 서버 관리 권한이 있는 사람만 바꿀 수 있어요.";
const BAD_OPT: &str = "옵션을 다시 골라 주세요(목록에서 고르면 돼요).";

/// 디스코드 길이 제한(임베드 필드 1,024자·자동완성 100자 등)에 맞춰 자른다
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
const OWNER_ONLY: &str = "주인만 쓸 수 있는 명령이에요.";

/// 프리셋 고르기 검사 — SPEC F-31
fn check_preset(app: &AppRef, cmd: &CommandInteraction, name: &str, allow_none: bool) -> Result<Option<String>, String> {
    if allow_none && (name == "없음" || name.is_empty()) {
        return Ok(None);
    }
    let cfg = app.cfg();
    let Some(p) = cfg.llm.presets.get(name) else {
        return Err(format!("'{name}' 프리셋이 없어요. 목록에서 골라 주세요."));
    };
    if p.owner_only && !is_owner(app, cmd) {
        return Err(format!("'{name}'은(는) 비싸서 주인만 고를 수 있어요."));
    }
    if p.trains_on_input && !is_owner(app, cmd) {
        return Err(format!("'{name}'은(는) 입력을 학습에 쓰는 무료 등급이라 주인만 켤 수 있어요."));
    }
    // 유료 두뇌는 서버 관리 권한이 있어야 — 친구 한 명이 비싼 모델을 서버 기본으로 바꿔 두는 비용 위험(2026-10-15 검토)
    if !p.free && !can_manage(app, cmd) {
        return Err(format!("'{name}'은(는) 유료라 주인이나 서버 관리자만 고를 수 있어요. 무료 두뇌는 누구나 돼요."));
    }
    // 골라 놓고 대답마다 조용히 대체로 넘어가는 일이 없게 — 지금 못 쓰는 프리셋은 받지 않는다
    if !p.has_key() {
        return Err(format!("'{name}'은(는) API 키({})가 아직 없어서 못 써요. 주인이 키를 넣으면 쓸 수 있어요.", p.key_env()));
    }
    if app.store.lock().unwrap().state.disabled_presets.contains(name) {
        return Err(format!("'{name}'은(는) 웹 콘솔에서 꺼져 있어요."));
    }
    Ok(Some(name.to_string()))
}

fn preset_label(app: &AppRef, name: &str) -> String {
    let cfg = app.cfg();
    match cfg.llm.presets.get(name) {
        Some(p) => {
            let tag = if p.free { "무료".to_string() } else { format!("유료 ${}/${}", p.price_in, p.price_out) };
            let key = if p.has_key() { "" } else { " ⚠️키 없음" };
            format!("`{name}` ({} · {tag}{key})", p.model)
        }
        None => format!("`{name}`"),
    }
}

// ------------------------------------------------------------------ 처리

pub async fn handle(app: &AppRef, ctx: &Context, cmd: &CommandInteraction) {
    tracing::info!("명령 /{} user={} guild={:?}", cmd.data.name, cmd.user.id, cmd.guild_id);
    let Some(guild) = cmd.guild_id else {
        reply(ctx, cmd, "서버 채널에서 써 주세요.", true).await;
        return;
    };
    let g = guild.get();
    let opts = cmd.data.options();
    match cmd.data.name.as_str() {
        "들어와" => {
            let cached = ctx
                .cache
                .guild(guild)
                .and_then(|gu| gu.voice_states.get(&cmd.user.id).and_then(|v| v.channel_id));
            // 음성 상태 조회(HTTP)·연결이 3초를 넘길 수 있다 — 느린 일 전에 미룬다.
            // 캐시에 있으면 입장 안내는 모두에게, 없으면(조회 필요) 결과를 본인에게만
            if cached.is_some() {
                let _ = cmd.defer(&ctx.http).await;
            } else {
                let _ = cmd.defer_ephemeral(&ctx.http).await;
            }
            let channel = match cached {
                Some(c) => Some(c),
                None => ctx.http.get_user_voice_state(guild, cmd.user.id).await.ok().and_then(|v| v.channel_id),
            };
            let Some(channel) = channel else {
                let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content("먼저 음성 채널에 들어가 있어 주세요.")).await;
                return;
            };
            let msg = match voice::join(app, guild, channel, Some(cmd.channel_id)).await {
                Ok(s) => {
                    let listening = s.listening.load(Ordering::Relaxed);
                    let tc = voice::text_channel(app, guild).map(|c| format!("<#{c}>")).unwrap_or("-".into());
                    let mut m = format!("<#{channel}> 에 들어왔어요. 긴 답은 {tc} 에 올릴게요.");
                    if !listening {
                        m.push_str("\n⏸️ 주인이 채널에 없어서 주인이 들어오면 듣기 시작해요.");
                    }
                    m
                }
                Err(e) => format!("들어가지 못했어요: {e}"),
            };
            let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(msg)).await;
        }
        "나가" => {
            if !app.sessions.lock().await.contains_key(&g) {
                return reply(ctx, cmd, "지금 음성 채널에 없어요.", true).await;
            }
            // 음성 연결 정리가 3초를 넘길 수 있다
            let _ = cmd.defer(&ctx.http).await;
            let msg = if voice::leave(app, guild).await { "👋 나갈게요. 기억한 대화도 지웠어요." } else { "지금 음성 채널에 없어요." };
            let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(msg)).await;
        }
        "무시해줘" => {
            let u = cmd.user.id.get();
            {
                let mut st = app.store.lock().unwrap();
                st.guild(g).self_ignored.insert(u);
                // 그 사람이 부탁해 남긴 장기 기억도 지운다(SPEC F-11·F-37)
                st.guild(g).memories.retain(|m| m.user != u);
                st.bump();
            }
            app.save_store();
            let s = app.sessions.lock().await.get(&g).cloned();
            if let Some(s) = s {
                s.buffer.lock().unwrap().remove_user(u); // SPEC F-11: 즉시 버퍼에서도 지움
                s.forget_calls(u);
                s.reset_allowed();
            }
            reply(ctx, cmd, "🙈 이제 당신 목소리는 듣지 않아요. 기억해 둔 말도 지웠어요. `/다시들어줘`로 되돌릴 수 있어요.", true).await;
        }
        "다시들어줘" => {
            {
                let mut st = app.store.lock().unwrap();
                st.guild(g).self_ignored.remove(&cmd.user.id.get());
                st.bump();
            }
            app.save_store();
            let s = app.sessions.lock().await.get(&g).cloned();
            if let Some(s) = s {
                s.reset_allowed();
            }
            reply(ctx, cmd, "👂 다시 들을게요.", true).await;
        }
        "잊어줘" => {
            let s = app.sessions.lock().await.get(&g).cloned();
            if let Some(s) = s {
                s.buffer.lock().unwrap().clear();
                s.forget_all_calls();
            }
            reply(ctx, cmd, "🧹 기억한 대화를 전부 지웠어요.", false).await;
        }
        "기억" => {
            let Some(sc) = cmd.data.options().into_iter().next() else { return reply(ctx, cmd, BAD_OPT, true).await };
            let ResolvedValue::SubCommand(opts) = sc.value else { return reply(ctx, cmd, BAD_OPT, true).await };
            let u = cmd.user.id.get();
            match sc.name {
                "보기" => {
                    let mems = app.store.lock().unwrap().state.guilds.get(&g).map(|x| x.memories.clone()).unwrap_or_default();
                    let body = if mems.is_empty() {
                        "아직 기억하는 게 없어요. 음성으로 \"클짱, ○○ 기억해\"라고 해 보세요.".to_string()
                    } else {
                        fit(&mems.iter().enumerate().map(|(i, m)| format!("`{}` {} — {} · {}", i + 1, m.text, m.by, m.date)).collect::<Vec<_>>().join("\n"), 1900)
                    };
                    reply(ctx, cmd, format!("📝 **장기 기억** ({}개)\n{body}", mems.len()), true).await;
                }
                "지우기" => {
                    let Some(n) = opt_int(&opts, "번호") else { return reply(ctx, cmd, BAD_OPT, true).await };
                    let manage = can_manage(app, cmd);
                    let msg = {
                        let mut st = app.store.lock().unwrap();
                        let mems = &mut st.guild(g).memories;
                        let i = (n.max(1) - 1) as usize;
                        match mems.get(i) {
                            None => "그 번호의 기억이 없어요. `/기억 보기`로 번호를 확인해 주세요.".to_string(),
                            Some(m) if !manage && m.user != u => "내가 부탁한 기억만 지울 수 있어요(관리자는 전부).".to_string(),
                            Some(_) => format!("🗑️ 지웠어요: {}", mems.remove(i).text),
                        }
                    };
                    app.save_store();
                    reply(ctx, cmd, msg, true).await;
                }
                "전부지우기" => {
                    if !can_manage(app, cmd) {
                        return reply(ctx, cmd, NO_PERM, true).await;
                    }
                    app.store.lock().unwrap().guild(g).memories.clear();
                    app.save_store();
                    reply(ctx, cmd, "🧹 장기 기억을 전부 지웠어요.", false).await;
                }
                _ => reply(ctx, cmd, BAD_OPT, true).await,
            }
        }
        "상태" => reply_embed(ctx, cmd, status_embed(app, guild).await, true).await,
        "도움말" => reply_embed(ctx, cmd, help_embed(app, cmd.guild_id.is_some_and(|g| app.hey_only(g.get()))), true).await,
        "공급자" => {
            if !is_owner(app, cmd) {
                return reply(ctx, cmd, OWNER_ONLY, true).await;
            }
            reply_embed(ctx, cmd, provider_embed(app), true).await;
        }
        "설정다시읽기" => {
            if !is_owner(app, cmd) {
                return reply(ctx, cmd, OWNER_ONLY, true).await;
            }
            // 다시 읽기는 세션마다 듣기 정책을 다시 적용해 3초를 넘길 수 있다 — 먼저 미뤄 둔다
            let _ = cmd.defer_ephemeral(&ctx.http).await;
            let msg = match crate::reload(app).await {
                Ok(()) => "🔄 config.toml을 다시 읽었어요. (명령 목록도 갱신)".to_string(),
                Err(e) => format!("다시 읽기 실패 — 기존 설정 유지: {e:#}"),
            };
            let _ = cmd.edit_response(&ctx.http, EditInteractionResponse::new().content(fit(&msg, 1900))).await;
            if let Some(c) = app.ctx() {
                crate::register_all(app, &c).await;
            }
        }
        "채널지정" => set_channel(app, ctx, cmd, g, opt_channel(&opts, "채널")).await,
        "모델" => set_brain(app, ctx, cmd, g, &opts, false).await,
        "무시목록" => ignore_cmd(app, ctx, cmd, g, &opts).await,
        "설정" => settings_cmd(app, ctx, cmd, guild, &opts).await,
        _ => {}
    }
}

async fn set_channel(app: &AppRef, ctx: &Context, cmd: &CommandInteraction, g: u64, ch: Option<ChannelId>) {
    if !can_manage(app, cmd) {
        return reply(ctx, cmd, NO_PERM, true).await;
    }
    let ch = ch.unwrap_or(cmd.channel_id);
    app.store.lock().unwrap().guild(g).text_channel = Some(ch.get());
    app.save_store();
    reply(ctx, cmd, format!("📝 긴 답은 이제 <#{ch}> 에 올릴게요."), false).await;
}

async fn set_brain(app: &AppRef, ctx: &Context, cmd: &CommandInteraction, g: u64, opts: &[ResolvedOption<'_>], fallback: bool) {
    let (Some(ch), Some(p)) = (opt_str(opts, "캐릭터"), opt_str(opts, "프리셋")) else {
        return reply(ctx, cmd, "캐릭터와 프리셋을 골라 주세요.", true).await;
    };
    if !known_char(app, ch) {
        return reply(ctx, cmd, NO_CHAR, true).await;
    }
    // 두뇌 자체는 누구나(F-30), 대체 두뇌는 관리 권한
    if fallback && !can_manage(app, cmd) {
        return reply(ctx, cmd, NO_PERM, true).await;
    }
    let preset = match check_preset(app, cmd, p, fallback) {
        Ok(p) => p,
        Err(e) => return reply(ctx, cmd, e, true).await,
    };
    {
        let mut st = app.store.lock().unwrap();
        let o = st.guild(g).characters.entry(ch.to_string()).or_default();
        if fallback {
            o.fallback = Some(preset.clone().unwrap_or_default()); // ""는 "대체 없음"
        } else {
            o.brain = preset.clone();
        }
    }
    app.save_store();
    let label = preset.as_deref().map(|p| preset_label(app, p)).unwrap_or("없음".into());
    let what = if fallback { "대체 두뇌" } else { "두뇌" };
    reply(ctx, cmd, format!("🧠 {ch}의 {what}를 {label}(으)로 바꿨어요."), false).await;
}

async fn ignore_cmd(app: &AppRef, ctx: &Context, cmd: &CommandInteraction, g: u64, opts: &[ResolvedOption<'_>]) {
    let Some(ResolvedOption { name, value: ResolvedValue::SubCommand(sub), .. }) = opts.first() else {
        return reply(ctx, cmd, BAD_OPT, true).await;
    };
    // 보기 포함 전부 주인·서버 관리자만(SPEC §9)
    if !can_manage(app, cmd) {
        return reply(ctx, cmd, NO_PERM, true).await;
    }
    // 유저 무시를 더하면 그 사람의 기존 대화·받아쓰기 기록·장기 기억도 지운다(SPEC F-10·F-11)
    let mut purge: Option<u64> = None;
    // 역할 무시를 더하면 누가 그 역할인지 따지지 않고 이 서버의 호출 대기(다음 말·판단)를 모두 무효로 — 늦은 판단 결과가 적용되지 않게
    let mut role_added = false;
    let msg = {
        let mut st = app.store.lock().unwrap();
        let gs = st.guild(g);
        let m = match (*name, sub.first().map(|o| &o.value)) {
            ("유저추가", Some(ResolvedValue::User(u, _))) => {
                gs.ignore_users.insert(u.id.get());
                gs.memories.retain(|m| m.user != u.id.get());
                purge = Some(u.id.get());
                format!("🔇 <@{}> 목소리를 무시할게요.", u.id)
            }
            ("유저삭제", Some(ResolvedValue::User(u, _))) => {
                gs.ignore_users.remove(&u.id.get());
                format!("🔈 <@{}> 무시를 풀었어요.", u.id)
            }
            ("역할추가", Some(ResolvedValue::Role(r))) => {
                gs.ignore_roles.insert(r.id.get());
                role_added = true;
                format!("🔇 역할 <@&{}> 가진 사람은 전부 무시할게요.", r.id)
            }
            ("역할삭제", Some(ResolvedValue::Role(r))) => {
                gs.ignore_roles.remove(&r.id.get());
                format!("🔈 역할 <@&{}> 무시를 풀었어요.", r.id)
            }
            ("보기", _) => {
                let us: Vec<String> = gs.ignore_users.iter().map(|u| format!("<@{u}>")).collect();
                let rs: Vec<String> = gs.ignore_roles.iter().map(|r| format!("<@&{r}>")).collect();
                let ss: Vec<String> = gs.self_ignored.iter().map(|u| format!("<@{u}>")).collect();
                format!(
                    "**유저 무시**: {}\n**역할 무시**: {}\n**스스로 거부(/무시해줘)**: {}\n봇은 자동으로 듣지 않아요.",
                    if us.is_empty() { "없음".into() } else { us.join(" ") },
                    if rs.is_empty() { "없음".into() } else { rs.join(" ") },
                    if ss.is_empty() { "없음".into() } else { ss.join(" ") },
                )
            }
            _ => "알 수 없는 명령이에요.".into(),
        };
        st.bump();
        m
    };
    app.save_store();
    let s = app.sessions.lock().await.get(&g).cloned();
    if let Some(s) = s {
        s.reset_allowed();
        if let Some(u) = purge {
            s.buffer.lock().unwrap().remove_user(u);
            s.forget_calls(u);
        }
        if role_added {
            s.forget_all_calls();
        }
    }
    let msg = CreateInteractionResponseMessage::new()
        .content(msg)
        .ephemeral(true)
        .allowed_mentions(serenity::all::CreateAllowedMentions::new());
    let _ = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await;
}

async fn settings_cmd(app: &AppRef, ctx: &Context, cmd: &CommandInteraction, guild: GuildId, opts: &[ResolvedOption<'_>]) {
    let g = guild.get();
    let Some(ResolvedOption { name, value: ResolvedValue::SubCommand(sub), .. }) = opts.first() else {
        return reply(ctx, cmd, BAD_OPT, true).await;
    };
    let name = *name;
    match name {
        "보기" => return reply_embed(ctx, cmd, settings_embed(app, guild), true).await,
        "두뇌" => {
            // /설정은 서버 설정 — 관리 권한 필요(누구나 쓰는 건 /모델, SPEC F-30·F-36)
            if !can_manage(app, cmd) {
                return reply(ctx, cmd, NO_PERM, true).await;
            }
            return set_brain(app, ctx, cmd, g, sub, false).await;
        }
        "대체두뇌" => return set_brain(app, ctx, cmd, g, sub, true).await,
        "채널" => return set_channel(app, ctx, cmd, g, opt_channel(sub, "채널")).await,
        _ => {}
    }
    if name == "한도" {
        if !is_owner(app, cmd) {
            return reply(ctx, cmd, OWNER_ONLY, true).await;
        }
        let mut lim = app.limits();
        if let Some(c) = opt_int(sub, "하루호출") {
            lim.daily_calls = c as u32;
        }
        if let Some(u) = opt_num(sub, "하루금액") {
            lim.daily_usd = u;
        }
        let msg = format!("💸 하루 유료 한도: {}회 / ${:.2}", lim.daily_calls, lim.daily_usd);
        app.store.lock().unwrap().state.limits = Some(lim);
        app.save_store();
        return reply(ctx, cmd, msg, true).await;
    }
    if !can_manage(app, cmd) {
        return reply(ctx, cmd, NO_PERM, true).await;
    }
    // 캐릭터를 받는 설정은 먼저 있는 캐릭터인지 본다
    if let Some(ch) = opt_str(sub, "캐릭터") {
        if !known_char(app, ch) {
            return reply(ctx, cmd, NO_CHAR, true).await;
        }
    }
    let msg = match name {
        "말투선택" => {
            let (Some(ch), Some(style)) = (opt_str(sub, "캐릭터"), opt_str(sub, "말투")) else { return reply(ctx, cmd, BAD_OPT, true).await };
            if !crate::config::STYLES.contains(&style) {
                return reply(ctx, cmd, BAD_OPT, true).await;
            }
            app.store.lock().unwrap().guild(g).characters.entry(ch.to_string()).or_default().style = Some(style.to_string());
            app.save_store();
            if style == "존댓말" { format!("🎀 {ch}는 이제 상냥한 존댓말로 대답해요.") } else { format!("😼 {ch}는 이제 성격 그대로(반말) 대답해요.") }
        }
        "목소리" => {
            let (Some(ch), Some(v)) = (opt_str(sub, "캐릭터"), opt_str(sub, "목소리")) else {
                return reply(ctx, cmd, BAD_OPT, true).await;
            };
            let Some(mut cur) = app.character(g, ch) else { return reply(ctx, cmd, BAD_OPT, true).await };
            // 속도·높낮이를 비우면 지금 값을 그대로 둔다(목소리만 바꿀 때 높낮이가 날아가지 않게)
            let old = cur.voice.get("edge-tts").cloned().unwrap_or_default();
            let keep = |s: &str, d: &str| if s.is_empty() { d.to_string() } else { s.to_string() };
            let rate = opt_int(sub, "속도").map(|r| format!("{:+}%", r.clamp(-50, 100))).unwrap_or_else(|| keep(&old.rate, "+0%"));
            let pitch = opt_int(sub, "높낮이").map(|p| format!("{:+}Hz", p.clamp(-50, 50))).unwrap_or_else(|| keep(&old.pitch, "+0Hz"));
            let nv = Voice { voice: v.to_string(), rate, pitch };
            if let Err(e) = crate::config::validate_voice("edge-tts", &nv) {
                return reply(ctx, cmd, e, true).await;
            }
            // edge-tts와 azure-tts는 같은 음성 이름을 쓴다
            cur.voice.insert("edge-tts".into(), nv.clone());
            cur.voice.insert("azure-tts".into(), nv.clone());
            app.store.lock().unwrap().guild(g).characters.entry(ch.to_string()).or_default().voice = Some(cur.voice);
            app.save_store();
            // 음성 채널에 있으면 바로 들려준다
            let sess = app.sessions.lock().await.get(&g).cloned();
            if let Some(s) = sess {
                let (app2, ch2) = (app.clone(), ch.to_string());
                tokio::spawn(async move {
                    let _ = voice::speak(&app2, &s, &ch2, "이 목소리 어때?", (0, String::new())).await;
                });
            }
            format!("🗣️ {ch} 목소리: `{}` 속도 {} 높낮이 {}", nv.voice, nv.rate, nv.pitch)
        }
        "말투" => {
            let Some(ch) = opt_str(sub, "캐릭터") else { return reply(ctx, cmd, BAD_OPT, true).await };
            let content = opt_str(sub, "내용").map(str::to_string);
            app.store.lock().unwrap().guild(g).characters.entry(ch.to_string()).or_default().persona = content.clone();
            app.save_store();
            match content {
                Some(c) => format!("✍️ {ch} 말투를 바꿨어요: {}", c.chars().take(80).collect::<String>()),
                None => format!("✍️ {ch} 말투를 기본값으로 돌렸어요."),
            }
        }
        "주인없으면" => {
            let v = opt_bool(sub, "안듣기").unwrap_or(true);
            app.store.lock().unwrap().guild(g).require_owner_present = Some(v);
            app.save_store();
            // 잠금을 먼저 놓고 부른다 — apply_listen이 sessions를 다시 잠근다.
            // 듣기 재적용은 채널에 안내 글을 올려 3초를 넘길 수 있으니 응답과 따로 돌린다
            let s = app.sessions.lock().await.get(&g).cloned();
            if let Some(s) = s {
                let app2 = app.clone();
                tokio::spawn(async move { voice::apply_listen(&app2, &s).await });
            }
            if v { "⏸️ 주인이 채널에 없으면 듣지 않아요.".into() } else { "▶️ 주인이 없어도 들어요(녹음 고지는 계속 보여요).".into() }
        }
        "호출방식" => {
            let hey = match opt_str(sub, "방식") {
                Some("헤이만") => true,
                Some("이름") => false,
                _ => return reply(ctx, cmd, BAD_OPT, true).await,
            };
            app.store.lock().unwrap().guild(g).hey_only = Some(hey);
            app.save_store();
            if hey { "📣 이제 \"헤이 클짱\"·\"헤이 아짱\"처럼 헤이를 붙여 불러야 대답해요.".into() } else { "📣 \"클짱\"·\"아짱\"만 불러도 대답해요(헤이를 붙여도 돼요).".into() }
        }
        "음성대답" => {
            let Some(on) = opt_bool(sub, "켜기") else { return reply(ctx, cmd, BAD_OPT, true).await };
            app.store.lock().unwrap().guild(g).voice_reply = Some(on);
            app.save_store();
            if on { "🔊 음성으로도 대답해요.".into() } else { "🔇 이제 말하지 않고 글로만 대답해요.".into() }
        }
        "인사말" => {
            let Some(on) = opt_bool(sub, "켜기") else { return reply(ctx, cmd, BAD_OPT, true).await };
            let text = opt_str(sub, "내용").map(|t| t.trim().to_string());
            {
                let mut st = app.store.lock().unwrap();
                let gs = st.guild(g);
                gs.greeting_on = Some(on);
                match text.as_deref() {
                    Some("기본") => gs.greeting = None,
                    Some(t) if !t.is_empty() => gs.greeting = Some(t.chars().take(200).collect()),
                    _ => {}
                }
            }
            app.save_store();
            match app.greeting(g) {
                Some(t) => format!("👋 들어올 때 이렇게 인사할게요: {}", t.chars().take(120).collect::<String>()),
                None => "🤫 들어올 때 음성 인사는 안 할게요(채팅 고지는 그대로 올라가요).".into(),
            }
        }
        "음량" => {
            let Some(v) = opt_int(sub, "퍼센트") else { return reply(ctx, cmd, BAD_OPT, true).await };
            let v = v.clamp(0, 200) as u32;
            app.store.lock().unwrap().guild(g).volume = Some(v);
            app.save_store();
            format!("🔉 대답 음량을 {v}%로 맞췄어요(다음 대답부터).")
        }
        "기억시간" => {
            let m = opt_int(sub, "분").unwrap_or(15).clamp(1, 120) as u64;
            app.store.lock().unwrap().guild(g).buffer_minutes = Some(m);
            app.save_store();
            format!("⏳ 대화를 최근 {m}분만 기억해요.")
        }
        "호출어" => {
            if !is_owner(app, cmd) {
                return reply(ctx, cmd, OWNER_ONLY, true).await;
            }
            let Some(ch) = opt_str(sub, "캐릭터").map(str::to_string) else { return reply(ctx, cmd, BAD_OPT, true).await };
            let risky = opt_bool(sub, "위험").unwrap_or(false);
            let mut words = {
                let st = app.store.lock().unwrap();
                let map = if risky { &st.state.wake_risky } else { &st.state.wake_extra };
                map.get(&ch).cloned().unwrap_or_default()
            };
            let add = opt_str(sub, "추가").map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "));
            let del = opt_str(sub, "빼기").map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "));
            if add.is_none() && del.is_none() {
                let base = app.cfg().characters.get(&ch).map(|c| c.wake.join(", ")).unwrap_or_default();
                let (extra, risk) = {
                    let st = app.store.lock().unwrap();
                    let j = |m: &std::collections::BTreeMap<String, Vec<String>>| m.get(&ch).filter(|v| !v.is_empty()).map(|v| v.join(", ")).unwrap_or_else(|| "없음".into());
                    (j(&st.state.wake_extra), j(&st.state.wake_risky))
                };
                return reply(ctx, cmd, fit(&format!("📣 **{ch}** 호출어\n기본: {base}\n추가: {extra}\n위험 별칭: {risk}"), 1900), true).await;
            }
            if let Some(a) = add {
                words.push(a);
            }
            if let Some(d) = del {
                words.retain(|w| w != &d);
            }
            let r = if risky { voice::set_wake_risky(app, &ch, &words).await } else { voice::set_wake_extra(app, &ch, &words).await };
            match r {
                Ok(w) => format!("📣 {ch} {}: {}", if risky { "위험 별칭" } else { "추가 호출어" }, if w.is_empty() { "없음".into() } else { w.join(", ") }),
                Err(e) => return reply(ctx, cmd, format!("❌ {e}"), true).await,
            }
        }
        "초기화" => {
            let Some(ch) = opt_str(sub, "캐릭터") else { return reply(ctx, cmd, BAD_OPT, true).await };
            app.store.lock().unwrap().guild(g).characters.remove(ch);
            app.save_store();
            format!("♻️ {ch} 설정을 기본값으로 돌렸어요.")
        }
        _ => "알 수 없는 설정이에요.".into(),
    };
    reply(ctx, cmd, msg, false).await;
}

// ------------------------------------------------------------------ 보기(임베드)

pub fn settings_embed(app: &AppRef, guild: GuildId) -> CreateEmbed {
    let g = guild.get();
    let cfg = app.cfg();
    let mut e = CreateEmbed::new().title("⚙️ 아짱·클짱 설정").color(COLOR);
    for (k, _) in &cfg.characters {
        let Some(c) = app.character(g, k) else { continue };
        let v = c.voice.get("edge-tts").cloned().unwrap_or_default();
        let fb = c.fallback.as_deref().map(|f| preset_label(app, f)).unwrap_or("없음".into());
        let persona: String = c.active_persona().chars().take(60).collect();
        e = e.field(
            format!("{} {}", c.emoji, c.name),
            fit(&format!(
                "두뇌 {}\n대체 {}\n목소리 `{}` {} {}\n말투 {} · {}…",
                preset_label(app, &c.brain),
                fb,
                if v.voice.is_empty() { "기본" } else { &v.voice },
                v.rate,
                v.pitch,
                c.style,
                persona.replace('\n', " ")
            ), 1024),
            true,
        );
    }
    let (req_owner, minutes) = app.listen_opts(g);
    let hey_only = app.hey_only(g);
    let (voice_on, vol) = (app.voice_reply(g), app.volume(g));
    let greet = app.greeting(g).map(|t| t.chars().take(40).collect::<String>()).unwrap_or_else(|| "끔".into());
    let mem_n = app.store.lock().unwrap().state.guilds.get(&g).map(|x| x.memories.len()).unwrap_or(0);
    let (tc, iu, ir, si) = {
        let st = app.store.lock().unwrap();
        let gs = st.state.guilds.get(&g);
        (
            gs.and_then(|x| x.text_channel).map(|c| format!("<#{c}>")).unwrap_or("미지정(/들어와 친 채널)".into()),
            gs.map(|x| x.ignore_users.len()).unwrap_or(0),
            gs.map(|x| x.ignore_roles.len()).unwrap_or(0),
            gs.map(|x| x.self_ignored.len()).unwrap_or(0),
        )
    };
    e = e.field(
        "이 서버",
        format!(
            "긴 답 채널 {tc}\n주인 없으면 {}\n호출 방식 {}\n음성 대답 {} · 음량 {vol}%\n입장 인사 {greet}\n장기 기억 {mem_n}개(/기억)\n기억 시간 {minutes}분\n무시: 유저 {iu} · 역할 {ir} · 스스로 {si}",
            if req_owner { "안 들음" } else { "들음" },
            if hey_only { "헤이만 (헤이 클짱)" } else { "이름 (클짱·헤이 클짱)" },
            if voice_on { "켬" } else { "끔(글로만)" }
        ),
        false,
    );
    let lim = app.limits();
    let (paid, usd) = {
        let mut st = app.store.lock().unwrap();
        let d = st.day();
        (d.paid_calls, d.usd)
    };
    let chain = if cfg.llm.free_chain.is_empty() { "없음".to_string() } else { cfg.llm.free_chain.join(" → ") };
    e = e.field(
        "공통",
        fit(
            &format!(
                "받아쓰기 `{}`\n목소리 `{}`\n무료 대체 체인 `{chain}` (두뇌·대체가 다 실패하면)\n하루 유료 한도 {}회 / ${:.2} (오늘 {paid}회 · ${usd:.3})",
                app.stt_chain().join(" → "),
                app.tts_chain().join(" → "),
                lim.daily_calls,
                lim.daily_usd
            ),
            1024,
        ),
        false,
    );
    e.footer(serenity::all::CreateEmbedFooter::new(
        "바꾸기: /설정 두뇌·대체두뇌·목소리·말투선택·말투·채널·한도·주인없으면·호출방식·음성대답·음량·인사말·기억시간·초기화 · 가격은 1M 토큰당 입력/출력 USD · /도움말",
    ))
}

pub async fn status_embed(app: &AppRef, guild: GuildId) -> CreateEmbed {
    let side = app.sidecar.lock().unwrap().clone();
    let mut e = CreateEmbed::new().title("📡 상태").color(COLOR);
    let sess = app.sessions.lock().await.get(&guild.get()).cloned();
    match sess {
        Some(s) => {
            let ch = *s.channel.lock().unwrap();
            let st = s.stats.lock().unwrap().clone();
            let mut listening_names = vec![];
            let mut skipped = vec![];
            for (u, name) in s.channel_members(app) {
                match voice::exclusion(app, &s, u) {
                    Some(why) => skipped.push(format!("{name}({why})")),
                    None => listening_names.push(name),
                }
            }
            e = e.field(
                "음성",
                format!(
                    "<#{ch}> · {} · {}분째\n듣는 사람: {}\n제외: {}",
                    if s.listening.load(Ordering::Relaxed) { "듣는 중" } else { "멈춤(주인 없음)" },
                    s.started.elapsed().as_secs() / 60,
                    if listening_names.is_empty() { "-".into() } else { listening_names.join(", ") },
                    if skipped.is_empty() { "-".into() } else { skipped.join(", ") },
                ),
                false,
            );
            e = e.field(
                "이번 세션",
                format!(
                    "발화 {} · 잡음 {} · 환각 거름 {} · 받아쓰기 실패 {}\n호출 {} · 대답 {} · 실패 {} · 기억 {}줄",
                    st.utterances, st.dropped_noise, st.stt_filtered, st.stt_failed, st.wakes, st.answers, st.answer_failed,
                    s.buffer.lock().unwrap().len()
                ),
                false,
            );
        }
        None => e = e.description("음성 채널에 없어요. `/들어와`로 불러 주세요."),
    }
    let (n, p50, p95) = app.latency_stats();
    let gpu = side.detail.get("gpu").map(|g| format!("{} / {} MiB", g["used_mib"], g["total_mib"])).unwrap_or("-".into());
    e.field(
        "받아쓰기 사이드카",
        format!(
            "{} · GPU {gpu}\n응답 지연(말 끝→첫 소리) {}",
            if side.ok { "정상" } else { "⚠️ 응답 없음" },
            match (p50, p95) {
                (Some(a), Some(b)) => format!("p50 {:.1}초 · p95 {:.1}초 (n={n})", a as f64 / 1000.0, b as f64 / 1000.0),
                _ => "아직 없음".into(),
            }
        ),
        false,
    )
}

pub fn provider_embed(app: &AppRef) -> CreateEmbed {
    let cfg = app.cfg();
    let last = app.last_used.lock().unwrap().clone();
    let rl = app.ratelimits.lock().unwrap().clone();
    let disabled = app.store.lock().unwrap().state.disabled_presets.clone();
    let mut presets = String::new();
    for (name, p) in &cfg.llm.presets {
        let mut flags = vec![];
        flags.push(if p.free { "무료" } else { "유료" });
        if !p.has_key() {
            flags.push("키 없음");
        }
        if disabled.contains(name) {
            flags.push("꺼짐");
        }
        if p.owner_only {
            flags.push("주인 전용");
        }
        if p.trains_on_input {
            flags.push("⚠️학습 사용");
        }
        let r = rl
            .get(name)
            .map(|r| format!(" · 남은 요청 {} 토큰 {} ({})", r.remaining_requests.as_deref().unwrap_or("?"), r.remaining_tokens.as_deref().unwrap_or("?"), r.at))
            .unwrap_or_default();
        let price = if p.free { String::new() } else { format!(" ${}/${}", p.price_in, p.price_out) };
        presets.push_str(&format!("`{name}` {}{price} — {}{r}\n", p.model, flags.join("·")));
    }
    let lim = app.limits();
    let (day, month) = {
        let mut st = app.store.lock().unwrap();
        let d = st.day().clone();
        (d, st.month_usd())
    };
    let capped = llm::capped(app);
    CreateEmbed::new()
        .title("🔌 공급자")
        .color(COLOR)
        .field("받아쓰기 체인", format!("`{}`\n최근 사용: {}", app.stt_chain().join(" → "), or_dash(&last.stt)), false)
        .field("목소리 체인", format!("`{}`\n최근 사용: {}", app.tts_chain().join(" → "), or_dash(&last.tts)), false)
        .field("두뇌 프리셋", fit(&format!("{presets}최근 사용: {}", or_dash(&last.llm)), 1024), false)
        .field(
            "사용량",
            format!(
                "오늘 유료 {}회 · ${:.4} / 한도 {}회 · ${:.2}{}\n오늘 무료 {}회\n이번 달 유료 {}회 · 무료 {}회 · ${:.3}",
                day.paid_calls, day.usd, lim.daily_calls, lim.daily_usd, if capped { " (**상한 도달 — 무료로 대답 중**)" } else { "" },
                day.free_calls, month.0, month.1, month.2
            ),
            false,
        )
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

pub fn help_embed(app: &AppRef, hey_only: bool) -> CreateEmbed {
    let cfg = app.cfg();
    let names: Vec<String> = cfg.characters.values().map(|c| format!("{} {}", c.emoji, c.name)).collect();
    let how = if hey_only {
        "\"헤이 클짱, 오늘 저녁 뭐 먹지?\"처럼 **헤이를 붙여** 부르면 대답해요(이 서버는 `/설정 호출방식` 헤이만)."
    } else {
        "\"클짱, 오늘 저녁 뭐 먹지?\"처럼 이름을 부르면 대답해요. \"헤이 클짱\"도 돼요."
    };
    let emojis: Vec<String> = cfg.characters.values().filter(|c| !c.emoji.is_empty()).map(|c| format!("{}({})", c.emoji, c.name)).collect();
    let emoji_how = if emojis.is_empty() { String::new() } else { format!(" 음성 채널에서 {} 이모지 반응을 누르고 말해도 돼요.", emojis.join("·")) };
    CreateEmbed::new()
        .title("📖 아짱·클짱 쓰는 법")
        .color(COLOR)
        .description(format!(
            "음성 채널에 들어가서 `/들어와` → {how}{emoji_how}\n\
             음성으로는 짧게, 긴 답은 지정 채널에 부른 사람을 멘션해서 올려요. 이름만 부르고 멈추면 다음 말을 질문으로 들어요.\n\
             캐릭터: {}",
            names.join(" · ")
        ))
        .field("누구나", "`/들어와` `/나가` `/잊어줘` `/상태` `/설정 보기` `/도움말`\n`/무시해줘` `/다시들어줘` — 내 목소리 안 듣기·다시 듣기\n`/모델` — 캐릭터 두뇌 바꾸기(목록에서 고르기)\n`/기억 보기·지우기` — \"○○ 기억해\"로 남긴 장기 기억", false)
        .field("주인·서버 관리", "`/설정 두뇌·대체두뇌·목소리·말투선택(성격/존댓말)·말투·채널·주인없으면·호출방식·음성대답·음량·인사말·기억시간·초기화`\n`/채널지정` `/무시목록 유저추가·역할추가·…·보기` `/기억 전부지우기`", false)
        .field("주인", "`/설정 한도` `/설정 호출어` `/공급자` `/설정다시읽기` · 웹 콘솔 voicebot.macham.net", false)
        .footer(serenity::all::CreateEmbedFooter::new("음성은 저장하지 않고, 받아쓴 글은 정해진 시간만 기억했다가 지워요."))
}

// ------------------------------------------------------------------ 자동완성

static VOICES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// 목소리 목록 가져오기 실패 시각 — 1분 동안은 다시 시도하지 않는다(키 입력마다 사이드카를 두드리지 않게)
static VOICES_FAIL: Mutex<Option<Instant>> = Mutex::new(None);

async fn load_voices(app: &AppRef, wait: Duration) -> bool {
    if !VOICES.lock().unwrap().is_empty() {
        return true;
    }
    if VOICES_FAIL.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return false;
    }
    match tokio::time::timeout(wait, sidecar::voices(app, "edge-tts")).await {
        Ok(Ok(v)) => {
            let names: Vec<String> =
                v["voices"].as_array().map(|a| a.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect()).unwrap_or_default();
            if names.is_empty() {
                // 빈 목록도 실패로 본다 — 키 입력마다 다시 기다리지 않게
                *VOICES_FAIL.lock().unwrap() = Some(Instant::now());
                return false;
            }
            *VOICES.lock().unwrap() = names;
            true
        }
        _ => {
            *VOICES_FAIL.lock().unwrap() = Some(Instant::now());
            false
        }
    }
}

/// 접속 때 목소리 목록을 미리 받아 둔다 — 첫 자동완성이 디스코드 3초 제한에 걸리지 않게
pub async fn preload_voices(app: AppRef) {
    for _ in 0..18 {
        *VOICES_FAIL.lock().unwrap() = None;
        if load_voices(&app, Duration::from_secs(15)).await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub async fn autocomplete(app: &AppRef, ctx: &Context, ac: &CommandInteraction) {
    let Some(focused) = ac.data.autocomplete() else { return };
    let typed = focused.value.to_lowercase();
    let mut choices: Vec<AutocompleteChoice> = vec![];
    match focused.name {
        "캐릭터" => {
            for (k, c) in &app.cfg().characters {
                if typed.is_empty() || k.to_lowercase().contains(&typed) || c.name.to_lowercase().contains(&typed) {
                    choices.push(AutocompleteChoice::new(fit(&format!("{} {}", c.emoji, c.name), 100), fit(k, 100)));
                }
            }
        }
        "프리셋" => {
            let cfg = app.cfg();
            let owner = is_owner(app, ac);
            let disabled = app.store.lock().unwrap().state.disabled_presets.clone();
            let is_fb = ac.data.options().iter().any(|o| o.name == "대체두뇌");
            if is_fb {
                choices.push(AutocompleteChoice::new("없음 (대체 안 함)", "없음"));
            }
            // 쓸 수 있는 것 먼저, 못 쓰는 것(키 없음·꺼짐)은 뒤로 — 고르면 이유를 알려 준다
            let mut rows: Vec<(bool, AutocompleteChoice)> = vec![];
            for (name, p) in &cfg.llm.presets {
                if !typed.is_empty() && !name.to_lowercase().contains(&typed) && !p.model.to_lowercase().contains(&typed) {
                    continue;
                }
                // 주인만 고를 수 있는 것은 주인에게만 보여 준다
                if (p.owner_only || p.trains_on_input) && !owner {
                    continue;
                }
                let tag = if p.free { "무료".to_string() } else { format!("유료 ${}/${}", p.price_in, p.price_out) };
                let lock = if p.owner_only { " · 주인 전용" } else { "" };
                let usable = p.has_key() && !disabled.contains(name);
                let why = if !p.has_key() { " · 키 없음" } else if disabled.contains(name) { " · 꺼짐" } else { "" };
                let label = fit(&format!("{name} — {} ({tag}{lock}{why})", p.model), 100);
                rows.push((usable, AutocompleteChoice::new(label, fit(name, 100))));
            }
            rows.sort_by_key(|(u, _)| !*u);
            choices.extend(rows.into_iter().map(|(_, c)| c));
        }
        "목소리" => {
            // 디스코드는 자동완성 응답을 3초 안에 받아야 한다 — 목록이 없으면 짧게만 기다린다
            load_voices(app, Duration::from_millis(2000)).await;
            for n in VOICES.lock().unwrap().iter() {
                // 디스코드 제한: 이름·값 100자. 그보다 긴 이름은 값이 잘리면 못 쓰므로 아예 뺀다
                if n.chars().count() > 100 {
                    continue;
                }
                if typed.is_empty() || n.to_lowercase().contains(&typed) {
                    choices.push(AutocompleteChoice::new(n.clone(), n.clone()));
                }
            }
        }
        _ => {}
    }
    choices.truncate(25);
    let _ = ac
        .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(CreateAutocompleteResponse::new().set_choices(choices)))
        .await;
}

