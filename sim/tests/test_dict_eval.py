"""dict_eval (SPEC 12.2, 20.9, 21.2): corpus rules, splits, PDU overhead, round trips."""

import hashlib
import json
import re

import pytest

from cairn_sim import dict_eval as de
from cairn_sim import pdu


def test_pdu_overhead_matches_spec_and_pdu_module():
    # Length 2 + Type + Stream + Encoding + CRC-32 4 = 9; Dictionary ID adds 4
    assert de.pdu_bytes(0, 0) == 9 and de.pdu_bytes(0, 1) == 9 and de.pdu_bytes(0, 2) == 13
    p = pdu.Pdu(pdu.PduType.MESSAGE, b"x" * 50, 0, 0, 2, bytes(4))
    assert len(p.encode()) == de.pdu_bytes(50, 2)


def test_blocks_for():
    assert de.blocks_for(126) == 1 and de.blocks_for(127) == 2 and de.blocks_for(1) == 1


def test_chunks_are_100_to_500_bytes_and_deterministic():
    text = " ".join(f"word{i}" for i in range(2000))
    a = de.chunk_text(text, "k")
    assert a == de.chunk_text(text, "k") and len(a) > 20
    assert all(de.MIN_MSG <= len(m) <= de.MAX_MSG for m in a)


def test_synthetic_messages_reproducible_bounded_and_ascii():
    a = de.generate_messages(300, seed=7)
    assert a == de.generate_messages(300, seed=7) and a != de.generate_messages(300, seed=8)
    assert all(de.MIN_MSG <= len(m) <= de.MAX_MSG and m.isascii() for m in a)
    assert not any(b"@" in m for m in a)                 # no e-mail addresses
    kinds = {m.split(b"\n", 1)[0][:6] for m in a}
    assert len(kinds) >= 5                               # a spread of form types


def test_message_split_is_20_percent_and_disjoint():
    msgs = [str(i).encode() * 40 for i in range(500)]
    tr, te = de.split_messages(msgs)
    assert len(te) == 100 and len(tr) == 400 and not set(tr) & set(te)


def test_doc_split_holds_out_20_percent_per_source():
    docs = [de.Doc("a", f"a/{i}.txt", "x") for i in range(10)] + [de.Doc("b", f"b/{i}.txt", "x") for i in range(5)]
    tr, te = de.split_docs(docs)
    assert sorted(d.source for d in te).count("a") == 2 and sorted(d.source for d in te).count("b") == 1
    assert not {d.name for d in tr} & {d.name for d in te}


def _small_samples():
    return de.generate_messages(1500, seed=3)


def test_training_is_reproducible_and_round_trips():
    samples = _small_samples()
    d1 = de.train(samples, 8 * 1024)
    assert d1 == de.train(samples, 8 * 1024)
    assert len(d1) <= 8 * 1024
    codec = de.Codec(19, d1)
    for m in de.generate_messages(40, seed=99):
        blob = codec.compress(m)
        assert codec.decompress(blob, len(m)) == m
    assert len(de.dictionary_id(d1)) == 4


def test_dictionary_helps_forms_and_magicless_saves_four_bytes():
    samples = _small_samples()
    d = de.train(samples, 16 * 1024)
    test = de.generate_messages(60, seed=1234)
    plain = sum(len(de.Codec(19).compress(m)) for m in test)
    with_d = sum(len(de.Codec(19, d).compress(m)) for m in test)
    assert with_d < 0.7 * plain
    m = test[0]
    std, ml = de.Codec(19, d).compress(m), de.Codec(19, d, magicless=True).compress(m)
    assert len(std) - len(ml) == 4
    assert de.Codec(19, d, magicless=True).decompress(ml, len(m)) == m


def test_evaluate_counts_raw_fallback():
    msgs = [bytes(range(256))[:200]]       # incompressible-ish
    rows = de.evaluate(msgs, de.Codec(3), 1)
    assert rows[-1].n == 1 and rows[-1].body_bytes <= 200
    assert rows[-1].pdu_bytes == de.pdu_bytes(rows[-1].body_bytes, 0 if rows[-1].fallback else 1)


def test_air_time_grows_with_blocks_and_falls_with_rung():
    assert de.air_time_s(2, 3, 24) > de.air_time_s(1, 3, 24)
    assert de.block_air_s(0, 24) > de.block_air_s(3, 24) > 0


# --- the committed corpus and registry (SPEC 12.2 license rules) ------------------


def test_corpus_manifest_matches_files_and_registry_lists_every_source():
    root = de.CORPUS_DIR
    manifest = json.loads((root / "manifest.json").read_text())
    assert manifest
    registry = (de.REPO / "dictionaries" / "REGISTRY.md").read_text()
    for e in manifest:
        data = (root / e["file"]).read_bytes()
        assert hashlib.sha256(data).hexdigest() == e["clean_sha256"], e["file"]
        assert e["url"].startswith("https://") and e["retrieved"]
    for source in {e["source"] for e in manifest}:
        assert source in registry
    # every corpus file is accounted for in the manifest
    assert {p.relative_to(root).as_posix() for p in root.rglob("*.txt")} == {e["file"] for e in manifest}


def test_corpus_has_no_email_addresses_and_is_ascii():
    for f in de.CORPUS_DIR.rglob("*.txt"):
        t = f.read_text()
        assert t.isascii() and not re.search(r"\S+@\S+", t), f


def test_registry_lists_the_eval_dictionaries_as_unreleased():
    registry = (de.REPO / "dictionaries" / "REGISTRY.md").read_text()
    assert "not a released dictionary" in registry.lower() or "evaluation artifact" in registry.lower()
    for f in sorted((de.REPO / "dictionaries" / "eval").glob("*.zdict")):
        assert hashlib.sha256(f.read_bytes()).hexdigest() in registry, f.name


@pytest.mark.slow
def test_full_run_headline_dictionaries_beat_plain_zstd():
    res = de.run()
    for setname in ("natural", "synthetic"):
        r = res["results"][setname]
        assert r["dict112k_l19"][-1]["ratio"] > r["zstd19"][-1]["ratio"] > 1.0
