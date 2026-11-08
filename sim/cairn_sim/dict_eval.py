"""zstd dictionary evaluation (SPEC 12.2, decision 20.9, Stage 1 module ``dict_eval``, 21.2).

Corpus loader, dictionary training and held-out ratios for 100-500 byte
messages at 32, 64 and 112 kB dictionaries, with the PDU overhead (12.1), the
number of Cairn data blocks (126 payload bytes each, ADR 0009) and the air
time at representative rungs (``link_sim.Budget``).

Corpus (``dictionaries/corpus``, every source public domain; see
``dictionaries/REGISTRY.md``):

* *natural* text: FEMA ICS form templates and instructions, 47 CFR Part 97,
  a snapshot of NWS text products. Documents are split 80/20 by document
  (per source, ranked by SHA-256 of the file name), then cut into 100-500 byte
  messages, so no held-out message shares a document with a training message.
* *synthetic* messages (``generate_messages``): filled-in ICS-213, ICS-214,
  ICS-205, ICS-211, ICS-213RR forms, position reports and short weather and
  status reports, from templates with random fictitious names and callsigns.
  They are generated, not stored; (seed, count) reproduces them. The training
  and held-out messages come from the *same templates*, so the synthetic
  held-out ratios are an upper bound for forms traffic, not a measure of
  generalisation to unseen form layouts.

Measurement uses ``zstandard`` (python bindings of libzstd). The compressed
body is a standard zstd frame without checksum (the PDU's CRC-32 covers the
decoded body) and without the in-frame dictionary ID (the PDU carries it),
except where a variant says otherwise. A PDU that would not shrink is sent
raw (encoding 0), so a PDU's size is ``min(raw, compressed)`` plus its header.

    python -m cairn_sim.dict_eval            # writes sim/results/dict_eval.{md,json}
"""

from __future__ import annotations

import hashlib
import json
import random
import re
from dataclasses import dataclass, field
from pathlib import Path

import zstandard as zstd

from . import pdu as pdu_mod
from .link_sim import Budget

REPO = Path(__file__).resolve().parents[2]
CORPUS_DIR = REPO / "dictionaries" / "corpus"
EVAL_DICT_DIR = REPO / "dictionaries" / "eval"
RESULTS = Path(__file__).resolve().parents[1] / "results"

MIN_MSG, MAX_MSG = 100, 500
DICT_SIZES = (32 * 1024, 64 * 1024, 112 * 1024)
HELD_OUT = 0.20
SEED = 20261107                       # corpus split, chunk sizes and synthetic messages
SYNTH_COUNT = 7500
BUCKETS = ((100, 199), (200, 299), (300, 399), (400, 500))
NODICT_LEVELS = (1, 3, 9, 19)
DICT_LEVELS = (3, 19)
EVAL_LEVEL = 19                       # level of the headline "with dictionary" columns

# Training parameters, recorded in REGISTRY.md. python-zstandard's
# ``train_dictionary`` runs ZDICT_optimizeTrainFromBuffer_fastCover with these;
# libzstd's trainer takes no random seed (the split of samples is by position),
# so the "seed" of a dictionary is the corpus order and the SEED above.
TRAIN_PARAMS = dict(k=0, d=8, f=20, split_point=0.75, accel=1, steps=40, level=3, threads=1, dict_id=0)


# --- corpus loader -----------------------------------------------------------


@dataclass(frozen=True)
class Doc:
    source: str          # fema_ics | ecfr_part97 | nws
    name: str            # path relative to the corpus root
    text: str


def load_corpus(root: Path = CORPUS_DIR) -> list[Doc]:
    """Every ``*.txt`` under ``root``; the eCFR part is cut into one document
    per section so the 20% hold-out is spread across the rules."""
    docs: list[Doc] = []
    for f in sorted(root.rglob("*.txt")):
        rel = f.relative_to(root).as_posix()
        source = rel.split("/", 1)[0]
        text = f.read_text()
        if source == "ecfr_part97":
            parts = re.split(r"\n\n(?=Sec\. 97\.\d+ )", text)
            docs += [Doc(source, f"{rel}#{i:03d}", p.strip() + "\n") for i, p in enumerate(parts) if p.strip()]
        else:
            docs.append(Doc(source, rel, text))
    return docs


def _h(s: str) -> int:
    return int.from_bytes(hashlib.sha256(f"{SEED}:{s}".encode()).digest()[:8], "big")


def split_docs(docs: list[Doc], held_out: float = HELD_OUT) -> tuple[list[Doc], list[Doc]]:
    """Per source, the ``held_out`` fraction of documents with the smallest
    hash is held out (at least one per source with two or more documents)."""
    train: list[Doc] = []
    test: list[Doc] = []
    for source in sorted({d.source for d in docs}):
        ds = sorted((d for d in docs if d.source == source), key=lambda d: _h(d.name))
        k = max(1, round(held_out * len(ds))) if len(ds) > 1 else 0
        test += ds[:k]
        train += ds[k:]
    return train, test


