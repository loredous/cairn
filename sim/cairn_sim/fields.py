"""Frame header and control block fields (SPEC 8.2, 8.3, Appendix B).

Pack and unpack the 144-bit header (SPEC 8.2, ADR 0011
``header-field-layout``), the 40-bit address (8.3, B.1) and every 256-bit
control block of Appendix B: ACK (B.2, ADR 0011 ``ack-control-block-layout``),
CONNECT / CONNECT-ACK (B.3), CAL-REQUEST and CAL-REPORT (B.5), ID (B.6),
path (B.7) and block map (B.8, ADR 0008 ``block-map-control-block``); the
control-block order of each frame type (ADR 0010 ``control-block-order``,
``session-establishment-steps``, ADR 0012 ``probe-frame-contents``:
``check_controls``) and the block map's rules (``block_sequences``). Bits are numpy uint8 arrays, first bit on air first; every
field is big-endian and every block ends with its CRC-16/CCITT-FALSE over
the bits before it (6.3).

Readings chosen where the SPEC leaves room (listed in ``vectors/README.md``):

- **Bit numbering.** "Bit n" of a field is the bit of weight 2^n: bit 0 is
  the least significant and is sent last. Fields made of one bit per item
  (the ACK block bitmap, the tone masks, CAL-REPORT's clip flags) put item
  i at bit i (tone k at bit k, block base + i at bit i, step j at bit
  j - 1). Fields made of a sequence of multi-bit items (callsign
  characters, CAL-REPORT amplitudes and distortions, dictionary IDs, ID
  block text) put the first item in the most significant bits, as B.1 does
  for the callsign.
- **Absent optional fields** (the ACK tone mask without flag bit 0, the ID
  block's grid locator without flag bit 0, unused path entries) are sent
  as zeros and ignored on receipt.
- **Reserved bits** are sent as zero and ignored on receipt, except that a
  header whose version is not 1 is rejected (its layout is unknown).
- **Header MCS without data blocks** is sent as given (0 by default) and
  ignored on receipt; such a frame is clipped at 7 dB (SPEC 7.1).
- **Callsign characters.** Short and long callsigns use the B.1 alphabet
  (A-Z, 0-9, '/', '-'); a long callsign has 7-10 characters. An address
  with a reserved code, a padding code before a character, code 63 outside
  the first position, or no character at all is malformed.
- **Measured SNR** (B.2) is quantized to the nearest 0.25 dB, halves up,
  and saturates at 0 and 63.75 dB.
- **Grid locator** (B.6): 4 or 6 Maidenhead characters, space-padded to 6.
- **Dictionary-only B.3** (ADR 0010 ``session-establishment-steps``): the
  second B.3 block of a CONNECT or CONNECT-ACK sends every field but the
  dictionary IDs as zero, so its width cap (0) and maximum frame time (0)
  lie outside 23-55 and 5-40. A B.3 block whose width cap and maximum frame
  time are both 0 is read as such a block (``ConnectBlock.dictionary_only``);
  any other value outside the ranges is malformed.

Sequence numbers compare with serial-number arithmetic (RFC 1982,
SERIAL_BITS = 16; ADR 0008 ``block-sequence-rules``): ``serial_diff``.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from enum import IntEnum

import numpy as np

from .crc import bits_to_int, crc16_bits, int_to_bits

HEADER_MESSAGE_BITS = 128
CONTROL_MESSAGE_BITS = 240
CRC_BITS = 16
VERSION = 1
MAX_DATA_BLOCKS = 128
MAX_CONTROL_BLOCKS = 3
MAX_TONES = 56
ALL_TONES = frozenset(range(MAX_TONES))


class FieldError(ValueError):
    """A header or control block whose fields are malformed."""


class CrcError(FieldError):
    """A header or control block whose CRC-16 fails."""


class FrameType(IntEnum):
    """SPEC 8.4 frame types (11-30 reserved)."""

    BEACON = 0
    UI = 1
    CONNECT = 2
    CONNECT_ACK = 3
    DATA = 4
    ACK = 5
    SOUND = 6
    DISCONNECT = 7
    PROBE = 8
    CAL_REQUEST = 9
    POLL = 10
    EXTENDED = 31


class ControlType(IntEnum):
    """Appendix B control block types (2 and 8-15 reserved, ADR 0008
    ``reserved-control-types-after-block-map``)."""

    ACK = 0
    CONNECT = 1
    CAL_REQUEST = 3
    CAL_REPORT = 4
    ID = 5
    PATH = 6
    BLOCK_MAP = 7


SEQUENCE_MODULUS = 1 << 16
MASK_IDS = 4                  # 2-bit mask ID (ADR 0011 ``mask-id``)
BLOCK_SPAN = 128              # blocks one block map or ACK bitmap covers (ADR 0008)


def serial_diff(a: int, b: int) -> int:
    """b - a in serial-number arithmetic (RFC 1982, 16 bits): positive when b
    is after a, in -32767 .. 32768."""
    d = (b - a) % SEQUENCE_MODULUS
    return d - SEQUENCE_MODULUS if d > SEQUENCE_MODULUS // 2 else d


# B.3 capability bits.
CAP_WIDE = 1 << 0             # widths above 24 tones
CAP_HARQ = 1 << 1             # reserved for HARQ (9.4), sent as zero
CAP_SIGNATURES = 1 << 2
CAP_SCHEMA = 1 << 3           # reserved for schema encoding (12.2)
CAP_SHORT_HEADERS = 1 << 4    # reserved (v2)
CAP_CALIBRATION = 1 << 5      # accepts CAL-REQUEST
CAP_MULTIPLEX = 1 << 6        # stream multiplexing
CAP_DICT_FETCH = 1 << 7


def session_width(width_cap: int, peer_capabilities: int) -> int:
    """A station's session width W (ADR 0011 ``session-width``): its B.3
    width cap + 1 tones when its peer advertised capability bit 0 (widths
    above 24 tones), else 24."""
    return width_cap + 1 if peer_capabilities & CAP_WIDE else 24


# --- bit packing ----------------------------------------------------------------


class _Writer:
    def __init__(self) -> None:
        self.value = 0
        self.n = 0

    def put(self, value: int, width: int, name: str) -> None:
        value = int(value)
        if not 0 <= value < (1 << width):
            raise FieldError(f"{name} = {value} does not fit {width} bits")
        self.value = (self.value << width) | value
        self.n += width

    def bits(self, total: int) -> np.ndarray:
        assert self.n == total, (self.n, total)
        return int_to_bits(self.value, total)


class _Reader:
    def __init__(self, bits: np.ndarray) -> None:
        self.value = bits_to_int(bits)
        self.n = len(bits)
        self.pos = 0

    def get(self, width: int) -> int:
        self.pos += width
        return (self.value >> (self.n - self.pos)) & ((1 << width) - 1)


def _check_crc(bits: np.ndarray, n_info: int, what: str) -> np.ndarray:
    bits = np.asarray(bits, dtype=np.uint8).reshape(-1)
    if bits.size != n_info:
        raise FieldError(f"{what} needs {n_info} bits, got {bits.size}")
    msg = bits[: n_info - CRC_BITS]
    if crc16_bits(msg) != bits_to_int(bits[n_info - CRC_BITS:]):
        raise CrcError(f"{what} CRC-16 failed")
    return msg


def with_crc(msg: np.ndarray) -> np.ndarray:
    """Message bits followed by their CRC-16/CCITT-FALSE (SPEC 6.3)."""
    msg = np.asarray(msg, dtype=np.uint8)
    return np.concatenate([msg, int_to_bits(crc16_bits(msg), CRC_BITS)])


def _flags(*bits: bool) -> int:
    return sum(int(bool(b)) << i for i, b in enumerate(bits))


def _flag(value: int, bit: int) -> bool:
    return bool((value >> bit) & 1)


def mask_to_int(usable: frozenset[int] | set[int]) -> int:
    """Usable tones -> 56-bit field, tone k at bit k."""
    if any(not 0 <= k < MAX_TONES for k in usable):
        raise FieldError(f"tone mask tones must be 0-{MAX_TONES - 1}")
    return sum(1 << int(k) for k in usable)


def int_to_mask(value: int) -> frozenset[int]:
    return frozenset(k for k in range(MAX_TONES) if (value >> k) & 1)


# --- addresses (SPEC 8.3, B.1) ----------------------------------------------------

_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/-"      # codes 1-38
_CODE = {ch: i + 1 for i, ch in enumerate(_ALPHABET)}
HASH_CODE = 63
BROADCAST_VALUE = (1 << 40) - 1
_HASH_MASK = (1 << 30) - 1


def callsign_hash(text: str) -> int:
    """30-bit hash of a long callsign (8.3, B.1): the first 30 bits of SHA-256
    over the upper-case ASCII text; all ones becomes all ones minus one."""
    digest = hashlib.sha256(text.upper().encode("ascii")).digest()
    h = int.from_bytes(digest[:4], "big") >> 2
    return h - 1 if h == _HASH_MASK else h


@dataclass(frozen=True)
class Address:
    """A header address: callsign and SSID, a long callsign's hash, or broadcast.

    ``Address("N0CALL", 3)`` is a station. A callsign of 7-10 characters is
    sent as code 63 and its 30-bit hash; decoding gives back an address with
    only ``hash30`` set (``callsign`` None), and ``on_air()`` gives the same
    form for comparison.
    """

    callsign: str | None = None
    ssid: int = 0
    hash30: int | None = None
    broadcast: bool = False

    def __post_init__(self) -> None:
        if self.broadcast:
            if self.callsign is not None or self.hash30 is not None:
                raise FieldError("a broadcast address has no callsign")
            object.__setattr__(self, "ssid", 15)
            return
        if not 0 <= self.ssid <= 15:
            raise FieldError(f"SSID {self.ssid} outside 0-15")
        if self.callsign is not None:
            text = self.callsign.upper()
            if not 1 <= len(text) <= 10 or any(ch not in _CODE for ch in text):
                raise FieldError(f"callsign {self.callsign!r}: 1-10 characters from A-Z, 0-9, '/', '-'")
            object.__setattr__(self, "callsign", text)
            if len(text) > 6:
                h = callsign_hash(text)
                if self.hash30 is not None and self.hash30 != h:
                    raise FieldError("hash does not match the callsign")
                object.__setattr__(self, "hash30", h)
            elif self.hash30 is not None:
                raise FieldError("a callsign of up to 6 characters is not hashed")
        elif self.hash30 is None:
            raise FieldError("an address needs a callsign, a hash or broadcast")
        elif not 0 <= self.hash30 < _HASH_MASK:
            raise FieldError("hash outside 0 .. 2^30 - 2")

    @classmethod
    def broadcast_address(cls) -> Address:
        return cls(broadcast=True)

    @property
    def is_hashed(self) -> bool:
        return self.hash30 is not None

    def on_air(self) -> Address:
        """The address as a receiver decodes it (a long callsign keeps only its hash)."""
        if self.is_hashed and self.callsign is not None:
            return Address(None, self.ssid, self.hash30)
        return self

    def encode(self) -> int:
        """The 40-bit field value."""
        if self.broadcast:
            return BROADCAST_VALUE
        if self.is_hashed:
            return (HASH_CODE << 34) | (self.hash30 << 4) | self.ssid
        codes = [_CODE[ch] for ch in self.callsign] + [0] * (6 - len(self.callsign))
        v = 0
        for c in codes:
            v = (v << 6) | c
        return (v << 4) | self.ssid

    @classmethod
    def decode(cls, value: int) -> Address:
        if not 0 <= value <= BROADCAST_VALUE:
            raise FieldError("address outside 40 bits")
        if value == BROADCAST_VALUE:
            return cls(broadcast=True)
        ssid = value & 0xF
        codes = [(value >> (4 + 6 * (5 - i))) & 0x3F for i in range(6)]
        if codes[0] == HASH_CODE:
            return cls(None, ssid, (value >> 4) & _HASH_MASK)
        text = ""
        for i, c in enumerate(codes):
            if c == 0:
                if any(codes[i:]):
                    raise FieldError("address padding before a character")
                break
            if c > len(_ALPHABET):
                raise FieldError(f"address character code {c} is reserved")
            text += _ALPHABET[c - 1]
        if not text:
            raise FieldError("address has no characters")
        return cls(text, ssid)

    def __str__(self) -> str:
        if self.broadcast:
            return "*"
        name = self.callsign if self.callsign is not None else f"#{self.hash30:08x}"
        return f"{name}-{self.ssid}" if self.ssid else name


# --- header (SPEC 8.2) -------------------------------------------------------------


@dataclass(frozen=True)
class Header:
    """The 144-bit frame header (SPEC 8.2, ADR 0011 ``header-field-layout``).

    Flag bits 0 and 1 (control and data blocks present) follow from
    ``n_control`` and ``n_data``; the data block count is sent as count - 1.
    ``sequence`` is, in a session frame, the sequence of its first (lowest)
    data block, or the sender's next unsent sequence without data (ADR 0008
    ``block-sequence-rules``); in BEACON, UI and PROBE the frame counter
    (ADR 0010 ``broadcast-frame-counter``). ``mask_id`` names the tone mask
    applied to a session frame's control and data blocks (ADR 0011
    ``mask-id``); 0 otherwise.
    """

    frame_type: int
    source: Address
    destination: Address
    sequence: int = 0
    mcs: int = 0
    n_control: int = 0
    n_data: int = 0
    more_fragments: bool = False    # flag bit 2 (broadcast)
    session: bool = False           # flag bit 3
    probe: bool = False             # flag bit 4: width probe follows the header
    repeat: bool = False            # flag bit 5: identical repeat (soft combining)
    first_fragment: bool = False    # flag bit 6: starts a message (every BEACON/UI but continuations)
    block_map: bool = False         # flag bit 7: the second control block is a block map (B.8)
    mask_id: int = 0                # ID of the tone mask applied (session frames), 0-3
    version: int = VERSION

    def flags(self) -> int:
        return _flags(self.n_control > 0, self.n_data > 0, self.more_fragments, self.session,
                      self.probe, self.repeat, self.first_fragment, self.block_map)

    def message_bits(self) -> np.ndarray:
        """The 128 bits before the CRC."""
        if not 0 <= self.n_control <= MAX_CONTROL_BLOCKS:
            raise FieldError(f"control block count {self.n_control} outside 0-3")
        if not 0 <= self.n_data <= MAX_DATA_BLOCKS:
            raise FieldError(f"data block count {self.n_data} outside 0-128")
        w = _Writer()
        w.put(self.version, 3, "version")
        w.put(self.frame_type, 5, "frame type")
        w.put(self.flags(), 8, "flags")
        w.put(self.source.encode(), 40, "source")
        w.put(self.destination.encode(), 40, "destination")
        w.put(self.sequence, 16, "sequence")
        w.put(self.mcs, 4, "MCS")
        w.put(self.n_control, 2, "control block count")
        w.put(self.n_data - 1 if self.n_data else 0, 7, "data block count")
        w.put(self.mask_id, 2, "mask ID")
        w.put(0, 1, "reserved")
        return w.bits(HEADER_MESSAGE_BITS)

    def bits(self) -> np.ndarray:
        """All 144 bits, CRC included."""
        return with_crc(self.message_bits())

    @classmethod
    def unpack(cls, bits: np.ndarray) -> Header:
        """Parse 144 header bits; raises CrcError or FieldError."""
        r = _Reader(_check_crc(bits, HEADER_MESSAGE_BITS + CRC_BITS, "header"))
        version = r.get(3)
        if version != VERSION:
            raise FieldError(f"header version {version} is not {VERSION}")
        frame_type, flags = r.get(5), r.get(8)
        source, destination = Address.decode(r.get(40)), Address.decode(r.get(40))
        sequence, mcs, n_control, count, mask_id = r.get(16), r.get(4), r.get(2), r.get(7), r.get(2)
        if _flag(flags, 0) != (n_control > 0):
            raise FieldError("header flag bit 0 disagrees with the control block count")
        return cls(frame_type, source, destination, sequence, mcs, n_control,
                   count + 1 if _flag(flags, 1) else 0, more_fragments=_flag(flags, 2),
                   session=_flag(flags, 3), probe=_flag(flags, 4), repeat=_flag(flags, 5),
                   first_fragment=_flag(flags, 6), block_map=_flag(flags, 7), mask_id=mask_id,
                   version=version)


# --- control blocks (Appendix B) --------------------------------------------------


def snr_code(snr_db: float) -> int:
    """B.2 measured SNR: 0.25 dB steps, nearest (halves up), saturating at 0 and 63.75 dB."""
    return int(min(255, max(0, np.floor(snr_db * 4 + 0.5))))


@dataclass(frozen=True)
class AckBlock:
    """B.2 ACK control block (type 0, ADR 0011 ``ack-control-block-layout``);
    also the measurement block of a PROBE's SOUND reply (ADR 0012) and the
    resume acknowledgment of CONNECT-ACK (ADR 0008).

    Cumulative (ADR 0008 ``cumulative-ack``): ``base_sequence`` is the
    receiver's next in-order sequence and bitmap bit i is set when it holds
    block base + i (``cumulative``), so bit 0 is clear. ``mask_id`` is the ID
    of ``tone_mask`` (ADR 0011 ``mask-id``); every session ACK carries the
    mask (flag bit 0, ``mask-in-every-session-ack``).
    """

    TYPE = ControlType.ACK
    base_sequence: int = 0
    bitmap: int = 0                          # bit i: the receiver holds block base + i
    requested_mcs: int = 0
    snr_q: int = 0                           # measured SNR, 0.25 dB steps (``snr_code``)
    block_errors: int = 0
    tone_mask: frozenset[int] | None = None  # usable tones; sent with flag bit 0
    reprobe: bool = False                    # flag bit 2
    long_prefix: bool = False                # flag bit 3
    distortion: int = 0
    adc_clipped: bool = False
    mask_id: int = 0                         # ID of the tone mask, 0-3 (0 outside a session)

    @property
    def snr_db(self) -> float:
        return self.snr_q / 4

    @staticmethod
    def bitmap_from(held: list[bool] | np.ndarray) -> int:
        return sum(1 << i for i, ok in enumerate(held) if ok)

    @staticmethod
    def cumulative(next_in_order: int, held) -> tuple[int, int]:
        """(base sequence, bitmap) for a receiver whose next in-order
        sequence is ``next_in_order`` and that holds the blocks ``held``
        beyond it (ADR 0008 ``cumulative-ack``). Blocks at or before the base,
        or 128 or more beyond it, are not in the bitmap."""
        bitmap = 0
        for seq in held:
            d = serial_diff(next_in_order, seq)
            if 0 < d < BLOCK_SPAN:
                bitmap |= 1 << d
        return next_in_order % SEQUENCE_MODULUS, bitmap

    def holds(self, i: int) -> bool:
        """Bit i: the receiver holds block base + i."""
        return _flag(self.bitmap, i)

    def acknowledged(self, seq: int) -> bool:
        """Block ``seq`` is acknowledged: before the base, or its bit is set."""
        d = serial_diff(self.base_sequence, seq)
        return d < 0 or (d < BLOCK_SPAN and self.holds(d))

    def _pack(self, w: _Writer) -> None:
        w.put(_flags(self.tone_mask is not None, False, self.reprobe, self.long_prefix), 4, "flags")
        w.put(self.base_sequence, 16, "base sequence")
        w.put(self.bitmap, 128, "block bitmap")
        w.put(self.requested_mcs, 4, "requested MCS")
        w.put(self.snr_q, 8, "measured SNR")
        w.put(self.block_errors, 8, "block errors")
        w.put(0 if self.tone_mask is None else mask_to_int(self.tone_mask), 56, "tone mask")
        w.put(self.distortion, 3, "distortion index")
        w.put(int(self.adc_clipped), 1, "own ADC clipped")
        w.put(self.mask_id, 2, "mask ID")
        w.put(0, 6, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> AckBlock:
        flags = r.get(4)
        base, bitmap, mcs, snr, errors, mask = r.get(16), r.get(128), r.get(4), r.get(8), r.get(8), r.get(56)
        distortion, clipped, mask_id = r.get(3), r.get(1), r.get(2)
        return cls(base, bitmap, mcs, snr, errors, int_to_mask(mask) if _flag(flags, 0) else None,
                   _flag(flags, 2), _flag(flags, 3), distortion, bool(clipped), mask_id)


@dataclass(frozen=True)
class ConnectBlock:
    """B.3 CONNECT / CONNECT-ACK control block (type 1); also the link-test
    PROBE's block (ADR 0012, ``link_test``). A second B.3 block carries only
    dictionary IDs (``dictionaries``, ``dictionary_only``)."""

    TYPE = ControlType.CONNECT
    session_id: int = 0
    capabilities: int = 0
    width_cap: int = 23                      # highest tone index transmitted, 23-55
    requested_mcs: int = 0
    max_frame_time: int = 40                 # units of 0.1 s, 5-40
    tone_mask: frozenset[int] = ALL_TONES    # usable tones (all ones in CONNECT)
    dictionary_ids: tuple[int, int, int, int] = (0, 0, 0, 0)
    more_dictionaries: bool = False          # flag bit 0
    resume: bool = False                     # flag bit 1

    @classmethod
    def dictionaries(cls, ids: tuple[int, int, int, int]) -> ConnectBlock:
        """The second B.3 block of CONNECT or CONNECT-ACK: dictionary IDs only,
        every other field zero (ADR 0010 ``session-establishment-steps``)."""
        return cls(width_cap=0, max_frame_time=0, tone_mask=frozenset(), dictionary_ids=tuple(ids))

    @classmethod
    def link_test(cls, capabilities: int, width_cap: int, max_frame_time: int = 40) -> ConnectBlock:
        """A link-test PROBE's block (ADR 0012 ``probe-frame-contents``): flags
        and session ID 0, requested MCS 0, all-ones mask, no dictionaries."""
        return cls(capabilities=capabilities, width_cap=width_cap, max_frame_time=max_frame_time)

    @property
    def dictionary_only(self) -> bool:
        """A dictionary-ID block: width cap and max frame time both 0."""
        return self.width_cap == 0 and self.max_frame_time == 0

    @property
    def is_link_test(self) -> bool:
        """The fields ADR 0012 ``probe-frame-contents`` fixes for a link-test PROBE."""
        return (not self.more_dictionaries and not self.resume and self.session_id == 0
                and self.requested_mcs == 0 and self.tone_mask == ALL_TONES
                and not any(self.dictionary_ids) and not self.dictionary_only)

    @staticmethod
    def _check(cap: int, mft: int) -> None:
        if cap == 0 and mft == 0:
            return                              # a dictionary-only block
        if not 23 <= cap <= 55:
            raise FieldError(f"width cap {cap} outside 23-55")
        if not 5 <= mft <= 40:
            raise FieldError(f"max frame time {mft} outside 5-40")

    def _pack(self, w: _Writer) -> None:
        self._check(self.width_cap, self.max_frame_time)
        if len(self.dictionary_ids) != 4:
            raise FieldError("four dictionary ID slots")
        w.put(_flags(self.more_dictionaries, self.resume), 4, "flags")
        w.put(self.session_id, 16, "session ID")
        w.put(self.capabilities, 16, "capabilities")
        w.put(self.width_cap, 6, "width cap")
        w.put(self.requested_mcs, 4, "requested MCS")
        w.put(self.max_frame_time, 6, "max frame time")
        w.put(mask_to_int(self.tone_mask), 56, "tone mask")
        for i, d in enumerate(self.dictionary_ids):
            w.put(d, 32, f"dictionary ID {i}")

    @classmethod
    def _unpack(cls, r: _Reader) -> ConnectBlock:
        flags = r.get(4)
        sid, caps, cap, mcs, mft, mask = r.get(16), r.get(16), r.get(6), r.get(4), r.get(6), r.get(56)
        dicts = tuple(r.get(32) for _ in range(4))
        cls._check(cap, mft)
        return cls(sid, caps, cap, mcs, mft, int_to_mask(mask), dicts, _flag(flags, 0), _flag(flags, 1))


@dataclass(frozen=True)
class CalRequestBlock:
    """B.5 CAL-REQUEST control block (type 3)."""

    TYPE = ControlType.CAL_REQUEST
    tone_freq: int = 100        # units of 10 Hz
    step_count: int = 8
    step_length: int = 40       # units of 10 ms
    gap_length: int = 10        # units of 10 ms
    start_level: int = 96       # dB below nominal output, 0.25 dB steps
    step_size: int = 3          # dB per step

    def _pack(self, w: _Writer) -> None:
        w.put(0, 4, "flags")
        w.put(self.tone_freq, 8, "tone frequency")
        w.put(self.step_count, 4, "step count")
        w.put(self.step_length, 8, "step length")
        w.put(self.gap_length, 8, "gap length")
        w.put(self.start_level, 8, "start level")
        w.put(self.step_size, 4, "step size")
        w.put(0, 192, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> CalRequestBlock:
        r.get(4)
        return cls(r.get(8), r.get(4), r.get(8), r.get(8), r.get(8), r.get(4))


@dataclass(frozen=True)
class CalReportBlock:
    """B.5 CAL-REPORT control block (type 4)."""

    TYPE = ControlType.CAL_REPORT
    amplitudes: tuple[int, ...] = (0,) * 8   # dB below full scale, 0.25 dB steps, steps 1-8
    distortion: tuple[int, ...] = (0,) * 8   # percent, steps 1-8
    clipped: tuple[bool, ...] = (False,) * 8  # step j at bit j - 1
    sweep_valid: bool = False                # flag bit 0

    def _pack(self, w: _Writer) -> None:
        if not len(self.amplitudes) == len(self.distortion) == len(self.clipped) == 8:
            raise FieldError("CAL-REPORT carries eight steps")
        w.put(_flags(self.sweep_valid), 4, "flags")
        for i, a in enumerate(self.amplitudes):
            w.put(a, 8, f"amplitude {i + 1}")
        for i, d in enumerate(self.distortion):
            w.put(d, 4, f"distortion {i + 1}")
        w.put(_flags(*self.clipped), 8, "clip flags")
        w.put(0, 128, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> CalReportBlock:
        flags = r.get(4)
        amps = tuple(r.get(8) for _ in range(8))
        dist = tuple(r.get(4) for _ in range(8))
        clip = r.get(8)
        return cls(amps, dist, tuple(_flag(clip, j) for j in range(8)), _flag(flags, 0))


def _ascii(text: str, n: int, name: str) -> list[int]:
    if len(text) > n or any(not 0x20 <= ord(ch) <= 0x7E for ch in text):
        raise FieldError(f"{name} {text!r}: at most {n} printable ASCII characters")
    return [ord(ch) for ch in text.ljust(n)]


@dataclass(frozen=True)
class IdBlock:
    """B.6 ID control block (type 5): the callsign in ASCII (decision 20.7)."""

    TYPE = ControlType.ID
    callsign: str = ""
    ssid: int = 0
    grid: str | None = None     # Maidenhead locator, 4 or 6 characters; flag bit 0

    def __post_init__(self) -> None:
        object.__setattr__(self, "callsign", self.callsign.upper())
        if not self.callsign or " " in self.callsign:
            raise FieldError("ID callsign must be 1-10 characters without spaces")
        if self.grid is not None and len(self.grid) not in (4, 6):
            raise FieldError("grid locator has 4 or 6 characters")

    def _pack(self, w: _Writer) -> None:
        w.put(_flags(self.grid is not None), 4, "flags")
        for c in _ascii(self.callsign, 10, "callsign"):
            w.put(c, 8, "callsign")
        w.put(self.ssid, 4, "SSID")
        for c in ([0] * 6 if self.grid is None else _ascii(self.grid, 6, "grid locator")):
            w.put(c, 8, "grid locator")
        w.put(0, 100, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> IdBlock:
        flags = r.get(4)
        call = bytes(r.get(8) for _ in range(10))
        ssid = r.get(4)
        grid = bytes(r.get(8) for _ in range(6))
        if any(not 0x20 <= b <= 0x7E for b in call) or call != call.upper():
            raise FieldError("ID callsign is not upper-case printable 7-bit ASCII")
        g = None
        if _flag(flags, 0):
            if any(not 0x20 <= b <= 0x7E for b in grid):
                raise FieldError("grid locator is not printable 7-bit ASCII")
            g = grid.decode("ascii").rstrip(" ")
        return cls(call.decode("ascii").rstrip(" "), ssid, g)


@dataclass(frozen=True)
class PathBlock:
    """B.7 path control block (type 6, v1.x)."""

    TYPE = ControlType.PATH
    entries: tuple[Address, ...] = ()
    hops: int = 0
    used: tuple[bool, bool] = (False, False)   # flag bits 0 and 1
    via: bool = False                          # flag bit 2

    def _pack(self, w: _Writer) -> None:
        if len(self.entries) > 2:
            raise FieldError("at most two path entries")
        w.put(_flags(self.used[0], self.used[1], self.via), 4, "flags")
        w.put(self.hops, 2, "hops remaining")
        w.put(len(self.entries), 2, "entry count")
        for i in range(2):
            w.put(self.entries[i].encode() if i < len(self.entries) else 0, 40, f"entry {i + 1}")
        w.put(0, 148, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> PathBlock:
        flags, hops, count = r.get(4), r.get(2), r.get(2)
        raw = (r.get(40), r.get(40))
        if count > 2:
            raise FieldError(f"path entry count {count} above 2")
        return cls(tuple(Address.decode(raw[i]) for i in range(count)), hops,
                   (_flag(flags, 0), _flag(flags, 1)), _flag(flags, 2))


@dataclass(frozen=True)
class BlockMapBlock:
    """B.8 block map control block (type 7, ADR 0008 ``block-map-control-block``).

    Bit i of ``bitmap`` set: block ``base_sequence`` + i is carried in the
    frame; the frame's k-th data block is the k-th set bit from bit 0. The
    rules a receiver checks (bit 0 set, base equal to the header's Sequence,
    as many set bits as data blocks) are ``block_sequences``'s; ``unpack``
    only parses.
    """

    TYPE = ControlType.BLOCK_MAP
    base_sequence: int = 0
    bitmap: int = 1

    @classmethod
    def for_sequences(cls, sequences) -> BlockMapBlock:
        """The map of ``sequences`` (ascending in serial order, within 128 of the first)."""
        seqs = list(sequences)
        check_ascending(seqs)
        base = seqs[0] % SEQUENCE_MODULUS
        return cls(base, sum(1 << serial_diff(base, s) for s in seqs))

    def sequences(self) -> list[int]:
        """The sequences of the frame's data blocks, in order."""
        return [(self.base_sequence + i) % SEQUENCE_MODULUS for i in range(BLOCK_SPAN) if _flag(self.bitmap, i)]

    def _pack(self, w: _Writer) -> None:
        w.put(0, 4, "flags")
        w.put(self.base_sequence, 16, "base sequence")
        w.put(self.bitmap, 128, "block bitmap")
        w.put(0, 88, "reserved")

    @classmethod
    def _unpack(cls, r: _Reader) -> BlockMapBlock:
        r.get(4)
        return cls(r.get(16), r.get(128))


