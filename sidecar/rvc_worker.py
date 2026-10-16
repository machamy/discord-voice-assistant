"""RVC 목소리 변환 작업 프로세스 — 사이드카가 별도 venv(rvc-venv, Python 3.11)로 띄운다(SPEC F-60).

edge-tts 등으로 만든 한국어 음성을 학습한 RVC 모델 목소리로 바꾼다. 추론 코드는 Applio(rvc/applio)를 그대로 쓴다.
모델은 models/rvc/<이름>/ 아래 *.pth(필수)·*.index(선택). 모델 파일은 저장소에 넣지 않는다(배포 금지 라이선스).
표준입력 한 줄 = {"wav": base64, "model": "teto", "pitch": 0, "index_rate": 0.5}
표준출력 한 줄 = {"wav": base64} 또는 {"error": ...}. 시작하면 {"ready": true}(기본 모델을 미리 올린 뒤)
"""

import base64
import json
import os
import re
import sys
import time


def main() -> None:
    # 프로토콜은 원래 표준출력으로만. 라이브러리가 print하는 것(진행 표시 등)은 stderr로 돌린다
    proto = sys.stdout
    sys.stdout = sys.stderr

    def send(obj: dict) -> None:
        proto.write(json.dumps(obj) + "\n")
        proto.flush()

    applio = os.path.abspath(os.environ.get("RVC_APPLIO", os.path.join("rvc", "applio")))
    # 변환용 임시 파일은 설치 폴더 안 고정 폴더에 두고 시작할 때마다 비운다 — 변환 중 강제 종료(취소·시간 초과)돼도
    # 발화 WAV가 시스템 임시 폴더에 쌓이지 않게(다음 시작 때 지워진다)
    tmp = os.path.abspath(os.environ.get("RVC_TMP", os.path.join("data", "rvc-tmp")))
    models = os.path.abspath(os.environ.get("RVC_MODELS", os.path.join("models", "rvc")))
    preload = os.environ.get("RVC_PRELOAD", "")
    # Applio는 작업 폴더 기준으로 rvc/models/... 를 찾는다
    os.chdir(applio)
    sys.path.insert(0, applio)
    from rvc.infer.infer import VoiceConverter

    import atexit
    import shutil

    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp, exist_ok=True)
    atexit.register(shutil.rmtree, tmp, True)
    vc = VoiceConverter()

    def model_files(name: str) -> tuple[str, str]:
        if not re.fullmatch(r"[a-z0-9_-]{1,40}", name):
            raise ValueError("모델 이름 형식")
        d = os.path.join(models, name)
        files = [f for f in os.listdir(d) if os.path.isfile(os.path.join(d, f))]
        pth = sorted(f for f in files if f.endswith(".pth"))
        idx = sorted(f for f in files if f.endswith(".index"))
        if not pth:
            raise FileNotFoundError("pth 없음")
        return os.path.join(d, pth[0]), (os.path.join(d, idx[0]) if idx else "")

    def convert(wav: bytes, name: str, pitch: int, index_rate: float) -> bytes:
        pth, idx = model_files(name)
        src, dst = os.path.join(tmp, "in.wav"), os.path.join(tmp, "out.wav")
        try:
            with open(src, "wb") as f:
                f.write(wav)
            vc.convert_audio(
                audio_input_path=src, audio_output_path=dst, model_path=pth, index_path=idx,
                pitch=pitch, f0_method="rmvpe", index_rate=index_rate if idx else 0.0, protect=0.33,
                embedder_model="contentvec", export_format="WAV", split_audio=False,
            )
            with open(dst, "rb") as f:
                return f.read()
        finally:
            # 합성 입력·출력은 남기지 않는다
            for p in (src, dst):
                try:
                    os.remove(p)
                except OSError:
                    pass

    # 기본 모델을 미리 올린다(첫 대답이 모델 로드를 기다리지 않게) — 짧은 무음 한 번 변환
    if preload:
        try:
            import io
            import wave

            buf = io.BytesIO()
            with wave.open(buf, "wb") as w:
                w.setnchannels(1)
                w.setsampwidth(2)
                w.setframerate(16000)
                w.writeframes(b"\x00\x00" * 8000)
            t = time.perf_counter()
            convert(buf.getvalue(), preload, 0, 0.5)
            print(f"rvc 예열 {preload} {time.perf_counter() - t:.1f}s", file=sys.stderr)
        except Exception as e:  # 예열 실패해도 요청 때 다시 시도
            print(f"rvc 예열 실패 {type(e).__name__}: {e}", file=sys.stderr)
    send({"ready": True})
    for line in sys.stdin:
        try:
            req = json.loads(line)
            pitch = max(-12, min(12, int(req.get("pitch", 0))))
            index_rate = max(0.0, min(1.0, float(req.get("index_rate", 0.5))))
            out = convert(base64.b64decode(req["wav"]), str(req.get("model", "")), pitch, index_rate)
            send({"wav": base64.b64encode(out).decode()})
        except Exception as e:  # 요청 하나가 실패해도 프로세스는 계속
            send({"error": f"{type(e).__name__}: {e}"[:300]})


if __name__ == "__main__":
    main()