def chunk_text(text: str, key: str) -> list[bytes]:
    """Cut a document into messages of 100-500 bytes at word boundaries, each
    size drawn uniformly (seeded by ``key``). Paragraph breaks become newlines.
    A tail shorter than 100 bytes is merged into the previous message when it
    fits and dropped otherwise."""
    rng = random.Random(_h(key))
    words = text.replace("\n\n", " \n ").split(" ")
    out: list[bytes] = []
    cur: list[str] = []
    size = 0
    target = rng.randint(MIN_MSG, MAX_MSG)
    for w in words:
        add = len(w) + (1 if cur else 0)
        if size + add > target and size >= MIN_MSG:
            out.append(" ".join(cur).replace(" \n ", "\n").strip().encode())
            cur, size, target = [], 0, rng.randint(MIN_MSG, MAX_MSG)
            add = len(w)
        cur.append(w)
        size += add
    if cur:
        tail = " ".join(cur).replace(" \n ", "\n").strip().encode()
        if len(tail) >= MIN_MSG:
            out.append(tail)
        elif out and len(out[-1]) + 1 + len(tail) <= MAX_MSG:
            out[-1] += b" " + tail
    return [m for m in out if MIN_MSG <= len(m) <= MAX_MSG]


def natural_messages(docs: list[Doc]) -> list[bytes]:
    return [m for d in docs for m in chunk_text(d.text, d.name)]


# --- synthetic messages ------------------------------------------------------

FIRST = ["Alex", "Maria", "Devon", "Priya", "Sam", "Lena", "Omar", "Ruth", "Tomas", "Keiko", "Dana", "Jonas",
         "Amira", "Pat", "Wen", "Nadia", "Luis", "Greta", "Ravi", "Joan", "Marcus", "Ines", "Hugo", "Tamsin"]
LAST = ["Alder", "Brook", "Castellan", "Dunmore", "Eastwick", "Fairweather", "Garnet", "Hollis", "Ingram",
        "Jessup", "Kestrel", "Lindqvist", "Marlow", "Norcross", "Okafor", "Pemberton", "Quill", "Redfern",
        "Salazar", "Thorne", "Underhill", "Vance", "Whitlock", "Yarrow"]
POSITIONS = ["Incident Commander", "Operations Section Chief", "Logistics Section Chief",
             "Planning Section Chief", "Safety Officer", "Communications Unit Leader", "Medical Unit Leader",
             "Staging Area Manager", "Shelter Manager", "Net Control", "Division A Supervisor",
             "Public Information Officer", "Resources Unit Leader", "Finance Section Chief",
             "Damage Assessment Lead", "Liaison Officer"]
INCIDENTS = ["Riverbend Flood", "North Ridge Wildfire", "County Ice Storm", "Harbor Road Landslide",
             "Cedar Valley Tornado Response", "Lakeshore Evacuation", "Mill Creek Fire", "Fairgrounds Exercise",
             "Ashford Power Outage", "Pine Hollow Search"]
PLACES = ["the high school shelter", "Station 4", "the county EOC", "the Route 9 bridge", "the fairgrounds",
          "the staging area at the quarry", "the community center", "Mile Marker 41", "the north trailhead",
          "the water treatment plant", "the Elm Street substation", "Hospital East entrance", "the ICP",
          "the airport hangar", "the school bus depot"]
RESOURCES = ["2 water tenders", "6 cases of bottled water", "one 5 kW generator", "4 volunteers with chainsaws",
             "10 cots and blankets", "a medical team", "2 portable radios and chargers", "one light tower",
             "3 dump trucks", "a mobile repeater trailer", "50 sandbags per hour", "fuel for 3 generators",
             "an ambulance strike team", "two search dog teams", "a field kitchen"]
TOPICS = [
    "Request {res} delivered to {place} by {t}.",
    "Road closed at {place}, use the alternate route and expect delays of about {n} minutes.",
    "{n} evacuees have checked in at {place}, capacity is {m}.",
    "Power is out to roughly {n} customers, crews report restoration by {t}.",
    "All teams report accountable. No injuries reported since the last update.",
    "Please confirm receipt and advise ETA of {res}.",
    "Water is rising about {n} inches per hour at {place}, recommend moving vehicles.",
    "Shift change at {t}, relief crew is briefed and en route to {place}.",
    "Cell service is down in the north sector. Radio is the primary link until further notice.",
    "Hold current assignments until the {pos} gives the next briefing at {t}.",
    "Medical needs: {n} persons requesting insulin or oxygen support at {place}.",
    "Fuel at {place} is low, request resupply before {t}.",
    "Debris cleared from {place}, one lane open to emergency traffic only.",
    "Wind has shifted to the northwest at {n} mph, smoke now drifting toward {place}.",
    "Damage assessment of the east side complete, {n} structures with major damage.",
    "Net check-in is complete with {n} stations, next net at {t} on the primary channel.",
]
WX_STATUS = ["clear", "partly cloudy", "overcast", "light rain", "heavy rain", "fog", "snow showers", "windy",
             "thunderstorms nearby", "freezing drizzle", "smoke haze"]
