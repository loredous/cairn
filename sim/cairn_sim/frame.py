"""Frame assembly and reception (SPEC 6, 8.1).

A frame is ``preamble (4) | header (7) | [probe (2)] | control blocks |
data blocks``. The receiver finds the frame by cross-correlating with the
known preamble waveform (or takes a start from the SPEC 6.1 detector),
estimates the per-tone channel and noise from the two S2 symbols on the base
tones and from the width probe above them, tracks common phase and timing
drift with the continuous pilots, and decodes the header, every control
block and every data block.

Header-first reception (SPEC 8.2). ``receive(rx)`` without a
``FrameConfig`` decodes the preamble and header alone, unpacks the header
fields (``fields.Header``) and takes the frame's geometry from them: the
data rung, the control and data block counts and whether a width probe
follows. Width and tone mask are link state, not header fields, so they come
from ``link`` (default: 24 tones, no mask), which may be a callable that
gives the link state for the decoded header; the header's Mask ID selects
the mask (ADR 0011 ``mask-id``); the prefix comes from the detector (SPEC
6.1). The result names each data block's sequence (``RxResult.block_sequences``,
from the header and the block map of ADR 0008). ``build`` takes header fields and control blocks
(``fields``) the same way; without them it sends pseudo-random header bits,
as the Stage 1 simulator always has (``vectors/bitmap/``).

Control blocks (SPEC 8.1, ADR 0005). Up to three 256-bit blocks, each coded
at MCS 0 (512 bits), fill their own run of symbols between the probe and the
data, on the base data tones less masked ones, with pilots 3 and 20 and
every active tone scaled by sqrt(24 / active tones). Scrambler positions:
header 0, control block i 1 + i, data block j 1 + n_control + j.

Tone mask (SPEC 5.4). ``FrameConfig.mask`` is the set of usable tones. It
applies to data symbols only: preamble, header and width probe never change,
so any station can decode the header and the probe measures every tone up to
the width. In a data symbol a masked tone carries nothing (a masked pilot 37
or 52 is simply absent), coded bits fill the remaining data tones in order,
and every active tone is scaled by ``FrameConfig.data_gain`` =
sqrt(width / active tones) so the symbol's total power is the same as an
unmasked one. The receiver divides that known factor back out.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field, replace
from functools import cached_property

import numpy as np
from scipy import signal

from . import fields, ldpc, mapping, ofdm, params, preamble
from .crc import append_crc

PILOT_C_INIT = 0x0B1D   # pilot BPSK sequence seed (ADR 0005 pilot-sequence)
PROBE_C_INIT = 0x0D0E   # width-probe BPSK sequence seed (ADR 0005 probe-sequence)
PAD_POSITION = 255      # scrambler position for padding bits (ADR 0005 scrambler-sequence)
HEADER_POSITION = 0     # scrambler position of the header; control block i uses 1 + i,
                        # data block j 1 + n_control + j
QPSK = 2


class UnknownMask(ValueError):
    """A session frame's header names a mask ID the link state does not hold
    (ADR 0011 ``mask-id``): its control and data blocks count as failed."""


@dataclass(frozen=True)
class LinkState:
    """What a station in a session must know that the header does not say
    (SPEC 5.4, 8.2): the width W of the frames (ADR 0011 ``session-width``),
    the tone masks by ID, the width probe's extent in tones (``cap``; None:
    the width, as in every session frame since ADR 0011
    ``width-probe-extent``), and optionally the prefix (None: detect it,
    SPEC 6.1).

    Masks (ADR 0011 ``mask-id``): ``mask`` is the current mask, with ID
    ``mask_id``; ``pending`` an (ID, mask) reported but not yet named by a
    header, so at most two are held. A session frame's header names the
    mask its control and data blocks use (``mask_for``). A receiver holds
    the masks it reported for its direction; a sender builds with the mask
    its peer last reported and names its ID.

    It applies to session frames (header flag bit 3), except that CONNECT
    and CONNECT-ACK are unmasked. A frame outside a session takes its width
    from its own probe (ADR 0007 ``broadcast-width-from-probe``) or is 24
    tones without one.
    """

    width: int = params.BASE_TONES
    mask: frozenset[int] | None = None
    cp: int | None = None
    cap: int | None = None
    mask_id: int = 0
    pending: tuple[int, frozenset[int] | None] | None = None

    def __post_init__(self) -> None:
        ids = [self.mask_id] + ([self.pending[0]] if self.pending is not None else [])
        if any(not 0 <= i < fields.MASK_IDS for i in ids) or len(set(ids)) != len(ids):
            raise ValueError(f"mask IDs {ids}: 0-3, current and pending different")

    @classmethod
    def from_cap(cls, cap: int, mask: frozenset[int] | None = None, cp: int | None = None) -> LinkState:
        """A link ``cap`` tones wide whose probe reaches the same tones."""
        return cls(width=cap, mask=mask, cp=cp, cap=cap)

    @classmethod
    def session(cls, sender: fields.ConnectBlock, receiver_capabilities: int, *,
                mask: frozenset[int] | None = None, mask_id: int = 0, cp: int | None = None) -> LinkState:
        """The frames a station sends in a session, from the B.3 blocks of
        setup: W = the sender's width cap + 1 when the receiver advertised
        capability bit 0, else 24 (``fields.session_width``), with the probe
        to W (ADR 0011 ``session-width``, ``width-probe-extent``). A receiver
        passes its peer's B.3 and its own capabilities; a sender its own B.3
        and the peer's capabilities."""
        w = fields.session_width(sender.width_cap, receiver_capabilities)
        return cls(width=w, mask=mask, cp=cp, cap=w, mask_id=mask_id)

    @classmethod
    def probe_to(cls, width_cap: int, cp: int | None = None) -> LinkState:
        """CONNECT, a link-test PROBE and its SOUND reply: unmasked, probing
        tones 0 .. width cap (ADR 0011 ``width-probe-extent``)."""
        return cls(width=width_cap + 1, cp=cp, cap=width_cap + 1)

    @property
    def masks(self) -> dict[int, frozenset[int] | None]:
        """Mask ID -> usable tones (None: every tone)."""
        out = {self.mask_id: self.mask}
        if self.pending is not None:
            out[self.pending[0]] = self.pending[1]
        return out

    def mask_for(self, header: fields.Header) -> frozenset[int] | None:
        """The mask of a frame with this header: the one its Mask ID names in
        a session frame, none in CONNECT, CONNECT-ACK and outside a session.
        Raises ``UnknownMask`` for an ID not held."""
        if not header.session or header.frame_type in (fields.FrameType.CONNECT, fields.FrameType.CONNECT_ACK):
            return None
        masks = self.masks
        if header.mask_id not in masks:
            raise UnknownMask(f"mask ID {header.mask_id} is not held (held: {sorted(masks)})")
        return masks[header.mask_id]


