//! 대화 버퍼(메모리에만, 최근 N분 — SPEC N-06)와 호출어 검출(SPEC F-20, F-21).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use chrono::{FixedOffset, Utc};
use regex::Regex;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Line {
    /// 버퍼에 들어온 순번(캐시 블록 경계용, 비워도 계속 증가)
    pub seq: u64,
    pub time: String,
    #[serde(skip)]
    pub at: Instant,
    /// 사람이면 유저 ID, 캐릭터면 0
    pub user: u64,
    pub speaker: String,
    pub text: String,
}

/// 받아쓰기 기록 한 건(웹 콘솔 "받아쓰기 상황") — 글 내용은 담지 않는다(글은 대화 버퍼에만).
/// 버퍼와 같이 지워진다(본인 거부·무시·잊어줘·기억 시간)
#[derive(Debug, Clone, serde::Serialize)]
pub struct SttEvent {
    pub time: String,
    #[serde(skip)]
    pub at: Instant,
    #[serde(skip)]
    pub user: u64,
    pub speaker: String,
    /// ok(받아씀) · noise(짧거나 작아 버림) · filtered(환각·빈 결과 거름) · failed(받아쓰기 실패)
    pub kind: &'static str,
    /// 말한 길이
    pub audio_ms: u64,
    /// 받아쓰기에 걸린 시간(noise는 0)
    pub stt_ms: u64,
    pub provider: String,
    pub chars: usize,
    /// 이 발화로 부른 캐릭터(질문 대기 포함)
    pub wake: Option<String>,
}

/// 받아쓰기 기록은 최근 이만큼만
const MAX_EVENTS: usize = 200;

/// 두뇌 입출력 원문 한 건(웹 콘솔 F-45) — 대화가 들어 있으므로 버퍼와 똑같이 지운다
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmTrace {
    pub time: String,
    #[serde(skip)]
    pub at: Instant,
    /// 프롬프트에 들어간 사람들 + 부른 사람(본인 거부·무시 때 이 건을 통째로 지운다)
    #[serde(skip)]
    pub users: Vec<u64>,
    pub character: String,
    pub caller: String,
    pub preset: String,
    pub ms: u64,
    pub usd: f64,
    pub tokens: [u64; 4],
    pub prompt: String,
    pub raw: String,
}

/// 입출력 원문은 최근 이만큼만(프롬프트가 길어 메모리를 아낀다)
const MAX_TRACES: usize = 20;

#[derive(Default)]
pub struct Buffer {
    lines: VecDeque<Line>,
    next_seq: u64,
    events: VecDeque<SttEvent>,
    traces: VecDeque<LlmTrace>,
}

pub fn now_kst() -> String {
    let kst = FixedOffset::east_opt(9 * 3600).unwrap();
    Utc::now().with_timezone(&kst).format("%H:%M:%S").to_string()
}