SPOT_NAMES = ["hilltop", "bridge", "shelter", "trailhead", "dam", "EOC", "firehouse", "ridge camp"]
CHANNELS = [("Command", "Tac 1"), ("Tactical", "Tac 2"), ("Logistics", "Log 1"), ("Medical", "Med 1"),
            ("Net", "Net 1"), ("Air-Ground", "AG 1"), ("Repeater", "RPT A"), ("Simplex", "SIM 3")]


def _call(rng: random.Random) -> str:
    """A fictitious callsign in the N0CALL style; any resemblance to a real one is coincidental."""
    return "N0" + "".join(rng.choice("ABCDEFGHJKLMNPQRSTUVWXYZ") for _ in range(rng.choice((2, 3))))


def _name(rng: random.Random) -> str:
    return f"{rng.choice(FIRST)} {rng.choice(LAST)}"


def _clock(rng: random.Random) -> str:
    return f"{rng.randint(0, 23):02d}{rng.choice((0, 15, 30, 45)):02d}"


def _date(rng: random.Random) -> str:
    return f"2026-{rng.randint(1, 12):02d}-{rng.randint(1, 28):02d}"


def _sentence(rng: random.Random) -> str:
    return rng.choice(TOPICS).format(res=rng.choice(RESOURCES), place=rng.choice(PLACES), t=_clock(rng),
                                     n=rng.randint(2, 480), m=rng.randint(50, 900), pos=rng.choice(POSITIONS))


def _body(rng: random.Random, lo: int, hi: int) -> str:
    return " ".join(_sentence(rng) for _ in range(rng.randint(lo, hi)))


def _ics213(rng: random.Random) -> str:
    return (f"ICS 213 GENERAL MESSAGE\nIncident: {rng.choice(INCIDENTS)}\n"
            f"To: {_name(rng)}, {rng.choice(POSITIONS)}\nFrom: {_name(rng)}, {rng.choice(POSITIONS)}\n"
            f"Subject: {rng.choice(['Resource request', 'Status update', 'Road closure', 'Shelter count', 'Fuel resupply', 'Shift change'])}\n"
            f"Date: {_date(rng)} Time: {_clock(rng)}\nMessage: {_body(rng, 1, 3)}\n"
            f"Approved by: {_name(rng)}, {rng.choice(POSITIONS)}")


def _ics214(rng: random.Random) -> str:
    rows = "\n".join(f"{_clock(rng)} {_sentence(rng)}" for _ in range(rng.randint(2, 4)))
    return (f"ICS 214 ACTIVITY LOG\nIncident: {rng.choice(INCIDENTS)}\n"
            f"Operational Period: {_date(rng)} {_clock(rng)} to {_clock(rng)}\n"
            f"Name: {_name(rng)}\nPosition: {rng.choice(POSITIONS)}\nActivity:\n{rows}")


def _ics205(rng: random.Random) -> str:
    rows = []
    for fn, nm in rng.sample(CHANNELS, rng.randint(2, 4)):
        f = rng.choice((146.52, 146.46, 147.42, 446.0, 446.1, 147.555, 146.58, 444.7))
        rows.append(f"{fn} | {nm} | RX {f:.3f} | TX {f:.3f} | Tone {rng.choice(['CSQ', '100.0', '123.0', '88.5', '131.8'])} | Mode A")
    return (f"ICS 205 INCIDENT RADIO COMMUNICATIONS PLAN\nIncident: {rng.choice(INCIDENTS)}\n"
            f"Prepared: {_date(rng)} {_clock(rng)}\nOperational Period: {_clock(rng)} to {_clock(rng)}\n"
            + "\n".join(rows) + f"\nSpecial Instructions: {_sentence(rng)}\nPrepared by: {_name(rng)}, Communications Unit Leader")


def _ics211(rng: random.Random) -> str:
    rows = "\n".join(f"{_call(rng)} {_name(rng)} {_clock(rng)} {rng.choice(['Net control', 'Staging', 'Shelter', 'Field team', 'Liaison'])}"
                     for _ in range(rng.randint(2, 6)))
    return f"ICS 211 CHECK-IN LIST\nIncident: {rng.choice(INCIDENTS)}\nDate: {_date(rng)}\n{rows}"


