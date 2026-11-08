#!/usr/bin/env python3
"""Fetch and clean the public-domain training corpus for dict_eval (SPEC 12.2, 20.9).

Sources (every one a US federal government work, 17 U.S.C. 105; see
``dictionaries/REGISTRY.md`` for URL, license basis and retrieval date):

* ``fema_ics``: the FEMA ICS form templates and instructions (training.fema.gov)
* ``ecfr_part97``: 47 CFR Part 97, the amateur radio service rules (ecfr.gov)
* ``nws``: a snapshot of National Weather Service text products (api.weather.gov)

Raw downloads go to ``--raw`` (outside the repository); the cleaned text goes
to ``--out`` (default ``dictionaries/corpus``) together with ``manifest.json``
(URL, retrieval date, SHA-256 of the raw bytes and of the cleaned text).
Downloaded content is treated strictly as data. The NWS snapshot is live data:
re-running this script yields a different snapshot, so the committed cleaned
corpus, not this script, is what the published numbers were measured on.

    python3 -I sim/scripts/fetch_dict_corpus.py --raw /tmp/dictcorpus
"""

from __future__ import annotations

import argparse
import datetime
import gzip
import hashlib
import html
import json
import re
import shutil
import subprocess
import time
import urllib.parse
import urllib.request
from pathlib import Path

UA = "Mozilla/5.0 (cairn dict_eval corpus fetch)"
FEMA_INDEX = "https://training.fema.gov/icsresource/icsforms.htm"
FEMA_BASE = "https://training.fema.gov"
ECFR_API = "https://www.ecfr.gov/api/versioner/v1"
NWS_API = "https://api.weather.gov"

# ICS forms most useful for radio traffic, plus the instructions (prose).
FEMA_WANTED = re.compile(
    r"ICS Form (201|202|203|204|205|205A|206|207|208|209|210|211|213|213RR|214|215|217A|218|221|"
    r"260), |ICS Forms - Instructions", re.I)

NWS_OFFICES = ["LOT", "DVN", "GRR", "MKX", "DTX", "IWX", "ILN", "PBZ", "CTP", "OKX", "BOX", "GYX",
               "BUF", "ALY", "RLX", "LWX", "AKQ", "RAH", "MHX", "ILM", "CAE", "FFC", "BMX", "JAN",
               "LIX", "HGX", "FWD", "OUN", "TSA", "EAX", "SGF", "LSX", "DMX", "MPX", "FGF", "ABR",
               "BIS", "UNR", "CYS", "BOU", "GJT", "SLC", "PIH", "MSO", "TFX", "PDT", "PQR", "SEW",
               "MFR", "STO", "HNX", "SGX", "VEF", "FGZ", "PSR", "TWC", "EPZ", "ABQ", "AMA", "LUB"]
NWS_TYPES = ["AFD", "SPS", "HWO", "NOW", "RWR", "ZFP", "PNS", "WSW", "SVR", "FFW", "NPW", "RVS"]
NWS_MAX_PRODUCTS = 600
NWS_MAX_BYTES = 5000


def get(url: str, *, retries: int = 3) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept-Encoding": "gzip"})
    for i in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                data = r.read()
                return gzip.decompress(data) if r.headers.get("Content-Encoding") == "gzip" else data
        except Exception:
            if i == retries - 1:
                raise
            time.sleep(2 * (i + 1))
    raise AssertionError


