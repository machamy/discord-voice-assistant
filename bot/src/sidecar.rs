//! Python 사이드카(받아쓰기·목소리) 띄우기·감시·호출. SPEC N-09: 죽으면 다시 띄운다.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

use crate::app::AppRef;
use crate::config::Voice;

#[derive(Debug, Deserialize)]
pub struct SttResult {
    pub text: String,
    pub provider: Option<String>,
    pub filtered: bool,
    pub ms: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct TtsVoice {
    pub provider: String,
    pub voice: String,
    pub rate: String,
    pub pitch: String,
}

pub fn base(app: &AppRef) -> String {
    format!("http://127.0.0.1:{}", app.cfg().sidecar.port)
}

/// 16kHz mono s16 PCM → 받아쓰기(체인 순서대로, SPEC F-34)
pub async fn stt(app: &AppRef, pcm: &[i16]) -> Result<SttResult> {
    let chain = app.stt_chain().join(",");
    let hint = app.store.lock().unwrap().state.stt_hint.clone();
    let mut q: Vec<(&str, String)> = vec![("chain", chain)];
    if hint.on && !hint.words.trim().is_empty() {
        q.push(("hotwords", hint.words.clone()));
        q.push(("hint_min_ms", hint.min_ms.to_string()));
    }
    let mut body = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        body.extend_from_slice(&s.to_le_bytes());
    }
    let r = app
        .http
        .post(format!("{}/stt", base(app)))
        .query(&q)
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("사이드카 STT HTTP {} {}", r.status(), r.text().await.unwrap_or_default());
    }
    Ok(r.json().await?)
}

/// 캐릭터 목소리로 TTS 체인을 만든다 — 전역 체인 순서 + 캐릭터의 공급자별 목소리
pub fn voice_chain(app: &AppRef, voices: &std::collections::BTreeMap<String, Voice>) -> Vec<TtsVoice> {
    app.tts_chain()
        .into_iter()
        .map(|p| {
            let v = voices.get(&p).cloned().unwrap_or_default();
            TtsVoice {
                provider: p,
                voice: v.voice,
                rate: if v.rate.is_empty() { "+0%".into() } else { v.rate },
                pitch: if v.pitch.is_empty() { "+0Hz".into() } else { v.pitch },
            }
        })
        .collect()
}

/// TTS → 48kHz mono WAV 바이트와 실제 쓴 공급자
pub async fn tts(app: &AppRef, text: &str, chain: &[TtsVoice]) -> Result<(Vec<u8>, String)> {
    let r = app
        .http
        .post(format!("{}/tts", base(app)))
        .json(&serde_json::json!({ "text": text, "chain": chain }))
        // 사이드카 쪽 공급자 제한(melotts 첫 기동 180초)보다 길게
        .timeout(Duration::from_secs(200))
        .send()
        .await?;
    if !r.status().is_success() {
        bail!("사이드카 TTS HTTP {} {}", r.status(), r.text().await.unwrap_or_default());
    }
    let provider = r
        .headers()
        .get("x-provider")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
        .to_string();
    Ok((r.bytes().await?.to_vec(), provider))
}

pub async fn voices(app: &AppRef, provider: &str) -> Result<serde_json::Value> {
    let r = app
        .http
        .get(format!("{}/voices", base(app)))
        .query(&[("provider", provider)])
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    Ok(r.json().await?)
}

async fn health(app: &AppRef) -> Result<serde_json::Value> {
    let r = app
        .http
        .get(format!("{}/health", base(app)))
        .timeout(Duration::from_secs(3))
        .send()
        .await?;
    if !r.status().is_success() {
        return Err(anyhow!("HTTP {}", r.status()));
    }
    Ok(r.json().await?)
}

/// Windows에서 venv python은 진짜 python을 자식으로 띄운다 — 부모만 죽이면 자식이 남으니 트리째 끈다.
/// 부모가 먼저 죽어 트리에서 떨어진 고아(사이드카·Melo 작업 프로세스)도 설치 폴더 기준으로 찾아 끈다
async fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
    }
    let _ = child.kill().await;
    sweep_orphans().await;
}