def _ics213rr(rng: random.Random) -> str:
    return (f"ICS 213 RR RESOURCE REQUEST\nIncident: {rng.choice(INCIDENTS)}\nOrder number: {rng.randint(1, 400)}\n"
            f"Requested: {rng.choice(RESOURCES)}\nDeliver to: {rng.choice(PLACES)}\nNeeded by: {_date(rng)} {_clock(rng)}\n"
            f"Priority: {rng.choice(['Urgent', 'Routine', 'Immediate'])}\nRequested by: {_name(rng)}, {rng.choice(POSITIONS)}\n"
            f"Remarks: {_sentence(rng)}")


def _position(rng: random.Random) -> str:
    lat, lon = rng.uniform(30, 48), rng.uniform(-122, -75)
    return (f"{_call(rng)}>{_call(rng)},WIDE1-1:!{abs(lat) // 1:.0f}{(lat % 1) * 60:05.2f}N/{abs(lon) // 1:03.0f}{(abs(lon) % 1) * 60:05.2f}W>"
            f"{rng.randint(0, 359):03d}/{rng.randint(0, 60):03d} {rng.choice(['Mobile 1', 'Staging', 'Shelter', 'Net control'])} "
            f"at {rng.choice(PLACES)}, {rng.randint(5, 98)}% battery, {rng.choice(['QSL', 'monitoring Tac 1', 'en route', 'on scene'])}. "
            f"Position {lat:.4f} {lon:.4f} at {_clock(rng)}Z")


def _wx(rng: random.Random) -> str:
    return (f"WX REPORT {_call(rng)} {rng.choice(SPOT_NAMES)} {_date(rng)} {_clock(rng)}L: "
            f"{rng.choice(WX_STATUS)}, temp {rng.randint(-10, 40)}C, wind {rng.choice(['N', 'NE', 'E', 'SE', 'S', 'SW', 'W', 'NW'])} "
            f"{rng.randint(0, 45)} gusting {rng.randint(10, 70)} km/h, pressure {rng.randint(985, 1035)} hPa, "
            f"rain {rng.randint(0, 40)} mm last hour. Visibility {rng.choice(['good', 'poor', '1 km', '5 km'])}. {_sentence(rng)}")


def _status(rng: random.Random) -> str:
    return (f"STATUS {_call(rng)} {_clock(rng)}: {rng.choice(PLACES)} {rng.choice(['secure', 'operational', 'evacuating', 'on standby'])}, "
            f"{rng.randint(1, 30)} personnel, {rng.choice(RESOURCES)} on hand. {_body(rng, 1, 2)} Next report {_clock(rng)}.")


_KINDS = (("ics213", _ics213, 5), ("ics214", _ics214, 3), ("ics205", _ics205, 2), ("ics211", _ics211, 1),
          ("ics213rr", _ics213rr, 2), ("position", _position, 2), ("wx", _wx, 2), ("status", _status, 3))


def generate_messages(count: int = SYNTH_COUNT, seed: int = SEED) -> list[bytes]:
    """``count`` synthetic messages of 100-500 bytes (rejection sampled), in a
    fixed, reproducible order. Kinds are weighted as in ``_KINDS``."""
    rng = random.Random(seed)
    makers = [m for _, m, w in _KINDS for _ in range(w)]
    out: list[bytes] = []
    while len(out) < count:
        msg = rng.choice(makers)(rng).encode("ascii", "replace")
        if MIN_MSG <= len(msg) <= MAX_MSG:
            out.append(msg)
    return out


def split_messages(msgs: list[bytes], held_out: float = HELD_OUT) -> tuple[list[bytes], list[bytes]]:
    """Every fifth message (by a seeded shuffle of the indices) is held out."""
    idx = list(range(len(msgs)))
    random.Random(SEED + 1).shuffle(idx)
    k = round(held_out * len(msgs))
    held = set(idx[:k])
    return ([m for i, m in enumerate(msgs) if i not in held], [m for i, m in enumerate(msgs) if i in held])


# --- dictionaries and compression ---------------------------------------------


def train(samples: list[bytes], size: int, **over) -> bytes:
    """Train a dictionary of ``size`` bytes (fastCover, ``TRAIN_PARAMS``)."""
    p = {**TRAIN_PARAMS, **over}
    return zstd.train_dictionary(size, samples, **p).as_bytes()


def dictionary_id(dict_bytes: bytes) -> bytes:
    """SPEC 12.1 Dictionary ID: the first 4 bytes of the SHA-256 of the file."""
    return hashlib.sha256(dict_bytes).digest()[:4]


