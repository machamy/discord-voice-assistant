//! 두뇌(LLM). 어댑터 두 개 — Anthropic Messages API, OpenAI 호환 API(OpenAI·Groq·Gemini·OpenRouter·Ollama). SPEC §6.
//! 체인: 캐릭터 두뇌 → (하루 상한·실패 시) 대체 프리셋. SPEC F-32, F-34.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::app::{AppRef, RateInfo};
use crate::config::{Character, Preset};
use crate::convo::Line;

pub struct Answer {
    /// 음성으로 읽을 짧은 말 — SPEC F-22
    pub speech: String,
    /// 텍스트 채널에 올릴 긴 글
    pub text: String,
    pub preset: String,
    pub usd: f64,
    pub ms: u64,
    /// 하루 상한 때문에 대체 프리셋으로 내려갔음 — SPEC F-32
    pub capped: bool,
    /// <말>/<글> 형식을 지켰나(시험·통계용)
    pub format_ok: bool,
    /// 토큰: 입력, 출력, 캐시 읽기, 캐시 쓰기
    pub tokens: [u64; 4],
    /// 요청부터 <말>이 다 도착할 때까지(스트리밍) — 음성은 이때 바로 시작한다(SPEC N-01)
    pub speech_ms: Option<u64>,
    /// 기억 도구 호출 — 대답 끝의 <기억>·<잊기> 태그(SPEC F-37)
    pub memory: Vec<MemOp>,
    /// 입출력 원문(웹 콘솔 F-45) — 보낸 프롬프트 전체와 받은 응답 그대로
    pub prompt: String,
    pub raw: String,
}

/// 기억 도구 — 두뇌가 대답 끝에 붙이는 태그. 누가 분명히 부탁했을 때만 쓰게 프롬프트로 시킨다
#[derive(Debug, Clone, PartialEq)]
pub enum MemOp {
    Remember(String),
    Forget(String),
}

/// 기억 하나의 최대 길이·서버당 최대 개수·한 대답에서 받는 도구 호출 수
pub const MEMORY_MAX_CHARS: usize = 150;
pub const MEMORY_MAX_ITEMS: usize = 50;
const MEMORY_MAX_OPS: usize = 3;

/// 대답에서 <기억>…</기억>·<잊기>…</잊기>를 떼어 낸다(어디에 있든). 닫히지 않은 태그는 끝까지 지운다
pub fn extract_memory(raw: &str) -> (String, Vec<MemOp>) {
    let mut out = String::with_capacity(raw.len());
    let mut ops = Vec::new();
    let mut rest = raw;
    loop {
        let next = ["<기억>", "<잊기>"].iter().filter_map(|t| rest.find(t).map(|i| (i, *t))).min_by_key(|x| x.0);
        let Some((i, tag)) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..i]);
        let after = &rest[i + tag.len()..];
        let close = if tag == "<기억>" { "</기억>" } else { "</잊기>" };
        let (body, tail) = match after.find(close) {
            Some(j) => (&after[..j], &after[j + close.len()..]),
            None => (after, ""),
        };
        let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let body: String = body.chars().take(MEMORY_MAX_CHARS).collect();
        if !body.is_empty() && !body.contains('<') && ops.len() < MEMORY_MAX_OPS {
            ops.push(if tag == "<기억>" { MemOp::Remember(body) } else { MemOp::Forget(body) });
        }
        rest = tail;
    }
    (out.trim_end().to_string(), ops)
}

/// 스트리밍 중 <말>…</말>이 다 오면 음성 쪽으로 먼저 넘긴다 — 긴 <글>을 기다리지 않고 TTS 시작(SPEC N-01)
pub struct SpeechHook {
    tx: Option<tokio::sync::oneshot::Sender<String>>,
    t0: Instant,
    at: Option<u64>,
    /// 보낸(음성으로 나간) 말
    speech: String,
    /// 지금 프리셋에서 받은 글(중간에 끊겼을 때 살린다)
    partial: String,
    /// 공급자가 요청을 받아 스트림을 시작했음(입력 토큰은 과금됐을 수 있다)
    started: bool,
}

impl SpeechHook {
    pub fn new(tx: Option<tokio::sync::oneshot::Sender<String>>) -> Self {
        Self { tx, t0: Instant::now(), at: None, speech: String::new(), partial: String::new(), started: false }
    }
    /// 다음 프리셋을 시도하기 전에 — 이미 말을 내보냈으면 다음 프리셋으로 가지 않으므로 여기 오지 않는다
    fn reset(&mut self) {
        self.t0 = Instant::now();
        self.at = None;
        self.partial.clear();
        self.started = false;
    }
    /// 지금까지 받은 글에 닫힌 <말>이 맨 앞에 있으면 한 번만 보낸다
    fn feed(&mut self, so_far: &str) {
        self.partial.clear();
        self.partial.push_str(so_far);
        if self.at.is_some() {
            return;
        }
        let s = strip_think(so_far);
        let Some(r) = s.trim_start().strip_prefix("<말>") else { return };
        let Some((speech, rest)) = r.split_once("</말>") else { return };
        // 정상 형식일 때만 미리 보낸다: 말 안에 다른 태그가 없고, 바로 뒤에 <글>이 시작됐을 때(태그 사이 다른 글자 없음)
        if speech.contains('<') || !rest.trim_start().starts_with("<글>") {
            return;
        }
        let speech = first_sentences(speech.trim(), 160);
        if speech.is_empty() {
            return;
        }
        self.at = Some(self.t0.elapsed().as_millis() as u64);
        if let Some(tx) = self.tx.take() {
            self.speech = speech.clone();
            let _ = tx.send(speech);
        }
    }
    /// 이 프리셋의 말이 이미 음성으로 나갔나
    fn spoken(&self) -> bool {
        !self.speech.is_empty()
    }
}

/// 첫 응답(헤더)까지 기다리는 시간, 스트림이 멈춰 있어도 되는 시간
const FIRST_BYTE: Duration = Duration::from_secs(20);
const IDLE: Duration = Duration::from_secs(20);