@dataclass(frozen=True)
class FrameConfig:
    width: int = params.BASE_TONES
    mcs: int = 0
    n_blocks: int = 1
    cp: int = params.CP_SHORT
    clip: bool = True
    clip_db: float | None = None  # clip level, dB above RMS; default: the rung's (params.clip_db)
    probe: bool | None = None   # width probe after the header; default: when width > 24
    mask: frozenset[int] | None = None  # usable tones (SPEC 5.4); None: every tone usable
    interleaver: str = "linear"         # mapping.INTERLEAVERS; "random" is an evaluation reference
    n_control: int = 0                  # control blocks (0-3), MCS 0 on the base tones
    cap: int | None = None              # the probe's extent in tones (ADR 0011); None: the width

    def __post_init__(self) -> None:
        if not 0 <= self.mcs < len(params.RUNGS):
            raise ValueError(f"no rung MCS {self.mcs}")
        if not 0 <= self.n_blocks <= fields.MAX_DATA_BLOCKS:
            raise ValueError(f"data block count {self.n_blocks} outside 0-{fields.MAX_DATA_BLOCKS}")
        if not 0 <= self.n_control <= fields.MAX_CONTROL_BLOCKS:
            raise ValueError(f"control block count {self.n_control} outside 0-{fields.MAX_CONTROL_BLOCKS}")
        if self.clip_db is None:
            # The whole frame (preamble, header, probe, control, data) is
            # clipped at the data rung's level, so every symbol shares one
            # per-tone scale and the S2 channel estimate stays valid for the
            # data. A frame with no data blocks uses 7 dB (SPEC 7.1).
            object.__setattr__(self, "clip_db", params.clip_db(self.mcs) if self.n_blocks
                               else params.CLIP_DB_ABOVE_RMS)
        if not params.BASE_TONES <= self.width <= params.MAX_TONES:
            raise ValueError(f"width must be {params.BASE_TONES}-{params.MAX_TONES} tones")
        if self.cap is None:
            object.__setattr__(self, "cap", self.width)
        if not self.width <= self.cap <= params.MAX_TONES:
            raise ValueError(f"cap {self.cap} must be {self.width}-{params.MAX_TONES} tones: neither the "
                             "data nor the probe exceeds the sender's cap (SPEC 5.4)")
        if self.mask is not None:
            object.__setattr__(self, "mask", frozenset(int(k) for k in self.mask))
        if self.interleaver not in mapping.INTERLEAVERS:
            raise ValueError(f"unknown interleaver {self.interleaver!r}")
        params.masked_tones(self.width, self.mask)          # validates the mask
        if self.probe is None:
            object.__setattr__(self, "probe", self.needs_probe)
        elif self.needs_probe and not self.probe:
            raise ValueError("data blocks on tones above 23 need the width probe (ADR 0007 "
                             "width-probe-every-wide-frame, ADR 0011 width-probe-extent)")
        if self.n_control and not self.control_tones:
            raise ValueError("the mask leaves no control tones")

    @classmethod
    def from_header(cls, header: fields.Header, link: LinkState | None = None, *,
                    cp: int | None = None, **kw) -> FrameConfig:
        """The frame a header describes, at the link's width and the mask its
        Mask ID names (SPEC 8.2, ADR 0011; ``LinkState.mask_for``, which raises
        ``UnknownMask``).

        The rung is the header's when it has data blocks (MCS 0 otherwise:
        control blocks are always MCS 0 and the field is then ignored). The
        prefix is ``cp``, else the link's, else the short one.
        """
        link = link or LinkState()
        cp = cp if cp is not None else (link.cp if link.cp is not None else params.CP_SHORT)
        if header.n_data and not 0 <= header.mcs < len(params.RUNGS):
            raise ValueError(f"header MCS {header.mcs} is a reserved rung")
        return cls(width=link.width, mcs=header.mcs if header.n_data else 0, n_blocks=header.n_data,
                   n_control=header.n_control, cp=cp, probe=header.probe, mask=link.mask_for(header),
                   cap=link.cap if link.cap is not None else link.width, **kw)

    @property
    def needs_probe(self) -> bool:
        """ADR 0007, ADR 0011 ``width-probe-extent``: data symbols use a tone
        above 23 (a data tone or an upper pilot), so the receiver needs the
        probe of the same frame to estimate it."""
        return self.n_blocks > 0 and self.data_active_tones[-1] >= params.BASE_TONES

    @property
    def has_probe(self) -> bool:
        return bool(self.probe)

    @property
    def n_tones(self) -> int:
        """Tones in the frame's grid: the probe's extent (the sender's cap)
        when it has a probe, else the base tones (without a probe no symbol
        uses a tone above 23, ``needs_probe``)."""
        return self.cap if self.has_probe else params.BASE_TONES

    @property
    def probe_symbols(self) -> int:
        return params.PROBE_SYMBOLS if self.has_probe else 0

    @property
    def rung(self) -> params.Rung:
        return params.RUNGS[self.mcs]

    @property
    def stride(self) -> int:
        return params.N_USEFUL + self.cp

    @cached_property
    def base_data_tones(self) -> tuple[int, ...]:
        return params.data_tones_for(params.BASE_TONES)

    @cached_property
    def masked(self) -> frozenset[int]:
        """Tones below the width that data symbols leave silent."""
        return params.masked_tones(self.width, self.mask)

    @cached_property
    def data_tones(self) -> tuple[int, ...]:
        """Tones carrying coded bits in data symbols, lowest first."""
        return params.data_tones_for(self.width, self.masked)

    @cached_property
    def data_pilots(self) -> tuple[int, ...]:
        """Pilot tones present in data symbols (37 and 52 drop out when masked)."""
        return tuple(p for p in params.pilots_for(self.width) if p not in self.masked)

    @cached_property
    def data_active_tones(self) -> tuple[int, ...]:
        return tuple(sorted(self.data_tones + self.data_pilots))

    @property
    def data_gain(self) -> float:
        """Amplitude factor on every active tone of a data symbol: total
        power stays that of ``width`` unit-power tones whatever the mask."""
        return float(np.sqrt(self.width / len(self.data_active_tones)))

    @property
    def header_symbols(self) -> int:
        e = params.HEADER_CODE.coded_bits[0]
        return -(-e // (QPSK * len(self.base_data_tones)))

    @cached_property
    def control_tones(self) -> tuple[int, ...]:
        """Tones carrying coded bits in control symbols: base data tones less masked ones."""
        return tuple(k for k in self.base_data_tones if k not in self.masked)

    @property
    def control_pilots(self) -> tuple[int, ...]:
        return params.pilots_for(params.BASE_TONES)        # 3 and 20 cannot be masked

    @cached_property
    def control_active_tones(self) -> tuple[int, ...]:
        return tuple(sorted(self.control_tones + self.control_pilots))

    @property
    def control_gain(self) -> float:
        """Amplitude factor on every active tone of a control symbol (ADR 0005):
        a masked control symbol keeps the power of 24 unit-power tones."""
        return float(np.sqrt(params.BASE_TONES / len(self.control_active_tones)))

    @property
    def control_symbols(self) -> int:
        e = params.CONTROL_CODE.coded_bits[0]
        return -(-self.n_control * e // (QPSK * len(self.control_tones))) if self.n_control else 0

    @property
    def first_probe(self) -> int:
        """Index of the first probe symbol (of the first control or data symbol without a probe)."""
        return params.PREAMBLE_SYMBOLS + self.header_symbols

    @property
    def first_control(self) -> int:
        """Index of the first control symbol (of the first data symbol without control blocks)."""
        return params.PREAMBLE_SYMBOLS + self.header_symbols + self.probe_symbols

    @property
    def first_data(self) -> int:
        return self.first_control + self.control_symbols

    @property
    def data_bits(self) -> int:
        return self.n_blocks * self.rung.coded_bits

    @property
    def data_symbols(self) -> int:
        per_symbol = self.rung.bits_per_symbol * len(self.data_tones)
        return -(-self.data_bits // per_symbol)

    @property
    def total_symbols(self) -> int:
        return self.first_data + self.data_symbols

    @property
    def n_samples(self) -> int:
        return self.total_symbols * self.stride


def pilot_values(n_symbols: int, n_pilots: int) -> np.ndarray:
    """Pilot BPSK values (n_symbols, n_pilots) for the first n_pilots pilot tones.

    Pilot j of symbol s is 1 - 2 c(4 s + j), so a pilot's value depends only
    on the symbol and which pilot tone it is, not on how many are active.
    """
    slots = len(params.PILOT_TONES)
    c = mapping.gold_sequence(PILOT_C_INIT, n_symbols * slots)
    return (1.0 - 2.0 * c.astype(float)).reshape(n_symbols, slots)[:, :n_pilots]


def pilots_on(n_symbols: int, pilots: tuple[int, ...]) -> np.ndarray:
    """Pilot values (n_symbols, len(pilots)) for any subset of the pilot tones."""
    slots = [params.PILOT_TONES.index(p) for p in pilots]
    return pilot_values(n_symbols, len(params.PILOT_TONES))[:, slots]


def _fill_symbols(points: np.ndarray, n_sym: int, width: int, data_tones: tuple[int, ...],
                  pilots: tuple[int, ...], first_symbol: int) -> np.ndarray:
    g = np.zeros((n_sym, width), dtype=complex)
    g[:, list(data_tones)] = points.reshape(n_sym, len(data_tones))
    g[:, list(pilots)] = pilots_on(first_symbol + n_sym, pilots)[first_symbol:]
    return g


def probe_grid(width: int, first_symbol: int) -> np.ndarray:
    """Width-probe symbols (SPEC 5.4): known BPSK on every tone, pilots on the pilot tones.

    Tone k of probe symbol r is 1 - 2 c(56 r + k) (ADR 0005 probe-sequence),
    so a tone's value does not depend on the probe width.
    """
    c = mapping.gold_sequence(PROBE_C_INIT, params.PROBE_SYMBOLS * params.MAX_TONES)
    g = (1.0 - 2.0 * c.astype(float)).reshape(params.PROBE_SYMBOLS, params.MAX_TONES)[:, :width]
    g = g.astype(complex)
    pilots = params.pilots_for(width)
    g[:, list(pilots)] = pilot_values(first_symbol + params.PROBE_SYMBOLS, len(pilots))[first_symbol:]
    return g


def header_points(cfg: FrameConfig, header_bits: np.ndarray) -> np.ndarray:
    """The header's QPSK points on the base data tones, padding included."""
    hcode = ldpc.code_for(params.HEADER_CODE)
    e_h = params.HEADER_CODE.coded_bits[0]
    hcoded = hcode.rate_match(hcode.encode(header_bits[None])[0], e_h)
    hbits = mapping.interleave(mapping.scramble(hcoded, HEADER_POSITION), cfg.interleaver, HEADER_POSITION)
    h_res = cfg.header_symbols * len(cfg.base_data_tones) * QPSK
    hbits = np.concatenate([hbits, _pad_bits(h_res - hbits.size)])
    return mapping.modulate(hbits, QPSK)


def _pad_bits(n: int) -> np.ndarray:
    return mapping.scramble_sequence(PAD_POSITION, n) if n else np.zeros(0, dtype=np.uint8)


def _coded_stream(code: ldpc.LiftedCode, info: np.ndarray, e: int, first_position: int,
                  interleaver: str) -> np.ndarray:
    """Blocks (n, k) with CRC -> their interleaved bits back to back (SPEC 6.4)."""
    if not len(info):
        return np.zeros(0, dtype=np.uint8)
    coded = code.rate_match(code.encode(info), e)
    return np.concatenate([mapping.interleave(mapping.scramble(coded[i], first_position + i), interleaver,
                                              first_position + i) for i in range(len(info))])


def control_points(cfg: FrameConfig, control_bits: np.ndarray) -> np.ndarray:
    """The control segment's QPSK points (blocks at positions 1 .. n_control, then padding)."""
    ccode = ldpc.code_for(params.CONTROL_CODE)
    e_c = params.CONTROL_CODE.coded_bits[0]
    stream = _coded_stream(ccode, control_bits, e_c, 1, cfg.interleaver)
    c_res = cfg.control_symbols * len(cfg.control_tones) * QPSK
    return mapping.modulate(np.concatenate([stream, _pad_bits(c_res - stream.size)]), QPSK)


@dataclass
class TxFrame:
    config: FrameConfig
    samples: np.ndarray
    header_bits: np.ndarray        # (144,) incl. CRC
    block_bits: np.ndarray         # (n_blocks, 1024) incl. CRC
    grid: np.ndarray = field(repr=False)  # (total_symbols, width) tone values before clipping
    control_bits: np.ndarray = field(default_factory=lambda: np.zeros((0, params.CONTROL_INFO_BITS), np.uint8))
    header: fields.Header | None = None   # the header's fields, when built from them

    @property
    def preamble_samples(self) -> np.ndarray:
        return self.samples[: params.PREAMBLE_SYMBOLS * self.config.stride]


def _check_header(cfg: FrameConfig, header: fields.Header) -> None:
    want = FrameConfig.from_header(header, LinkState(cfg.width, cfg.mask, cap=cfg.cap, mask_id=header.mask_id),
                                   cp=cfg.cp)
    got = (cfg.n_blocks, cfg.n_control, cfg.has_probe, cfg.mcs if cfg.n_blocks else 0)
    if got != (want.n_blocks, want.n_control, want.has_probe, want.mcs):
        raise ValueError("header fields (data and control block counts, probe flag, MCS) "
                         "disagree with the frame configuration")


def build(cfg: FrameConfig, rng: np.random.Generator, header_msg: np.ndarray | None = None,
          payloads: np.ndarray | None = None, *, header: fields.Header | None = None,
          controls: list | None = None) -> TxFrame:
    """Assemble a frame.

    ``header`` gives the header's fields (SPEC 8.2), which must agree with
    ``cfg``; otherwise ``header_msg`` gives its 128 bits before the CRC, and
    without either they are pseudo-random (the Stage 1 frames and
    ``vectors/bitmap/``). ``controls`` lists ``cfg.n_control`` control blocks,
    as ``fields`` objects or 240-bit messages; random bits if omitted.
    Payloads are random unless given. The waveform itself comes from
    ``rebuild``, driven by the bits chosen here.
    """
    if header is not None:
        if header_msg is not None:
            raise ValueError("give header fields or header bits, not both")
        _check_header(cfg, header)
        header_msg = header.message_bits()
    if header_msg is None:
        header_msg = rng.integers(0, 2, params.HEADER_INFO_BITS - 16, dtype=np.uint8)
    header_bits = append_crc(header_msg[None])[0]
    if payloads is None:
        payloads = rng.integers(0, 2, (cfg.n_blocks, params.DATA_PAYLOAD_BITS), dtype=np.uint8)
    payloads = np.asarray(payloads, dtype=np.uint8).reshape(cfg.n_blocks, params.DATA_PAYLOAD_BITS)
    block_bits = append_crc(payloads)

    if controls is None:
        cmsgs = (rng.integers(0, 2, (cfg.n_control, params.CONTROL_INFO_BITS - 16), dtype=np.uint8)
                 if cfg.n_control else np.zeros((0, params.CONTROL_INFO_BITS - 16), np.uint8))
    else:
        if len(controls) != cfg.n_control:
            raise ValueError(f"{len(controls)} control blocks for a frame of {cfg.n_control}")
        cmsgs = np.array([c if isinstance(c, np.ndarray) else fields.control_message_bits(c)
                          for c in controls], dtype=np.uint8).reshape(cfg.n_control, params.CONTROL_INFO_BITS - 16)
    control_bits = append_crc(cmsgs) if cfg.n_control else np.zeros((0, params.CONTROL_INFO_BITS), np.uint8)
    tx = rebuild(cfg, header_bits, block_bits, control_bits)
    tx.header = header
    return tx


def rebuild(cfg: FrameConfig, header_bits: np.ndarray, block_bits: np.ndarray,
            control_bits: np.ndarray | None = None) -> TxFrame:
    """The frame carrying given header, data block and control block bits
    (CRCs included, as ``TxFrame.header_bits``, ``block_bits`` and
    ``control_bits`` hold them; ``control_bits`` may be omitted only when
    ``cfg.n_control`` is 0).

    This is the transmitter's own path, driven by bits: the receiver calls it
    with what it decoded to reproduce the waveform that was sent (ADR 0006),
    preamble, header, probe, control and data segments alike.
    """
    if control_bits is None:
        if cfg.n_control:
            raise ValueError(f"a frame of {cfg.n_control} control blocks needs their bits")
        control_bits = np.zeros((0, params.CONTROL_INFO_BITS), np.uint8)
    control_bits = np.asarray(control_bits, dtype=np.uint8).reshape(cfg.n_control, params.CONTROL_INFO_BITS)
    block_bits = np.asarray(block_bits, dtype=np.uint8).reshape(cfg.n_blocks, params.DATA_INFO_BITS)
    dcode = ldpc.code_for(params.DATA_CODE)
    rung = cfg.rung
    hpoints = header_points(cfg, header_bits)
    stream = _coded_stream(dcode, block_bits, rung.coded_bits, 1 + cfg.n_control, cfg.interleaver)
    d_res = cfg.data_symbols * len(cfg.data_tones) * rung.bits_per_symbol
    stream = np.concatenate([stream, _pad_bits(d_res - stream.size)])
    dpoints = mapping.modulate(stream, rung.bits_per_symbol)

    cpoints = control_points(cfg, control_bits)

    p_base = params.pilots_for(params.BASE_TONES)
    first_h = params.PREAMBLE_SYMBOLS
    nt = cfg.n_tones                    # the probe may reach above the width, to the sender's cap
    hgrid = _fill_symbols(hpoints, cfg.header_symbols, nt, cfg.base_data_tones, p_base, first_h)
    first_p = first_h + cfg.header_symbols
    pgrid = np.zeros((0, nt), complex)
    if cfg.has_probe:
        pgrid = np.zeros((params.PROBE_SYMBOLS, nt), complex)
        pgrid[:, :cfg.cap] = probe_grid(cfg.cap, first_p)
    first_c, first_d = cfg.first_control, cfg.first_data
    cgrid = _fill_symbols(cpoints, cfg.control_symbols, nt, cfg.control_tones,
                          cfg.control_pilots, first_c) * cfg.control_gain
    dgrid = _fill_symbols(dpoints, cfg.data_symbols, nt, cfg.data_tones,
                          cfg.data_pilots, first_d) * cfg.data_gain
    grid = np.concatenate([preamble.grid(nt), hgrid, pgrid, cgrid, dgrid])

    # Every symbol uses the same per-tone scale (SPEC 6.2), set by the link
    # width; a tone mask raises control and data symbols by the known
    # cfg.control_gain and cfg.data_gain. Clip-and-filter projects each
    # symbol back onto the tones it uses, so clipping noise does not refill
    # masked tones. The probe covers every tone up to the cap (SPEC 5.4).
    scale = cfg.width
    segments = [(grid[:first_p], range(params.BASE_TONES)),
                (grid[first_p:first_c], range(cfg.cap)),
                (grid[first_c:first_d], cfg.control_active_tones),
                (grid[first_d:], cfg.data_active_tones)]
    out = []
    for g, tones in segments:
        if not len(g):
            continue
        x = ofdm.modulate(g, cfg.cp, active=scale)
        if cfg.clip:
            x = ofdm.clip_and_filter(x, nt, np.array(list(tones)), cfg.cp,
                                     clip_db=cfg.clip_db, scale_tones=scale)
        out.append(x)
    return TxFrame(cfg, np.concatenate(out), header_bits, block_bits, grid, control_bits)


def check_block_map(header: fields.Header, controls) -> None:
    """Raise ``ValueError`` unless the frame's block map agrees with its
    header (ADR 0008 ``block-map-control-block``): flag bit 7 set exactly when
    the second control block is a block map, whose base is the header's
    Sequence, bit 0 set and one bit per data block."""
    controls = list(controls)
    if header.block_map:
        if fields.block_sequences(header, [None] + controls[1:2]) is None:
            raise ValueError("flag bit 7 needs a block map as the second control block, base = Sequence, "
                             "bit 0 set, one bit per data block")
    elif any(isinstance(c, fields.BlockMapBlock) for c in controls):
        raise ValueError("a block map needs header flag bit 7")


def build_frame(header: fields.Header, controls: list | tuple = (), payloads: np.ndarray | None = None, *,
                link: LinkState | None = None, rng: np.random.Generator | None = None,
                **kw) -> TxFrame:
    """Build the frame a header describes (SPEC 8.2), at the link's width,
    the mask its Mask ID names, the probe's extent and the prefix; ``kw``
    goes to ``FrameConfig`` (for example ``clip``). Header flag bit 4 is set
    if the frame's data blocks use tones above 23 (ADR 0007, ADR 0011
    ``width-probe-extent``). The k-th payload is the data block the block
    map names k-th (``check_block_map``; ``fields.plan_blocks`` gives the
    header's Sequence and the map for a set of sequences)."""
    check_block_map(header, controls)
    if not header.probe:
        with_probe = replace(header, probe=True)
        if FrameConfig.from_header(with_probe, link, **kw).needs_probe:
            header = with_probe
    cfg = FrameConfig.from_header(header, link, **kw)
    return build(cfg, rng if rng is not None else np.random.default_rng(0), payloads=payloads,
                 header=header, controls=list(controls))


# --- receiver ---------------------------------------------------------------


@dataclass
class RxResult:
    header_ok: bool
    header_bits: np.ndarray
    block_ok: np.ndarray            # (n_blocks,) bool
    block_bits: np.ndarray          # (n_blocks, 1024)
    start: int                      # detected frame start (sample index)
    snr_db: np.ndarray              # per tone, from S2 (base tones) and the probe
    iterations: np.ndarray
    # Receiver internals for measurement tools (bench rx): channel ``h`` and
    # ``noise`` per tone, equalized symbols ``eq`` (rows after the preamble),
    # ``d0`` (first data row of ``eq``), ``window_start``, ``shifts``, ``drift``,
    # the level tracker's ``level`` (``LevelFit``) and the noise variance ``nv``
    # each cell of ``eq`` was demapped with (header rows: the S2 estimate's).
    detail: dict = field(default_factory=dict, repr=False)
    control_ok: np.ndarray = field(default_factory=lambda: np.zeros(0, dtype=bool))
    control_bits: np.ndarray = field(default_factory=lambda: np.zeros((0, params.CONTROL_INFO_BITS), np.uint8))
    # Parsed fields (``fields``): the header when its CRC passed and its
    # fields are valid (else ``header_error`` says why), and each control
    # block (None where its CRC failed or its fields are malformed).
    header: fields.Header | None = None
    header_error: str | None = None
    controls: tuple = ()
    config: FrameConfig | None = None       # the configuration the frame was decoded with
    # The sequence of each data block (ADR 0008, ``fields.block_sequences``):
    # consecutive from the header's Sequence, or as the block map names them;
    # None when the header failed, or flag bit 7 is set and the block map
    # failed or breaks its rules (then no data block counts as received).
    block_sequences: tuple[int, ...] | None = None
    # Why the frame's control and data blocks were not decoded although its
    # header was: a Mask ID the link state does not hold (ADR 0011).
    link_error: str | None = None

    def delivered(self) -> list[tuple[int, np.ndarray]]:
        """(sequence, 1,008 payload bits) of every data block received: CRC
        passed and its sequence known."""
        if self.block_sequences is None:
            return []
        return [(seq, self.block_bits[k, :params.DATA_PAYLOAD_BITS])
                for k, seq in enumerate(self.block_sequences) if self.block_ok[k]]


def header_noise_floor(pre: np.ndarray, h: np.ndarray, thermal: np.ndarray,
                       pilot_excess: np.ndarray) -> np.ndarray:
    """Noise variance the preamble and pilots predict for header symbols.

    ``pre`` is the four preamble symbols on the base tones (S1, S1, S2, S2),
    ``h`` the channel, ``thermal`` the S2 noise estimate and
    ``pilot_excess`` the pilots' distortion (relative to signal power) in
    the data symbols. Returned as variance of the equalized symbols.

    * Noise: the differences of both repeated pairs (two samples per tone,
      against one from S2 alone), smoothed by a linear fit over 11 tones.
      That follows a smooth tilt (a flat port's discriminator noise) but
      not the frame-to-frame scatter of a 7-tone average, which mis-weights
      tones in AWGN. Never below a quarter of the S2 estimate.
    * Distortion: the pilots' excess, averaged into one band-wide figure.
      Interpolated from pilot 3 to pilot 20 instead, as the data symbols
      use it, a distorted low band edge (the repeater's) raises every tone
      up to 20 and drowns the good ones; local distortion is left to the
      header's own measurement (``estimate_header_noise``).
    """
    raw = (np.abs(pre[0] - pre[1]) ** 2 + np.abs(pre[2] - pre[3]) ** 2) / 4
    smooth = np.maximum(signal.savgol_filter(raw, 11, 1, mode="interp"), thermal / 4)
    return smooth / np.abs(h) ** 2 + (float(np.mean(pilot_excess)) if pilot_excess.size else 0.0)


def estimate_header_noise(eq: np.ndarray, known_pilots: np.ndarray, floor: np.ndarray,
                          data_tones: list[int], pilot_tones: list[int],
                          margin: float = 2.0) -> np.ndarray:
    """Noise variance per base tone for the header's equalized symbols.

    ``floor`` is the variance expected from what the preamble and pilots
    measured (as variance of ``eq``). The header symbols themselves give a
    second, local measurement: QPSK has unit power, so mean |eq|^2 - 1 over
    the header's symbols is the error variance on a data tone, whatever its
    cause (noise, inter-symbol interference beyond the prefix, a limiter,
    channel-estimate error); on a pilot tone the known value gives it
    directly. Pooled over three neighbouring tones, it is noisy (each
    sample has variance 2v + v^2), so it raises the floor only by what it
    exceeds it with confidence: its value less ``margin`` standard
    deviations.
    """
    n = floor.size
    m = np.full(n, np.nan)
    m[data_tones] = np.mean(np.abs(eq[:, data_tones]) ** 2, axis=0) - 1
    m[pilot_tones] = np.mean(np.abs(eq[:, pilot_tones] - known_pilots) ** 2, axis=0)
    kernel = np.ones(3)
    count = np.convolve(np.ones(n), kernel, mode="same")
    pooled = np.convolve(m, kernel, mode="same") / count
    sd = np.sqrt((2 * floor + floor ** 2) / (count * eq.shape[0]))
    return np.maximum(floor, pooled - margin * sd)


def estimate_channel(y2: np.ndarray, y3: np.ndarray, s2: np.ndarray, window: int = 7,
                     second: np.ndarray | None = None) -> np.ndarray:
    """Per-tone channel from the two S2 symbols, smoothed across tones.

    The audio chain's response is smooth in frequency apart from a linear
    phase (the delay), so the estimate is de-rotated by its mean phase step,
    smoothed with a local quadratic fit over ``window`` tones, and rotated
    back. ``window`` < 3 returns the raw estimate.
    """
    return smooth_channel((y2 / s2 + y3 / (s2 if second is None else second)) / 2, window)


def smooth_channel(raw: np.ndarray, window: int = 7, step: float | None = None) -> np.ndarray:
    """A raw per-tone channel estimate smoothed across tones as
    ``estimate_channel`` does (``step``: the phase step per tone to de-rotate
    by; None: measured from ``raw``)."""
    if window < 3:
        return raw
    k = np.arange(raw.size)
    if step is None:
        step = channel_phase_step(raw)
    flat = raw * np.exp(-1j * step * k)
    smooth = (signal.savgol_filter(flat.real, window, 2, mode="interp")
              + 1j * signal.savgol_filter(flat.imag, window, 2, mode="interp"))
    return smooth * np.exp(1j * step * k)


def channel_phase_step(raw: np.ndarray) -> float:
    """The mean phase step from tone to tone of a channel estimate (the delay)."""
    return float(np.angle(np.sum(raw[1:] * np.conj(raw[:-1]))))


def estimate_noise(y2: np.ndarray, y3: np.ndarray, window: int = 7) -> np.ndarray:
    """Per-tone noise variance from the difference of the two S2 symbols."""
    raw = np.abs(y2 - y3) ** 2 / 2
    return _box_smooth(raw, window)


def _box_smooth(raw: np.ndarray, window: int) -> np.ndarray:
    """A moving average over ``window`` tones, shortened at the band edges."""
    kernel = np.ones(window) / window
    return np.convolve(raw, kernel, mode="same") / np.convolve(np.ones(raw.size), kernel, mode="same")


@dataclass
class HeaderChannel:
    """The base tones' channel re-estimated from the decoded header
    (``header_channel_estimate``)."""
    h: np.ndarray            # (24,) the combined estimate, smoothed
    h_s2: np.ndarray         # (24,) the S2 estimate it replaces
    h_loo: np.ndarray        # (header rows, 24) the estimate without each header row's own symbols
    weight: np.ndarray       # (24,) the known rows' share of each tone's estimate (0: S2 alone, 1: rows alone)
    rows: int                # fully known rows used (header, and the probe when present)


def header_channel_estimate(tones: np.ndarray, h_s2: np.ndarray, thermal: np.ndarray, corr: np.ndarray,
                            level: np.ndarray, x: np.ndarray, full: np.ndarray, hs: int,
                            window: int = 7) -> HeaderChannel:
    """Each base tone's channel from the S2 pair and every fully known row
    after it (the decoded header, and the width probe), weighted by their
    noise.

    The S2 pair alone (two symbols, ``estimate_channel``) leaves about a
    sixth of the S2 noise in each tone's estimate (``FALLBACK_H_BIAS``),
    which costs every data symbol; once the header has decoded, its seven
    symbols are known on every base tone, nine known symbols in all. ``tones``
    are the demodulated symbols (all rows, ``level`` and ``corr`` for the rows
    after the preamble: each row's tracked gain against the S2 estimate and
    the pilots' phase and timing correction, zero at S2), ``x`` and ``full``
    the known values and fully known rows (``level_cells``). A known row r
    measures tone k as g = y / (corr level x): the channel at the S2 pair's
    level and phase reference, each row at its own fitted level, so a row
    the radios' ramp left louder than S2 counts for more. The rows' noise
    per tone is their scatter about their mean (pooled over 7 tones, as
    the S2 noise ``thermal`` is), which includes whatever
    distortion the header symbols carry and the low-crest preamble escapes.
    The S2 average (variance thermal / 2) and the rows' mean (variance
    v / n) are combined by inverse variance and smoothed across tones as
    the S2 estimate is. ``h_loo`` repeats the combination without each
    header row in turn, for measuring the header's error on an estimate that
    does not contain it (the data's estimate does not contain their noise).
    """
    b = params.BASE_TONES
    first = params.PREAMBLE_SYMBOLS
    s2 = preamble.s2()
    raw_s2 = (tones[2, :b] + tones[3, :b]) / s2 / 2
    rows = np.flatnonzero(full)
    n = rows.size
    g = tones[first + rows, :b] / (corr[rows, :b] * level[rows, None] * x[rows])
    g_mean = np.mean(g, axis=0)
    floor = 1e-9 * float(np.mean(np.abs(h_s2) ** 2))
    v = np.maximum(_box_smooth(np.sum(np.abs(g - g_mean) ** 2, axis=0) / max(n - 1, 1), 7), floor)
    w_s2 = 2 / np.maximum(thermal[:b], floor)

    def combine(mean: np.ndarray, count: int) -> tuple[np.ndarray, np.ndarray]:
        w_rows = count / v
        return (w_s2 * raw_s2 + w_rows * mean) / (w_s2 + w_rows), w_rows / (w_s2 + w_rows)

    raw, weight = combine(g_mean, n)
    step = channel_phase_step(raw)
    loo = np.empty((hs, b), dtype=complex)
    for i, r in enumerate(rows):
        if r < hs:
            loo[r] = smooth_channel(combine((n * g_mean - g[i]) / (n - 1), n - 1)[0], window, step)
    return HeaderChannel(smooth_channel(raw, window, step), h_s2[:b].copy(), loo, weight, n)


CLOCK_PRIOR_SD_PPM = 50.0   # SPEC 14.1 allows +-100 ppm between stations: two standard deviations
CLOCK_BOUND_PPM = 150.0     # no clock estimate exceeds this: SPEC 14.1's +-100 ppm plus margin


@dataclass(frozen=True)
class ClockPrior:
    """What the receiver assumes about the sample-clock offset before it
    measures a frame: a Gaussian prior, ppm, with the sign the receiver
    reports (``-drift / stride * 1e6``; the channel's ``sco_ppm``).

    The pilots measure the offset with a variance that falls with the cube
    of the frame's length: about +-10 ppm on a one-block frame at 24 tones
    and 13 dB, +-300 ppm on a header-only frame, under 1 ppm on a 4 s frame.
    ``track_drift`` combines the measurement with this prior by their
    variances (a long frame is measured, a short one keeps close to the
    prior) and bounds the result by ``CLOCK_BOUND_PPM``. The default is
    SPEC 14.1's tolerance: 0 ppm, standard deviation ``CLOCK_PRIOR_SD_PPM``.
    A link layer that has measured a peer on long frames can supply a
    tighter one (``receive(clock_prior=...)``).
    """
    ppm: float = 0.0
    sd_ppm: float = CLOCK_PRIOR_SD_PPM

    def drift_units(self, stride: int) -> tuple[float, float, float]:
        """(mean, standard deviation, bound) as drift in samples per symbol."""
        def d(ppm: float) -> float:
            return ppm * 1e-6 * stride
        return -d(self.ppm), d(max(self.sd_ppm, 1e-3)), d(CLOCK_BOUND_PPM)


def track_drift(pilot_obs: np.ndarray, pilots: tuple[int, ...], width: int, *,
                s_ref: float | np.ndarray, window: int = 15,
                prior: tuple[float, float, float] | None = None) -> tuple[np.ndarray, float, float]:
    """Phase correction per symbol and tone from the continuous pilots.

    ``pilot_obs`` is (n_sym, n_pilots): received pilot values divided by the
    known pilot symbols. The channel itself is static, so only the change of
    pilot phase over the frame is used: each pilot's phase track gets its own
    intercept (which soaks up channel-estimate error) and the slopes are fit
    as alpha + beta * f, a common phase drift plus the timing drift a
    sample-clock offset causes. The correction is zero at ``s_ref``, the
    symbol position (relative to ``pilot_obs`` row 0) where the channel was
    estimated: one for every tone, or per tone (the tones above 23 are
    estimated from the width probe, after the header).

    Each pilot's phase is unwrapped against a smoothed track (smoothing
    alone flattens the ends of a short run and halves its slope) and fitted
    with a line, whose residuals give the slope's variance (none with fewer
    than five rows). The timing drift beta implies is combined with
    ``prior`` (mean, standard deviation, bound, in samples per symbol;
    default ``ClockPrior()`` at the short prefix) by their variances and
    bounded, and alpha is re-fitted for the beta kept: a frame too short to
    measure the drift keeps the prior's.

    Returns the correction (n_sym, width), the timing drift in samples per
    symbol implied by beta (a delay of d samples turns tone bin f by
    -2 pi f d / N) and its standard deviation.
    """
    if prior is None:
        prior = ClockPrior().drift_units(params.N_USEFUL + params.CP_SHORT)
    mu, sd0, bound = prior
    n_sym = pilot_obs.shape[0]
    kernel = np.ones(window) / window
    s = np.arange(n_sym, dtype=float)
    # A pilot absent from all but one row (an upper pilot in a frame with no
    # data symbols) tells nothing about drift.
    use = [j for j in range(len(pilots)) if np.count_nonzero(~np.isnan(pilot_obs[:, j])) >= 2]
    pilot_obs, pilots = pilot_obs[:, use], tuple(pilots[j] for j in use)
    slopes = np.zeros(len(pilots))
    var = np.full(len(pilots), np.inf)
    for j in range(len(pilots)):
        valid = ~np.isnan(pilot_obs[:, j])        # a contiguous run of rows
        rows, vals = s[valid], pilot_obs[valid, j]
        k = kernel if rows.size >= kernel.size else np.ones(rows.size) / rows.size
        track = np.unwrap(np.angle(np.convolve(vals, k, mode="same")))
        theta = track + np.angle(vals * np.exp(-1j * track))
        sc = rows - rows.mean()
        sxx = max(sc @ sc, 1e-12)
        tc = theta - theta.mean()
        slopes[j] = (sc @ tc) / sxx
        nu = rows.size - 2                         # degrees of freedom of the residual
        if nu > 2:
            # The slope's variance from the residuals, widened by nu / (nu - 2)
            # (a t distribution's): a few rows estimate it poorly.
            var[j] = np.sum((tc - slopes[j] * sc) ** 2) / nu / sxx * nu / (nu - 2)
    f = params.BIN_OFFSET + np.array(pilots, dtype=float)
    to_drift = -params.N_USEFUL / (2 * np.pi)     # drift = to_drift * beta
    if len(pilots) > 1:
        fc = f - f.mean()
        sff = fc @ fc
        measured = to_drift * (fc @ slopes) / sff
        var_d = float(np.sum(fc ** 2 * var) / sff ** 2) * to_drift ** 2
    else:
        measured, var_d = mu, np.inf
    if np.isfinite(var_d):
        p0 = sd0 ** 2
        drift, var_post = (measured * p0 + mu * var_d) / (p0 + var_d), p0 * var_d / (p0 + var_d)
    else:
        drift, var_post = mu, sd0 ** 2
    drift = float(np.clip(drift, -bound, bound))
    beta = drift / to_drift
    alpha = float(np.mean(slopes) - beta * np.mean(f)) if len(pilots) else 0.0
    all_f = params.BIN_OFFSET + np.arange(width)
    ref = np.broadcast_to(np.asarray(s_ref, dtype=float), (width,))
    corr = np.exp(1j * (alpha + beta * all_f)[None, :] * (s[:, None] - ref[None, :]))
    return corr, drift, float(np.sqrt(var_post))


def _estimate(tones: np.ndarray, cfg: FrameConfig, smooth_window: int, phase_window: int,
              robust: bool = False, clock_prior: ClockPrior | None = None):
    """Channel, noise and drift estimates from demodulated tones.

    ``tones`` has the control and data symbols already divided by
    ``cfg.control_gain`` and ``cfg.data_gain``. Returns the channel ``h``,
    ``thermal`` (the additive noise per tone the S2 pair and the probe
    measure, at the preamble's scale; a data symbol raised by the mask gain
    sees it reduced by ``data_gain ** 2``; ``pilot_excess`` adds the
    distortion), the phase correction ``corr``, the rows after the preamble,
    the drift and its standard deviation, and the pilot observations. With
    ``robust``, data symbols whose power shows a noise burst are left out of
    the pilots' phase tracking and distortion estimate.
    """

    b = params.BASE_TONES
    y2, y3 = tones[2, :b], tones[3, :b]
    nt = cfg.n_tones
    h = np.zeros(nt, dtype=complex)
    h[:b] = estimate_channel(y2, y3, preamble.s2(), smooth_window)
    noise = np.zeros(nt)
    noise[:b] = estimate_noise(y2, y3)
    first = params.PREAMBLE_SYMBOLS
    hs = cfg.header_symbols
    if cfg.has_probe:
        p0 = first + hs
        known = probe_grid(nt, p0)
        hp = estimate_channel(tones[p0], tones[p0 + 1], known[0], smooth_window, second=known[1])
        np_ = estimate_noise(tones[p0] / known[0], tones[p0 + 1] / known[1]) * np.abs(known[0]) ** 2
        h[b:] = hp[b:]
        noise[b:] = np_[b:]
    noise = np.maximum(noise, 1e-6 * np.mean(np.abs(h) ** 2))
    thermal = noise.copy()

    body = tones[first:]
    pilots = cfg.data_pilots          # masked pilots are absent from data symbols
    pv = pilots_on(tones.shape[0], pilots)[first:]
    obs = body[:, list(pilots)] / pv
    upper = [j for j, p in enumerate(pilots) if p >= b]
    obs[:hs, upper] = np.nan                                   # header: base pilots only
    d0 = cfg.first_data - first
    if cfg.n_control:
        # Control symbols carry base pilots only; the upper pilots' rows are
        # then the data symbols alone, a contiguous run (probe rows dropped).
        obs[hs:d0, upper] = np.nan
    if robust and body.shape[0] > d0:
        # A burst hitting one symbol's pilots would turn the smoothed phase
        # track for the symbols around it: leave such symbols out.
        act = list(cfg.data_active_tones)
        power = np.mean(np.abs(body[d0:, act]) ** 2 / np.abs(h[act]) ** 2, axis=1)
        obs[d0:][_outliers(power)] = np.nan
    prior = (clock_prior or ClockPrior()).drift_units(cfg.stride)
    # The drift correction is zero where each tone's channel was estimated:
    # the S2 pair (rows 2-3) for the base tones, the probe pair for the rest
    # (a clock offset turns them by the drift between the two, 18 degrees at
    # tone 50 and 100 ppm).
    s_ref = np.full(nt, 2.5 - first)
    s_ref[b:] = hs + 0.5
    corr, drift, drift_sd = track_drift(obs, pilots, nt, s_ref=s_ref, window=phase_window, prior=prior)
    return h, thermal, corr, body, drift, drift_sd, obs


def pilot_excess(obs: np.ndarray, cfg: FrameConfig, h: np.ndarray, corr: np.ndarray, level: np.ndarray,
                 thermal: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """The pilots' distortion in the data symbols, and the noise with it.

    The S2 pair sees only noise: both symbols carry the same clipping
    distortion, so it cancels in their difference. Data symbols also carry
    distortion (clipping, a radio limiter) and inter-symbol interference,
    which the pilots in the data symbols measure; it is added, as a fraction
    of signal power interpolated between pilots, to every tone. ``obs`` are
    the pilot observations of ``_estimate`` and ``level`` the level tracker's
    gain per row and tone (``level_matrix``): a level the S2 estimate missed
    is not distortion. Returns (excess per data pilot, noise per tone).
    """
    pilots = list(cfg.data_pilots)
    d0 = cfg.first_data - params.PREAMBLE_SYMBOLS
    excess = np.zeros(len(pilots))
    noise = thermal.copy()
    if obs.shape[0] > d0:
        hp = h[pilots]
        resid = obs[d0:] / (hp[None, :] * corr[d0:, pilots] * level[d0:, pilots]) - 1
        r2 = np.abs(resid) ** 2
        r2 = r2[~np.isnan(r2).any(axis=1)]
        floor = thermal[pilots] / (np.abs(hp) ** 2 * cfg.data_gain ** 2)
        excess = np.maximum(np.mean(r2, axis=0) - floor, 0)
        noise = noise + np.interp(np.arange(cfg.n_tones), pilots, excess) * np.abs(h) ** 2
    return excess, noise


# --- level tracking -----------------------------------------------------------
#
# Over the air the received level is not what the S2 pair measured: the
# 2026-11-07 captures show it rising from 13-22 dB below its steady value at
# the first preamble sample (likely the radios' transmit audio AGC), which
# leaves the S2 estimate 3-6 dB low, and settling after S2 with a 19-26 ms
# time constant; on links without that ramp the low-crest preamble escapes
# the radios' limiters and reads 0.5-2 dB above the data. Equalised by the
# S2 estimate alone, the data symbols sit off unit power, which moves QAM
# decision regions and mis-scales the LLRs.

LEVEL_TAUS_S = (0.012, 0.019, 0.027, 0.038, 0.054)  # settling time constants tried (captures: 19-26 ms)
LEVEL_MISMATCH = 0.035      # sd of the data's level against the header's, relative (captures: 0.2-0.5 dB)
LEVEL_K_SIGMA = 3.0         # a level term is applied only this many standard errors from no change
LEVEL_BOUNDS = (0.1, 10.0)  # a fitted level outside this (-20..+20 dB) is not believed: no tracking
LEVEL_MIN_SETTLE = 0.05     # the settling term is fitted only with two rows or more where it is above this
LEVEL_SETTLED = 0.02        # data rows whose settling term is below this measure the data's own power


@dataclass
class LevelFit:
    """The level tracker's common gain (``track_level``)."""
    level: np.ndarray            # (rows after the preamble,) gain against the S2 estimate; 1 if not tracked
    reference: float             # the level at the probe rows, which the tones above 23 are referenced to
    segments: tuple[float, ...]  # steady level per segment (header and control; probe and data); 1 if held
    settle: float                # the settling term at the first header row; 0 if held
    tau: float                   # the settling term's time constant, s
    sd: tuple[float, ...]        # standard errors of the segment levels and the settling term (inf if held)
    applied: bool                # any term applied


def level_cells(cfg: FrameConfig, header_known: np.ndarray | None) -> tuple[np.ndarray, np.ndarray]:
    """What the receiver knows of the base tones of each row after the
    preamble, for ``track_level``: the transmitted values (0 where unknown),
    as the equalised symbols see them (mask gains divided out), and which
    rows are known on every base tone (the probe, and the header given its
    decoded points ``header_known``). The other rows know their pilots."""
    first = params.PREAMBLE_SYMBOLS
    b = params.BASE_TONES
    nb = cfg.total_symbols - first
    hs = cfg.header_symbols
    x = np.zeros((nb, b), dtype=complex)
    full = np.zeros(nb, dtype=bool)
    bp = params.pilots_for(b)
    x[:hs, list(bp)] = pilots_on(first + hs, bp)[first:]
    if header_known is not None:
        x[:hs, list(cfg.base_data_tones)] = np.asarray(header_known).reshape(hs, -1)
        full[:hs] = True
    if cfg.has_probe:
        p0 = cfg.first_probe - first
        x[p0:p0 + params.PROBE_SYMBOLS] = probe_grid(cfg.n_tones, cfg.first_probe)[:, :b]
        full[p0:p0 + params.PROBE_SYMBOLS] = True
    c0, d0 = cfg.first_control - first, cfg.first_data - first
    cp = tuple(p for p in cfg.control_pilots if p < b)
    if d0 > c0:
        x[c0:d0, list(cp)] = pilots_on(cfg.first_data, cp)[cfg.first_control:]
    dp = tuple(p for p in cfg.data_pilots if p < b)
    if nb > d0:
        x[d0:, list(dp)] = pilots_on(cfg.total_symbols, dp)[cfg.first_data:]
    return x, full


def _no_level(nb: int, n_seg: int) -> LevelFit:
    return LevelFit(np.ones(nb), 1.0, (1.0,) * n_seg, 0.0, LEVEL_TAUS_S[0], (np.inf,) * (n_seg + 1), False)


def _qam_fourth_moment(bits_per_symbol: int) -> float:
    """E|x|^4 of the unit-power square QAM constellation."""
    m = 2 ** (bits_per_symbol // 2)                 # levels per axis
    norm2 = 2 * (m * m - 1) / 3                     # per-axis power of the odd-integer levels, twice
    return 2 * (3 * m ** 4 - 10 * m * m + 7) / 15 / norm2 ** 2 + 0.5


def track_level(eq: np.ndarray, x: np.ndarray, full: np.ndarray, nv: np.ndarray, cfg: FrameConfig) -> LevelFit:
    """The common (all-tone) gain of each row after the preamble against the
    S2 channel estimate.

    ``eq`` holds the base tones' equalised symbols (rows after the
    preamble), ``x`` and ``full`` the known values and the fully known rows
    (``level_cells``), ``nv`` each base tone's noise variance as the S2 pair
    measured it (equalised units). Each row's known cells give its gain z,
    the real part of a noise-weighted projection on the known values (the
    level changes without turning the phase, which the pilots track), with
    a variance from the cells' own scatter. The gains are fitted by weighted
    least squares with

        a(row) = A_segment + B exp(-t / tau),

    t from the first header row: a steady level per segment and the radios'
    settling, tau the one of ``LEVEL_TAUS_S`` that fits best. The segments
    are the rows before the data (header, control) and the data rows; with
    a probe, the rows from the probe on (wider than the header, the data may
    sit lower in a limiter). Each kind of row measures the data's level with
    an error of its own, which the fit carries:

    * A row known on every base tone (the header once decoded, the probe)
      measures its own segment's level precisely.
    * A row that knows only its pilots measures it through the S2 estimate
      of two or three tones, whose error does not average out over rows:
      those rows share an offset with the spread the S2 noise predicts
      (``FALLBACK_H_BIAS``) as its prior.
    * Without a probe, the data's level is tied to the header's with the
      spread ``LEVEL_MISMATCH``, the difference the captures show.
    * Once the header is known, the data symbols' own power over the settled
      data rows: E|eq|^2 = A^2 (1 + r), r the noise the header measured,
      constellation power 1; its error is the statistical scatter plus the
      header's noise estimate being wrong by as much as itself, so it leads
      at high SNR and counts for little at low SNR.

    A term is applied only if it stands ``LEVEL_K_SIGMA`` standard errors from
    no change (a segment level from 1, the settling term from 0), else it is
    held there and the rest refitted: a frame whose level did not change, or
    one too noisy to tell, is equalised as before. Where any term is
    applied, a fully known row takes its own measured gain.
    """
    nb = eq.shape[0]
    first = params.PREAMBLE_SYMBOLS
    hs, d0 = cfg.header_symbols, cfg.first_data - first
    seg = np.zeros(nb, dtype=int)
    if cfg.has_probe:
        seg[cfg.first_probe - first:] = 1
    elif d0 < nb:
        seg[d0:] = 1
    n_seg = int(seg.max()) + 1 if nb else 1
    known = np.abs(x) > 0
    w = 1 / np.maximum(nv, 1e-12)
    info = np.sum(w[None, :] * np.abs(x) ** 2, axis=1)
    rows = np.flatnonzero(info > 0)
    if rows.size < 2:
        return _no_level(nb, n_seg)
    xr, er, kr = x[rows], eq[rows], known[rows]
    z = np.sum(w[None, :] * np.real(np.conj(xr) * er), axis=1) / info[rows]
    # The cells' scatter against what the S2 noise predicts (one real
    # parameter per row fitted from its complex cells).
    resid = float(np.sum(w[None, :] * np.abs(er - z[:, None] * xr) ** 2 * kr))
    scale = resid / max(float(np.sum(kr)) - 0.5 * rows.size, 1.0)
    var = np.maximum(scale, 1e-12) / (2 * info[rows])
    pilot_only = ~full[rows]
    has_d = bool(pilot_only.any())
    static = 0.0
    if has_d:
        pw = w * kr[pilot_only].any(axis=0)
        static = float(np.sum(pw ** 2 * nv * FALLBACK_H_BIAS / 2) / max(float(np.sum(pw)) ** 2, 1e-30))
    level0 = float(np.median(z))
    sr = seg[rows]
    data_seg = n_seg - 1 if d0 < nb else -1

    # The data symbols' power needs the header's noise: each header row at its own gain.
    noise_hdr = None
    if full[:hs].all() and cfg.n_blocks and d0 < nb:
        bt = [k for k in cfg.base_data_tones if k in set(cfg.data_tones)]
        if bt:
            r_k = np.mean(np.abs(eq[:hs, bt] / z[:hs, None] - x[:hs, bt]) ** 2, axis=0)
            noise_hdr = (bt, r_k)

    def data_power(phi: np.ndarray) -> tuple[float, float] | None:
        if noise_hdr is None:
            return None
        bt, r_k = noise_hdr
        settled = d0 + np.flatnonzero(phi[d0:] < LEVEL_SETTLED)
        if not settled.size:
            return None
        wk = 1 / np.maximum(r_k, 1e-6)
        p_k = np.mean(np.abs(eq[np.ix_(settled, bt)]) ** 2, axis=0)
        q = max(float(np.sum(wk * p_k / (1 + r_k)) / np.sum(wk)), 1e-12)
        n_eff = float(np.sum(wk * r_k) / np.sum(wk))
        spread = _qam_fourth_moment(cfg.rung.bits_per_symbol) - 1
        vq = float(np.sum(wk ** 2 * (spread + 2 * r_k + r_k ** 2) / (1 + r_k) ** 2) / np.sum(wk) ** 2 / settled.size)
        zp = float(np.sqrt(q))
        return zp, vq / (4 * q) + (zp * n_eff / (2 * (1 + n_eff))) ** 2

    def fit(free_a: list[bool], free_b: bool, phi: np.ndarray):
        """Weighted least squares with the priors: {name: (value, sd)} and the objective."""
        cols, names = [], []
        for s in range(n_seg):
            if free_a[s]:
                cols.append((sr == s).astype(float))
                names.append(f"a{s}")
        if free_b:
            cols.append(phi[rows])
            names.append("b")
        if has_d:
            cols.append(pilot_only.astype(float))
            names.append("d")
        held = np.array([0.0 if free_a[s] else 1.0 for s in range(n_seg)])
        target = z - held[sr]
        m = np.stack(cols, axis=1)
        nmat = m.T @ (m / var[:, None])
        rhs = m.T @ (target / var)
        # Pseudo-observations: (row of coefficients, value, variance).
        pseudo = []
        if has_d:
            pseudo.append(({"d": 1.0}, 0.0, max(static * level0 ** 2, 1e-12)))
        if n_seg == 2 and not cfg.has_probe:
            coef = {f"a{s}": sign for s, sign in ((0, -1.0), (1, 1.0)) if free_a[s]}
            value = -sum(sign * held[s] for s, sign in ((0, -1.0), (1, 1.0)))
            pseudo.append((coef, value, (LEVEL_MISMATCH * level0) ** 2))
        power = data_power(phi)
        if power is not None and data_seg >= 0 and free_a[data_seg]:
            pseudo.append(({f"a{data_seg}": 1.0}, power[0], power[1]))
        objective = 0.0
        for coef, value, v in pseudo:
            row = np.array([coef.get(n, 0.0) for n in names])
            nmat += np.outer(row, row) / v
            rhs += row * value / v
        try:
            cov = np.linalg.inv(nmat)
        except np.linalg.LinAlgError:
            return None, np.inf
        theta = cov @ rhs
        objective = float(np.sum((target - m @ theta) ** 2 / var))
        for coef, value, v in pseudo:
            objective += (value - sum(c * theta[names.index(n)] for n, c in coef.items())) ** 2 / v
        return {n: (float(theta[i]), float(np.sqrt(max(cov[i, i], 0.0)))) for i, n in enumerate(names)}, objective

    t_rows = np.arange(nb) * cfg.stride / params.FS
    free_a = [bool(np.any(sr == s)) for s in range(n_seg)]
    best = None
    for tau in LEVEL_TAUS_S:
        phi = np.exp(-t_rows / tau)
        can_b = bool(np.sum(phi[rows] > LEVEL_MIN_SETTLE) >= 2)
        t, objective = fit(free_a, can_b, phi)
        if t is not None and (best is None or objective < best[0]):
            best = (objective, tau, phi, can_b, t)
    if best is None:
        return _no_level(nb, n_seg)
    _, tau, phi, can_b, t = best
    keep_a = [free_a[s] and abs(t[f"a{s}"][0] - 1) > LEVEL_K_SIGMA * t[f"a{s}"][1] for s in range(n_seg)]
    keep_b = can_b and abs(t["b"][0]) > LEVEL_K_SIGMA * t["b"][1]
    if not any(keep_a) and not keep_b:
        return _no_level(nb, n_seg)
    if keep_a != free_a or keep_b != can_b:
        t, _ = fit(keep_a, keep_b, phi)
        if t is None:
            return _no_level(nb, n_seg)
    a = tuple(t[f"a{s}"][0] if keep_a[s] else 1.0 for s in range(n_seg))
    settle = t["b"][0] if keep_b else 0.0
    level = np.array(a)[seg] + settle * phi
    # A fully known row is measured on every base tone: it keeps its own
    # gain (the settling need not follow the exponential exactly, and the
    # header's decision-directed noise would count the misfit).
    level[rows[~pilot_only]] = z[~pilot_only]
    if np.any(level < LEVEL_BOUNDS[0]) or np.any(level > LEVEL_BOUNDS[1]):
        return _no_level(nb, n_seg)
    ref = 1.0
    if cfg.has_probe:
        p0 = cfg.first_probe - first
        ref = float(np.mean(level[p0:p0 + params.PROBE_SYMBOLS]))
    sd = tuple(t[f"a{s}"][1] if keep_a[s] else np.inf for s in range(n_seg)) + (t["b"][1] if keep_b else np.inf,)
    return LevelFit(level, ref, a, settle, tau, sd, True)


def level_matrix(fit: LevelFit, n_tones: int) -> np.ndarray:
    """The tracked gain per row and tone: the level on the base tones (whose
    channel is the S2 estimate), the level over the probe's on the tones
    above 23 (whose channel is the probe's)."""
    b = params.BASE_TONES
    m = np.empty((fit.level.size, n_tones))
    m[:, :b] = fit.level[:, None]
    m[:, b:] = (fit.level / fit.reference)[:, None]
    return m


def _outliers(m: np.ndarray, k_sigma: float = 6.0) -> np.ndarray:
    """Values more than ``k_sigma`` robust standard deviations above the median."""
    med = np.median(m)
    spread = 1.4826 * np.median(np.abs(m - med)) + 1e-9
    return m - med > k_sigma * spread


def burst_noise(eq: np.ndarray, nv: np.ndarray, h: np.ndarray) -> np.ndarray:
    """Extra noise variance (n_sym, n_tones) for symbols hit by a burst.

    ``eq`` and ``nv`` are equalized data symbols on their active tones and the
    noise variance assumed for them; ``h`` the channel on those tones. A
    symbol's mean |eq|^2 is 1 plus its noise; one well above the frame's
    median (``_outliers``) carries extra white noise of that excess power,
    which is spread over the tones as 1/|h|^2 (white noise before
    equalization) and added to their variance.
    """
    m = np.mean(np.abs(eq) ** 2 - nv, axis=1)
    excess = np.where(_outliers(m), m - np.median(m), 0.0)
    inv = 1 / np.abs(h) ** 2
    return excess[:, None] * (inv / inv.mean())[None, :]


PILOT_EXCESS_RATIO = 6.0   # a symbol's pilot error this many times its assumed noise is distortion
MIN_PILOTS = 3             # fewer pilots (24 tones: two) scatter too much to single out a symbol


def symbol_distortion(eq: np.ndarray, nv: np.ndarray, pilots: tuple[int, ...], known: np.ndarray,
                      act: list[int]) -> np.ndarray:
    """Noise variance on the active tones of each data symbol, raised to the
    symbol's own pilot error where that is far above what ``nv`` assumes.

    The noise estimate is an average over the frame, but clip-and-filter
    distortion is not spread evenly: most symbols are clean (EVM below
    -50 dB) and the few whose peaks were clipped carry -15 to -25 dB, flat
    across the symbol's tones. LLRs scaled by the frame average trust those
    symbols far too much and a block that spans only a few wide symbols
    (3.3 at 56 tones, MCS 6) fails: a noise-free error floor near 1%. The
    pilots of the symbol itself measure its distortion. ``eq`` and ``nv``
    are the data rows over all tones, ``known`` the pilots' transmitted
    values per row. Only errors beyond ``PILOT_EXCESS_RATIO`` times the
    assumed noise count, so a noise-limited symbol (a few pilots scatter
    widely) keeps the frame estimate.
    """
    if len(pilots) < MIN_PILOTS:
        return nv[:, act]
    perr = np.mean(np.abs(eq[:, list(pilots)] - known) ** 2, axis=1)
    assumed = np.mean(nv[:, list(pilots)], axis=1)
    out = nv[:, act].copy()
    hit = perr > PILOT_EXCESS_RATIO * assumed
    out[hit] = np.maximum(out[hit], perr[hit, None])
    return out


def _data_noise(noise: np.ndarray, thermal: np.ndarray, gain: float) -> np.ndarray:
    """Noise plus distortion on data symbols scaled up by ``gain``: the
    additive part drops by gain ** 2 once the receiver divides the gain out;
    distortion scales with the signal and stays."""
    return np.maximum(noise - thermal * (1 - gain ** -2), noise / gain ** 2)


# Lines at harmonics of 1 kHz (non-normative): a USB sound card's 1 ms frame
# can leak into a transmitter's audio as a comb of steady lines. On the
# 2026-11-08 70 cm captures they sat 13-24 dB below the data, each midway
# between two tones (k kHz is never a tone: tones are at 350 + 100 t Hz),
# and cost those tones 8-12 dB. Over a whole frame a line stands far above
# the data's own projection on its frequency (about 41 dB below the data for
# a 4 s frame), so it can be measured and subtracted without knowing what
# was sent.
LINE_F0_HZ = 1000.0
LINE_REFERENCE_OFFSETS_HZ = (-30.0, -20.0, 20.0, 30.0)   # the data's own level beside each line
LINE_MARGIN_DB = 10.0       # a line is removed only this far above that level
LINE_MIN_SAMPLES = 4800     # spans shorter than 0.1 s are left alone
LINE_PRESCREEN_SLACK_DB = 6.0   # a line needing a neighbourhood this much below the band average is skipped
LINE_PERIOD = 48            # samples in one period of LINE_F0_HZ


def _projection(seg: np.ndarray, f_hz: float) -> complex:
    """Complex amplitude of the component of ``seg`` at ``f_hz`` (least squares
    for a span much longer than the period): x(t) ~ Re{c exp(j 2 pi f t)}."""
    w = 2 * np.pi * f_hz / params.FS * np.arange(seg.size)
    return complex(2.0 / seg.size * np.sum(seg * np.cos(w)), -2.0 / seg.size * np.sum(seg * np.sin(w)))


def remove_lines(rx: np.ndarray, start: int, n: int, width: int) -> tuple[np.ndarray, list[tuple[int, float]]]:
    """Subtract the lines at harmonics of ``LINE_F0_HZ`` inside the occupied
    band (and the first above it) from the frame's span ``rx[start:start + n]``:
    each harmonic's complex amplitude over the span, removed when its power is
    ``LINE_MARGIN_DB`` above the mean of the projections at
    ``LINE_REFERENCE_OFFSETS_HZ`` beside it. Returns the samples (``rx``
    itself when nothing is removed) and (harmonic, line power in dB against
    a full-scale sine) for each line removed."""
    lo, hi = max(start, 0), min(start + n, rx.size)
    if hi - lo < LINE_MIN_SAMPLES:
        return rx, []
    seg = rx[lo:hi]
    out, removed = rx, []
    margin = 10 ** (LINE_MARGIN_DB / 10)
    # What a projection of the span would hold if all its power were spread
    # evenly over the occupied band. A line less than
    # LINE_MARGIN_DB - LINE_PRESCREEN_SLACK_DB above that could clear the
    # margin only if its neighbourhood were that much quieter than the band,
    # so the reference projections are skipped for it.
    even = 2 * params.FS * float(np.var(seg)) / (seg.size * (params.band_high_hz(width) - params.BAND_LOW_HZ))
    # A harmonic of 1 kHz repeats every LINE_PERIOD samples, so its
    # projection over the span is a DFT bin of the span folded onto one period.
    fold = np.bincount(np.arange(seg.size) % LINE_PERIOD, weights=seg, minlength=LINE_PERIOD)
    m = np.arange(LINE_PERIOD)
    wave = np.zeros(LINE_PERIOD)
    for k in range(1, int(params.band_high_hz(width) // LINE_F0_HZ) + 2):   # the first above the band too
        f = k * LINE_F0_HZ
        bins = np.exp(2j * np.pi * k * m / LINE_PERIOD)
        c = complex(2.0 / seg.size * np.sum(fold * np.conj(bins)))
        if abs(c) ** 2 <= even * 10 ** ((LINE_MARGIN_DB - LINE_PRESCREEN_SLACK_DB) / 10):
            continue
        ref = np.mean([abs(_projection(seg, f + d)) ** 2 for d in LINE_REFERENCE_OFFSETS_HZ])
        if abs(c) ** 2 > margin * ref:
            wave += np.real(c * bins)
            removed.append((k, float(10 * np.log10(abs(c) ** 2))))
    if removed:
        out = rx.astype(float, copy=True)
        out[lo:hi] -= wave[np.arange(hi - lo) % LINE_PERIOD]
    return out, removed


def find_start(rx: np.ndarray, reference: np.ndarray) -> int:
    """Frame start by cross-correlation with the known preamble waveform."""
    corr = signal.correlate(rx, reference, mode="valid", method="fft")
    return int(np.argmax(np.abs(corr)))


def receive(rx: np.ndarray, cfg: FrameConfig | None = None, *, link: LinkSource = None,
            reference: np.ndarray | None = None, cp: int | None = None,
            start: int | None = None, advance: int | None = None,
            phase_window: int = 15, smooth_window: int = 7,
            decision_directed: bool = True, burst_aware: bool = True,
            header_noise: str = "measured", measure_snr: bool = True,
            clock_prior: ClockPrior | Callable[[fields.Header], ClockPrior | None] | None = None,
            track_levels: bool = True, header_channel: bool = True, cancel_lines: bool = True) -> RxResult:
    """Decode one frame from real samples ``rx``.

    Give ``start`` (index of the frame's first sample) or ``reference`` (the
    preamble waveform) to locate it. ``advance`` moves the FFT window that
    many samples into the prefix, to absorb channel delay spread. With
    ``decision_directed``, the decoded header re-estimates each base tone's
    noise plus distortion before the data blocks are demapped. With
    ``burst_aware``, data symbols whose power shows a noise burst get their
    noise variance raised by it (``burst_noise``), so their LLRs are
    discounted instead of trusted (on by default; it never triggered on the
    AWGN, mic or repeater profiles in Stage 1). ``header_noise`` picks the header's
    per-tone noise: "measured" (``header_noise_floor`` raised by
    ``estimate_header_noise``) or "s2" (the S2 difference plus the pilots'
    excess interpolated across the band, as used for the data).
    ``measure_snr`` adds the channel SNR of ADR 0006 to ``detail`` (see
    ``channel_snr``); it costs about one frame build. ``clock_prior`` is the
    sample-clock offset expected before the frame is measured (``ClockPrior``,
    default 0 +- 50 ppm; see ``track_drift``); header first, it may be a
    callable giving the prior for the decoded header (None: the default),
    e.g. what the link layer has measured of the sender on long frames.
    With ``track_levels``, the common gain of the rows after the preamble is
    tracked against the S2 estimate (``track_level``: the radios' level
    settling at the start of a transmission, a preamble that a limiter
    leaves louder than the data), from the pilots and probe and then the
    decoded header, and divided out of the equalised symbols; the noise
    estimates are then made on them (``detail["level"]``). With
    ``header_channel``, once the header decodes the base tones' channel is
    estimated again from the S2 pair and the header's symbols (and the
    probe's), each row at its tracked level (``header_channel_estimate``),
    and every row equalised by it (``detail["header_channel"]``; the S2
    estimate stays in ``detail["h_s2"]``). With ``cancel_lines``, lines at
    harmonics of 1 kHz well above the data are first subtracted from the
    frame's span (``remove_lines``; those removed in ``detail["lines"]``).

    Without ``cfg`` the header is decoded first and the frame's geometry
    taken from its fields (``receive_header_first``), with width and mask
    from ``link`` (a ``LinkState`` or a callable giving one for the header)
    and the prefix ``cp`` (default: the detector's).
    """
    if header_noise not in ("s2", "measured"):
        raise ValueError(f"unknown header_noise {header_noise!r}")
    if cfg is None:
        return receive_header_first(rx, link, reference=reference, start=start, cp=cp, advance=advance,
                                    phase_window=phase_window, smooth_window=smooth_window,
                                    decision_directed=decision_directed, burst_aware=burst_aware,
                                    header_noise=header_noise, measure_snr=measure_snr,
                                    clock_prior=clock_prior, track_levels=track_levels,
                                    header_channel=header_channel, cancel_lines=cancel_lines)
    if callable(clock_prior):
        raise ValueError("a clock prior per header needs the header-first receiver (no cfg)")
    if start is None:
        if reference is None:
            reference = ofdm.modulate(preamble.grid(cfg.width), cfg.cp, active=cfg.width)
        start = find_start(rx, reference)
    lines: list[tuple[int, float]] = []
    if cancel_lines:
        rx, lines = remove_lines(rx, start, cfg.n_samples, cfg.width)
    advance = cfg.cp // 4 if advance is None else advance
    n_sym = cfg.total_symbols
    first = params.PREAMBLE_SYMBOLS
    window_start = start + cfg.cp - advance
    c_first, d_first = cfg.first_control, cfg.first_data       # first control and data symbols
    if window_start + n_sym * cfg.stride > rx.size:            # a frame cut short: decode what is there
        rx = np.concatenate([rx, np.zeros(window_start + n_sym * cfg.stride - rx.size)])
    tones = ofdm.demodulate(rx, window_start, n_sym, cfg.n_tones, cfg.cp, active=cfg.width)
    tones[c_first:d_first] /= cfg.control_gain                 # the mask's known power boost
    tones[d_first:] /= cfg.data_gain
    h, thermal, corr, body, drift, drift_sd, obs = _estimate(
        tones, cfg, smooth_window, phase_window, burst_aware, clock_prior)
    # Follow sample-clock drift: if the windows would wander by a sample or
    # more over the frame, move each symbol's window and estimate again.
    pos = np.arange(n_sym) - 2.5                      # symbols from the channel estimate
    shifts = np.round(drift * pos).astype(int)
    if np.any(shifts != 0):
        rx = np.concatenate([rx, np.zeros(max(int(shifts.max()), 0) + 1)])
        tones = ofdm.demodulate(rx, window_start, n_sym, cfg.n_tones, cfg.cp, active=cfg.width,
                                shifts=shifts)
        # Undo each shift's known phase (moving the window d samples later
        # turns tone bin f by 2 pi f d / N), so the tracker still sees a
        # smooth drift; the window move itself keeps the prefix centred.
        f = params.BIN_OFFSET + np.arange(cfg.n_tones)
        tones = tones * np.exp(-2j * np.pi * f[None, :] * shifts[:, None] / params.N_USEFUL)
        tones[c_first:d_first] /= cfg.control_gain
        tones[d_first:] /= cfg.data_gain
        h, thermal, corr, body, drift, drift_sd, obs = _estimate(
            tones, cfg, smooth_window, phase_window, burst_aware, clock_prior)
    hs = cfg.header_symbols
    c0, d0 = c_first - first, d_first - first                  # rows of ``body`` and ``eq``
    b = params.BASE_TONES
    nv_s2 = thermal[:b] / np.abs(h[:b]) ** 2

    def equalise(lev: LevelFit):
        """Equalised symbols, the pilots' excess and the noise with it, and
        the noise variances, for a level fit."""
        lm = level_matrix(lev, cfg.n_tones)
        ex, nz = pilot_excess(obs, cfg, h, corr, lm, thermal)
        e = body / (h[None, :] * corr * lm)
        v = nz[None, :] / np.abs(h[None, :]) ** 2 * np.ones_like(body.real)
        v[c0:d0] = (_data_noise(nz, thermal, cfg.control_gain) / np.abs(h) ** 2)[None, :]
        v[d0:] = (_data_noise(nz, thermal, cfg.data_gain) / np.abs(h) ** 2)[None, :]
        return e, ex, nz, v

    # Level tracking, first from the pilots and the probe alone (the header
    # is still unknown): a large level change (the radios' ramp at the start
    # of a transmission) would otherwise count as noise in the header.
    level = _no_level(body.shape[0], 2 if cfg.has_probe else 1)
    if track_levels:
        x_known, full = level_cells(cfg, None)
        level = track_level(body[:, :b] / (h[None, :b] * corr[:, :b]), x_known, full, nv_s2, cfg)
    eq, excess, noise, nv = equalise(level)
    snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)

    # Header.
    hcode = ldpc.code_for(params.HEADER_CODE)
    btones = list(cfg.base_data_tones)
    if header_noise == "measured":
        b = params.BASE_TONES
        floor = header_noise_floor(tones[:4, :b], h[:b], thermal[:b], excess)
        bp = list(params.pilots_for(b))
        known_p = pilots_on(first + hs, tuple(bp))[first:]
        nv[:hs, :b] = estimate_header_noise(eq[:hs, :b], known_p, floor, btones, bp)[None, :]
    hl = mapping.demodulate_llr(eq[:hs, btones], nv[:hs, btones], QPSK)
    e_h = params.HEADER_CODE.coded_bits[0]
    hl = mapping.descramble_llr(mapping.deinterleave(hl[:e_h], cfg.interleaver, HEADER_POSITION),
                                HEADER_POSITION)
    hres = ldpc.decode(hcode, hcode.rate_recover(hl, e_h), rows=hcode.rows_needed(hcode.tx_positions(e_h)))
    known = header_points(cfg, hres.bits[0]).reshape(hs, len(btones)) if hres.ok[0] else None
    if track_levels and known is not None:
        # The decoded header is known on every base tone: the level again,
        # with it.
        x_known, full = level_cells(cfg, known)
        level = track_level(body[:, :b] / (h[None, :b] * corr[:, :b]), x_known, full, nv_s2, cfg)
        eq, excess, noise, nv = equalise(level)
        snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)
    h_s2, hch = h, None
    if header_channel and known is not None:
        # The decoded header is known on every base tone: nine known symbols
        # per tone (eleven with the probe) instead of S2's two.
        x_known, full = level_cells(cfg, known)
        hch = header_channel_estimate(tones, h, thermal, corr, level.level, x_known, full, hs, smooth_window)
        h = h.copy()
        h[:b] = hch.h
        eq, excess, noise, nv = equalise(level)
        snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)
    if known is not None and decision_directed:
        # The decoded header is a known signal on every base data tone: its
        # error per tone is noise plus distortion plus ISI, measured where
        # it happens rather than interpolated between two pilots. Equalised
        # by an estimate that contains the header, each row is measured on
        # the estimate without it, as the data's estimate is without theirs.
        eq_h = eq[:hs, btones]
        if hch is not None:
            eq_h = body[:hs, btones] / (hch.h_loo[:, btones] * corr[:hs, btones] * level.level[:hs, None])
        err = np.mean(np.abs(eq_h - known) ** 2, axis=0)
        rel = np.interp(np.arange(params.BASE_TONES), btones, err)
        rel = np.convolve(rel, np.ones(3) / 3, mode="same") / np.convolve(
            np.ones(params.BASE_TONES), np.ones(3) / 3, mode="same")
        noise[:params.BASE_TONES] = np.maximum(rel * np.abs(h[:params.BASE_TONES]) ** 2,
                                               1e-6 * np.mean(np.abs(h) ** 2))
        snr_db = 10 * np.log10(np.abs(h) ** 2 / noise)
        nv[c0:d0] = (_data_noise(noise, thermal, cfg.control_gain) / np.abs(h) ** 2)[None, :]
        nv[d0:] = (_data_noise(noise, thermal, cfg.data_gain) / np.abs(h) ** 2)[None, :]

    # Control blocks: MCS 0 on the base tones, scrambler positions 1 .. n_control.
    ct = list(cfg.control_tones)
    cres = _decode_blocks(eq[c0:d0, ct], nv[c0:d0, ct], QPSK, params.CONTROL_CODE,
                          params.CONTROL_CODE.coded_bits[0], cfg.n_control, 1, cfg.interleaver)

    # Data blocks.
    rung = cfg.rung
    dt = list(cfg.data_tones)
    if body.shape[0] > d0:
        nv[d0:, list(cfg.data_active_tones)] = symbol_distortion(
            eq[d0:], nv[d0:], cfg.data_pilots, pilots_on(body.shape[0] + first, cfg.data_pilots)[first:][d0:],
            list(cfg.data_active_tones))
    if burst_aware and body.shape[0] > d0:
        act = list(cfg.data_active_tones)
        nv[d0:, act] += burst_noise(eq[d0:, act], nv[d0:, act], h[act])
    dres = _decode_blocks(eq[d0:, dt], nv[d0:, dt], rung.bits_per_symbol, params.DATA_CODE, rung.coded_bits,
                          cfg.n_blocks, 1 + cfg.n_control, cfg.interleaver)
    detail = {"h": h, "noise": noise, "thermal": thermal, "eq": eq, "d0": d0, "c0": c0,
              "window_start": window_start, "shifts": shifts, "drift": drift, "drift_sd": drift_sd,
              "corr": corr, "level": level, "nv": nv, "h_s2": h_s2, "header_channel": hch, "lines": lines}
    header, header_error = parse_header(bool(hres.ok[0]), hres.bits[0])
    controls = parse_controls(cres.ok, cres.bits)
    out = RxResult(bool(hres.ok[0]), hres.bits[0], dres.ok, dres.bits, start, snr_db, dres.iterations, detail,
                   control_ok=cres.ok, control_bits=cres.bits, header=header, header_error=header_error,
                   controls=controls, config=cfg,
                   block_sequences=_sequences(header, controls, cfg.n_blocks))
    if measure_snr:
        _add_channel_snr(rx, cfg, out)
    return out


def _decode_blocks(eq: np.ndarray, nv: np.ndarray, bits_per_symbol: int, spec: params.CodeSpec, e: int,
                   n: int, first_position: int, interleaver: str) -> ldpc.DecodeResult:
    """Demap a segment's equalized points and decode its first ``n`` blocks of
    ``e`` coded bits, the first at scrambler position ``first_position``."""
    if n == 0:
        return ldpc.DecodeResult(np.zeros((0, spec.info_bits), np.uint8), np.zeros(0, bool),
                                 np.zeros(0, np.int32))
    code = ldpc.code_for(spec)
    ll = mapping.demodulate_llr(eq, nv, bits_per_symbol)
    llrs = np.stack([mapping.descramble_llr(mapping.deinterleave(ll[i * e:(i + 1) * e], interleaver,
                                                                 first_position + i), first_position + i)
                     for i in range(n)])
    return ldpc.decode(code, code.rate_recover(llrs, e), rows=code.rows_needed(code.tx_positions(e)))


def _sequences(header: fields.Header | None, controls: tuple, n_blocks: int) -> tuple[int, ...] | None:
    if header is None or header.n_data != n_blocks:
        return None
    return fields.block_sequences(header, controls)


def parse_header(ok: bool, bits: np.ndarray) -> tuple[fields.Header | None, str | None]:
    """The header's fields, or why there are none (failed decode, CRC or malformed fields)."""
    if not ok:
        return None, "header did not decode"
    try:
        return fields.Header.unpack(bits), None
    except fields.FieldError as e:
        return None, str(e)


def parse_controls(ok: np.ndarray, bits: np.ndarray) -> tuple:
    """Each decoded control block's fields (None where it failed or is malformed)."""
    out = []
    for good, b in zip(ok, bits):
        try:
            out.append(fields.unpack_control(b) if good else None)
        except fields.FieldError:
            out.append(None)
    return tuple(out)


def locate(rx: np.ndarray, cp: int | None = None,
           reference: np.ndarray | None = None) -> tuple[int, int] | None:
    """Frame start and prefix: the strongest SPEC 6.1 detection (the prefix is
    the hypothesis that wins, unless ``cp`` is given), else cross-correlation
    with ``reference`` or the preamble waveform at ``cp`` (short by default)."""
    if reference is None:
        dets = [d for d in preamble.detect(rx) if cp is None or d.cp == cp]
        if dets:
            best = max(dets, key=lambda d: d.quality)
            return int(best.start), int(best.cp)
    cp = params.CP_SHORT if cp is None else cp
    if reference is None:
        reference = ofdm.modulate(preamble.grid(params.BASE_TONES), cp, active=params.BASE_TONES)
    if rx.size < reference.size:
        return None
    return find_start(rx, reference), cp


# --- probe extent (ADR 0007 broadcast-width-from-probe) -----------------------

EXTENT_SIGMA = 5.0          # a tone carries the probe when its signal clears this many noise deviations
EXTENT_FLOOR_DB = -18.0     # ... and this level against the base tones (regrowth, mic-path leakage: -22 to -33 dB)
EXTENT_NOISE_TONES = 11     # neighbouring upper tones pooled for each tone's noise reference


@dataclass
class ProbeExtent:
    """Which tones above 23 carry a frame's width probe (``probe_extent``).

    ``signal``, ``threshold`` and ``present`` are per tone 24-55; ``width``
    is the highest tone carrying the probe plus one (24 if none does).
    ``data_width`` is set by the header-first receiver: the width at which a
    data block decoded (its CRC passed), the probe's or one tried after it
    (``width_alternatives``); None when the frame has no data blocks, or when
    none decodes at any width tried, so the frame's width is unknown and the
    configuration's (the probe's reading) is unconfirmed.
    """
    width: int
    signal: np.ndarray
    threshold: np.ndarray
    present: np.ndarray
    data_symbols: int
    data_width: int | None = None

    def margin_db(self) -> tuple[float, float]:
        """Decision margins at the edge, dB: the last tone in the extent over
        its threshold, and the threshold over the first tone beyond it (inf
        where there is no such tone)."""
        b = params.BASE_TONES
        i_in, i_out = self.width - 1 - b, self.width - b
        inside = (10 * np.log10(max(self.signal[i_in], 1e-30) / self.threshold[i_in])
                  if i_in >= 0 else np.inf)
        outside = (10 * np.log10(self.threshold[i_out] / max(self.signal[i_out], 1e-30))
                   if i_out < self.signal.size else np.inf)
        return float(inside), float(outside)


def extent_geometry(header: fields.Header, cp: int) -> FrameConfig:
    """The frame a header describes at 56 tones: the rows before the data do
    not depend on the width, and its data symbols are the fewest any width
    gives, so they are present whatever the width turns out to be."""
    return FrameConfig(width=params.MAX_TONES, mcs=header.mcs if header.n_data else 0,
                       n_blocks=header.n_data, n_control=header.n_control, cp=cp, probe=True)


def extent_samples(header: fields.Header, cp: int) -> int:
    """Samples from the frame's start that ``probe_extent`` reads."""
    cfg = extent_geometry(header, cp)
    return (cfg.first_data + cfg.data_symbols) * cfg.stride


def probe_extent(rx: np.ndarray, start: int, cp: int, header: fields.Header, *,
                 advance: int | None = None) -> ProbeExtent:
    """A frame's width from its probe (ADR 0007 ``broadcast-width-from-probe``).

    Each tone k above 23 is tested for the probe sequence. Its two probe
    symbols, divided by the known values, both measure the channel when the
    tone carries the probe, so their correlation Re{z0 conj(z1)} estimates
    the tone's signal power; when it does not, the two are unrelated and the
    correlation averages zero (clip-and-filter regrowth above the cap is not
    the probe sequence). In a broadcast the data blocks use exactly the
    probed tones, so the power in the data symbols that every width has
    (``extent_geometry``) adds to the estimate, less the noise measured on
    the same tone in the preamble, header and control symbols, which carry
    nothing above tone 23. A tone counts when that estimate clears
    ``EXTENT_SIGMA`` standard deviations of its noise-only value and
    ``EXTENT_FLOOR_DB`` below the base tones' probe power. The width is the
    change point of the run: the count of tones that do minus tones that do
    not, summed from tone 24 upward, peaks one past the last probed tone
    (ties to the higher), so one stray tone on either side does not move it.
    """
    b, m = params.BASE_TONES, params.MAX_TONES
    if not header.probe:
        z = np.zeros(m - b)
        return ProbeExtent(b, z, np.ones(m - b), z.astype(bool), 0)
    cfg = extent_geometry(header, cp)
    n_data = cfg.data_symbols
    rows = cfg.first_data + n_data
    advance = cp // 4 if advance is None else advance
    ws = start + cp - advance
    need = ws + rows * cfg.stride
    if rx.size < need:
        rx = np.concatenate([rx, np.zeros(need - rx.size)])
    y = ofdm.demodulate(rx, ws, rows, m, cp, active=m)
    p0 = cfg.first_probe
    known = probe_grid(m, p0)
    z0, z1 = y[p0] / known[0], y[p0 + 1] / known[1]
    q = np.real(z0 * np.conj(z1))
    silent = np.r_[0:p0, cfg.first_control:cfg.first_data]
    pw = np.mean(np.abs(y[silent]) ** 2, axis=0)
    half = EXTENT_NOISE_TONES // 2
    upper = np.arange(b, m)
    lo = np.clip(upper - half, b, m)
    hi = np.clip(upper + half + 1, b, m)
    noise = np.array([np.mean(pw[a:c]) for a, c in zip(lo, hi)])
    n_ref = (hi - lo) * silent.size
    if n_data:
        energy = np.mean(np.abs(y[cfg.first_data:rows, b:]) ** 2, axis=0) - noise
    else:
        energy = np.zeros(m - b)
    sig = (2 * q[b:] + n_data * energy) / (2 + n_data)
    sd = noise * np.sqrt(2 + n_data + n_data ** 2 / n_ref) / (2 + n_data)
    ref = float(np.median(np.abs((z0[:b] + z1[:b]) / 2) ** 2))
    thr = np.maximum(EXTENT_SIGMA * sd, ref * 10 ** (EXTENT_FLOOR_DB / 10))
    present = sig > thr
    run = np.concatenate([[0], np.cumsum(np.where(present, 1, -1))])
    j = int(np.flatnonzero(run == run.max())[-1])
    return ProbeExtent(b + j, sig, thr, present, n_data)


EXTENT_RETRY = (1, -1, 2, -2, 3, -3)     # widths tried around the probe's when no block decodes


def width_alternatives(width: int) -> list[int]:
    """Widths to try when nothing decodes at the probe's: its neighbours,
    nearest first (a weak edge tone can mislead the extent by a tone or
    two), then every wider width up to 56 in turn.

    The extent bounds the width from below only. Tones on which the probe
    sequence is received were sent (up to a stray tone or two), but a
    channel that rolls off below the sent width hides the probe's top tones
    under its noise: the 2026-11-07 radios pass audio to about 4.25 kHz
    (tone 39), and a 48-tone broadcast through them reads 38. The probe
    cannot tell those widths apart; the data can, as its blocks decode only
    at the width they were sent at.
    """
    b, m = params.BASE_TONES, params.MAX_TONES
    near = [width + d for d in EXTENT_RETRY if b <= width + d <= m]
    return near + list(range(width + max(EXTENT_RETRY) + 1, m + 1))


def frame_link(header: fields.Header, link: LinkState | None, extent: ProbeExtent | None) -> LinkState:
    """The link state a frame is decoded with: the caller's for a session
    frame (header flag bit 3); outside a session, or with no link state
    (``link`` None, as for a CONNECT-ACK before its B.3 is known), the
    probe's extent as width and cap, with no mask (ADR 0007), or 24 tones
    without a probe. The prefix stays the caller's."""
    if link is not None and header.session:
        return link
    width = extent.width if extent is not None and header.probe else params.BASE_TONES
    return LinkState(width=width, cp=link.cp if link is not None else None, cap=width)


LinkSource = LinkState | Callable[[fields.Header], LinkState | None] | None


def _link_for(source: LinkSource, header: fields.Header) -> LinkState | None:
    return source(header) if callable(source) else source


def receive_header_first(rx: np.ndarray, link: LinkSource = None, *,
                         reference: np.ndarray | None = None, start: int | None = None,
                         cp: int | None = None, measure_snr: bool = True, **kw) -> RxResult:
    """Decode a frame without being told its configuration (SPEC 8.2).

    Find the frame (``locate``; giving ``start`` skips the search and takes
    the prefix from ``cp``, else ``link.cp``, default short), decode the
    preamble and header on the base tones, unpack the header, and take the
    data rung, the control and data block counts and the probe flag from it.
    ``link`` is a ``LinkState`` or a callable that returns one (or None) for
    the decoded header: the caller supplies link state once it knows who
    sent the frame. A session frame (flag bit 3) takes its width, probe
    extent and the mask its Mask ID names from it (ADR 0011); a Mask ID it
    does not hold fails the frame's control and data blocks
    (``link_error``). Any other frame, or a session frame with no link state
    (None), takes its width from its own probe (``probe_extent``, ADR 0007;
    in ``detail["probe_extent"]``), or is 24 tones without one; if no data
    block decodes at the probe's width, the widths around it and then every
    wider one are tried (``width_alternatives``) and the first at which a
    block decodes is kept (``ProbeExtent.data_width``; None when none does:
    the frame's width is then unknown, not the probe's).
    Then decode the whole frame with that configuration, and name each data
    block's sequence (``block_sequences``, ADR 0008).
    A frame whose header fails (decode, CRC or fields) comes back with no
    blocks, ``header`` None and ``header_error`` saying why. ``measure_snr``
    measures the channel SNR (ADR 0006) once the final header is settled;
    a frame whose header fails gets the fallback over its preamble.
    ``clock_prior`` (a ``ClockPrior``, or a callable giving one or None for
    the decoded header) is the clock offset expected of the frame's sender
    (``track_drift``); the header pass uses the default unless it is fixed.
    """
    prior = kw.pop("clock_prior", None)
    cancel = kw.get("cancel_lines", True)
    if callable(prior):
        kw_head = kw
        frame_prior = None
    else:
        kw_head = dict(kw, clock_prior=prior)
        frame_prior = prior
    known = link if isinstance(link, LinkState) else None
    if start is None:
        found = locate(rx, cp if cp is not None else (known.cp if known else None), reference)
        if found is None:
            raise ValueError("no frame found")
        start, cp = found
    elif cp is None:
        cp = known.cp if known is not None and known.cp is not None else params.CP_SHORT
    head_cfg = FrameConfig(width=params.BASE_TONES, n_blocks=0, cp=cp, probe=False)
    head = receive(rx, head_cfg, start=start, measure_snr=False, **kw_head)
    rx_head = remove_lines(rx, start, head_cfg.n_samples, head_cfg.width)[0] if cancel else rx
    if head.header is None:
        if measure_snr:
            _add_channel_snr(rx_head, head_cfg, head)
        head.config = None
        return head
    link = _link_for(link, head.header)
    if callable(prior):
        frame_prior = prior(head.header)
    kw = dict(kw, clock_prior=frame_prior)
    extent = None
    if (link is None or not head.header.session) and head.header.probe:
        extent = probe_extent(rx, start, cp, head.header, advance=kw.get("advance"))
    try:
        cfg = FrameConfig.from_header(head.header, frame_link(head.header, link, extent), cp=cp)
    except UnknownMask as e:
        # ADR 0011 mask-id: the frame's control and data blocks count as failed.
        if measure_snr:
            _add_channel_snr(rx_head, head_cfg, head)
        h = head.header
        head.config, head.link_error, head.block_sequences = None, str(e), None
        head.control_ok = np.zeros(h.n_control, dtype=bool)
        head.control_bits = np.zeros((h.n_control, params.CONTROL_INFO_BITS), np.uint8)
        head.controls = (None,) * h.n_control
        head.block_ok = np.zeros(h.n_data, dtype=bool)
        head.block_bits = np.zeros((h.n_data, params.DATA_INFO_BITS), np.uint8)
        head.iterations = np.zeros(h.n_data, np.int32)
        return head
    except ValueError as e:
        if measure_snr:
            _add_channel_snr(rx_head, head_cfg, head)
        head.header_error = f"header describes no decodable frame: {e}"
        head.header, head.config = None, None
        return head
    removed: list[tuple[int, float]] = []
    if cancel:
        # Once, over the whole frame: the receptions below find nothing more.
        rx, removed = remove_lines(rx, start, cfg.n_samples, cfg.width)
    r = receive(rx, cfg, start=start, measure_snr=False, **kw)
    if extent is not None and cfg.n_blocks and not r.block_ok.any():
        # No block decoded at the probe's width: a weak edge tone can mislead
        # the extent by a tone or two, and a channel narrower than the frame
        # hides its top tones entirely. The data decodes only at the width it
        # was sent at, so try the neighbours and then every wider width.
        for w in width_alternatives(extent.width):
            try:
                alt = FrameConfig.from_header(head.header, LinkState(width=w, cap=w), cp=cp)
            except ValueError:
                continue
            ra = receive(rx, alt, start=start, measure_snr=False, **kw)
            if ra.block_ok.any():
                cfg, r = alt, ra
                break
    # The header pass decided the geometry; report its header.
    r.header_ok, r.header_bits, r.header, r.header_error = True, head.header_bits, head.header, None
    r.block_sequences = _sequences(r.header, r.controls, cfg.n_blocks)
    if extent is not None:
        # The width is known only where the data confirms it: without that,
        # the probe's reading stands in the configuration, unconfirmed.
        extent.data_width = cfg.width if cfg.n_blocks and r.block_ok.any() else None
    r.detail["probe_extent"] = extent
    r.detail["lines"] = removed
    if measure_snr:
        _add_channel_snr(rx, cfg, r)
    return r


# --- channel SNR (ADR 0006) ---------------------------------------------------

SNR_MAX_DB = 63.75      # B.2 field range: larger values saturate
MIN_KNOWN_SYMBOLS = 8   # fewer known symbols than this: use the fallback estimate
FALLBACK_MARGIN_DB = 1.0  # the fallback sees a weak frame through a few symbols (+-2 dB at 0 dB): err low
FALLBACK_H_BIAS = 1 / 6  # noise power left in |h|^2: two S2 symbols averaged (1/2), 7-tone quadratic smoothing (1/3)
# Symbols from the frame's start in which the radios' audio level may still be
# settling: the 2026-11-07 over-the-air captures show the received level rise
# from 13 to 22 dB below its steady value at the first preamble sample, with a
# 19 ms time constant, frequency-flat. In the channel-SNR fit each known
# symbol after the preamble and before this has a gain of its own.
SNR_SETTLE_SYMBOLS = 8
SETTLE_ITERATIONS = 4   # alternations between the per-tone and per-symbol gains
LOW_DOF_POOL = 2        # a tone measured on two symbols only pools its noise over +-2 such tones


@dataclass
class ChannelSnr:
    snr_db: float             # 7 dB-equivalent channel SNR, saturated at SNR_MAX_DB
    per_tone_db: np.ndarray   # (width,) same scale; measured on tones with 2+ known symbols (probed tones), else interpolated
    known_symbols: int        # symbols whose content the receiver knew
    fallback: bool            # the estimate counts the designed clipping (errs low)
    # Per tone, linear and average-referenced (no 7 dB-equivalent offset): the
    # signal and noise powers that snr_db sums over the data's active tones,
    # both 0 on a tone not measured. Summed over another set of tones they give
    # the channel SNR that set would see (the tones a mask keeps).
    tone_signal: np.ndarray | None = None
    tone_noise: np.ndarray | None = None


def known_symbols(cfg: FrameConfig, header_ok: bool, block_ok: np.ndarray,
                  control_ok: np.ndarray | None = None) -> np.ndarray:
    """Rows of the frame whose transmitted tones the receiver knows: the
    preamble and probe always, the header once it decoded, each control
    symbol whose control blocks (those its points fall in) all decoded, and
    each data symbol whose data blocks all decoded. Symbols holding only
    padding are known. ``control_ok`` None counts no control block as
    decoded."""
    known = np.ones(cfg.total_symbols, dtype=bool)
    h0 = params.PREAMBLE_SYMBOLS
    known[h0:h0 + cfg.header_symbols] = header_ok
    control_ok = (np.zeros(cfg.n_control, dtype=bool) if control_ok is None
                  else np.asarray(control_ok, dtype=bool))
    ct = len(cfg.control_tones)
    per_block = params.CONTROL_CODE.coded_bits[0] // QPSK       # QPSK points per control block
    for s in range(cfg.control_symbols):
        lo = (s * ct) // per_block
        hi = min(((s + 1) * ct - 1) // per_block, cfg.n_control - 1)   # beyond the last block: padding
        known[cfg.first_control + s] = bool(np.all(control_ok[lo:hi + 1])) if lo <= hi else True
    d0 = cfg.first_data
    per_symbol = cfg.rung.bits_per_symbol * len(cfg.data_tones)
    e = cfg.rung.coded_bits
    for s in range(cfg.data_symbols):
        lo = (s * per_symbol) // e
        hi = min(((s + 1) * per_symbol - 1) // e, cfg.n_blocks - 1)   # beyond the last block: padding, known
        known[d0 + s] = bool(np.all(block_ok[lo:hi + 1])) if lo <= hi else True
    return known


def _clip_included_snr(res: RxResult, cfg: FrameConfig) -> tuple[float, np.ndarray, np.ndarray, np.ndarray]:
    """Fallback: the measured SNR over the data tones, which counts the
    designed clipping as noise (errs low), on the 7 dB-equivalent scale, and
    per tone the signal (margin taken off) and noise powers behind it."""
    h, noise, thermal = res.detail["h_s2"], res.detail["noise"], res.detail["thermal"]
    t = [k for k in cfg.data_active_tones if k < cfg.n_tones]     # a frame without a probe: the base tones
    off = cfg.clip_db - 7.0
    # |h|^2 carries the noise left in the smoothed S2 estimate (about a third
    # of half the S2 noise): take it out, or a weak frame reads high.
    p = np.maximum(np.abs(h) ** 2 - thermal * FALLBACK_H_BIAS, 0.1 * np.abs(h) ** 2)
    off -= FALLBACK_MARGIN_DB
    overall = 10 * np.log10(np.sum(p[t]) / np.sum(noise[t])) + off
    return (float(overall), 10 * np.log10(p / np.maximum(noise, 1e-30)) + off,
            p * 10 ** (-FALLBACK_MARGIN_DB / 10), np.asarray(noise, dtype=float).copy())


def channel_snr(rx: np.ndarray, cfg: FrameConfig, res: RxResult) -> ChannelSnr:
    """Channel SNR on the 7 dB-equivalent scale (ADR 0006).

    The receiver rebuilds the symbols it knows from the decoded bits
    (``rebuild``: re-encode, scramble, interleave, map, clip-and-filter as the
    transmitter did) and demodulates the rebuilt waveform, so the designed
    clipping is part of the reference. Each tone's gain is then fit by least
    squares against the rebuilt tones over the known symbols (the channel
    estimate from the preamble is too noisy to leave a 40 dB residue), after
    the pilots' phase and timing tracking already in ``res.detail``. The
    preamble is left out of the fit (built for a low crest factor, it escapes
    the radios' limiters, and their level is still settling under it), and
    each known symbol before ``SNR_SETTLE_SYMBOLS`` gets a gain of its own,
    its residual taken at the steady level and its degree of freedom from
    the residual's (the radios' level settling at the start of a
    transmission would otherwise count as noise: up to 7 dB on a header-only
    frame over the air). The
    residual is noise plus everything else the channel and radios add;
    signal is the fitted gain's power on the rebuilt tones as sent, a mask's
    power boost (``control_gain``, ``data_gain``) included: that is what
    the decoder sees and what the rung thresholds were measured against
    (ADR 0006 interpretation ``mask-boost-counts-as-signal``). The known symbols
    are the preamble and probe, the header once it decoded, and the control
    and data symbols whose blocks decoded (``known_symbols``).

    With fewer than ``MIN_KNOWN_SYMBOLS`` known symbols, or a frame with data
    of which no data symbol decoded (a weak frame), the result is the
    fallback estimate, which counts the designed clipping. Frames with no
    data blocks (header only, or control blocks only) are measured over
    everything they carry.
    """
    det = res.detail
    n_sym, w = cfg.total_symbols, cfg.n_tones
    known = known_symbols(cfg, res.header_ok, res.block_ok, res.control_ok)
    off = cfg.clip_db - 7.0
    n_known = int(known.sum())
    fb_snr, fb_tone, fb_sig, fb_noise = _clip_included_snr(res, cfg)
    d0 = cfg.first_data
    # A frame with data whose data symbols all failed is weak, and its header
    # decoding at all selects for a lucky, low-noise header: measured over the
    # preamble and header alone, the figure reads high (up to +1.6 dB at 1 dB
    # in Stage 1). Header-only and control-only frames are measured over
    # everything they carry.
    no_data_known = cfg.data_symbols > 0 and not known[d0:].any()
    if n_known < MIN_KNOWN_SYMBOLS or no_data_known:
        return ChannelSnr(min(fb_snr, SNR_MAX_DB), np.minimum(fb_tone, SNR_MAX_DB), n_known, True, fb_sig, fb_noise)

    shifts = det["shifts"]
    first = params.PREAMBLE_SYMBOLS
    # Zeros past the end: room for the window shifts, and for a frame cut short.
    need = int(det["window_start"]) + n_sym * cfg.stride + max(int(np.max(shifts)), 0) + 1
    pad = np.concatenate([rx, np.zeros(max(need - rx.size, 0))])
    y = ofdm.demodulate(pad, det["window_start"], n_sym, w, cfg.cp, active=w,
                        shifts=shifts if np.any(shifts != 0) else None)
    if np.any(shifts != 0):
        f = params.BIN_OFFSET + np.arange(w)
        y = y * np.exp(-2j * np.pi * f[None, :] * shifts[:, None] / params.N_USEFUL)

    drift_c = np.ones((n_sym, w), dtype=complex)
    drift_c[first:] = det["corr"]

    sent = rebuild(cfg, res.header_bits, res.block_bits, res.control_bits)
    x = ofdm.demodulate(np.concatenate([sent.samples, np.zeros(1)]), cfg.cp, n_sym, w, cfg.cp, active=w)
    x = x * (sent.grid != 0)             # silent tones carry nothing: leave the rebuild's leakage out
    # The preamble is left out of the fit: its symbols are built for a low
    # crest factor (SPEC 6.1), so a radio's limiter, which sets the noise of
    # a strong FM link, does not touch them, and the radios' level is still
    # settling under them (over the air they read 5-8 dB cleaner than the
    # data). Counted in, they let a short frame over-report.
    fit = known.copy()
    fit[:first] = False
    y, x, drift_c = y[fit], x[fit], drift_c[fit]
    rows = np.flatnonzero(fit)
    u = rows - 2.5                                      # symbols from the S2 pair, where the channel was set
    fk = params.BIN_OFFSET + np.arange(w)
    live = np.abs(x) > 0
    # The radios' level settles over the first symbols (SNR_SETTLE_SYMBOLS):
    # those known symbols get a gain of their own, when a later one anchors the level.
    settle = rows < SNR_SETTLE_SYMBOLS if np.any(rows >= SNR_SETTLE_SYMBOLS) else np.zeros(rows.size, dtype=bool)
    a = np.ones(rows.size, dtype=complex)
    for _ in range(3):
        # Gains per tone (and per settling symbol), then the common phase drift
        # (alpha + beta f) per symbol re-fit against the rebuilt tones: pilots
        # alone leave a few hundredths of a radian of the designed clipping's
        # phase noise.
        h, a = _fit_gains(y / drift_c, x, a, settle)
        ref = h[None, :] * a[:, None] * x
        theta = np.angle(y * np.conj(ref * drift_c) + 1e-300)           # residual phase of each cell
        wgt = np.abs(ref) ** 2 * live
        uu = np.broadcast_to(u[:, None], theta.shape)
        ff = np.broadcast_to(fk[None, :], theta.shape)
        a_mat = np.stack([uu.ravel(), (uu * ff).ravel()], axis=1) * np.sqrt(wgt.ravel())[:, None]
        sol = np.linalg.lstsq(a_mat, theta.ravel() * np.sqrt(wgt.ravel()), rcond=None)[0]
        drift_c = drift_c * np.exp(1j * (sol[0] * uu + sol[1] * uu * ff))
    y = y / drift_c
    h, a = _fit_gains(y, x, a, settle)
    n_k = np.sum(live, axis=0)
    # Each settling symbol's own gain takes one complex degree of freedom
    # from the tones it carries.
    m_k = np.sum(live[settle] / np.maximum(np.sum(live[settle], axis=1, keepdims=True), 1), axis=0)
    # Twice: then again with each tone weighted by its inverse noise in the
    # settling symbols' gains, so a few very noisy tones (an interferer, a
    # tone the mask will drop) do not spread their noise over the others.
    for rnd in range(2 if settle.any() else 1):
        if rnd == 1:
            h, a = _fit_gains(y, x, a, settle, 1 / np.maximum(noise, 1e-12 * max(float(np.max(noise)), 1e-30)))
        xa = a[:, None] * x
        den = np.sum(np.abs(xa) ** 2, axis=0)
        # Only the cells a tone carries something in: a silent cell (upper
        # tones in the preamble, header and control symbols, masked tones)
        # holds noise but no fit, and is not counted in n_k either. A
        # settling symbol's level scales its noise too (the radios'
        # clipping, which follows the level): its residual counts at the
        # steady level.
        resid = np.sum(np.abs(y - h[None, :] * xa) ** 2 * live / np.abs(a[:, None]) ** 2, axis=0)
        noise = resid / np.maximum(n_k - 1 - m_k, 1)    # one complex gain fitted per tone
    # |h|^2 over-reads by the noise left in the fit (noise / sum |x|^2).
    h2 = np.maximum(np.abs(h) ** 2 - noise / np.maximum(den, 1e-30), 0.05 * np.abs(h) ** 2)
    sig = h2 * np.sum(np.abs(x) ** 2, axis=0) / np.maximum(n_k, 1)     # the mask's boost counts
    tiny = 1e-12 * max(float(np.max(sig)), 1e-30)
    ok = n_k >= 3
    t = np.array([k for k in cfg.data_active_tones if k < w and ok[k]], dtype=int)
    if t.size == 0:
        return ChannelSnr(min(fb_snr, SNR_MAX_DB), np.minimum(fb_tone, SNR_MAX_DB), n_known, True, fb_sig, fb_noise)
    overall = 10 * np.log10(np.sum(sig[t]) / max(np.sum(noise[t]), tiny)) + off
    per = np.full(w, np.nan)
    # A tone with one degree of freedom left (a probed tone above 23 in a
    # frame whose data did not reach it: its two probe symbols) is measured in
    # its own right, not taken from its neighbours.
    # One degree of freedom scatters a tone's noise by +-9 dB: such tones
    # pool their residual with their like within LOW_DOF_POOL tones.
    dof = n_k - 1 - m_k
    meas = dof >= 1 - 1e-9
    low = meas & (dof < 2 - 1e-9)
    noise_tone = noise.copy()
    for k in np.flatnonzero(low):
        js = [j for j in range(k - LOW_DOF_POOL, k + LOW_DOF_POOL + 1) if 0 <= j < w and low[j]]
        noise_tone[k] = np.sum(resid[js]) / np.sum(dof[js])
    per[meas] = 10 * np.log10(sig[meas] / np.maximum(noise_tone[meas], tiny)) + off
    per = np.interp(np.arange(w), np.flatnonzero(meas), per[meas])
    return ChannelSnr(float(min(overall, SNR_MAX_DB)), np.minimum(per, SNR_MAX_DB), n_known, False,
                      np.where(ok, sig, 0.0), np.where(ok, noise, 0.0))


def _fit_gains(y: np.ndarray, x: np.ndarray, a: np.ndarray, settle: np.ndarray,
               wt: np.ndarray | None = None) -> tuple[np.ndarray, np.ndarray]:
    """Per-tone gains h, and the gains a of the ``settle`` symbols (1 for the
    rest), of y = h_k a_s x by alternating least squares from ``a``, each
    tone weighted by ``wt`` (default 1) in the symbols' gains."""
    wt = np.ones(x.shape[1]) if wt is None else wt
    def tone_gains(a):
        xa = a[:, None] * x
        return np.sum(y * np.conj(xa), axis=0) / np.maximum(np.sum(np.abs(xa) ** 2, axis=0), 1e-30)
    if not settle.any():
        return tone_gains(a), a
    a = a.copy()
    for _ in range(SETTLE_ITERATIONS):
        h = tone_gains(a)
        ref = h[None, :] * x[settle]
        a[settle] = (np.sum(y[settle] * np.conj(ref) * wt, axis=1)
                     / np.maximum(np.sum(np.abs(ref) ** 2 * wt, axis=1), 1e-300))
    return tone_gains(a), a


def _add_channel_snr(rx: np.ndarray, cfg: FrameConfig, res: RxResult) -> None:
    """Put the ADR 0006 channel SNR (``channel_snr``) into ``res.detail``."""
    m = channel_snr(rx, cfg, res)
    res.detail.update(snr_db=m.snr_db, snr_per_tone_db=m.per_tone_db, snr_known_symbols=m.known_symbols,
                      snr_total_symbols=cfg.total_symbols, snr_fallback=m.fallback,
                      snr_tone_signal=m.tone_signal, snr_tone_noise=m.tone_noise)
