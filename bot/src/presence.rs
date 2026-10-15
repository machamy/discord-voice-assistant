//! 봇 상태 메시지 — 듣는 중 상시 표시(SPEC F-12), 사이드카가 죽으면 "잠깐 못 들어요"(SPEC N-09).

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use serenity::all::ActivityData;

use crate::app::AppRef;

static LAST: Mutex<String> = Mutex::new(String::new());

pub async fn refresh(app: &AppRef) {
    let Some(ctx) = app.ctx() else { return };
    let sessions: Vec<_> = app.sessions.lock().await.values().cloned().collect();
    let side_ok = app.sidecar.lock().unwrap().ok;
    let text = if sessions.is_empty() {
        "💤 /들어와 로 불러 주세요".to_string()
    } else if !side_ok {
        "⚠️ 잠깐 못 들어요 (받아쓰기 재시작 중)".to_string()
    } else if sessions.iter().any(|s| s.listening.load(Ordering::Relaxed)) {
        app.cfg().notice.status.clone()
    } else {
        "⏸️ 주인을 기다리는 중".to_string()
    };
    {
        let mut last = LAST.lock().unwrap();
        if *last == text {
            return;
        }
        *last = text.clone();
    }
    ctx.set_activity(Some(ActivityData::custom(text)));
}