async fn send_first(req: reqwest::RequestBuilder, body: &Value) -> Result<reqwest::Response> {
    match tokio::time::timeout(FIRST_BYTE, req.json(body).send()).await {
        Ok(r) => Ok(r?),
        Err(_) => bail!("응답 시작 시간 초과"),
    }
}

/// SSE 본문을 이벤트 단위로 읽는다(`data:` 줄만). 청크 경계에서 글자가 잘려도 이벤트 경계(\n\n)에서만 해석한다
/// 받은 조각을 버퍼에 붙이고 완성된 이벤트(빈 줄로 끝남)만 꺼낸다. CR은 버린다(JSON 문자열 안에는 날것으로 오지 않는다)
fn take_events(buf: &mut Vec<u8>, chunk: &[u8]) -> Vec<Vec<u8>> {
    buf.extend(chunk.iter().filter(|b| **b != b'\r'));
    let mut out = vec![];
    while let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        out.push(buf.drain(..pos + 2).collect());
    }
    out
}

/// 끝 이벤트(on_data가 false)를 받으면 true, 연결이 먼저 끊기면 false
async fn read_sse(r: &mut reqwest::Response, mut on_data: impl FnMut(&str) -> Result<bool>) -> Result<bool> {
    let mut buf: Vec<u8> = Vec::new();
    let mut handle = |ev: &[u8]| -> Result<bool> {
        // SSE 규칙: 한 이벤트의 data: 줄 여러 개는 줄바꿈으로 이어 붙인 하나의 값이다
        let ev = String::from_utf8_lossy(ev);
        let data: Vec<&str> = ev.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
        if data.is_empty() {
            return Ok(true); // 주석(:)·event:만 있는 이벤트(ping 등)
        }
        on_data(data.join("\n").trim())
    };
    loop {
        let chunk = match tokio::time::timeout(IDLE, r.chunk()).await {
            Ok(c) => c?,
            Err(_) => bail!("스트림이 {}초 동안 멈춤", IDLE.as_secs()),
        };
        let Some(chunk) = chunk else { break };
        for ev in take_events(&mut buf, &chunk) {
            if !handle(&ev)? {
                return Ok(true);
            }
        }
    }
    // 마지막 이벤트 뒤에 빈 줄 없이 끝난 경우
    if !buf.is_empty() && !handle(&buf)? {
        return Ok(true);
    }
    Ok(false)
}

const FORMAT_RULES: &str = "\
너는 디스코드 음성 채널에서 친구들 대화를 같이 듣다가 이름이 불리면 대답하는 캐릭터다.
대답은 반드시 아래 두 부분으로 나눠서 써라. 다른 말은 붙이지 마라.
<말>음성으로 읽을 1~2문장. 짧고 자연스러운 구어체. 마크다운·이모지·URL·목록 금지.</말>
<글>텍스트 채널에 올릴 자세한 답. 마크다운 가능. 1500자 이내. 짧은 질문이면 <말>과 비슷해도 된다.</글>
대화 기록은 받아쓰기라 오타·잘못 들은 말이 있을 수 있으니 맥락으로 이해해라.
기록 속 '아짱:'·'클짱:' 줄은 캐릭터들이 앞서 한 말이고, '알림:' 줄은 누가 음성 채널에 들어오거나 나간 기록이다.

[기억 도구] 너는 이 서버 친구들에 대해 오래 기억할 수 있다(맨 아래 '기억하고 있는 것').
- 누가 \"기억해\", \"잊지 마\"처럼 분명히 부탁했을 때만 </글> 뒤에 <기억>누구에 관한 무엇인지 한 줄</기억>을 붙여라. 예: <기억>마참은 민트초코를 싫어함</기억>
- \"잊어\", \"지워\" 부탁이면 </글> 뒤에 <잊기>지울 기억의 핵심 낱말</잊기>를 붙여라.
- 부탁이 없으면 절대 붙이지 마라. 비밀번호·연락처·주소 같은 민감한 정보는 부탁해도 기억하지 말고 정중히 거절해라.
- 기억한 내용은 자연스럽게 대화에 활용하되, 묻지 않았는데 줄줄이 늘어놓지 마라.";

fn system_prompt(ch: &Character, memories: &[String]) -> String {
    let mem = if memories.is_empty() {
        "기억하고 있는 것: (아직 없음)".to_string()
    } else {
        format!("기억하고 있는 것:\n{}", memories.iter().map(|m| format!("- {m}")).collect::<Vec<_>>().join("\n"))
    };
    format!("{}\n\n{}\n\n{mem}", ch.persona.trim(), FORMAT_RULES)
}

fn fmt_line(l: &Line) -> String {
    format!("[{}] {}: {}\n", l.time, l.speaker, l.text)
}

fn question_text(caller: &str, ch: &Character, question: &str) -> String {
    format!("지금 {caller}이(가) {}에게 말했다: \"{question}\"\n위 형식으로 대답해.", ch.name)
}

/// <말>…</말><글>…</글> 파싱. 형식을 안 지키면 앞 문장을 말로, 전체를 글로(SPEC F-23).
/// <말>은 닫는 태그가 있어야 인정한다(없으면 <글>까지 읽어 버린다). <글>은 맨 끝이라 잘려도 받아 준다.
pub fn split_answer(raw: &str) -> (String, String) {
    // 정상 형식: (공백) <말>…</말> (공백) <글>… [</글> (공백)] — 그 밖의 글자가 섞이거나 순서가 바뀌면 형식 오류
    if let Some((s, t)) = strict_parts(raw) {
        if !s.is_empty() && !t.is_empty() {
            return (first_sentences(&s, 160), t);
        }
    }
    let grab = |tag: &str, need_close: bool| -> Option<String> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let s = raw.find(&open)? + open.len();
        let e = match raw[s..].find(&close) {
            Some(i) => s + i,
            None if need_close => return None,
            None => raw.len(),
        };
        Some(raw[s..e].trim().to_string())
    };
    // 형식 오류: 글은 항상 원문 전체(태그 조각만 지움) — 상세 답이 사라지지 않게.
    // 말은 닫힌 <말>이 있으면 그것, 없으면 원문 앞 문장
    let whole = raw.replace("<말>", "").replace("</말>", "").replace("<글>", "").replace("</글>", "").trim().to_string();
    let speech = grab("말", true).filter(|s| !s.is_empty()).map(|s| first_sentences(&s, 80));
    let speech = speech.unwrap_or_else(|| first_sentences(&whole, 80));
    (speech, whole)
}