class Codec:
    """One compression variant: ``level``, optional dictionary, frame options."""

    def __init__(self, level: int, dict_bytes: bytes | None = None, *, content_size: bool = False,
                 dict_id_in_frame: bool = False, magicless: bool = False):
        self.d = zstd.ZstdCompressionDict(dict_bytes) if dict_bytes else None
        fmt = zstd.FORMAT_ZSTD1_MAGICLESS if magicless else zstd.FORMAT_ZSTD1
        self.params = zstd.ZstdCompressionParameters.from_level(
            level, format=fmt, write_content_size=int(content_size), write_checksum=0,
            write_dict_id=int(dict_id_in_frame))
        self.c = zstd.ZstdCompressor(dict_data=self.d, compression_params=self.params)
        self.dec = zstd.ZstdDecompressor(dict_data=self.d, format=fmt)

    def compress(self, msg: bytes) -> bytes:
        return self.c.compress(msg)

    def decompress(self, blob: bytes, size: int) -> bytes:
        return self.dec.decompress(blob, max_output_size=size)


def pdu_bytes(body_len: int, encoding: int) -> int:
    """On-air bytes of a MESSAGE PDU whose body is ``body_len`` bytes (12.1)."""
    did = bytes(4) if encoding == pdu_mod.ENCODING_DICT else None
    return pdu_mod.Pdu(pdu_mod.PduType.MESSAGE, bytes(body_len), 0, 0, encoding, did).length + 2


