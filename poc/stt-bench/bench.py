"""PoC 2 — faster-whisper 한국어 받아쓰기 속도·정확도 측정.

입력은 매니페스트(JSONL, 한 줄에 {"audio": "경로", "text": "정답"})다. 데이터셋 준비와 측정을 분리해서
공개 데이터셋이든 직접 녹음이든 같은 스크립트로 잰다.

봇에서 실제로 들어올 모양을 흉내 낸다:
- 디스코드 수신은 48kHz Opus → songbird가 PCM으로 풀어 준다. `--opus` 를 주면 48kHz Opus(64kbps)로
  한 번 압축했다가 풀어서 코덱 손실을 재현한다.
- VAD로 잘린 발화 단위가 들어오므로 파일 하나 = 발화 하나로 보고, 발화 길이별 지연을 따로 집계한다.

출력: 콘솔 요약 + `--out` JSONL(발화별 결과). 비용 드는 API는 호출하지 않는다.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

import numpy as np

SR = 16000


def add_cuda_dll_dirs() -> None:
    """Windows: pip의 nvidia-cublas-cu12·nvidia-cudnn-cu12 DLL을 찾게 한다.
    시스템 CUDA(v13.0)는 PATH에 없고, CTranslate2 4.8 휠은 CUDA 12 + cuDNN 9를 요구한다."""
    if sys.platform != "win32":
        return
    import os
    import site

    for sp in site.getsitepackages():
        for bin_dir in Path(sp).glob("nvidia/*/bin"):
            os.add_dll_directory(str(bin_dir))
            os.environ["PATH"] = str(bin_dir) + os.pathsep + os.environ["PATH"]

# 호출어 변형. PoC 3에서 실측으로 늘린다. 띄어쓰기를 허용하면 "아 장난"이 걸려서 붙여 쓴 것만 본다.
WAKE_PATTERNS = {
    "아짱": re.compile(r"아[짱장쨩]"),
    "클짱": re.compile(r"(클|크|끌)[짱장쨩]"),
}


def load_audio(path: str, opus: bool) -> np.ndarray:
    """오디오를 16kHz mono float32로. opus=True면 48kHz Opus 64kbps 왕복을 거친다."""
    import av

    if not opus:
        from faster_whisper.audio import decode_audio

        return decode_audio(path, sampling_rate=SR)

    # 1) 48kHz stereo로 디코드 → 2) libopus 64kbps로 인코드 → 3) 다시 16kHz mono로 디코드
    import io

    buf = io.BytesIO()
    with av.open(path) as src, av.open(buf, "w", format="ogg") as dst:
        stream = dst.add_stream("libopus", rate=48000, layout="stereo")
        stream.bit_rate = 64000
        # libopus는 20ms(960샘플) 프레임만 받는다. 디스코드도 20ms 프레임이다.
        resampler = av.AudioResampler(format="s16", layout="stereo", rate=48000, frame_size=960)
        for frame in src.decode(audio=0):
            for rf in resampler.resample(frame):
                for pkt in stream.encode(rf):
                    dst.mux(pkt)
        # 리샘플러에 남은 960샘플 미만 꼬리를 흘려보낸다. 안 하면 발화 끝이 잘린다.
        for rf in resampler.resample(None):
            for pkt in stream.encode(rf):
                dst.mux(pkt)
        for pkt in stream.encode(None):
            dst.mux(pkt)
    buf.seek(0)
    from faster_whisper.audio import decode_audio

    return decode_audio(buf, sampling_rate=SR)


def normalize_ko(s: str) -> str:
    """CER용 정규화: 문장부호·공백 제거, 영문 소문자화. 한국어는 띄어쓰기가 흔들려서 공백을 뺀다."""
    s = s.lower()
    s = re.sub(r"[^\w]", "", s)
    return s.replace("_", "")


def edit_distance(a: str, b: str) -> int:
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def gpu_mem_used_mib() -> int | None:
    try:
        out = subprocess.run(
            ["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5,
        ).stdout.strip()
        return int(out.splitlines()[0])
    except Exception:
        return None


def bucket(sec: float) -> str:
    if sec < 2:
        return "<2s"
    if sec < 5:
        return "2-5s"
    if sec < 10:
        return "5-10s"
    return ">=10s"


def pct(xs: list[float], p: float) -> float:
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p * (len(xs) - 1))))]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("manifest")
    ap.add_argument("--model", default="large-v3-turbo")
    ap.add_argument("--device", default="cuda", choices=["cuda", "cpu"])
    # GTX 1060(Pascal, CC 6.1): int8 가능, float16·int8_float16 불가(CC 7.0+ 필요)
    ap.add_argument("--compute-type", default="int8")
    ap.add_argument("--beam-size", type=int, default=1)
    ap.add_argument("--cpu-threads", type=int, default=4)
    # 기본은 프롬프트 없음. "아짱, 클짱"을 넣으면 잘린 음성에서 호출어를 지어낸다(2026-10-12 실측, README 결과).
    ap.add_argument("--initial-prompt", default="")
    # faster-whisper hotwords — 호출어 쪽으로 기울이는 짧은 힌트(prefix가 없을 때만 적용). 오탐을 같이 잰다
    ap.add_argument("--hotwords", default="")
    ap.add_argument("--opus", action="store_true", help="48kHz Opus 64kbps 왕복으로 디스코드 코덱 손실 재현")
    ap.add_argument("--limit", type=int, default=0)
    # 공개 데이터셋엔 짧은 발화가 거의 없다. 앞 N초만 잘라 "짧은 발화 지연"만 잰다(정답이 안 맞으므로 CER 제외).
    ap.add_argument("--crop-sec", type=float, default=0)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    add_cuda_dll_dirs()
    from faster_whisper import WhisperModel

    items = [json.loads(l) for l in Path(args.manifest).read_text(encoding="utf-8").splitlines() if l.strip()]
    if args.limit:
        items = items[: args.limit]

    mem0 = gpu_mem_used_mib()
    t = time.perf_counter()
    model = WhisperModel(args.model, device=args.device, compute_type=args.compute_type,
                         cpu_threads=args.cpu_threads)
    load_s = time.perf_counter() - t

    # 첫 호출은 CUDA 커널 준비 때문에 느리다. 봇은 상주하므로 워밍업을 따로 재고 집계에서 뺀다.
    warm = np.zeros(SR, dtype=np.float32)
    t = time.perf_counter()
    list(model.transcribe(warm, language="ko", beam_size=args.beam_size)[0])
    warm_s = time.perf_counter() - t

    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    out_f = open(args.out, "w", encoding="utf-8") if args.out else None
    rows = []
    mem_peak = gpu_mem_used_mib() or 0
    for i, it in enumerate(items):
        audio = load_audio(it["audio"], args.opus)
        if args.crop_sec:
            audio = audio[: int(args.crop_sec * SR)]
        dur = len(audio) / SR
        t = time.perf_counter()
        segs, _info = model.transcribe(
            audio, language="ko", beam_size=args.beam_size,
            initial_prompt=args.initial_prompt or None,
            hotwords=args.hotwords or None,
            vad_filter=False,  # 봇에선 앞단 VAD가 이미 잘라 준다
            condition_on_previous_text=False,
        )
        hyp = "".join(s.text for s in segs).strip()  # 제너레이터라 여기서 실제 디코딩이 끝난다
        lat = time.perf_counter() - t
        ref_n, hyp_n = ("", "") if args.crop_sec else (normalize_ko(it["text"]), normalize_ko(hyp))
        ed = edit_distance(ref_n, hyp_n)
        row = {"audio": it["audio"], "dur": round(dur, 3), "lat": round(lat, 3), "ref": it["text"],
               "hyp": hyp, "edits": ed, "ref_chars": len(ref_n)}
        for name, pat in WAKE_PATTERNS.items():
            if name in it["text"]:
                row.setdefault("wake", {})[name] = bool(pat.search(hyp))
            elif pat.search(hyp):
                row.setdefault("false_wake", []).append(name)  # 안 불렀는데 부른 걸로 잡힘
        rows.append(row)
        if out_f:
            out_f.write(json.dumps(row, ensure_ascii=False) + "\n")
        if i % 10 == 0:
            mem_peak = max(mem_peak, gpu_mem_used_mib() or 0)
            print(f"[{i + 1}/{len(items)}] {dur:5.2f}s → {lat:5.2f}s  {hyp[:40]}", file=sys.stderr)
    if out_f:
        out_f.close()

    total_dur = sum(r["dur"] for r in rows)
    total_lat = sum(r["lat"] for r in rows)
    cer = sum(r["edits"] for r in rows) / max(1, sum(r["ref_chars"] for r in rows))
    print("\n=== 결과")
    print(f"model={args.model} device={args.device} compute={args.compute_type} beam={args.beam_size} opus={args.opus} hotwords={args.hotwords!r} crop={args.crop_sec}")
    print(f"로드 {load_s:.1f}s, 워밍업 {warm_s:.2f}s, GPU 메모리 {mem0}→최대 {mem_peak} MiB")
    print(f"발화 {len(rows)}개, 음성 {total_dur:.0f}s, 처리 {total_lat:.0f}s, RTF {total_lat / max(total_dur, 1e-9):.3f}")
    if not args.crop_sec:
        print(f"CER {cer * 100:.2f}%  (공백·문장부호 제거 후 글자 단위)")
    by = {}
    for r in rows:
        by.setdefault(bucket(r["dur"]), []).append(r["lat"])
    for b in ["<2s", "2-5s", "5-10s", ">=10s"]:
        if b in by:
            xs = by[b]
            print(f"  {b:>6}: n={len(xs):3d}  지연 p50 {statistics.median(xs):.2f}s  p95 {pct(xs, .95):.2f}s")
    wk = [(n, ok) for r in rows for n, ok in r.get("wake", {}).items()]
    if wk:
        for name in WAKE_PATTERNS:
            hits = [ok for n, ok in wk if n == name]
            if hits:
                print(f"  호출어 {name}: {sum(hits)}/{len(hits)} 인식")
    fw = [r for r in rows if r.get("false_wake")]
    print(f"  호출어 오탐: {len(fw)}/{len(rows)} 발화")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
