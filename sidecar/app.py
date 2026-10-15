"""아짱·클짱 Python 사이드카 — 받아쓰기(STT)·목소리(TTS) 체인.

봇 본체(Rust)가 이 프로세스를 띄우고 감시한다(SPEC N-09). localhost에서만 듣는다.

  GET  /health                 → 장치·모델·GPU 상태
  POST /stt?chain=a,b          → 본문: 16kHz mono s16le PCM. 체인 순서대로 시도(SPEC §6, F-34)
  POST /tts                    → {"text", "chain":[{"provider","voice","rate","pitch"}]} → 48kHz mono WAV
  GET  /voices?provider=edge-tts

음성·받아쓰기 내용은 디스크에도 로그에도 남기지 않는다(SPEC N-06, F-33).
"""

from __future__ import annotations

import asyncio
import io
import logging
import os
import re
import site
import subprocess
import sys
import threading
import time
import wave
from pathlib import Path

import numpy as np


def _add_cuda_dll_dirs() -> None:
    # pip의 nvidia-cublas-cu12·nvidia-cudnn-cu12 DLL (시스템 CUDA 13은 Pascal 미지원이라 안 쓴다)
    if sys.platform != "win32":
        return
    for sp in site.getsitepackages():
        for d in Path(sp).glob("nvidia/*/bin"):
            os.add_dll_directory(str(d))
            os.environ["PATH"] = str(d) + os.pathsep + os.environ["PATH"]


_add_cuda_dll_dirs()

import av  # noqa: E402
import httpx  # noqa: E402
from fastapi import FastAPI, HTTPException, Request, Response  # noqa: E402
from pydantic import BaseModel  # noqa: E402

log = logging.getLogger("sidecar")
logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
for _noisy in ("httpx", "huggingface_hub", "urllib3", "faster_whisper"):
    logging.getLogger(_noisy).setLevel(logging.WARNING)

SR = 16000
OUT_SR = 48000
app = FastAPI()

# ---------------------------------------------------------------- 공통


class ProviderError(Exception):
    pass


def pcm_to_wav(pcm: np.ndarray, sr: int) -> bytes:
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(sr)
        w.writeframes(pcm.astype("<i2").tobytes())
    return buf.getvalue()


def decode_to_pcm(data: bytes, sr: int = OUT_SR) -> np.ndarray:
    """mp3/wav/ogg 등 → mono s16 PCM (sr)."""
    out = []
    with av.open(io.BytesIO(data)) as c:
        rs = av.AudioResampler(format="s16", layout="mono", rate=sr)
        for frame in c.decode(audio=0):
            for f in rs.resample(frame):
                out.append(f.to_ndarray().reshape(-1))
        for f in rs.resample(None):
            out.append(f.to_ndarray().reshape(-1))
    if not out:
        raise ProviderError("빈 오디오")
    return np.concatenate(out).astype(np.int16)