/// 정상 형식이면 (말, 글)
fn strict_parts(raw: &str) -> Option<(String, String)> {
    let r = raw.trim_start().strip_prefix("<말>")?;
    let (speech, rest) = r.split_once("</말>")?;
    let rest = rest.trim_start().strip_prefix("<글>")?;
    let text = match rest.split_once("</글>") {
        Some((t, tail)) if tail.trim().is_empty() => t,
        Some(_) => return None,
        None => rest, // 맨 끝이 잘린 것은 받아 준다
    };
    if speech.contains('<') && (speech.contains("<글>") || speech.contains("<말>")) {
        return None;
    }
    Some((speech.trim().to_string(), text.trim().to_string()))
}

fn first_sentences(t: &str, max_chars: usize) -> String {
    let clean: String = t.chars().filter(|c| !matches!(c, '*' | '#' | '`' | '_' | '>')).collect();
    let mut out = String::new();
    for part in clean.split_inclusive(['.', '!', '?', '\n']) {
        if out.chars().count() + part.chars().count() > max_chars && !out.is_empty() {
            break;
        }
        out.push_str(part);
    }
    // 문장 부호 없이 긴 첫 문장이면 글자 수로 자른다 — 음성은 항상 짧게
    if out.trim().is_empty() || out.chars().count() > max_chars {
        out = clean.chars().take(max_chars).collect();
    }
    out.trim().to_string()
}

/// 오류 종류 코드만(짧은 영숫자) — 메시지 본문은 버린다
fn err_kind(v: &Value) -> String {
    let k = v["error"]["type"].as_str().or_else(|| v["error"]["code"].as_str()).unwrap_or("");
    k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect()
}

struct Raw {
    text: String,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

fn price(p: &Preset, r: &Raw) -> f64 {
    let pin = p.price_in / 1e6;
    // 캐시 읽기 단가는 모델마다 다르다(예: gpt-6-luna $0.01, gpt-6.1-sol $0.10) — 설정에 없으면 입력의 0.1배
    let pcache = p.price_cache_read.unwrap_or(p.price_in * 0.1) / 1e6;
    (r.input as f64 * pin + r.cache_write as f64 * pin * 1.25 + r.cache_read as f64 * pcache + r.output as f64 * p.price_out / 1e6)
        .max(0.0)
}

async fn call_anthropic(app: &AppRef, name: &str, p: &Preset, system: &str, lines: &[Line], q: &str, hook: &mut SpeechHook) -> Result<Raw> {
    let key = std::env::var(p.key_env()).map_err(|_| anyhow!("{} 없음", p.key_env()))?;
    // 대화 기록을 8줄씩 블록으로 나눈다. 버퍼는 뒤에 붙기만 하므로 앞 블록들이 그대로 남아 캐시가 맞는다
    let mut content: Vec<Value> = vec![json!({"type":"text","text":"최근 대화 기록:\n"})];
    let mut chunk = String::new();
    let mut last_seq_block = None;
    for l in lines {
        let b = l.seq / 8;
        if last_seq_block.is_some() && last_seq_block != Some(b) && !chunk.is_empty() {
            content.push(json!({"type":"text","text": std::mem::take(&mut chunk)}));
        }
        last_seq_block = Some(b);
        chunk.push_str(&fmt_line(l));
    }
    if !chunk.is_empty() {
        content.push(json!({"type":"text","text": chunk}));
    }
    if let Some(last) = content.last_mut() {
        last["cache_control"] = json!({"type":"ephemeral"});
    }
    content.push(json!({"type":"text","text": q}));

    let model = p.model.as_str();
    let mut body = json!({
        "model": model,
        "max_tokens": p.max_tokens.unwrap_or(1500),
        "system": [{"type":"text","text": system}],
        "messages": [{"role":"user","content": content}],
        "stream": true,
    });
    let req = app
        .http
        .post(p.base_url.clone().unwrap_or_else(|| "https://api.anthropic.com".into()) + "/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .timeout(Duration::from_secs(120)); // 전체 상한 — 시작·멈춤은 따로(FIRST_BYTE·IDLE)
    // effort 기본 low — 짧은 대화 답이라 지연·비용을 줄인다(5.5 모델 기본값은 medium)
    let effort = p.reasoning_effort.clone().unwrap_or_else(|| "low".into());
    body["output_config"] = json!({ "effort": effort });
    // 5.5 모델은 effort low~max 전부 받는다. 생각 끄기(disabled·between_tools)만 xhigh·max에서 400이라 그땐 thinking을 안 보낸다
    // (claude-api 참고: shared/models.md "Full effort ladder through max", shared/error-codes.md)
    if matches!(effort.as_str(), "low" | "medium" | "high") {
        if model.starts_with("claude-haiku") {
            // Haiku는 생각을 끌 수 있다(effort high 이하에서만 허용)
            body["thinking"] = json!({"type":"disabled"});
        } else if model.starts_with("claude-sonnet-5-5") {
            // Sonnet 5.5는 disabled가 400 — 도구를 안 쓰므로 between_tools면 사실상 생각 없이 바로 답한다
            body["thinking"] = json!({"type":"between_tools"});
        }
        // Opus 5.5는 생각을 끌 수 없다 — effort low로만 줄인다
    }
    // 서버 쪽 모델 대체(beta)는 쓰지 않는다 — 한 요청에 여러 모델이 과금돼 하루 상한 정산이 어긋난다.
    // 거절·실패는 봇의 대체 체인(대체 두뇌 → 무료 체인)이 맡는다(SPEC F-32·F-34)
    let mut r = send_first(req, &body).await?;
    let status = r.status();
    if !status.is_success() {
        let v: Value = r.json().await.unwrap_or(Value::Null);
        // 공급자 오류 메시지 원문은 남기지 않는다(요청 내용·키가 섞일 수 있다) — 상태·오류 종류만
        bail!("anthropic {name} HTTP {status} {}", err_kind(&v));
    }
    hook.started = true;
    let mut raw = Raw { text: String::new(), input: 0, output: 0, cache_read: 0, cache_write: 0 };
    let mut stop = String::new();
    let mut failed: Option<String> = None;
    let done = read_sse(&mut r, |d| {
        // 해석 못 하는 데이터는 오류 — 조각을 버리고 성공으로 넘기면 대답 일부가 빠진 채 게시된다
        let v: Value = serde_json::from_str(d).map_err(|_| anyhow!("anthropic {name} 스트림 데이터 형식 오류"))?;
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                raw.input = u["input_tokens"].as_u64().unwrap_or(0);
                raw.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                raw.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_delta" if v["delta"]["type"] == "text_delta" => {
                raw.text.push_str(v["delta"]["text"].as_str().unwrap_or(""));
                hook.feed(&raw.text);
            }
            "message_delta" => {
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    raw.output = o;
                }
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    stop = s.to_string();
                }
            }
            "message_stop" => return Ok(false),
            "error" => {
                failed = Some(err_kind(&v));
                return Ok(false);
            }
            _ => {}
        }
        Ok(true)
    })
    .await?;
    if let Some(k) = failed {
        bail!("anthropic {name} 스트림 오류 {k}");
    }
    if !done {
        bail!("anthropic {name} 스트림이 중간에 끊김");
    }
    if stop == "refusal" {
        bail!("anthropic {name} 거절(refusal)");
    }
    if raw.text.trim().is_empty() {
        bail!("anthropic {name} 빈 응답");
    }
    Ok(raw)
}

