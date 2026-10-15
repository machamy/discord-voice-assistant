"""Zeroth-Korean test 분할(457발화, CC BY 4.0, 60MB parquet)을 오디오 파일 + 매니페스트로 푼다.

    python prep_zeroth.py data/zeroth   →  data/zeroth/*.flac, data/zeroth/manifest.jsonl

낭독체 뉴스 문장이라 디스코드 잡담보다 쉽다. 그래서 이 값은 "상한선"으로 보고,
실제 판단은 직접 녹음한 잡담 세트(record/ 매니페스트)와 같이 본다.
"""

import json
import sys
from pathlib import Path

import pyarrow.parquet as pq
from huggingface_hub import hf_hub_download

REPO = "Bingsu/zeroth-korean"
FILE = "data/test-00000-of-00001-a41b955a631e582e.parquet"


def main() -> int:
    out = Path(sys.argv[1] if len(sys.argv) > 1 else "data/zeroth")
    out.mkdir(parents=True, exist_ok=True)
    path = hf_hub_download(REPO, FILE, repo_type="dataset")
    table = pq.read_table(path)
    lines = []
    for i, row in enumerate(table.to_pylist()):
        audio = row["audio"]
        ext = Path(audio.get("path") or "x.flac").suffix or ".flac"
        f = out / f"{i:04d}{ext}"
        f.write_bytes(audio["bytes"])
        lines.append(json.dumps({"audio": str(f), "text": row["text"]}, ensure_ascii=False))
    (out / "manifest.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"{len(lines)}개 → {out / 'manifest.jsonl'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