def sha(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


_FOLD = {"‘": "'", "’": "'", "“": '"', "”": '"', "–": "-", "—": "-",
         "•": "*", " ": " ", "…": "...", "§": "Sec. ", "°": " deg ",
         "☐": "[ ]", "☒": "[x]", "": "*", "": "*"}


def clean(text: str) -> str:
    """ASCII-fold, drop control characters and page furniture, normalise whitespace.
    Paragraphs (blank-line separated) are kept; lines inside a paragraph stay."""
    for k, v in _FOLD.items():
        text = text.replace(k, v)
    text = text.replace("\f", "\n\n")
    text = re.sub(r"\S*@\S*", "", text)       # no e-mail addresses or @handles in the corpus
    text = "".join(c if (c == "\n" or 32 <= ord(c) < 127) else " " for c in text)
    out, para = [], []
    for line in text.split("\n"):
        line = re.sub(r"[ \t]+", " ", line).strip()
        if re.fullmatch(r"(Page )?\d+( of \d+)?", line, re.I) or re.fullmatch(r"[_\-=. ]{3,}", line):
            continue
        line = re.sub(r"[_]{3,}", "____", line)
        if line:
            para.append(line)
        elif para:
            out.append(" ".join(para))
            para = []
    if para:
        out.append(" ".join(para))
    return "\n\n".join(out) + "\n"


def fetch_fema(raw: Path, out: Path, manifest: list[dict], today: str) -> None:
    index = get(FEMA_INDEX)
    (raw / "fema_ics").mkdir(parents=True, exist_ok=True)
    (raw / "fema_ics" / "icsforms.htm").write_bytes(index)
    links = re.findall(r"""href=["'](/EMIWeb[^"']+\.pdf)["']""", index.decode("utf-8", "replace"))
    seen: set[str] = set()
    for link in links:
        name = link.rsplit("/", 1)[1]
        if not FEMA_WANTED.search(name) or "T-Card" in name:
            continue
        key = re.sub(r"\s*\(v[\d.]+\)", "", name.lower())
        if key in seen:       # form 221 is listed in two versions; keep the first
            continue
        seen.add(key)
        url = FEMA_BASE + urllib.parse.quote(link.replace("//", "/"), safe="/")
        data = get(url)
        stem = re.sub(r"[^A-Za-z0-9]+", "_", name.rsplit(".", 1)[0]).strip("_")
        pdf = raw / "fema_ics" / f"{stem}.pdf"
        pdf.write_bytes(data)
        txt = subprocess.run(["pdftotext", "-enc", "UTF-8", str(pdf), "-"], check=True,
                             capture_output=True, text=True).stdout
        cleaned = clean(txt)
        dest = out / "fema_ics" / f"{stem}.txt"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(cleaned)
        manifest.append({"source": "fema_ics", "file": str(dest.relative_to(out)), "url": url,
                         "retrieved": today, "raw_sha256": sha(data), "clean_sha256": sha(cleaned.encode()),
                         "clean_bytes": len(cleaned.encode())})


def fetch_ecfr(raw: Path, out: Path, manifest: list[dict], today: str) -> None:
    titles = json.loads(get(f"{ECFR_API}/titles.json"))["titles"]
    asof = next(t["up_to_date_as_of"] for t in titles if t["number"] == 47)
    url = f"{ECFR_API}/full/{asof}/title-47.xml?part=97"
    data = get(url)
    (raw / "ecfr_part97").mkdir(parents=True, exist_ok=True)
    (raw / "ecfr_part97" / "part97.xml").write_bytes(data)
    xml = data.decode("utf-8", "replace")
    xml = re.sub(r"</(P|HEAD|DIV\d|FP|EXTRACT|NOTE|CITA)>", "\n\n", xml)
    xml = re.sub(r"<(?:TR|BR)[^>]*>", "\n", xml)
    xml = re.sub(r"<[^>]+>", "", xml)
    xml = html.unescape(xml)
    cleaned = clean(xml)
    dest = out / "ecfr_part97" / "part97.txt"
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(cleaned)
    manifest.append({"source": "ecfr_part97", "file": str(dest.relative_to(out)), "url": url,
                     "as_of": asof, "retrieved": today, "raw_sha256": sha(data),
                     "clean_sha256": sha(cleaned.encode()), "clean_bytes": len(cleaned.encode())})


def fetch_nws(raw: Path, out: Path, manifest: list[dict], today: str) -> None:
    (raw / "nws").mkdir(parents=True, exist_ok=True)
    shutil.rmtree(out / "nws", ignore_errors=True)
    ids: list[tuple[str, str, str]] = []
    for ptype in NWS_TYPES:
        for office in NWS_OFFICES:
            if len(ids) >= NWS_MAX_PRODUCTS * 3:
                break
            try:
                lst = json.loads(get(f"{NWS_API}/products/types/{ptype}/locations/{office}"))["@graph"]
            except Exception:
                continue
            for k, item in enumerate(lst[:2]):
                ids.append((ptype, office, f"{k}_{item['id']}"))
    n = 0
    per_type: dict[str, int] = {}
    for ptype, office, pid in ids:
        if n >= NWS_MAX_PRODUCTS or per_type.get(ptype, 0) >= 70:
            continue
        k, pid = pid.split("_", 1)
        url = f"{NWS_API}/products/{pid}"
        try:
            data = get(url)
            text = json.loads(data)["productText"]
        except Exception:
            continue
        cleaned = clean(text)[:NWS_MAX_BYTES]
        cleaned = cleaned[:cleaned.rfind("\n\n")] + "\n" if "\n\n" in cleaned else cleaned
        if len(cleaned) < 300:
            continue
        (raw / "nws" / f"{ptype}_{office}_{k}.json").write_bytes(data)
        dest = out / "nws" / f"{ptype}_{office}_{k}.txt"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(cleaned)
        manifest.append({"source": "nws", "file": str(dest.relative_to(out)), "url": url,
                         "retrieved": today, "raw_sha256": sha(data),
                         "clean_sha256": sha(cleaned.encode()), "clean_bytes": len(cleaned.encode())})
        per_type[ptype] = per_type.get(ptype, 0) + 1
        n += 1


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--raw", type=Path, required=True, help="directory for raw downloads (outside the repo)")
    ap.add_argument("--out", type=Path, default=Path("dictionaries/corpus"))
    ap.add_argument("--only", choices=["fema_ics", "ecfr_part97", "nws"], action="append")
    a = ap.parse_args()
    today = datetime.date.today().isoformat()
    manifest: list[dict] = []
    steps = {"fema_ics": fetch_fema, "ecfr_part97": fetch_ecfr, "nws": fetch_nws}
    for name, fn in steps.items():
        if a.only and name not in a.only:
            continue
        fn(a.raw, a.out, manifest, today)
    a.out.mkdir(parents=True, exist_ok=True)
    mf = a.out / "manifest.json"
    old = json.loads(mf.read_text()) if mf.exists() and a.only else []
    keep = [e for e in old if e["source"] not in (a.only or [])]
    mf.write_text(json.dumps(keep + manifest, indent=1) + "\n")
    print(f"{len(manifest)} files, {sum(e['clean_bytes'] for e in manifest)} bytes -> {a.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
