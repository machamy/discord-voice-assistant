"""MeloTTS 한국어 작업 프로세스 — 사이드카가 별도 venv(Python 3.11, melo-venv)로 띄운다.

MeloTTS 의존성(구버전 transformers·tokenizers, mecab)이 Python 3.13 사이드카 venv와 맞지 않아 프로세스를 나눴다.
표준입력 한 줄 = {"text": ..., "speed": 1.0} → 표준출력 한 줄 = {"wav": base64} 또는 {"error": ...}
"""

import base64
import io
import json
import sys


def _shim_eunjeon() -> None:
    """g2pkk(한국어 발음 변환)는 Windows에서 eunjeon을 찾는데, eunjeon은 C++ 빌드가 필요하다.
    Windows용 wheel이 있는 python-mecab-ko(모듈 이름 mecab)를 eunjeon.Mecab 자리에 끼운다(.pos() API가 같다)."""
    import importlib.machinery
    import types

    try:
        import eunjeon  # noqa: F401  진짜가 있으면 그대로
        return
    except ImportError:
        pass
    # python-mecab-ko는 venv 안 별도 폴더(mecabko)에 설치한다. site-packages에 같이 넣으면 Windows가 대소문자를
    # 구분하지 않아 mecab-python3의 MeCab 폴더와 섞여 둘 다 깨진다(2026-10-13 실제 발생).
    #   melo-venv\Scripts\python -m pip install --target melo-venv\mecabko python-mecab-ko
    import os

    sys.path.insert(0, os.path.join(sys.prefix, "mecabko"))
    import mecab

    m = types.ModuleType("eunjeon")
    m.Mecab = mecab.MeCab
    m.__spec__ = importlib.machinery.ModuleSpec("eunjeon", None)
    sys.modules["eunjeon"] = m


def main() -> None:
    # 프로토콜은 원래 표준출력으로만. 라이브러리가 print하는 것(진행 표시 등)은 stderr로 돌린다
    proto = sys.stdout
    sys.stdout = sys.stderr

    def send(obj: dict) -> None:
        proto.write(json.dumps(obj) + "\n")
        proto.flush()

    _shim_eunjeon()
    from melo.api import TTS

    tts = TTS(language="KR", device="cpu")
    spk = tts.hps.data.spk2id["KR"]
    send({"ready": True})
    for line in sys.stdin:
        try:
            req = json.loads(line)
            buf = io.BytesIO()
            tts.tts_to_file(req["text"], spk, buf, speed=float(req.get("speed", 1.0)), format="wav", quiet=True)
            send({"wav": base64.b64encode(buf.getvalue()).decode()})
        except Exception as e:  # 요청 하나가 실패해도 프로세스는 계속
            send({"error": f"{type(e).__name__}: {e}"[:300]})


if __name__ == "__main__":
    main()