async fn call_openai(app: &AppRef, name: &str, p: &Preset, system: &str, lines: &[Line], q: &str, hook: &mut SpeechHook) -> Result<Raw> {
    let base = p
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let mut transcript = String::from("최근 대화 기록:\n");
    for l in lines {
        transcript.push_str(&fmt_line(l));
    }
    transcript.push('\n');
    transcript.push_str(q);
    let mut body = json!({
        "model": p.model,
        "messages": [{"role":"system","content": system}, {"role":"user","content": transcript}],
        "stream": true,
    });
    if p.stream_usage {
        // 스트리밍에서 토큰 사용량은 마지막 청크로 따로 받는다(OpenAI·Groq·Gemini·Ollama 호환 엔드포인트 공통).
        // 이 필드를 거부하는 서버면 프리셋에 stream_usage = false
        body["stream_options"] = json!({"include_usage": true});
    }
    let max = p.max_tokens.unwrap_or(1500);
    if p.provider == "openai" {
        // 추론 모델은 추론 토큰도 이 한도에 포함된다
        body["max_completion_tokens"] = json!(max);
    } else {
        body["max_tokens"] = json!(max);
    }
    if let Some(e) = &p.reasoning_effort {
        body["reasoning_effort"] = json!(e);
    }
    if let Some(x) = &p.extra {
        for (k, v) in x {
            body[k.as_str()] = v.clone();
        }
    }
    let mut req = app
        .http
        .post(format!("{}/chat/completions", base.trim_end_matches('/')))
        .timeout(Duration::from_secs(120)); // 전체 상한 — 시작·멈춤은 따로(FIRST_BYTE·IDLE)
    if p.api_key_env.as_deref() != Some("") {
        let key = std::env::var(p.key_env()).map_err(|_| anyhow!("{} 없음", p.key_env()))?;
        req = req.bearer_auth(key);
    }
    let mut r = send_first(req, &body).await?;
    let status = r.status();
    // Groq 등은 남은 한도를 헤더로 준다 — SPEC F-35 "남은 무료 한도"
    let h = r.headers();
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(String::from);
    let rr = get("x-ratelimit-remaining-requests");
    let rt = get("x-ratelimit-remaining-tokens");
    if rr.is_some() || rt.is_some() {
        app.ratelimits.lock().unwrap().insert(
            name.to_string(),
            RateInfo { remaining_requests: rr, remaining_tokens: rt, at: chrono::Local::now().format("%H:%M:%S").to_string() },
        );
    }
    if !status.is_success() {
        let v: Value = r.json().await.unwrap_or(Value::Null);
        bail!("{name} HTTP {status} {}", err_kind(&v));
    }
    hook.started = true;
    let mut raw = Raw { text: String::new(), input: 0, output: 0, cache_read: 0, cache_write: 0 };
    let mut failed: Option<String> = None;
    let mut finished = false;
    let mut refused = false;
    let done = read_sse(&mut r, |d| {
        if d == "[DONE]" {
            return Ok(false);
        }
        let v: Value = serde_json::from_str(d).map_err(|_| anyhow!("{name} 스트림 데이터 형식 오류"))?;
        if v.get("error").is_some() {
            failed = Some(err_kind(&v));
            return Ok(false);
        }
        // 거절: OpenAI는 delta.refusal 또는 finish_reason=content_filter로 알린다
        if v["choices"][0]["delta"]["refusal"].as_str().is_some_and(|r| !r.is_empty())
            || v["choices"][0]["finish_reason"] == "content_filter"
        {
            refused = true;
            return Ok(false);
        }
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
            raw.text.push_str(t);
            hook.feed(&raw.text);
        }
        // [DONE]을 안 보내는 호환 서버도 있다 — finish_reason이 오면 끝난 것으로 본다(사용량 청크가 뒤따를 수 있어 계속 읽음)
        if v["choices"][0]["finish_reason"].as_str().is_some_and(|f| !f.is_empty()) {
            finished = true;
        }
        // OpenAI는 usage, Groq는 x_groq.usage로 마지막에 준다
        let u = if v["usage"].is_object() { &v["usage"] } else { &v["x_groq"]["usage"] };
        if let Some(i) = u["prompt_tokens"].as_u64() {
            raw.input = i;
            raw.output = u["completion_tokens"].as_u64().unwrap_or(0);
            raw.cache_read = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(i);
            raw.input -= raw.cache_read;
            // 캐시 쓰기(프롬프트 캐시 생성) — price()에서 1.25배로 계산
            raw.cache_write = u["prompt_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0).min(raw.input);
            raw.input -= raw.cache_write;
        }
        Ok(true)
    })
    .await?;
    if let Some(k) = failed {
        bail!("{name} 스트림 오류 {k}");
    }
    if refused {
        bail!("{name} 거절(refusal)");
    }
    if !done && !finished {
        bail!("{name} 스트림이 중간에 끊김");
    }
    raw.text = strip_think(&raw.text);
    if raw.text.trim().is_empty() {
        bail!("{name} 빈 응답");
    }
    Ok(raw)
}