def gpu_mem() -> dict | None:
    try:
        out = subprocess.run(
            ["nvidia-smi", "--query-gpu=memory.used,memory.total,utilization.gpu", "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=3,
        ).stdout.strip().splitlines()[0]
        used, total, util = (int(x.strip()) for x in out.split(","))
        return {"used_mib": used, "total_mib": total, "util_pct": util}
    except Exception:
        return None


# ---------------------------------------------------------------- STT

# 무음·잡음에서 Whisper가 지어내는 상투 문구 (SPEC F-04, PoC 2에서 "다음 영상에서 만나요." 실측)
HALLUCINATIONS = [
    "다음 영상에서 만나요", "시청해 주셔서 감사합니다", "시청해주셔서 감사합니다", "구독과 좋아요",
    "구독 좋아요", "좋아요와 구독", "MBC 뉴스", "KBS 뉴스", "SBS 뉴스", "이 영상은", "자막 제공", "자막 by",
    "끝까지 시청", "오늘도 시청", "감사합니다.", "고맙습니다.",

]
# 2026-10-15 실사용에서 아무도 안 한 말로 기록된 것 — 흔한 인사라 발화 전체가 딱 그 말일 때만 버린다("다들 수고하셨습니다"는 살림)
EXACT_HALLUCINATIONS = {"수고하셨습니다", "안녕히계세요", "안녕히가세요"}
# 한국어로 고정했는데 키릴·가나·한자만 나오면 환각(실사용: "Редактор субтитров А.Семкин")
_FOREIGN = re.compile(r"[Ѐ-ӿ぀-ヿ一-鿿]")
_HANGUL = re.compile(r"[가-힣]")
_ONLY_PUNCT = re.compile(r"^[\s\W_]*$")


def _err_kind(e: BaseException) -> str:
    """오류는 종류만 — 외부 라이브러리 예외 문구엔 요청 내용·주소·키가 섞일 수 있다.
    ProviderError는 이 파일이 만든 짧은 문구(\"HTTP 403\", \"… 없음\")라 그대로 둔다."""
    if isinstance(e, ProviderError):
        return f"ProviderError: {e}"[:120]
    return type(e).__name__


def is_hallucination(text: str, no_speech: float, logprob: float, audio_s: float = 99.0) -> bool:
    t = text.strip()
    if not t or _ONLY_PUNCT.match(t):
        return True
    if not _HANGUL.search(t):
        # 한글이 하나도 없는데 외국 문자가 섞였거나, 2초 미만 짧은 소리가 영어 한 줄로 나오면 버린다("hey y'all ciao!")
        if _FOREIGN.search(t) or audio_s < 2.0:
            return True
    if no_speech > 0.6 and logprob < -1.0:
        return True
    norm = t.replace(" ", "")
    if re.sub(r"[\W_]", "", norm) in EXACT_HALLUCINATIONS:
        return True
    for h in HALLUCINATIONS:
        hn = h.replace(" ", "")
        # 짧은 발화가 통째로 상투 문구일 때만 버린다(긴 문장 속 "감사합니다"는 살린다)
        if hn in norm and len(norm) <= len(hn) + 6:
            return True
    return False


class LocalWhisper:
    def __init__(self) -> None:
        self.models: dict[tuple[str, str], object] = {}
        self.lock = threading.Lock()
        self.cuda_bad_until = 0.0
        self.last_error = ""

    def _model(self, device: str, name: str):
        key = (device, name)
        if key not in self.models:
            from faster_whisper import WhisperModel

            ct = "int8"  # GTX 1060(Pascal CC 6.1): int8만 가능, float16 불가
            t = time.perf_counter()
            self.models[key] = WhisperModel(name, device=device, compute_type=ct, cpu_threads=8 if device == "cpu" else 4)
            log.info("모델 로드 %s/%s %.1fs", device, name, time.perf_counter() - t)
        return self.models[key]

    def transcribe(self, device: str, name: str, audio: np.ndarray, hotwords: str | None = None) -> dict:
        if device == "cuda" and time.time() < self.cuda_bad_until:
            raise ProviderError("GPU 일시 사용 불가(쿨다운)")
        with self.lock:
            try:
                m = self._model(device, name)
                segs, _ = m.transcribe(
                    audio, language="ko", beam_size=1, vad_filter=False,
                    condition_on_previous_text=False, without_timestamps=True,
                    hotwords=hotwords or None,
                )  # initial_prompt 없음 — 호출어를 지어낸다(SPEC F-20). hotwords는 웹 콘솔에서 켰을 때만, 긴 발화에만
                segs = list(segs)
            except Exception as e:  # CUDA OOM·드라이버 오류 → 5분간 GPU 끄고 다음 공급자로 (SPEC N-08)
                self.last_error = f"{type(e).__name__}: {e}"[:300]
                if device == "cuda":
                    self.cuda_bad_until = time.time() + 300
                    self.models.pop((device, name), None)
                raise ProviderError(self.last_error) from e
        text = "".join(s.text for s in segs).strip()
        no_speech = max((s.no_speech_prob for s in segs), default=1.0)
        logprob = min((s.avg_logprob for s in segs), default=-9.0)
        return {"text": text, "no_speech": no_speech, "logprob": logprob}


WHISPER = LocalWhisper()


async def stt_deepgram(audio_pcm: bytes) -> dict:
    """Deepgram Nova-3 한국어(가입 크레딧 $200, 카드 불필요). 키: DEEPGRAM_API_KEY"""
    key = os.environ.get("DEEPGRAM_API_KEY")
    if not key:
        raise ProviderError("DEEPGRAM_API_KEY 없음")
    wav = pcm_to_wav(np.frombuffer(audio_pcm, dtype="<i2"), SR)
    try:
        async with httpx.AsyncClient(timeout=20) as c:
            r = await c.post("https://api.deepgram.com/v1/listen",
                             params={"model": os.environ.get("DEEPGRAM_MODEL", "nova-3"), "language": "ko", "smart_format": "true"},
                             headers={"Authorization": f"Token {key}", "Content-Type": "audio/wav"}, content=wav)
        if r.status_code != 200:
            raise ProviderError(f"HTTP {r.status_code}")
        alt = r.json()["results"]["channels"][0]["alternatives"][0]
        text = alt.get("transcript", "")
    except ProviderError:
        raise
    except Exception as e:  # 전송·JSON 오류도 다음 공급자로(SPEC F-34)
        raise ProviderError(f"전송 오류 {type(e).__name__}") from e
    return {"text": str(text).strip(), "no_speech": 0.0, "logprob": 0.0}


async def stt_cloud(url: str, key_env: str, model: str, audio_pcm: bytes) -> dict:
    key = os.environ.get(key_env)
    if not key:
        raise ProviderError(f"{key_env} 없음")
    wav = pcm_to_wav(np.frombuffer(audio_pcm, dtype="<i2"), SR)
    try:
        async with httpx.AsyncClient(timeout=20) as c:
            r = await c.post(url, headers={"Authorization": f"Bearer {key}"},
                             files={"file": ("a.wav", wav, "audio/wav")},
                             data={"model": model, "language": "ko", "response_format": "json"})
        if r.status_code != 200:
            raise ProviderError(f"HTTP {r.status_code}")
        text = r.json().get("text", "")
    except ProviderError:
        raise
    except Exception as e:  # 전송·JSON 오류도 다음 공급자로(SPEC F-34)
        raise ProviderError(f"전송 오류 {type(e).__name__}") from e
    return {"text": str(text).strip(), "no_speech": 0.0, "logprob": 0.0}


async def run_stt(provider: str, pcm: bytes, hotwords: str | None = None) -> dict:
    if provider.startswith("local-whisper"):
        # local-whisper:cuda:large-v3-turbo / local-whisper:cpu:small
        parts = provider.split(":")
        device = parts[1] if len(parts) > 1 else "cuda"
        name = parts[2] if len(parts) > 2 else "large-v3-turbo"
        audio = np.frombuffer(pcm, dtype="<i2").astype(np.float32) / 32768.0
        return await asyncio.to_thread(WHISPER.transcribe, device, name, audio, hotwords)
    if provider in ("groq-whisper", "groq-whisper-turbo"):
        # 무료 하루 2,000회·8시간(요청당 최소 10초로 계산). 정확도는 v3 > turbo — 기본은 v3
        model = "whisper-large-v3" if provider == "groq-whisper" else "whisper-large-v3-turbo"
        return await stt_cloud("https://api.groq.com/openai/v1/audio/transcriptions", "GROQ_API_KEY", model, pcm)
    if provider == "deepgram":
        return await stt_deepgram(pcm)
    if provider == "openai-transcribe":
        return await stt_cloud("https://api.openai.com/v1/audio/transcriptions", "OPENAI_API_KEY", "gpt-4o-mini-transcribe", pcm)
    raise ProviderError(f"지원하지 않는 STT 공급자: {provider}")


@app.post("/stt")
async def stt(request: Request, chain: str = "local-whisper:cuda:large-v3-turbo,local-whisper:cpu:small", hotwords: str = "", hint_min_ms: int = 0):
    pcm = await request.body()
    # 받아쓰기 힌트 — 짧은 소리엔 호출어를 지어내므로(실측 1초 조각 271/382) 이 길이 이상만
    hw = hotwords.strip()[:100] if hotwords.strip() and len(pcm) >= SR * 2 * hint_min_ms / 1000 else None
    if len(pcm) < SR * 2 * 0.2:
        return {"text": "", "provider": None, "filtered": True, "ms": 0, "errors": []}
    errors = []
    for p in [x.strip() for x in chain.split(",") if x.strip()]:
        t = time.perf_counter()
        try:
            r = await run_stt(p, pcm, hw)
        except Exception as e:  # 어떤 공급자 오류든 다음 공급자로(SPEC F-34)
            errors.append({"provider": p, "error": _err_kind(e)})
            continue
        ms = int((time.perf_counter() - t) * 1000)
        filtered = is_hallucination(r["text"], r["no_speech"], r["logprob"], len(pcm) / (SR * 2))
        log.info("stt %s %dms %.1fs filtered=%s hint=%s", p, ms, len(pcm) / (SR * 2), filtered, bool(hw))  # 내용은 남기지 않는다
        return {"text": "" if filtered else r["text"], "provider": p, "filtered": filtered, "ms": ms, "errors": errors}
    raise HTTPException(503, detail={"errors": errors})


# ---------------------------------------------------------------- TTS


class TtsVoice(BaseModel):
    provider: str
    voice: str = ""
    rate: str = "+0%"
    pitch: str = "+0Hz"


class TtsReq(BaseModel):
    text: str
    chain: list[TtsVoice]


_EDGE_PATCHED = False


def _patch_edge() -> None:
    """2026-10-12부터 예전 합성 주소(speech.platform.bing.com)가 403을 낸다(edge-tts 이슈 #490).
    Edge 155처럼 api.msedgeservices.com으로 합성한다(PR #491과 같은 수정, 정식 배포 전).
    EDGE_TTS_WSS_URL로 덮어쓸 수 있고, "legacy"면 라이브러리 기본값을 쓴다."""
    global _EDGE_PATCHED
    if _EDGE_PATCHED:
        return
    import edge_tts.communicate as com
    import edge_tts.constants as const

    url = os.environ.get("EDGE_TTS_WSS_URL", "")
    if url != "legacy":
        url = url or ("wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1"
                      f"?Ocp-Apim-Subscription-Key={const.TRUSTED_CLIENT_TOKEN}")
        const.WSS_URL = url
        com.WSS_URL = url
    _EDGE_PATCHED = True


async def _edge_once(v: TtsVoice, text: str) -> bytes:
    import edge_tts

    com = edge_tts.Communicate(text, v.voice or "ko-KR-SunHiNeural", rate=v.rate, pitch=v.pitch)
    buf = bytearray()
    async for chunk in com.stream():
        if chunk["type"] == "audio":
            buf += chunk["data"]
    if not buf:
        raise ProviderError("edge-tts 빈 응답")
    return bytes(buf)


async def tts_edge(v: TtsVoice, text: str) -> np.ndarray:
    _patch_edge()
    # 비공식 서비스라 가끔 한 번씩 연결이 끊긴다(2026-10-13 실측) — 느린 대체(melotts 첫 기동 20초+)로 넘어가기 전에 한 번 더.
    # 받은 오디오가 깨져 디코딩이 실패하는 경우도 같이 재시도한다
    async def once() -> np.ndarray:
        data = await asyncio.wait_for(_edge_once(v, text), timeout=8)
        return decode_to_pcm(data)

    try:
        return await once()
    except Exception as e:
        log.warning("edge-tts 재시도: %s", type(e).__name__)
        return await once()


async def tts_azure(v: TtsVoice, text: str) -> np.ndarray:
    key, region = os.environ.get("AZURE_SPEECH_KEY"), os.environ.get("AZURE_SPEECH_REGION")
    if not key or not region:
        raise ProviderError("AZURE_SPEECH_KEY/REGION 없음")
    from xml.sax.saxutils import escape, quoteattr

    # 속성 값은 quoteattr(따옴표 포함 이스케이프) — 웹에서 넣은 값이 속성을 탈출하지 못하게
    ssml = (f"<speak version='1.0' xml:lang='ko-KR'><voice name={quoteattr(v.voice or 'ko-KR-SunHiNeural')}>"
            f"<prosody rate={quoteattr(v.rate)} pitch={quoteattr(v.pitch)}>{escape(text)}</prosody></voice></speak>")
    async with httpx.AsyncClient(timeout=20) as c:
        r = await c.post(f"https://{region}.tts.speech.microsoft.com/cognitiveservices/v1",
                         headers={"Ocp-Apim-Subscription-Key": key, "Content-Type": "application/ssml+xml",
                                  "X-Microsoft-OutputFormat": "riff-48khz-16bit-mono-pcm"},
                         content=ssml.encode("utf-8"))
    if r.status_code != 200:
        raise ProviderError(f"azure HTTP {r.status_code}")
    return decode_to_pcm(r.content)


class MeloWorker:
    """별도 venv(melo-venv, Python 3.11)의 melo_worker.py를 띄워 쓴다. 없으면 '미설치'로 다음 공급자에 넘긴다."""

    def __init__(self) -> None:
        self.proc: asyncio.subprocess.Process | None = None
        self.lock = asyncio.Lock()

    def python(self) -> str:
        # 절대 경로 — 명령줄에 설치 폴더가 드러나야 start.ps1이 이 설치본만 골라 끈다
        return os.path.abspath(os.environ.get("MELO_PYTHON", str(Path("melo-venv") / "Scripts" / "python.exe")))

    def available(self) -> bool:
        return Path(self.python()).exists()

    def _kill(self) -> None:
        p, self.proc = self.proc, None
        if p is not None and p.returncode is None:
            try:
                p.kill()
            except ProcessLookupError:
                pass

    async def _start(self) -> None:
        if not self.available():
            raise ProviderError("melotts 미설치(melo-venv 없음)")
        script = str(Path(__file__).with_name("melo_worker.py"))
        self.proc = await asyncio.create_subprocess_exec(
            self.python(), script, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL, limit=64 * 1024 * 1024,
        )
        # 모델 로드(첫 실행은 다운로드)까지 기다린다
        line = b""
        for _ in range(50):
            line = await asyncio.wait_for(self.proc.stdout.readline(), timeout=300)
            if not line or b"ready" in line:
                break
        if b"ready" not in line:
            raise ProviderError("melotts 작업 프로세스 시작 실패")

    async def synth(self, text: str, speed: float) -> bytes:
        import base64
        import json as _json

        async with self.lock:
            try:
                if self.proc is None or self.proc.returncode is not None:
                    await self._start()
                assert self.proc and self.proc.stdin and self.proc.stdout
                self.proc.stdin.write((_json.dumps({"text": text, "speed": speed}) + "\n").encode())
                await self.proc.stdin.drain()
                line = b""
                # 혹시 섞여 든 JSON 아닌 줄은 건너뛴다
                for _ in range(50):
                    line = await asyncio.wait_for(self.proc.stdout.readline(), timeout=90)
                    if not line or line.lstrip().startswith(b"{"):
                        break
            except BaseException:
                # 취소·시간 초과·오류 — 응답이 파이프에 남아 다음 요청이 남의 WAV를 읽지 않게 프로세스를 버린다
                self._kill()
                raise
        if not line:
            self.proc = None
            raise ProviderError("melotts 작업 프로세스 종료")
        r = _json.loads(line)
        if "error" in r:
            # 작업 프로세스가 보낸 원문(라이브러리 예외)은 넘기지 않는다 — 종류만
            raise ProviderError("melotts 합성 오류")
        return base64.b64decode(r["wav"])


MELO = MeloWorker()


async def tts_melo(v: TtsVoice, text: str) -> np.ndarray:
    speed = 1.0
    m = re.match(r"([+-]\d+)%", v.rate or "")
    if m:
        speed = max(0.5, 1 + int(m.group(1)) / 100)
    return decode_to_pcm(await MELO.synth(text, speed))


async def tts_elevenlabs(v: TtsVoice, text: str) -> np.ndarray:
    key = os.environ.get("ELEVENLABS_API_KEY")
    if not key or not v.voice:
        raise ProviderError("ELEVENLABS_API_KEY 또는 voice id 없음")
    async with httpx.AsyncClient(timeout=20) as c:
        r = await c.post(f"https://api.elevenlabs.io/v1/text-to-speech/{v.voice}",
                         headers={"xi-api-key": key}, json={"text": text, "model_id": "eleven_flash_v2_5"})
    if r.status_code != 200:
        raise ProviderError(f"elevenlabs HTTP {r.status_code}")
    return decode_to_pcm(r.content)


async def tts_openai(v: TtsVoice, text: str) -> np.ndarray:
    key = os.environ.get("OPENAI_API_KEY")
    if not key:
        raise ProviderError("OPENAI_API_KEY 없음")
    async with httpx.AsyncClient(timeout=20) as c:
        r = await c.post("https://api.openai.com/v1/audio/speech", headers={"Authorization": f"Bearer {key}"},
                         json={"model": "gpt-4o-mini-tts", "voice": v.voice or "coral", "input": text,
                               "response_format": "wav"})
    if r.status_code != 200:
        raise ProviderError(f"openai-tts HTTP {r.status_code}")
    return decode_to_pcm(r.content)


TTS = {"edge-tts": tts_edge, "azure-tts": tts_azure, "melotts": tts_melo,
       "elevenlabs": tts_elevenlabs, "openai-tts": tts_openai}


@app.post("/tts")
async def tts(req: TtsReq):
    text = req.text.strip()[:1000]
    if not text:
        raise HTTPException(400, "빈 텍스트")
    errors = []
    for v in req.chain:
        fn = TTS.get(v.provider)
        if fn is None:
            errors.append({"provider": v.provider, "error": "지원하지 않음"})
            continue
        t = time.perf_counter()
        try:
            # melotts는 첫 호출에 모델을 올려 오래 걸린다
            pcm = await asyncio.wait_for(fn(v, text), timeout=180 if v.provider == "melotts" else 25)
        except Exception as e:  # 공급자 오류·시간 초과·edge-tts 내부 예외 등 — 다음 공급자로
            errors.append({"provider": v.provider, "error": _err_kind(e)})
            log.warning("tts %s 실패 %s", v.provider, type(e).__name__)  # 글 내용은 남기지 않는다
            continue
        ms = int((time.perf_counter() - t) * 1000)
        log.info("tts %s %dms %d자", v.provider, ms, len(text))
        return Response(pcm_to_wav(pcm, OUT_SR), media_type="audio/wav",
                        headers={"X-Provider": v.provider, "X-Ms": str(ms)})
    raise HTTPException(503, detail={"errors": errors})


@app.get("/voices")
async def voices(provider: str = "edge-tts"):
    if provider != "edge-tts":
        return {"voices": []}
    import edge_tts

    vs = await edge_tts.list_voices()
    return {"voices": [{"name": v["ShortName"], "gender": v.get("Gender")} for v in vs if v["Locale"].startswith("ko-")]}


@app.get("/health")
async def health():
    return {
        "ok": True,
        "stt": {
            "loaded": [f"{d}:{n}" for d, n in WHISPER.models],
            "cuda_cooldown_s": max(0, int(WHISPER.cuda_bad_until - time.time())),
            "last_error": WHISPER.last_error,
        },
        "gpu": gpu_mem(),
        "tts": {"melotts": _melo_available()},
    }


def _melo_available() -> bool:
    return MELO.available()


@app.on_event("startup")
async def warmup():
    # 첫 호출 지연(모델 로드 4초 + 워밍업 1초)을 미리 치른다
    preload = os.environ.get("SIDECAR_PRELOAD", "local-whisper:cuda:large-v3-turbo")
    for p in [x for x in preload.split(",") if x]:
        try:
            await run_stt(p, np.zeros(SR // 2, dtype=np.int16).tobytes())
        except ProviderError as e:
            log.warning("예열 실패 %s: %s", p, e)
    # MeloTTS(오프라인 대체)도 미리 올린다 — 첫 호출 20초+를 대답 중에 치르지 않게. SIDECAR_WARM_MELO=0이면 끔
    if MELO.available() and os.environ.get("SIDECAR_WARM_MELO", "1") != "0":
        async def _warm_melo() -> None:
            try:
                t = time.perf_counter()
                await MELO.synth("준비", 1.0)
                log.info("melotts 예열 %.1fs", time.perf_counter() - t)
            except Exception as e:
                log.warning("melotts 예열 실패 %s", type(e).__name__)
        # 참조를 붙잡아 둔다(이벤트 루프는 약한 참조만 가져 작업이 도중에 수거될 수 있다)
        app.state.melo_warm = asyncio.get_running_loop().create_task(_warm_melo())


if __name__ == "__main__":
    import uvicorn

    port = int(os.environ.get("SIDECAR_PORT", "8695"))
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")