def check_ascending(sequences: list[int]) -> None:
    """``sequences`` are 1-128 blocks in ascending serial order, all within 128
    of the first (one frame's blocks, ADR 0008 ``block-sequence-rules``)."""
    if not 1 <= len(sequences) <= MAX_DATA_BLOCKS:
        raise FieldError(f"{len(sequences)} blocks: a frame carries 1-{MAX_DATA_BLOCKS}")
    for a, b in zip(sequences, sequences[1:]):
        if serial_diff(a, b) <= 0:
            raise FieldError(f"block sequences not ascending ({a}, {b})")
    if serial_diff(sequences[0], sequences[-1]) >= BLOCK_SPAN:
        raise FieldError("block sequences span 128 or more")


def plan_blocks(sequences) -> tuple[int, BlockMapBlock | None]:
    """A session frame carrying the blocks ``sequences`` (ascending): the
    header's Sequence (the lowest) and the block map to send, or None when the
    blocks are consecutive (ADR 0008 ``block-map-control-block``)."""
    seqs = [s % SEQUENCE_MODULUS for s in sequences]
    check_ascending(seqs)
    if serial_diff(seqs[0], seqs[-1]) == len(seqs) - 1:
        return seqs[0], None
    return seqs[0], BlockMapBlock.for_sequences(seqs)