/// 일부 오픈 모델(Qwen 등)은 생각을 <think>…</think>로 본문에 섞는다 — 대답만 남긴다
fn strip_think(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(a) = out.find("<think>") {
        match out[a..].find("</think>") {
            Some(b) => out.replace_range(a..a + b + "</think>".len(), ""),
            None => out.truncate(a), // 닫히지 않은 생각은 뒤를 버린다
        }
    }
    out.trim().to_string()
}

/// 오늘 유료 상한에 닿았나 — SPEC F-32
pub fn capped(app: &AppRef) -> bool {
    let lim = app.limits();
    let mut st = app.store.lock().unwrap();
    let d = st.day();
    d.paid_calls >= lim.daily_calls || d.usd >= lim.daily_usd
}

/// 이번 호출의 최대 예상 비용 — 입력은 글자 수 기준 넉넉히(한국어 ≈ 1글자 1토큰), 출력은 max_tokens 전부
/// 글자 수 → 토큰 수 보수적 추정. 한국어는 토크나이저에 따라 글자당 1토큰을 넘기도 하고 메시지 틀(역할·태그)도 붙는다
fn tokens_upper(chars: usize) -> f64 {
    chars as f64 * 2.0 + 200.0
}

fn estimate_usd(p: &Preset, prompt_chars: usize) -> f64 {
    let max_out = p.max_tokens.unwrap_or(1500) as f64;
    // 캐시 쓰기 입력은 1.25배로 정산한다(Anthropic, OpenAI cache_write_tokens) — 입력 전부가 캐시 쓰기인 최악을 잡아야
    // 정산액이 예약액을 넘지 않는다
    let input_rate = p.price_in * 1.25;
    (tokens_upper(prompt_chars) * input_rate + max_out * p.price_out) / 1e6
}

/// 상한 확인과 예약을 한 잠금 안에서 — 호출 수 +1, 예상 비용을 미리 잡아 둔다.
/// 여러 서버가 동시에 불러도 호출 수·금액(누적 + 진행 중 예상)이 상한을 넘지 않게. 끝나면 `settle_paid`.
/// 예약한 날짜(KST)를 돌려준다 — 정산도 같은 날짜에 해야 자정을 넘긴 호출이 두 날로 갈라지지 않는다
fn reserve_paid(app: &AppRef, est: f64) -> Option<String> {
    let lim = app.limits();
    let day = crate::store::today();
    let mut st = app.store.lock().unwrap();
    let d = st.day_of(&day);
    if d.paid_calls >= lim.daily_calls || d.usd + d.reserved_usd + est > lim.daily_usd {
        return None;
    }
    d.paid_calls += 1;
    d.reserved_usd += est;
    drop(st);
    // 호출 수는 예약 때 바로 저장 — 응답 전에 재시작돼도 상한 집계가 사라지지 않게
    app.save_store();
    Some(day)
}

/// 유료 예약 가드 — 정산 없이 사라지면(작업 취소·패닉) Drop에서 예약을 돌려놓는다
struct PaidGuard<'a> {
    app: &'a AppRef,
    est: f64,
    /// 예약한 날짜
    day: String,
    /// 취소 때 원장에 남길 프리셋·캐릭터
    preset: String,
    char_key: String,
    settled: bool,
}

impl PaidGuard<'_> {
    fn settle(mut self, actual: Option<f64>) {
        settle_paid(self.app, &self.day, self.est, actual);
        self.settled = true;
    }
}

impl Drop for PaidGuard<'_> {
    fn drop(&mut self) {
        if !self.settled {
            // 정산 없이 사라짐 = 호출 도중 취소(/나가 등). 이미 과금됐을 수 있으니 예약 최대액으로 잡고,
            // 프리셋·캐릭터 원장에도 같은 날짜로 남긴다(총계와 원장이 맞게)
            {
                let mut st = self.app.store.lock().unwrap();
                let d = st.day_of(&self.day);
                *d.characters.entry(self.char_key.clone()).or_default() += 1;
                let pu = d.presets.entry(self.preset.clone()).or_default();
                pu.calls += 1;
                pu.usd += self.est;
            }
            settle_paid(self.app, &self.day, self.est, Some(self.est));
        }
    }
}

/// 예약을 정산 — 예약액을 풀고, 성공이면 실제 비용을 더한다.
/// 실패·취소여도 호출 수는 그대로 센다(요청이 나갔으면 과금됐을 수 있다 — 반복 실패로 상한을 우회하지 못하게)
fn settle_paid(app: &AppRef, day: &str, est: f64, actual: Option<f64>) {
    let mut st = app.store.lock().unwrap();
    let d = st.day_of(day);
    d.reserved_usd = (d.reserved_usd - est).max(0.0);
    if let Some(usd) = actual {
        d.usd += usd;
    }
    drop(st);
    // 실패·취소 정산도 즉시 저장(재시작으로 비용이 사라져 상한을 우회하지 못하게)
    app.save_store();
}