/// 이 설치 폴더의 사이드카·Melo 작업 python 중 남은 것을 끈다(명령줄에 절대 경로가 들어 있다)
async fn sweep_orphans() {
    #[cfg(windows)]
    {
        let Ok(dir) = std::env::current_dir() else { return };
        // 설치 폴더 경로는 PowerShell 작은따옴표 문자열로 — 작은따옴표는 두 번 써서 이스케이프
        let d = dir.display().to_string().replace('\'', "''");
        let script = format!(
            "$d='{d}'; Get-CimInstance Win32_Process -Filter \"Name='python.exe'\" | Where-Object {{ $_.CommandLine -and \
             $_.CommandLine.IndexOf($d,[StringComparison]::OrdinalIgnoreCase) -ge 0 -and \
             ($_.CommandLine -like '*sidecar?app.py*' -or $_.CommandLine -like '*sidecar?melo_worker.py*') }} | \
             ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]).stdout(Stdio::null()).stderr(Stdio::null());
        cmd.creation_flags(0x0800_0000);
        let _ = tokio::time::timeout(Duration::from_secs(20), cmd.status()).await;
    }
}

/// 표준 핸들을 상속 불가로 — 자식(사이드카)이 봇 로그 파일 핸들을 물고 있지 않게
pub fn no_inherit_std_handles() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        for h in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(h);
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn spawn(app: &AppRef) -> Result<Child> {
    let cfg = app.cfg();
    let log_path = std::path::Path::new("data/sidecar.log");
    if std::fs::metadata(log_path).map(|m| m.len() > 5_000_000).unwrap_or(false) {
        let _ = std::fs::rename(log_path, "data/sidecar.log.1");
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_path)?;
    let err = log.try_clone()?;
    // 절대 경로로 — 프로세스 명령줄에 설치 폴더가 드러나야 start.ps1이 이 설치본만 골라 끈다
    let cwd = std::env::current_dir()?;
    let python = cwd.join(&cfg.sidecar.python);
    let script = cwd.join(&cfg.sidecar.script);
    let mut cmd = Command::new(&python);
    cmd.arg(&script)
        .env("SIDECAR_PORT", cfg.sidecar.port.to_string())
        .env("SIDECAR_PRELOAD", &cfg.sidecar.preload)
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .kill_on_drop(false);
    #[cfg(windows)]
    {
        // 콘솔 창 없이
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn()?;
    tracing::info!("사이드카 시작 pid={:?}", child.id());
    Ok(child)
}

/// 5초마다 상태를 보고, 3번 연속 응답이 없으면 다시 띄운다.
/// 이미 다른 프로세스가 같은 포트에서 돌고 있으면(봇 재시작 직후) 그걸 그대로 쓴다.
pub async fn supervise(app: AppRef) {
    let mut child: Option<Child> = None;
    let mut fails = 0u32;
    let mut first = true;
    loop {
        match health(&app).await {
            Ok(detail) => {
                fails = 0;
                let mut h = app.sidecar.lock().unwrap();
                h.ok = true;
                h.detail = detail;
                h.owned_pid = child.as_ref().and_then(|c| c.id());
            }
            Err(e) => {
                fails += 1;
                {
                    let mut h = app.sidecar.lock().unwrap();
                    if fails >= 2 {
                        h.ok = false;
                    }
                    h.last_error = e.to_string();
                }
                // 시작 직후에는 모델 예열 때문에 늦다 — 처음 띄울 땐 바로, 그 뒤엔 3번 실패 후
                let need = app.cfg().sidecar.autostart && (first || fails >= 3);
                if need {
                    if let Some(mut c) = child.take() {
                        kill_tree(&mut c).await;
                    }
                    match spawn(&app) {
                        Ok(c) => {
                            child = Some(c);
                            if !first {
                                app.sidecar.lock().unwrap().restarts += 1;
                            }
                        }
                        Err(e) => {
                            tracing::error!("사이드카 실행 실패: {e}");
                            app.sidecar.lock().unwrap().last_error = format!("실행 실패: {e}");
                        }
                    }
                    fails = 0;
                    // 모델 다운로드·로드·예열 — 처음엔 수 분 걸릴 수 있다. 응답할 때까지 최대 3분 기다린다
                    for _ in 0..60 {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        if health(&app).await.is_ok() {
                            break;
                        }
                    }
                }
            }
        }
        first = false;
        crate::presence::refresh(&app).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