def block_sequences(header: Header, controls) -> tuple[int, ...] | None:
    """The sequence of each data block of a decoded frame (ADR 0008):
    consecutive from the header's Sequence when flag bit 7 is clear; with it
    set, the block map that must be the second control block, which must have
    bit 0 set, base equal to the Sequence and one set bit per data block.
    None when flag bit 7 is set and the map is missing, failed or breaks a
    rule: every data block then counts as not received. ``controls`` holds
    the parsed control blocks (None where one failed)."""
    if not header.block_map:
        return tuple((header.sequence + k) % SEQUENCE_MODULUS for k in range(header.n_data))
    m = controls[1] if len(controls) > 1 else None
    if (not isinstance(m, BlockMapBlock) or m.base_sequence != header.sequence or not m.bitmap & 1
            or m.bitmap.bit_count() != header.n_data):
        return None
    return tuple(m.sequences())


@dataclass(frozen=True)
class UnknownControl:
    """A control block of a reserved type (2, 8-15): receivers skip it (Appendix B)."""

    control_type: int
    message: np.ndarray = field(repr=False, compare=False)


ControlBlock = AckBlock | ConnectBlock | CalRequestBlock | CalReportBlock | IdBlock | PathBlock | BlockMapBlock
CONTROL_CLASSES: dict[int, type] = {c.TYPE: c for c in (AckBlock, ConnectBlock, CalRequestBlock,
                                                         CalReportBlock, IdBlock, PathBlock, BlockMapBlock)}