/// `speech_tx`가 있으면 <말>이 도착하는 즉시 보낸다(대답 전체가 끝나기 전에 음성 시작)
#[allow(clippy::too_many_arguments)]
pub async fn answer(
    app: &AppRef, guild: u64, char_key: &str, ch: &Character, caller: &str, question: &str, lines: &[Line],
    speech_tx: Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<Answer> {
    answer_with(app, guild, char_key, ch, caller, question, lines, true, speech_tx).await
}

#[allow(clippy::too_many_arguments)]
async fn answer_with(
    app: &AppRef, guild: u64, char_key: &str, ch: &Character, caller: &str, question: &str, lines: &[Line], use_free_chain: bool,
    speech_tx: Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<Answer> {
    let mut hook = SpeechHook::new(speech_tx);
    let cfg = app.cfg();
    let disabled = app.store.lock().unwrap().state.disabled_presets.clone();
    let mut order: Vec<String> = vec![ch.brain.clone()];
    if let Some(f) = &ch.fallback {
        if !order.contains(f) {
            order.push(f.clone());
        }
    }
    // 그래도 안 되면 무료 체인(무료 한도 소진·장애 대비) — SPEC F-32
    if use_free_chain {
        for f in &cfg.llm.free_chain {
            if !order.contains(f) {
                order.push(f.clone());
            }
        }
    }
    let memories: Vec<String> = {
        let st = app.store.lock().unwrap();
        st.state.guilds.get(&guild).map(|g| g.memories.iter().map(|m| format!("{} ({}, {})", m.text, m.by, m.date)).collect()).unwrap_or_default()
    };
    let system = system_prompt(ch, &memories);
    let q = question_text(caller, ch, question);
    // 프롬프트 크기 상한 — 오래된 줄부터 뺀다. 상한(글자×2+200 ≤ 약 8만 토큰)은 Haiku 5.5의 10만 토큰 초과 단가 구간과
    // 모델 문맥 한도에 닿지 않게 잡았다. 예약·정산은 1구간 단가 하나로 충분해진다
    const MAX_PROMPT_CHARS: usize = 40_000;
    let line_chars = |l: &Line| l.text.chars().count() + l.speaker.chars().count() + 14;
    let fixed = system.chars().count() + q.chars().count();
    let mut start = 0;
    let mut body: usize = lines.iter().map(line_chars).sum();
    while start < lines.len() && fixed + body > MAX_PROMPT_CHARS {
        body -= line_chars(&lines[start]);
        start += 1;
    }
    let lines = &lines[start..];
    let prompt_chars = fixed + body;
    // 입출력 원문 보기용(웹 콘솔) — 실제로 보내는 것과 같은 내용(캐시 블록 경계만 생략)
    let prompt_dump = format!("[system]\n{system}\n\n[대화 기록]\n{}\n[질문]\n{q}", lines.iter().map(fmt_line).collect::<String>());
    let mut errors = vec![];
    let mut skipped_for_cap = false;
    for name in order {
        let Some(p) = cfg.llm.presets.get(&name) else { continue };
        if disabled.contains(&name) {
            errors.push(format!("{name}: 꺼짐"));
            continue;
        }
        if !p.has_key() {
            errors.push(format!("{name}: 키 없음({})", p.key_env()));
            continue;
        }
        // 상한 때문에 앞 프리셋을 건너뛰었으면 대체는 무료만 — "무료로 대답" 안내와 실제가 같게(SPEC F-32)
        if skipped_for_cap && !p.free {
            errors.push(format!("{name}: 상한 뒤라 유료 대체 안 씀"));
            continue;
        }
        let est = if p.free { 0.0 } else { estimate_usd(p, prompt_chars) };
        let day = if p.free {
            None
        } else {
            match reserve_paid(app, est) {
                Some(d) => Some(d),
                None => {
                    skipped_for_cap = true;
                    errors.push(format!("{name}: 오늘 유료 상한"));
                    continue;
                }
            }
        };
        // 사용량 원장(프리셋·캐릭터·토큰)도 예약한 날짜에 — 자정을 넘긴 호출이 두 날로 갈라지지 않게
        let ledger_day = day.clone().unwrap_or_else(crate::store::today);
        let guard = day.map(|day| PaidGuard { app, est, day, preset: name.clone(), char_key: char_key.to_string(), settled: false });
        let t = Instant::now();
        hook.reset();
        let r = match p.provider.as_str() {
            "anthropic" => call_anthropic(app, &name, p, &system, lines, &q, &mut hook).await,
            _ => call_openai(app, &name, p, &system, lines, &q, &mut hook).await,
        };
        match r {
            Ok(raw) => {
                let ms = t.elapsed().as_millis() as u64;
                let mut usd = if p.free { 0.0 } else { price(p, &raw) };
                if !p.free && raw.input + raw.output + raw.cache_read + raw.cache_write == 0 {
                    // 사용량을 안 주는 공급자 — 보이지 않는 추론 토큰까지는 알 수 없으니 예약한 최대 예상액으로 정산
                    usd = est;
                }
                if let Some(g) = guard {
                    g.settle(Some(usd));
                }
                // 사용량 기록 — 대화 내용 없이 숫자만(SPEC F-33)
                {
                    let mut st = app.store.lock().unwrap();
                    let d = st.day_of(&ledger_day);
                    if p.free {
                        d.free_calls += 1;
                    }
                    // 유료 호출 수·금액은 reserve_paid/settle_paid에서 반영했다
                    *d.characters.entry(char_key.to_string()).or_default() += 1;
                    let pu = d.presets.entry(name.clone()).or_default();
                    pu.calls += 1;
                    pu.input_tokens += raw.input + raw.cache_write;
                    pu.output_tokens += raw.output;
                    pu.cache_read_tokens += raw.cache_read;
                    pu.usd += usd;
                }
                app.save_store();
                tracing::info!(
                    "llm guild={guild} char={char_key} preset={name} model={} in={} out={} cache_read={} usd={usd:.5} ms={ms}",
                    p.model, raw.input, raw.output, raw.cache_read
                );
                app.last_used.lock().unwrap().llm = name.clone();
                // 기억 도구 태그를 먼저 떼어 낸다 — 형식 판정·글에는 남기지 않는다
                let (clean, memory) = extract_memory(&raw.text);
                let format_ok = strict_parts(&clean).is_some_and(|(s, t)| !s.is_empty() && !t.is_empty());
                let tokens = [raw.input, raw.output, raw.cache_read, raw.cache_write];
                let (speech, text) = split_answer(&clean);
                return Ok(Answer { speech, text, preset: name, usd, ms, capped: skipped_for_cap, format_ok, tokens, speech_ms: hook.at, memory, prompt: prompt_dump, raw: raw.text });
            }
            Err(e) => {
                let ms = t.elapsed().as_millis() as u64;
                // 스트림이 시작됐으면 입력과 (보이지 않는 추론 포함) 출력이 과금됐을 수 있다 — 얼마인지 알 수 없으니
                // 예약한 최대 예상액(입력 상한 + max_tokens 전부)으로 정산한다. 반복 실패로 상한을 넘지 못하게
                let est_used = (!p.free && hook.started).then_some(est);
                if let Some(g) = guard {
                    g.settle(est_used);
                }
                tracing::warn!("llm {name} 실패: {e}");
                // 스트림이 시작됐다 = 과금됐을 수 있는 호출 — 말이 안 나갔어도 프리셋·캐릭터 원장에 남긴다(총계와 원장이 맞게).
                // 말이 나간 경우는 아래에서 대답으로 기록한다
                // 유료는 시작 전 실패도 호출 수(paid_calls)에 잡히므로 원장에도 같이 남긴다
                if (hook.started || !p.free) && !hook.spoken() {
                    let mut st = app.store.lock().unwrap();
                    let d = st.day_of(&ledger_day);
                    if p.free {
                        d.free_calls += 1;
                    }
                    *d.characters.entry(char_key.to_string()).or_default() += 1;
                    let pu = d.presets.entry(name.clone()).or_default();
                    pu.calls += 1;
                    pu.usd += est_used.unwrap_or(0.0);
                    drop(st);
                    app.save_store();
                }
                if hook.spoken() {
                    // 말은 이미 음성으로 나갔다 — 다른 프리셋으로 다시 대답하면 말과 글이 어긋나므로 받은 데까지로 끝낸다
                    // 거절이면 받은 글은 버리고 이미 나간 말만 남긴다. 그 밖엔 받은 데까지(<think> 조각은 지움)
                    // 끊긴 대답의 기억 도구는 쓰지 않는다(부탁을 끝까지 못 들었을 수 있다)
                    let (_, mut text) = split_answer(&extract_memory(&strip_think(&hook.partial)).0);
                    if text.trim().is_empty() || e.to_string().contains("refusal") {
                        text = hook.speech.clone();
                    }
                    text.push_str("\n\n_(대답이 중간에 끊겼어요)_");
                    let usd = est_used.unwrap_or(0.0);
                    {
                        let mut st = app.store.lock().unwrap();
                        let d = st.day_of(&ledger_day);
                        if p.free {
                            d.free_calls += 1;
                        }
                        *d.characters.entry(char_key.to_string()).or_default() += 1;
                        let pu = d.presets.entry(name.clone()).or_default();
                        pu.calls += 1;
                        pu.usd += usd;
                    }
                    app.save_store();
                    return Ok(Answer {
                        speech: hook.speech.clone(), text, preset: name, usd, ms, capped: skipped_for_cap,
                        format_ok: false, tokens: [0; 4], speech_ms: hook.at, memory: vec![], prompt: prompt_dump, raw: hook.partial.clone(),
                    });
                }
                errors.push(format!("{name}: {e}"));
            }
        }
    }
    if skipped_for_cap {
        bail!("CAP: {}", errors.join(" / "));
    }
    bail!("모든 두뇌 실패: {}", errors.join(" / "))
}

/// 프리셋 하나만 실제로 불러 보는 시험(웹 콘솔 '두뇌 시험') — 대체 없이, 유료면 하루 상한에 똑같이 잡힌다.
/// fill이면 가짜 대화 기록을 길게 붙여 프롬프트 캐시가 걸리는지도 본다(같은 시험을 두 번 하면 두 번째에 cache_read).
pub async fn probe(app: &AppRef, preset: &str, question: &str, fill: bool) -> Result<Answer> {
    let cfg = app.cfg();
    let Some((key, base)) = cfg.characters.iter().next() else { bail!("캐릭터 설정이 없음") };
    let mut ch = base.clone();
    ch.brain = preset.to_string();
    ch.fallback = None;
    let lines: Vec<Line> = if fill {
        let now = Instant::now();
        (0..160u64)
            .map(|i| Line {
                seq: i,
                time: format!("21:{:02}:{:02}", i / 60, i % 60),
                at: now,
                user: 1 + i % 3,
                speaker: ["철수", "영희", "민수"][(i % 3) as usize].to_string(),
                text: format!("이건 캐시 시험용 가짜 대화 {i}번째 줄이야. 오늘 저녁에 같이 게임할 사람 있나 물어보는 중이고 맵은 아무거나 괜찮아."),
            })
            .collect()
    } else {
        vec![]
    };
    answer_with(app, 0, &format!("시험:{key}"), &ch, "주인", question, &lines, false, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_cap_keeps_under_haiku_tier() {
        // 상한 글자 수의 보수 토큰 추정이 10만 토큰(가격 단계 경계) 아래
        assert!(tokens_upper(40_000) < 100_000.0);
    }

    #[test]
    fn cache_read_price_configurable() {
        let p: Preset = toml::from_str("provider = \"openai\"\nmodel = \"gpt-6.1-sol\"\nprice_in = 2.0\nprice_out = 10.0\nprice_cache_read = 0.10").unwrap();
        let usd = price(&p, &Raw { text: String::new(), input: 0, output: 0, cache_read: 1_000_000, cache_write: 0 });
        assert!((usd - 0.10).abs() < 1e-9);
    }

    #[test]
    fn sse_events_split_across_chunks_and_crlf() {
        let mut buf = vec![];
        // 한글 한 글자(3바이트)가 조각 경계에서 잘려도 이벤트가 완성될 때만 꺼낸다
        let all = "event: a\r\ndata: {\"t\":\"안녕\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        let (x, y) = all.split_at(22);
        assert!(take_events(&mut buf, x).is_empty());
        let evs = take_events(&mut buf, y);
        assert_eq!(evs.len(), 2);
        assert_eq!(String::from_utf8(evs[0].clone()).unwrap(), "event: a\ndata: {\"t\":\"안녕\"}\n\n");
        assert!(String::from_utf8_lossy(&evs[1]).contains("[DONE]"));
        assert!(buf.is_empty());
    }

    #[test]
    fn speech_hook_fires_once_on_closed_tag() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut h = SpeechHook::new(Some(tx));
        h.feed("<말>안녕");
        assert!(rx.try_recv().is_err());
        h.feed("<말>안녕! 반가워.</말>\n");
        assert!(rx.try_recv().is_err()); // <글>이 시작돼야 보낸다
        h.feed("<말>안녕! 반가워.</말>\n<글>길");
        assert_eq!(rx.try_recv().unwrap(), "안녕! 반가워.");
        assert!(h.at.is_some());
        h.feed("<말>다른 말</말>"); // 두 번 보내지 않는다
        // 형식이 다르면 보내지 않는다
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        let mut h2 = SpeechHook::new(Some(tx2));
        h2.feed("그냥 답 <말>x</말>");
        assert!(rx2.try_recv().is_err() && h2.at.is_none());
        h2.feed("<말>x</말> 사이 글자 <글>y");
        assert!(rx2.try_recv().is_err() && h2.at.is_none());
    }

    #[test]
    fn estimate_covers_cache_write() {
        for prov in ["anthropic", "openai", "openai-compatible"] {
            let p: Preset = toml::from_str(&format!("provider = \"{prov}\"\nmodel = \"m\"\nprice_in = 0.1\nprice_out = 0.5\nmax_tokens = 100")).unwrap();
            // 입력 1,000토큰 전부 캐시 쓰기 + 출력 100토큰(최대)의 실제 정산액
            let worst = price(&p, &Raw { text: String::new(), input: 0, output: 100, cache_read: 0, cache_write: 1000 });
            assert!(estimate_usd(&p, 1000) >= worst - 1e-12, "{prov}");
        }
    }

    #[test]
    fn memory_tags() {
        let (clean, ops) = extract_memory("<말>응 기억할게!</말><글>알았어, 기억해 둘게.</글>\n<기억>마참은 민트초코를 싫어함</기억>");
        assert_eq!(clean, "<말>응 기억할게!</말><글>알았어, 기억해 둘게.</글>");
        assert_eq!(ops, vec![MemOp::Remember("마참은 민트초코를 싫어함".into())]);
        assert!(strict_parts(&clean).is_some());
        let (_, ops) = extract_memory("<말>응</말><글>응</글><잊기>민트초코</잊기><기억>a<b</기억><기억>   </기억>");
        assert_eq!(ops, vec![MemOp::Forget("민트초코".into())]);
        // 닫히지 않은 태그는 끝까지 지운다, 도구 호출은 3개까지
        let (clean, ops) = extract_memory("<말>x</말><글>y</글><기억>1</기억><기억>2</기억><기억>3</기억><기억>4</기억><기억>끝없음");
        assert_eq!(clean, "<말>x</말><글>y</글>");
        assert_eq!(ops.len(), 3);
    }

    #[test]
    fn think_tags_removed() {
        assert_eq!(strip_think("<think>음…</think>\n<말>응</말>"), "<말>응</말>");
        assert_eq!(strip_think("답<think>끝없음"), "답");
        assert_eq!(strip_think("그냥 답"), "그냥 답");
    }

    #[test]
    fn split_tags() {
        let (s, t) = split_answer("<말>안녕! 반가워.</말>\n<글>**안녕**하세요\n- 길게</글>");
        assert_eq!(s, "안녕! 반가워.");
        assert!(t.contains("길게"));
    }

    #[test]
    fn split_fallback() {
        let (s, t) = split_answer("첫 문장이야. 두 번째 문장은 아주 길게 이어진다고 가정하자, 그래서 여든 자를 넘기면 잘린다는 걸 확인해야 하니까 계속 쓴다. 그리고 한 문장 더 붙여서 확실히 여든 자를 넘긴다.");
        // 80자 안쪽에서 문장 단위로 끊는다
        assert!(s.starts_with("첫 문장이야.") && s.chars().count() <= 80);
        assert!(t.len() > s.len());
    }

    #[test]
    fn split_missing_close() {
        let (s, t) = split_answer("<말>짧게</말><글>닫는 태그 없음");
        assert_eq!(s, "짧게");
        assert_eq!(t, "닫는 태그 없음");
    }

    #[test]
    fn speech_missing_close_not_trusted() {
        // <말>이 안 닫히면 <글>까지 음성으로 읽지 않는다
        let (s, t) = split_answer("<말>짧게 <글>아주 긴 글이 여기에 있다</글>");
        assert!(!s.contains("<글>"));
        // 형식 오류라 글은 원문 전체(태그만 지움)
        assert!(t.contains("아주 긴 글이 여기에 있다") && !t.contains('<'));
    }

    #[test]
    fn extra_text_outside_tags_is_kept() {
        let (s, t) = split_answer("앞말 <말>짧게.</말><글>본문</글> 뒷말");
        assert_eq!(s, "짧게.");
        assert!(t.contains("앞말") && t.contains("본문") && t.contains("뒷말"));
    }

    #[test]
    fn reversed_tags_fallback() {
        let (_, t) = split_answer("<글>본문</글><말>짧게</말>");
        assert!(t.contains("본문") && t.contains("짧게"));
    }

    #[test]
    fn speech_only_keeps_whole_text() {
        let (s, t) = split_answer("<말>짧게 말해.</말> 그리고 태그 밖에 남은 긴 설명이 있다.");
        assert_eq!(s, "짧게 말해.");
        assert!(t.contains("긴 설명"));
    }

    #[test]
    fn long_unpunctuated_cut() {
        let long: String = std::iter::repeat("가나다라마바사 ").take(30).collect();
        let (s, _) = split_answer(&long);
        assert!(s.chars().count() <= 80);
    }
}