def blocks_for(pdu_len: int) -> int:
    """Data blocks (126 payload bytes, ADR 0009) for one PDU at the start of a
    frame; the filler that pads the last block is not counted as saved."""
    return -(-pdu_len // pdu_mod.BLOCK_BYTES)


def bucket_of(n: int) -> int:
    for i, (lo, hi) in enumerate(BUCKETS):
        if lo <= n <= hi:
            return i
    raise ValueError(n)


@dataclass
class Row:
    """Aggregates for one (test set, scheme, size bucket)."""
    n: int = 0
    raw_bytes: int = 0
    body_bytes: int = 0       # bytes after min(raw, compressed)
    pdu_bytes: int = 0        # including the PDU header and CRC-32
    raw_pdu_bytes: int = 0    # the same message as a raw PDU
    blocks: int = 0
    raw_blocks: int = 0
    fallback: int = 0         # messages sent raw because zstd did not shrink them
    hist: dict[int, int] = field(default_factory=dict)   # data blocks -> messages

    def add(self, msg_len: int, comp_len: int, encoding: int) -> None:
        use_raw = comp_len >= msg_len
        body = msg_len if use_raw else comp_len
        enc = 0 if use_raw else encoding
        p = pdu_bytes(body, enc)
        rp = pdu_bytes(msg_len, 0)
        self.n += 1
        self.raw_bytes += msg_len
        self.body_bytes += body
        self.pdu_bytes += p
        self.raw_pdu_bytes += rp
        self.blocks += blocks_for(p)
        self.hist[blocks_for(p)] = self.hist.get(blocks_for(p), 0) + 1
        self.raw_blocks += blocks_for(rp)
        self.fallback += use_raw

    def summary(self) -> dict:
        n = max(self.n, 1)
        return {"n": self.n, "mean_raw": self.raw_bytes / n, "mean_body": self.body_bytes / n,
                "ratio": self.raw_bytes / max(self.body_bytes, 1),
                "mean_pdu": self.pdu_bytes / n, "pdu_ratio": self.raw_pdu_bytes / max(self.pdu_bytes, 1),
                "mean_blocks": self.blocks / n, "mean_raw_blocks": self.raw_blocks / n,
                "raw_fallback": self.fallback / n,
                "blocks_hist": {str(k): v for k, v in sorted(self.hist.items())}}


def evaluate(msgs: list[bytes], codec: Codec | None, encoding: int, *, verify: bool = True) -> list[Row]:
    """Per-bucket rows plus a final all-sizes row. ``codec=None`` is no compression."""
    rows = [Row() for _ in range(len(BUCKETS) + 1)]
    for m in msgs:
        if codec is None:
            comp_len = len(m)
        else:
            blob = codec.compress(m)
            comp_len = len(blob)
            if verify and comp_len < len(m) and codec.decompress(blob, len(m)) != m:
                raise AssertionError("round trip failed")
        for r in (rows[bucket_of(len(m))], rows[-1]):
            r.add(len(m), comp_len, encoding)
    return rows


# --- air time -------------------------------------------------------------------

RUNG_POINTS = ((0, 24), (3, 24), (3, 56), (6, 56))     # (MCS, tones): robust, mid and fast rungs


def air_time_s(blocks: float, mcs: int, width: int) -> float:
    """One broadcast frame carrying ``blocks`` blocks (SPEC 7.4, ``Budget``)."""
    b = Budget(mcs, width)
    n = int(round(blocks))
    return Budget(mcs, width, blocks_override=n, frame_s=b.frame_s).frame_time_s


def block_air_s(mcs: int, width: int) -> float:
    """Marginal air time of one more data block in a frame."""
    return air_time_s(2, mcs, width) - air_time_s(1, mcs, width)


# --- the experiment --------------------------------------------------------------


@dataclass
class Sets:
    nat_train: list[bytes]
    nat_test: list[bytes]
    syn_train: list[bytes]
    syn_test: list[bytes]
    docs_train: int
    docs_test: int


def build_sets(root: Path = CORPUS_DIR, synth_count: int = SYNTH_COUNT) -> Sets:
    docs = load_corpus(root)
    tr, te = split_docs(docs)
    syn_tr, syn_te = split_messages(generate_messages(synth_count))
    return Sets(natural_messages(tr), natural_messages(te), syn_tr, syn_te, len(tr), len(te))


def _mix(a: list[bytes], b: list[bytes]) -> list[bytes]:
    out = a + b
    random.Random(SEED + 2).shuffle(out)
    return out


def run(root: Path = CORPUS_DIR, synth_count: int = SYNTH_COUNT, sizes=DICT_SIZES) -> dict:
    import zstandard
    s = build_sets(root, synth_count)
    train_all = _mix(s.nat_train, s.syn_train)
    dicts = {sz: train(train_all, sz) for sz in sizes}
    sets = {"natural": s.nat_test, "synthetic": s.syn_test}
    res: dict = {
        "zstd_version": zstandard.ZSTD_VERSION and ".".join(map(str, zstandard.ZSTD_VERSION)),
        "python_zstandard": zstandard.__version__, "seed": SEED, "train_params": TRAIN_PARAMS,
        "buckets": [list(b) for b in BUCKETS],
        "corpus": {"docs_train": s.docs_train, "docs_test": s.docs_test,
                   "natural_train_msgs": len(s.nat_train), "natural_train_bytes": sum(map(len, s.nat_train)),
                   "synthetic_train_msgs": len(s.syn_train), "synthetic_train_bytes": sum(map(len, s.syn_train)),
                   "natural_test_msgs": len(s.nat_test), "synthetic_test_msgs": len(s.syn_test)},
        "dictionaries": {str(sz): {"bytes": len(d), "sha256": hashlib.sha256(d).hexdigest(),
                                   "pdu_id": dictionary_id(d).hex()} for sz, d in dicts.items()},
        "results": {}, "framing": {}, "ablation": {}, "air": {}, "rungs": [list(r) for r in RUNG_POINTS],
    }
    for setname, msgs in sets.items():
        schemes: dict[str, list[Row]] = {"none": evaluate(msgs, None, 0)}
        for lv in NODICT_LEVELS:
            schemes[f"zstd{lv}"] = evaluate(msgs, Codec(lv), 1)
        for sz, d in dicts.items():
            for lv in DICT_LEVELS:
                schemes[f"dict{sz // 1024}k_l{lv}"] = evaluate(msgs, Codec(lv, d), pdu_mod.ENCODING_DICT)
        res["results"][setname] = {k: [r.summary() for r in v] for k, v in schemes.items()}
    # frame options (64 kB dictionary, level 19, natural + synthetic together)
    allm = s.nat_test + s.syn_test
    d64 = dicts[64 * 1024] if 64 * 1024 in dicts else next(iter(dicts.values()))
    variants = {
        "no dict: content size in frame": (None, dict(content_size=True), 1),
        "no dict: no content size": (None, dict(), 1),
        "no dict: magicless": (None, dict(magicless=True), 1),
        "dict: dict ID and content size in frame (python default)": (d64, dict(content_size=True, dict_id_in_frame=True), 2),
        "dict: no dict ID, no content size": (d64, dict(), 2),
        "dict: magicless": (d64, dict(magicless=True), 2),
    }
    for name, (d, kw, enc) in variants.items():
        res["framing"][name] = evaluate(allm, Codec(EVAL_LEVEL, d, **kw), enc)[-1].summary()
    # ablation (64 kB): which training material matters, tested on both held-out sets
    for label, tr in (("natural only", s.nat_train), ("synthetic only", s.syn_train), ("both", train_all)):
        d = train(tr, 64 * 1024)
        res["ablation"][label] = {
            setname: evaluate(msgs, Codec(EVAL_LEVEL, d), 2)[-1].summary() for setname, msgs in sets.items()}
    # air time: per message at representative rungs, for each held-out set
    for mcs, w in RUNG_POINTS:
        res["air"][f"{mcs}/{w}"] = {"block_s": block_air_s(mcs, w), "frame_1_block_s": air_time_s(1, mcs, w)}
    # one-time cost of fetching a dictionary over the air (12.2 dictionary fetch): file bytes at net rate
    res["dictionary_air_s"] = {
        str(sz // 1024): {f"{m}/{w}": 8 * len(d) / (Budget(m, w, frame_s=4.0).net_bps) for m, w in RUNG_POINTS}
        for sz, d in dicts.items()}
    return res


# --- report ----------------------------------------------------------------------


def _scheme(key: str) -> str:
    if key == "none":
        return "no compression"
    if key.startswith("zstd"):
        return f"zstd -{key[4:]}"
    sz, lv = key[4:].split("k_l")
    return f"{sz} kB dict, -{lv}"


def _air(res: dict, mcs: int, w: int, summary: dict) -> float:
    """Mean air time per message: each message's own block count, one frame."""
    a = res["air"][f"{mcs}/{w}"]
    hist = summary["blocks_hist"]
    total = sum(int(v) for v in hist.values())
    return sum(int(v) * (a["frame_1_block_s"] + (int(k) - 1) * a["block_s"]) for k, v in hist.items()) / total


def render(res: dict) -> str:
    c = res["corpus"]
    lines = ["## Dictionary evaluation (SPEC 12.2, 20.9, 21.2 `dict_eval`)", "",
             "Generated by `python -m cairn_sim.dict_eval`. **Evaluation artifacts, not released dictionaries**: "
             "SPEC 20.9 requires two signing keyholders for a release, and none of the dictionaries measured "
             "here has been published.", "",
             f"zstd {res['zstd_version']} (python-zstandard {res['python_zstandard']}), seed {res['seed']}, "
             f"training parameters `{json.dumps(res['train_params'])}`. Dictionaries trained on the 80% "
             f"training split and measured on the 20% held out (split by document for natural text, so no "
             f"held-out message shares a document with training data).", "",
             "| Training material | Training | Held out |", "| --- | --- | --- |",
             f"| Natural text (FEMA ICS forms, 47 CFR 97, NWS products), documents | {c['docs_train']} | {c['docs_test']} |",
             f"| Natural messages (100-500 B) | {c['natural_train_msgs']} ({c['natural_train_bytes'] / 1e6:.2f} MB) | {c['natural_test_msgs']} |",
             f"| Synthetic form and report messages | {c['synthetic_train_msgs']} ({c['synthetic_train_bytes'] / 1e6:.2f} MB) | {c['synthetic_test_msgs']} |",
             "", "Dictionaries:", "", "| Size | Bytes | SHA-256 | PDU Dictionary ID (first 4 bytes) |", "| --- | --- | --- | --- |"]
    for sz, d in res["dictionaries"].items():
        lines.append(f"| {int(sz) // 1024} kB | {d['bytes']} | `{d['sha256'][:16]}...` | `{d['pdu_id']}` |")
    lines += ["", "The PDU overhead (12.1) is 9 bytes (Length 2, Type, Stream, Encoding, CRC-32 4) for encodings 0 and 1 "
              "and 13 with a dictionary (Dictionary ID 4 more). A data block carries 126 bytes (ADR 0009). "
              "Ratios are bytes in over bytes out summed over the bucket. \"Body\" is the compressed body only "
              "(a message zstd would not shrink is sent raw, encoding 0); \"PDU\" adds the header and CRC-32 "
              "and is what goes on air. zstd frames are written without checksum, content size or in-frame "
              "dictionary ID.", ""]
    notes = {"natural": "Natural held-out text (FEMA form text, CFR rules and NWS products from documents the dictionary never saw). "
                        "This is prose and form-layout text; it is the conservative measure.",
             "synthetic": "Synthetic held-out messages (filled-in ICS forms, position and weather reports). Training and "
                          "held-out messages share templates, so this is an upper bound for traffic whose layout the "
                          "dictionary has seen, not a prediction for unseen layouts."}
    cols = ["none", "zstd1", "zstd3", "zstd9", "zstd19"] + [f"dict{s // 1024}k_l{EVAL_LEVEL}" for s in map(int, res["dictionaries"])]
    for setname in ("natural", "synthetic"):
        r = res["results"][setname]
        lines += [f"### {setname.capitalize()} held-out messages", "", notes[setname], "",
                  "#### Body ratio (bytes in / compressed body bytes)", "",
                  "| Bucket (B) | n | mean B | " + " | ".join(_scheme(k) for k in cols[1:]) + " |",
                  "| --- | --- | --- | " + " | ".join("---" for _ in cols[1:]) + " |"]
        labels = [f"{lo}-{hi}" for lo, hi in res["buckets"]] + ["100-500"]
        for i, lab in enumerate(labels):
            s0 = r["none"][i]
            lines.append(f"| {lab} | {s0['n']} | {s0['mean_raw']:.0f} | "
                         + " | ".join(f"{r[k][i]['ratio']:.2f}" for k in cols[1:]) + " |")
        lines += ["", "#### On-air PDU bytes and data blocks per message", "",
                  "| Bucket (B) | " + " | ".join(_scheme(k) for k in cols) + " |",
                  "| --- | " + " | ".join("---" for _ in cols) + " |"]
        for i, lab in enumerate(labels):
            lines.append(f"| {lab} | " + " | ".join(
                f"{r[k][i]['mean_pdu']:.0f} B, {r[k][i]['mean_blocks']:.2f} blk" for k in cols) + " |")
        lines += ["", "Saving against sending raw (all sizes); PDU ratio counts header and CRC-32; "
                  "raw fallback is the share of messages zstd did not shrink (sent as encoding 0):", "",
                  "| Scheme | PDU ratio | Blocks saved | Raw fallback |", "| --- | --- | --- | --- |"]
        for k in cols[1:]:
            a = r[k][-1]
            lines.append(f"| {_scheme(k)} | {a['pdu_ratio']:.2f} | {a['mean_raw_blocks'] - a['mean_blocks']:.2f} "
                         f"({100 * (1 - a['mean_blocks'] / a['mean_raw_blocks']):.0f}%) | {100 * a['raw_fallback']:.1f}% |")
        lines += ["", "#### Air time per message (one frame, broadcast, SPEC 7.4 `Budget`)", "",
                  "| Rung (MCS / tones) | " + " | ".join(_scheme(k) for k in cols) + " |",
                  "| --- | " + " | ".join("---" for _ in cols) + " |"]
        for mcs, w in res["rungs"]:
            lines.append(f"| {mcs} / {w} | " + " | ".join(
                f"{_air(res, mcs, w, r[k][-1]):.2f} s" for k in cols) + " |")
        lines += [""]
    lines += ["### Cost of getting a dictionary (12.2, dictionary fetch) and break-even", "",
              "Fetching the file as FILE-CHUNK PDUs at a rung's net rate (`Budget`, 4 s frames, ACK cycle included), "
              "and the number of natural held-out messages (100-500 B) after which the air time saved per "
              "message has paid for it. Delivery by any other route (a download in advance) costs no air time.", "",
              "| Dictionary | " + " | ".join(f"{m}/{w}: fetch s (break-even msgs)" for m, w in res["rungs"]) + " |",
              "| --- | " + " | ".join("---" for _ in res["rungs"]) + " |"]
    for sz, per in res["dictionary_air_s"].items():
        cells = []
        for m, w in res["rungs"]:
            nat = res["results"]["natural"]
            saved = _air(res, m, w, nat["zstd19"][-1]) - _air(res, m, w, nat[f"dict{sz}k_l{EVAL_LEVEL}"][-1])
            t = per[f"{m}/{w}"]
            cells.append(f"{t:.0f} s ({t / saved:.0f})" if saved > 0 else f"{t:.0f} s (never)")
        lines.append(f"| {sz} kB | " + " | ".join(cells) + " |")
    lines += ["", "Break-even compares the dictionary against zstd without one; against raw the counts are lower.", ""]
    lines += ["### Frame options (natural + synthetic held out, 64 kB dictionary, level 19)", "",
              "The SPEC says only \"zstd\"; it does not say whether the zstd frame header, magic number and "
              "content size are sent. On 100-500 byte messages these few bytes matter.", "",
              "| Variant | Mean body (B) | Body ratio | Mean PDU (B) | Mean blocks |", "| --- | --- | --- | --- | --- |"]
    for k, a in res["framing"].items():
        lines.append(f"| {k} | {a['mean_body']:.1f} | {a['ratio']:.2f} | {a['mean_pdu']:.1f} | {a['mean_blocks']:.2f} |")
    lines += ["", "### Which training material matters (64 kB, level 19, all sizes)", "",
              "| Dictionary trained on | Natural held-out ratio | Synthetic held-out ratio |", "| --- | --- | --- |"]
    for k, a in res["ablation"].items():
        lines.append(f"| {k} | {a['natural']['ratio']:.2f} | {a['synthetic']['ratio']:.2f} |")
    lines += [""]
    return "\n".join(lines)


def write_artifacts(res: dict, dicts: dict[int, bytes] | None = None) -> None:
    RESULTS.mkdir(exist_ok=True)
    (RESULTS / "dict_eval.json").write_text(json.dumps(res, indent=1) + "\n")
    (RESULTS / "dict_eval.md").write_text(render(res))


def main() -> int:
    import argparse
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--corpus", type=Path, default=CORPUS_DIR)
    ap.add_argument("--write-dicts", action="store_true",
                    help="also write the evaluation dictionaries to dictionaries/eval/")
    a = ap.parse_args()
    res = run(a.corpus)
    write_artifacts(res)
    if a.write_dicts:
        s = build_sets(a.corpus)
        EVAL_DICT_DIR.mkdir(parents=True, exist_ok=True)
        for sz in DICT_SIZES:
            d = train(_mix(s.nat_train, s.syn_train), sz)
            (EVAL_DICT_DIR / f"eval-forms-{sz // 1024}k.zdict").write_bytes(d)
    print(f"wrote {RESULTS / 'dict_eval.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