def control_message_bits(block: ControlBlock) -> np.ndarray:
    """A control block's 240 bits before the CRC."""
    w = _Writer()
    w.put(int(block.TYPE), 4, "control type")
    block._pack(w)
    return w.bits(CONTROL_MESSAGE_BITS)


def control_bits(block: ControlBlock) -> np.ndarray:
    """A control block's 256 bits, CRC included."""
    return with_crc(control_message_bits(block))


def unpack_control(bits: np.ndarray) -> ControlBlock | UnknownControl:
    """Parse 256 control block bits; raises CrcError or FieldError. A block of a
    reserved type comes back as ``UnknownControl``."""
    msg = _check_crc(bits, CONTROL_MESSAGE_BITS + CRC_BITS, "control block")
    r = _Reader(msg)
    ctype = r.get(4)
    cls = CONTROL_CLASSES.get(ctype)
    if cls is None:
        return UnknownControl(ctype, msg.copy())
    return cls._unpack(r)


# --- control-block order (ADR 0010, ADR 0012) -------------------------------------

_SESSION_FIXED = (ControlType.ACK, ControlType.BLOCK_MAP, ControlType.ID, ControlType.CONNECT)


def _ctype(c) -> int:
    return c.control_type if isinstance(c, UnknownControl) else int(c.TYPE)