impl Buffer {
    pub fn push(&mut self, user: u64, speaker: &str, text: &str) {
        self.lines.push_back(Line {
            seq: self.next_seq,
            time: now_kst(),
            at: Instant::now(),
            user,
            speaker: speaker.to_string(),
            text: text.to_string(),
        });
        self.next_seq += 1;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_stt(&mut self, user: u64, speaker: &str, kind: &'static str, audio_ms: u64, stt_ms: u64, provider: &str, chars: usize, wake: Option<String>) {
        self.events.push_back(SttEvent {
            time: now_kst(),
            at: Instant::now(),
            user,
            speaker: speaker.to_string(),
            kind,
            audio_ms,
            stt_ms,
            provider: provider.to_string(),
            chars,
            wake,
        });
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    pub fn log_llm(&mut self, t: LlmTrace) {
        self.traces.push_back(t);
        while self.traces.len() > MAX_TRACES {
            self.traces.pop_front();
        }
    }

    /// 최근 입출력 원문(최신이 앞)
    pub fn traces(&mut self, keep: Duration) -> Vec<LlmTrace> {
        self.prune(keep);
        self.traces.iter().rev().cloned().collect()
    }

    /// 최근 받아쓰기 기록(최신이 앞)
    pub fn events(&mut self, keep: Duration) -> Vec<SttEvent> {
        self.prune(keep);
        self.events.iter().rev().cloned().collect()
    }

    pub fn prune(&mut self, keep: Duration) {
        while self.lines.front().is_some_and(|l| l.at.elapsed() > keep) {
            self.lines.pop_front();
        }
        while self.events.front().is_some_and(|e| e.at.elapsed() > keep) {
            self.events.pop_front();
        }
        while self.traces.front().is_some_and(|e| e.at.elapsed() > keep) {
            self.traces.pop_front();
        }
    }

    pub fn snapshot(&mut self, keep: Duration) -> Vec<Line> {
        self.prune(keep);
        self.lines.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.events.clear();
        self.traces.clear();
    }

    /// 본인 거부 즉시 그 사람 줄을 지운다 — SPEC F-11
    pub fn remove_user(&mut self, user: u64) {
        self.lines.retain(|l| l.user != user);
        self.events.retain(|e| e.user != user);
        self.traces.retain(|t| !t.users.contains(&user));
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

pub struct WakeHit {
    pub character: String,
    /// 호출어 뒤에 붙은 말(같은 발화) — SPEC F-21
    pub rest: String,
    /// 판정 근거 — 이름 / 헤이 / 비슷한 말 / 위험 별칭(받아쓰기 상황·지연 기록에 보여 준다)
    pub via: &'static str,
}

pub struct Wake {
    /// (캐릭터 키, 이름만 불러도 되는 패턴, 헤이를 붙여야 하는 패턴)
    pats: Vec<(String, Regex, Regex)>,
    /// (캐릭터 키, 대표 이름의 자모) — 헤이 뒤 비슷한 말 인정용("헤이 야쩡" → 아짱)
    canon: Vec<(String, Vec<Jamo>)>,
    /// 헤이(접두어) + 바로 붙은 한글 두 글자
    hey_re: Option<Regex>,
    /// 위험 별칭(실장·약정처럼 평소에도 쓰는 말로 받아써진 이름) — 헤이·부르는 꼴·맨 앞 질문일 때만 인정
    risky: Vec<(String, Regex)>,
}

/// 받아쓰기가 "헤이"를 적는 모양 — 헤이·해이·에이·Hey
const HEY: &str = r"헤이|해이|에이|(?i:hey)";

/// 헤이 뒤 비슷한 말을 인정하는 자모 거리 상한. "에이"는 평소 감탄사로도 쓰여 더 엄격하게
const FUZZY_MAX: f32 = 2.0;
const FUZZY_MAX_EI: f32 = 1.5;

/// 한글 음절 하나 = (초성, 중성, 종성) 번호. 종성 0 = 없음
type Jamo = (u32, u32, u32);

fn jamo(s: &str) -> Option<Vec<Jamo>> {
    s.chars()
        .map(|c| {
            let u = c as u32;
            (0xAC00..=0xD7A3).contains(&u).then(|| {
                let i = u - 0xAC00;
                (i / 588, (i % 588) / 28, i % 28)
            })
        })
        .collect()
}

/// 초성: 평음↔된소리·거센소리(ㄱㄲㅋ, ㄷㄸㅌ, ㅂㅃㅍ, ㅅㅆ, ㅈㅉㅊ)는 받아쓰기가 자주 헷갈려 0.5
fn cost_l(a: u32, b: u32) -> f32 {
    const GROUPS: [&[u32]; 5] = [&[0, 1, 15], &[3, 4, 16], &[7, 8, 17], &[9, 10], &[12, 13, 14]];
    if a == b {
        0.0
    } else if GROUPS.iter().any(|g| g.contains(&a) && g.contains(&b)) {
        0.5
    } else {
        1.0
    }
}

/// 중성: ㅏ↔ㅑ, ㅓ↔ㅕ, ㅗ↔ㅛ, ㅜ↔ㅠ, ㅐ↔ㅔ(받아쓰기가 자주 섞음)는 0.5
fn cost_v(a: u32, b: u32) -> f32 {
    const PAIRS: [(u32, u32); 5] = [(0, 2), (4, 6), (8, 12), (13, 17), (1, 5)];
    if a == b {
        0.0
    } else if PAIRS.iter().any(|&(x, y)| (a, b) == (x, y) || (a, b) == (y, x)) {
        0.5
    } else {
        1.0
    }
}

/// 같은 글자 수의 두 낱말 자모 거리(음절별 초성·중성·종성 차이 합)
fn jamo_dist(a: &[Jamo], b: &[Jamo]) -> Option<f32> {
    (a.len() == b.len()).then(|| a.iter().zip(b).map(|(x, y)| cost_l(x.0, y.0) + cost_v(x.1, y.1) + if x.2 == y.2 { 0.0 } else { 1.0 }).sum())
}

/// 이름 앞에 있어도 "부르는 말"로 보는 말(그 밖의 말이 앞에 있으면 문장 속 언급으로 본다)
const FILLERS: [&str; 21] = [
    "야", "어", "아", "저기", "그럼", "근데", "자", "음", "오", "얘", "있잖아", "잠깐", "잠깐만", "혹시", "아니", "헐", "저기요", "야야", "와", "오오", "그래서",
];
/// 이름에 바로 붙으면 부름이 아니라 이야기 속 언급("아짱도 아니고", "클장이 긁어요?") — 2026-10-15 실사용 오호출
const PARTICLES: [&str; 30] = [
    "이랑", "이가", "이는", "이도", "이를", "이한테", "이나", "하고", "한테", "에게", "처럼", "보다", "까지", "부터", "으로", "께서", "도", "는", "은", "가",
    "를", "랑", "이", "의", "에", "께", "로", "만", "과", "님",
];

/// 문장 속 이름이 "부른 것"인가 — 맨 앞(부르는 말만 앞에 있음), '아/야' 부르는 꼴, 문장 끝 부름만 인정
fn is_called(text: &str, start: usize, end: usize) -> bool {
    let after = &text[end..];
    for voc in ["아", "야"] {
        if let Some(r) = after.strip_prefix(voc) {
            if r.chars().next().is_none_or(|c| !c.is_alphanumeric()) {
                return true;
            }
        }
    }
    let not_word = |c: char| !c.is_alphanumeric();
    let pre = text[..start].trim_matches(not_word);
    if PARTICLES.iter().any(|p| after.starts_with(p)) {
        // 맨 앞에서 조사를 붙여 묻는 말("클짱은 뭐 좋아해?")은 부른 것 — 물음표로 끝날 때만(받아쓰기가 의문문에 붙인다)
        // "클장이 긁어요?"(언급)와 구별하려고 은·는·도만
        return pre.is_empty() && ["은", "는", "도"].iter().any(|p| after.starts_with(p)) && text.trim_end().ends_with('?');
    }
    if pre.is_empty() || pre.split_whitespace().all(|w| FILLERS.contains(&w.trim_matches(not_word))) {
        return true;
    }
    // 문장 끝에서 부르기 — "이거 어때 클짱?" / "고마워 클짱." (뒤에 말이 더 이어지면 "오늘 아짱, 내일 보자"처럼 언급)
    let a = after.trim_start();
    if a.is_empty() || a.starts_with(['?', '!']) {
        return true;
    }
    a.starts_with([',', '.', '~']) && !a.chars().any(char::is_alphanumeric)
}

/// 호출어 뒤 질문 부분 — 부르는 말 '아/야'와 앞 문장부호를 뗀다
fn rest_after(text: &str, end: usize) -> String {
    let mut rest = &text[end..];
    // 부르는 말 "클짱아," "아짱야" — 이름 바로 뒤 '아/야' 한 글자만 뗀다("이거"의 '이'는 건드리지 않음)
    for voc in ["아", "야"] {
        if let Some(r) = rest.strip_prefix(voc) {
            if r.chars().next().is_none_or(|c| !c.is_alphanumeric()) {
                rest = r;
            }
            break;
        }
    }
    rest.trim_start_matches(|c: char| matches!(c, ',' | '!' | '?' | '~' | '.' | ':' | ';' | '"' | '\'' | '“' | '”' | '…') || c.is_whitespace())
        .trim()
        .to_string()
}

impl Wake {
    /// 호출어 목록으로 정규식을 만든다. 앞이 글자의 시작이어야 한다("사아짱" 제외). 붙여 쓴 것만 — "아 장난"은 안 걸린다.
    /// "헤이 클짱"은 띄어 쓰면 원래 걸리고, 받아쓰기가 "헤이클짱"으로 붙여 써도 걸리게 헤이 접두어를 허용한다
    /// 설정 검증용 — 실제로 쓰는 두 패턴(이름만·헤이만)을 모두 만들어 본다
    pub fn compile(words: &[String]) -> Result<(), regex::Error> {
        Self::build(words, false)?;
        Self::build(words, true)?;
        Ok(())
    }

    /// hey=true면 이름 앞에 헤이가 꼭 있어야 한다(`/설정 호출방식 헤이만`) — "헤이 클짱"·"헤이클짱"·"헤이, 클짱"·"헤이… 클짱".
    /// 두 모드 모두 헤이(없으면 이름) 앞은 글자의 시작이어야 한다 — "플랜에이클짱"은 안 걸린다
    fn build(words: &[String], hey: bool) -> Result<Regex, regex::Error> {
        // 띄어쓰기가 있는 호출어("오케이 클짱")는 받아쓰기 띄어쓰기와 상관없이 — 공백 자리를 \s*로
        let alt = words.iter().map(|w| regex::escape(w).replace(' ', r"\s*")).collect::<Vec<_>>().join("|");
        let opt = if hey { "" } else { "?" };
        // 1번 묶음 = 헤이, 2번 묶음 = 이름
        let pat = format!(r"(?:^|[^\p{{L}}\p{{N}}])(?:({HEY})[^\p{{L}}\p{{N}}]*){opt}({alt})");
        regex::RegexBuilder::new(&pat).size_limit(1 << 20).build()
    }

    /// characters: (캐릭터 키, 호출어 목록 — 첫 번째가 대표 이름). 설정 검증에서 이미 걸렀지만, 실패한 캐릭터는 패닉 없이 건너뛴다
    pub fn new(characters: &[(String, Vec<String>)]) -> Self {
        let pats = characters
            .iter()
            .filter_map(|(k, words)| match (Self::build(words, false), Self::build(words, true)) {
                (Ok(a), Ok(b)) => Some((k.clone(), a, b)),
                (Err(e), _) | (_, Err(e)) => {
                    tracing::error!("캐릭터 {k} 호출어 정규식 실패: {e}");
                    None
                }
            })
            .collect();
        // 대표 이름이 한글 두 글자일 때만 비슷한 말을 본다(그 밖은 정확한 사전만)
        let canon = characters
            .iter()
            .filter_map(|(k, words)| words.first().and_then(|w| jamo(w)).filter(|j| j.len() == 2).map(|j| (k.clone(), j)))
            .collect();
        let hey_re = Regex::new(&format!(r"(?:^|[^\p{{L}}\p{{N}}])({HEY})[^\p{{L}}\p{{N}}]*(\p{{Hangul}}{{2}})")).ok();
        Self { pats, canon, hey_re, risky: vec![] }
    }

    /// 위험 별칭을 붙인다(캐릭터 키, 낱말들)
    pub fn with_risky(mut self, risky: &[(String, Vec<String>)]) -> Self {
        self.risky = risky
            .iter()
            .filter(|(_, w)| !w.is_empty())
            .filter_map(|(k, w)| Self::build(w, false).ok().map(|re| (k.clone(), re)))
            .collect();
        self
    }

    /// 헤이 바로 뒤 한글 두 글자가 대표 이름과 비슷하면 그 캐릭터 — (시작, 끝, 키)
    fn fuzzy(&self, text: &str) -> Option<(usize, usize, &String)> {
        let re = self.hey_re.as_ref()?;
        for c in re.captures_iter(text) {
            let (Some(all), Some(hey), Some(word)) = (c.get(0), c.get(1), c.get(2)) else { continue };
            // 두 글자 뒤가 낱말 끝이거나 부르는 말('아/야/이요')일 때만 — "헤이 아마도"·"헤이 클라우드"는 이름이 아니다
            let tail = &text[word.end()..];
            // '이'는 "클짱이요·클짱이야"처럼 바로 끝날 때만 — "헤이 앞장이라고"는 긴 낱말
            // 부르는 말('아/야/이요/이야/이') 뒤도 낱말 끝이어야 — "헤이 야쩡야구"는 긴 낱말
            let ends = |r: &str| r.chars().next().is_none_or(|c| !c.is_alphanumeric());
            let ok_tail = ends(tail) || ["아", "야", "이요", "이야", "이"].iter().any(|p| tail.strip_prefix(p).is_some_and(ends));
            if !ok_tail {
                continue;
            }
            let Some(w) = jamo(word.as_str()) else { continue };
            let max = if hey.as_str() == "에이" { FUZZY_MAX_EI } else { FUZZY_MAX };
            let mut scored: Vec<(f32, &String)> = self.canon.iter().filter_map(|(k, j)| jamo_dist(&w, j).map(|d| (d, k))).collect();
            scored.sort_by(|a, b| a.0.total_cmp(&b.0));
            // 가장 가까운 하나가 상한 안이고, 둘째와 확실히 차이 날 때만
            if let Some(&(d, k)) = scored.first() {
                if d <= max && scored.get(1).is_none_or(|s| s.0 > d) {
                    return Some((all.start(), word.end(), k));
                }
            }
        }
        None
    }

    /// 가장 앞에 나온 호출어를 고른다. hey_only면 헤이를 붙여 부른 것만.
    /// 사전(정확한 호출어)에 없으면 헤이 바로 뒤의 비슷한 말("헤이 야쩡", "에이클잖")도 본다
    pub fn find(&self, text: &str, hey_only: bool) -> Option<WakeHit> {
        let mut best: Option<(usize, usize, &String, &'static str)> = None;
        for (k, plain, hey) in &self.pats {
            let re = if hey_only { hey } else { plain };
            // 헤이를 붙였으면 부른 것. 아니면 이름이 부르는 자리에 있을 때만(이야기 속 언급은 건너뛰고 다음 것을 본다)
            let hit = re.captures_iter(text).find_map(|c| {
                let (all, name) = (c.get(0)?, c.get(2)?);
                // "에이"는 감탄사로도 흔하다 — "에이 클짱도 그건 모르지"처럼 이름에 조사가 붙으면 언급으로 본다
                let hey = c.get(1).is_some_and(|h| h.as_str() != "에이" || !PARTICLES.iter().any(|p| text[name.end()..].starts_with(p)));
                if hey_only || hey {
                    Some((all.start(), name.end(), "헤이"))
                } else {
                    is_called(text, name.start(), name.end()).then(|| (all.start(), name.end(), "이름"))
                }
            });
            if let Some((s, e, via)) = hit {
                if best.is_none_or(|b| s < b.0) {
                    best = Some((s, e, k, via));
                }
            }
        }
        if let Some((s, e, k)) = self.fuzzy(text) {
            if best.is_none_or(|b| s < b.0) {
                best = Some((s, e, k, "비슷한 말"));
            }
        }
        // 위험 별칭 — 헤이를 붙였거나, 바로 '아/야'로 부르거나, 맨 앞에서 너에게 묻는 말("실장 나에 대해서 어떻게 생각해?")일 때만
        for (k, re) in &self.risky {
            let hit = re.captures_iter(text).find_map(|c| {
                let (all, name) = (c.get(0)?, c.get(2)?);
                let after = &text[name.end()..];
                let hey = c.get(1).is_some_and(|h| h.as_str() != "에이") && !PARTICLES.iter().any(|p| after.starts_with(p));
                let voc = ["아", "야"].iter().any(|v| after.strip_prefix(v).is_some_and(|r| r.chars().next().is_none_or(|ch| !ch.is_alphanumeric())));
                let pre_empty = text[..name.start()].trim_matches(|ch: char| !ch.is_alphanumeric()).is_empty();
                // 너에게 묻는 말 — 낱말 단위로(“너무”는 아님)
                const YOU: [&str; 10] = ["너", "니가", "넌", "네가", "너는", "너도", "너가", "나랑", "나한테", "너한테"];
                let asks = text.trim_end().ends_with('?') || after.split_whitespace().any(|w| YOU.contains(&w.trim_matches(|ch: char| !ch.is_alphanumeric())));
                let particle = PARTICLES.iter().any(|p| after.starts_with(p));
                // 헤이만 모드면 진짜 헤이(에이 제외)가 있어야
                let ok = if hey_only { hey } else { hey || voc || (pre_empty && asks && !particle) };
                ok.then(|| (all.start(), name.end()))
            });
            if let Some((s, e)) = hit {
                if best.is_none_or(|b| s < b.0) {
                    best = Some((s, e, k, "위험 별칭"));
                }
            }
        }
        let (_, end, k, via) = best?;
        Some(WakeHit { character: k.clone(), rest: rest_after(text, end), via })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Wake {
        fn find0(&self, t: &str) -> Option<WakeHit> {
            self.find(t, false)
        }
    }

    fn w() -> Wake {
        Wake::new(&[
            ("클짱".into(), vec!["클짱".into(), "클장".into(), "크짱".into(), "크장".into()]),
            ("아짱".into(), vec!["아짱".into(), "아장".into(), "아쨩".into()]),
        ])
    }

    #[test]
    fn basic() {
        let h = w().find0("클짱 오늘 저녁 뭐 먹지?").unwrap();
        assert_eq!(h.character, "클짱");
        assert_eq!(h.rest, "오늘 저녁 뭐 먹지?");
    }

    #[test]
    fn vocative_and_variant() {
        let h = w().find0("야 크장아, 이거 뭐야").unwrap();
        assert_eq!(h.character, "클짱");
        assert_eq!(h.rest, "이거 뭐야");
    }

    #[test]
    fn earliest_wins() {
        let h = w().find0("클짱, 아짱이 그러는데 맞아?").unwrap();
        assert_eq!(h.character, "클짱");
    }

    #[test]
    fn mention_is_not_call() {
        // 2026-10-15 실사용에서 봇 얘기를 하다가 끼어든 문장들
        for t in ["아짱도 아니고 글짱도 아니잖아", "여기 아장도 있고 아짱도 있고", "클장이 긁어요?", "아짱이랑 클짱 중에 누가 똑똑해", "근데 오늘 클짱한테 물어봤는데"] {
            assert!(w().find0(t).is_none(), "{t}");
        }
        // 부르는 자리면 걸린다
        for t in ["에이 클짱도 그건 모르지", "헤이 아마도", "헤이 클라우드 써 봐", "오늘 아짱, 내일 보자", "그건 클짱의 의견이고", "헤이 앞장이라고 했어", "헤이 야쩡야구"] {
            assert!(w().find0(t).is_none(), "{t}");
        }
        for (t, c) in [("야 클짱 오늘 뭐 해", "클짱"), ("이거 어때 클짱?", "클짱"), ("그거 말이야 아짱아 어떻게 생각해", "아짱"), ("근데 클짱 이거 알아?", "클짱"), ("헤이 아짱 해야죠", "아짱"), ("클짱은 뭐 좋아해?", "클짱"), ("아니 클짱 그게 아니라", "클짱"), ("헐 아짱 이거 봐", "아짱")] {
            assert_eq!(w().find0(t).map(|h| h.character), Some(c.to_string()), "{t}");
        }
    }

    #[test]
    fn no_false_hits() {
        assert!(w().find0("아 장난이야").is_none());
        assert!(w().find0("사아짱 아님").is_none());
        assert!(w().find0("오늘 날씨 좋다").is_none());
    }

    #[test]
    fn hey_prefix() {
        for (t, c, rest) in [("헤이 클짱 몇 시야", "클짱", "몇 시야"), ("헤이클짱, 몇 시야", "클짱", "몇 시야"), ("에이아짱 뭐해", "아짱", "뭐해"), ("Hey 아짱", "아짱", "")] {
            let h = w().find0(t).unwrap();
            assert_eq!((h.character.as_str(), h.rest.as_str()), (c, rest), "{t}");
        }
        for t in ["케이아짱", "플랜에이클짱", "사에이아짱"] {
            assert!(w().find0(t).is_none(), "{t}");
        }
    }

    #[test]
    fn hey_fuzzy_real_transcripts() {
        // 2026-10-15 실제 받아쓰기에서 "헤이 클짱/아짱"이 이렇게 적혔다
        let wk = w();
        for (t, c) in [("뭐야 에이클잖아 뭐해", "클짱"), ("해이클잖이요", "클짱"), ("헤이 클짝", "클짱"), ("헤이 야쩡", "아짱"), ("헤이 야정", "아짱"), ("헤이 앞장", "아짱"), ("에이, 클짱.", "클짱")] {
            for hey_only in [false, true] {
                let h = wk.find(t, hey_only).unwrap_or_else(|| panic!("{t} hey_only={hey_only}"));
                assert_eq!(h.character, c, "{t}");
            }
        }
        let h = wk.find("헤이 야쩡 오늘 날씨 어때", true).unwrap();
        assert_eq!(h.rest, "오늘 날씨 어때");
        // 헤이가 없거나 거리가 먼 말은 안 걸린다
        for t in ["야쩡", "앞장 서", "헤이 아니야", "에이 진짜", "에이 장난", "헤이 요샤우", "hey y'all ciao!", "헤이 야 정말", "에이 아까 그거"] {
            assert!(wk.find(t, false).is_none(), "{t}");
        }
    }

    #[test]
    fn jamo_distance() {
        let d = |a: &str, b: &str| jamo_dist(&jamo(a).unwrap(), &jamo(b).unwrap()).unwrap();
        assert_eq!(d("아짱", "아짱"), 0.0);
        assert_eq!(d("야쩡", "아짱"), 1.5);
        assert_eq!(d("클잖", "클짱"), 1.5);
        assert_eq!(d("클짝", "클짱"), 1.0);
        assert!(jamo("abc").is_none());
    }

    #[test]
    fn hey_only_mode() {
        let wk = w();
        for (t, c, rest) in [("헤이 클짱 몇 시야", "클짱", "몇 시야"), ("헤이클짱, 몇 시야", "클짱", "몇 시야"), ("해이, 아짱아 뭐해", "아짱", "뭐해"), ("야 HEY 크장", "클짱", ""), ("헤이… 클짱 뭐해", "클짱", "뭐해"), ("헤이? 아짱", "아짱", ""), ("헤이-클짱", "클짱", ""), ("헤이: 클짱", "클짱", ""), ("헤이 \"클짱\" 뭐해", "클짱", "뭐해")] {
            let h = wk.find(t, true).unwrap();
            assert_eq!((h.character.as_str(), h.rest.as_str()), (c, rest), "{t}");
        }
        for t in ["클짱 몇 시야", "아짱이랑 클짱", "오케이클짱", "헤이 오늘 클짱 봤어", "플랜에이클짱"] {
            assert!(wk.find(t, true).is_none(), "{t}");
        }
        // 이름만 부르는 기본 모드에선 그대로 걸린다
        assert!(wk.find("클짱 몇 시야", false).is_some());
    }

    #[test]
    fn risky_aliases() {
        let wk = w().with_risky(&[("클짱".into(), vec!["실장".into(), "약정".into()])]);
        for t in ["실장 나에 대해서 어떻게 생각해?", "헤이 실장 뭐해", "실장아 이거 봐", "실장 너 롤 잘해"] {
            let h = wk.find(t, false).unwrap_or_else(|| panic!("{t}"));
            assert_eq!((h.character.as_str(), h.via), ("클짱", "위험 별칭"), "{t}");
        }
        for t in ["실장 회의 들어가셨대", "우리 실장님이 그러는데", "약정 기간 끝났어", "실장이 뭐래?", "실장 너무 바쁘대", "약정 너무 비싸"] {
            assert!(wk.find(t, false).is_none(), "{t}");
        }
        // 헤이만 모드: 진짜 헤이만(에이·부르는 꼴만으로는 안 됨)
        assert!(wk.find("헤이 실장 뭐해", true).is_some());
        assert!(wk.find("에이 실장아 뭐해", true).is_none());
        assert!(wk.find("실장아 뭐해", true).is_none());
    }

    #[test]
    fn custom_words() {
        let wk = Wake::new(&[("클짱".into(), vec!["클짱".into(), "클로드".into(), "오케이 클짱".into()]), ("아짱".into(), vec!["아짱".into()])]);
        for t in ["클로드 오늘 뭐 해", "오케이 클짱 날씨 알려줘", "오케이클짱, 날씨"] {
            assert_eq!(wk.find(t, false).map(|h| h.character), Some("클짱".to_string()), "{t}");
        }
        assert!(wk.find("클로드가 그러는데", false).is_none());
    }

    #[test]
    fn only_name() {
        let h = w().find0("클짱!").unwrap();
        assert_eq!(h.rest, "");
    }

    #[test]
    fn buffer_remove_user() {
        let mut b = Buffer::default();
        b.push(1, "a", "x");
        b.push(2, "b", "y");
        b.log_stt(1, "a", "ok", 900, 300, "local", 1, None);
        b.log_stt(2, "b", "noise", 100, 0, "", 0, None);
        b.remove_user(1);
        assert_eq!(b.len(), 1);
        let ev = b.events(Duration::from_secs(60));
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].speaker, "b");
        b.clear();
        assert!(b.events(Duration::from_secs(60)).is_empty());
    }
}
