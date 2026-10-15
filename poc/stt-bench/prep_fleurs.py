"""FLEURS 한국어(ko_kr) test 분할을 오디오 파일 + 매니페스트로 푼다. (google/fleurs, CC BY 4.0)

    python prep_fleurs.py data/fleurs   →  data/fleurs/*.wav, data/fleurs/manifest.jsonl

Zeroth(뉴스 낭독)와 다른 화자·문장(위키 문장 낭독, 다양한 녹음 환경)으로 두 번째 기준을 만든다.
"""

import json
import sys
import tarfile
from pathlib import Path

from huggingface_hub import hf_hub_download

REPO = "google/fleurs"


def main() -> int:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "data/fleurs")
    out.mkdir(parents=True, exist_ok=True)
    tsv = hf_hub_download(REPO, "data/ko_kr/test.tsv", repo_type="dataset")
    tgz = hf_hub_download(REPO, "data/ko_kr/audio/test.tar.gz", repo_type="dataset")
    texts = {}
    for line in Path(tsv).read_text(encoding="utf-8").splitlines():
        cols = line.split("\t")
        if len(cols) >= 3:
            texts[cols[1]] = cols[2]  # 파일명 → 원문(raw transcription)
    n = 0
    lines = []
    with tarfile.open(tgz) as tf:
        for m in tf.getmembers():
            if not m.isfile() or not m.name.endswith(".wav"):
                continue
            name = Path(m.name).name
            if name not in texts:
                continue
            dst = out / name
            dst.write_bytes(tf.extractfile(m).read())
            lines.append(json.dumps({"audio": str(dst), "text": texts[name]}, ensure_ascii=False))
            n += 1
    (out / "manifest.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"{n}개 → {out / 'manifest.jsonl'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