def check_controls(header: Header, controls) -> None:
    """Raise ``FieldError`` when a frame's control blocks break the order and
    contents fixed for its type (a sender's check; receivers identify blocks
    by type):

    - CONNECT: B.3 (requested MCS 0), ID, then a dictionary-only B.3 exactly
      when the first sets flag bit 0 (ADR 0010 ``session-establishment-steps``,
      ``connect-requested-mcs-zero``).
    - CONNECT-ACK: B.3, ID, then on resume (B.3 flag bit 1) an ACK block,
      otherwise a dictionary-only B.3 exactly when flag bit 0 is set.
    - PROBE: nothing (a BEACON request) or one link-test B.3 (ADR 0012
      ``probe-frame-contents``); no data blocks.
    - SOUND outside a session (a PROBE reply): one ACK block with base,
      bitmap and block errors 0, the mask present and mask ID 0 (ADR 0012
      ``probe-link-test-reply``).
    - DISCONNECT outside a session (a refusal): an all-zero ACK block, then
      ID (ADR 0010 ``connect-refusal``).
    - Session frames: the ACK block, the block map exactly when header flag
      bit 7 is set, an ID block when due, then others; DISCONNECT carries
      the ID block (ADR 0010 ``control-block-order``, ``disconnect-handshake``).

    Also: at most three blocks, the header's count, flag bit 6 only on
    BEACON and UI, a block map only as a session frame's second block, Mask
    ID 0 outside a session and in CONNECT-ACK.
    """
    controls = list(controls)
    types = [_ctype(c) for c in controls]
    t = header.frame_type
    if len(controls) != header.n_control:
        raise FieldError(f"{len(controls)} control blocks for a header of {header.n_control}")
    if header.first_fragment and t not in (FrameType.BEACON, FrameType.UI):
        raise FieldError("flag bit 6 (first fragment) on a frame other than BEACON or UI")
    unmasked = not header.session or t in (FrameType.CONNECT, FrameType.CONNECT_ACK)
    if unmasked and header.mask_id:
        raise FieldError("mask ID other than 0 on an unmasked frame (ADR 0011 mask-id)")
    in_map = [i for i, ty in enumerate(types) if ty == ControlType.BLOCK_MAP]
    if header.block_map != bool(in_map) or in_map not in ([], [1]):
        raise FieldError("a block map is the second control block exactly when flag bit 7 is set")
    if header.block_map and (not header.session or not header.n_data):
        raise FieldError("a block map belongs to a session frame with data blocks")

    def want(expected: list[int]) -> None:
        if types != [int(e) for e in expected]:
            raise FieldError(f"{FrameType(t).name} control blocks {types}, expected {[int(e) for e in expected]}")

    C, A, I = ControlType.CONNECT, ControlType.ACK, ControlType.ID
    if t == FrameType.CONNECT:
        first = controls[0] if controls else None
        if not isinstance(first, ConnectBlock) or first.dictionary_only:
            raise FieldError("CONNECT starts with its B.3 block")
        want([C, I, C] if first.more_dictionaries else [C, I])
        if first.requested_mcs:
            raise FieldError("CONNECT sends requested MCS 0 (ADR 0010 connect-requested-mcs-zero)")
        if first.more_dictionaries and not controls[2].dictionary_only:
            raise FieldError("the second B.3 block carries only dictionary IDs")
    elif t == FrameType.CONNECT_ACK:
        first = controls[0] if controls else None
        if not isinstance(first, ConnectBlock) or first.dictionary_only:
            raise FieldError("CONNECT-ACK starts with its B.3 block")
        want([C, I, A] if first.resume else [C, I, C] if first.more_dictionaries else [C, I])
        if not first.resume and first.more_dictionaries and not controls[2].dictionary_only:
            raise FieldError("the second B.3 block carries only dictionary IDs")
    elif t == FrameType.PROBE:
        if header.n_data:
            raise FieldError("a PROBE carries no data blocks")
        if controls:
            want([C])
            if not controls[0].is_link_test:
                raise FieldError("a link-test PROBE's B.3 block: flags, session ID and requested MCS 0, "
                                 "all-ones mask, no dictionaries (ADR 0012)")
        elif header.probe:
            raise FieldError("a BEACON-request PROBE carries no width probe")
    elif header.session:
        if not types or types[0] != A:
            raise FieldError("a session frame leads with its ACK block (SPEC 8.4)")
        i = 2 if header.block_map else 1
        if i < len(types) and types[i] == I:
            i += 1
        rest = types[i:]
        if any(ty in _SESSION_FIXED for ty in rest):
            raise FieldError(f"session control blocks out of order: {types}")
        if t == FrameType.DISCONNECT and I not in types:
            raise FieldError("DISCONNECT carries the sender's ID block")
    elif t == FrameType.SOUND:
        want([A])
        a = controls[0]
        if a.base_sequence or a.bitmap or a.block_errors or a.tone_mask is None or a.mask_id:
            raise FieldError("a PROBE reply's ACK block: base, bitmap and block errors 0, mask present, "
                             "mask ID 0 (ADR 0012)")
    elif t == FrameType.DISCONNECT:
        want([A, I])
        if controls[0] != AckBlock():
            raise FieldError("a refusal DISCONNECT's ACK block has every field zero (ADR 0010)")
