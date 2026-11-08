"""PDUs in the block stream (SPEC 12.1, B.4, ADR 0009).

A PDU is ``Length (2) | Type (1) | Stream (1) | Encoding (1) |
[Dictionary ID (4), encoding 2 only] | Body | CRC-32 (4)``. Every multi-byte
field is big-endian (``pdu-byte-order``); the Length counts every byte after
itself, the CRC-32 included; the CRC-32 is CRC-32/ISO-HDLC, the zlib CRC,
over the decoded body (``pdu-crc32``).

PDUs go back to back into 126-byte data-block payloads and may span blocks.
Where a PDU would start, a Length of 0x0000 marks the rest of the block as
filler, sent as zeros (``pdu-filler``), and a Length field never straddles a
block boundary: one byte left at a PDU boundary is a 0x00 filler byte
(``pdu-length-within-block``). A block's 126 bytes become its first 1,008
information bits, each byte most significant bit first
(``data-block-byte-order``; ``block_bits``).

- ``Pdu``: one PDU, ``encode`` / ``decode``.
- ``BlockPacker`` / ``pack_blocks``: PDUs into block payloads (a session
  stream, or one broadcast frame's PDUs).
- ``BlockParser`` / ``unpack_blocks``: block payloads back into PDUs.
- ``fragment`` / ``Reassembler``: a broadcast message larger than one frame
  as one complete PDU per frame, with header flag bits 6 (first fragment)
  and 2 (more fragments) (``broadcast-message-fragments``,
  ``first-fragment-flag``).

Compression (encodings 1 and 2) is outside this module: ``Pdu.body`` is the
body as sent and ``Pdu.crc`` the CRC-32 of the decoded body, which for raw
PDUs (encoding 0) ``Pdu.raw`` computes.
"""

from __future__ import annotations

import zlib
from dataclasses import dataclass

import numpy as np

BLOCK_BYTES = 126              # payload bytes of a data block (B.4)
MAX_LENGTH = 0xFFFF            # Length field
ENCODING_DICT = 2              # zstd with dictionary: carries the Dictionary ID


class PduType:
    """SPEC 12.1 PDU types (7-255 reserved)."""

    AX25 = 0
    MESSAGE = 1
    FILE_CHUNK = 2
    SIGNATURE = 3
    CONTROL = 4
    RATELESS = 5
    AX25_CONNECTED = 6


class PduError(ValueError):
    """A PDU that does not fit its fields, or bytes that are not a PDU."""


def crc32(data: bytes) -> int:
    """CRC-32/ISO-HDLC (``pdu-crc32``): check value 0xCBF43926 over b"123456789"."""
    return zlib.crc32(data) & 0xFFFFFFFF


def stream_byte(priority: int, number: int) -> int:
    """SPEC 12.1 Stream field: bits 7-6 priority (0 highest), bits 5-0 stream number."""
    if not (0 <= priority < 4 and 0 <= number < 64):
        raise PduError("priority 0-3, stream number 0-63")
    return priority << 6 | number


@dataclass(frozen=True)
class Pdu:
    """One PDU. ``stream`` is the whole Stream byte (``stream_byte``);
    ``dictionary_id`` the 4-byte Dictionary ID, present exactly with
    encoding 2; ``crc`` the CRC-32 of the decoded body."""

    type: int
    body: bytes
    crc: int
    stream: int = 0
    encoding: int = 0
    dictionary_id: bytes | None = None

    @classmethod
    def raw(cls, type: int, body: bytes, stream: int = 0) -> Pdu:
        """An uncompressed PDU (encoding 0): its CRC-32 is over ``body``."""
        return cls(type, bytes(body), crc32(body), stream, 0, None)

    def __post_init__(self) -> None:
        for name, v in (("type", self.type), ("stream", self.stream), ("encoding", self.encoding)):
            if not 0 <= v <= 0xFF:
                raise PduError(f"{name} {v} does not fit a byte")
        if not 0 <= self.crc <= 0xFFFFFFFF:
            raise PduError("CRC-32 outside 32 bits")
        if (self.encoding == ENCODING_DICT) != (self.dictionary_id is not None):
            raise PduError("a Dictionary ID is present exactly with encoding 2")
        if self.dictionary_id is not None and len(self.dictionary_id) != 4:
            raise PduError("a Dictionary ID is 4 bytes")
        if self.length > MAX_LENGTH:
            raise PduError(f"PDU length {self.length} above {MAX_LENGTH}")

    @property
    def length(self) -> int:
        """The Length field: every byte after it, the CRC-32 included."""
        return 3 + (4 if self.dictionary_id is not None else 0) + len(self.body) + 4

    @property
    def crc_ok(self) -> bool:
        """For a raw PDU, the CRC-32 matches the body (compressed PDUs are
        checked after decompression, against the decoded body)."""
        return self.encoding != 0 or crc32(self.body) == self.crc

    def encode(self) -> bytes:
        return (self.length.to_bytes(2, "big") + bytes([self.type, self.stream, self.encoding])
                + (self.dictionary_id or b"") + self.body + self.crc.to_bytes(4, "big"))

    @classmethod
    def decode(cls, data: bytes) -> Pdu:
        """Parse one whole PDU (Length field included)."""
        if len(data) < 2:
            raise PduError("no Length field")
        length = int.from_bytes(data[:2], "big")
        if len(data) != 2 + length:
            raise PduError(f"Length {length} but {len(data) - 2} bytes follow it")
        if length < 7:
            raise PduError(f"Length {length} is shorter than the fixed fields")
        ptype, stream, encoding = data[2], data[3], data[4]
        at = 5
        dict_id = None
        if encoding == ENCODING_DICT:
            if length < 11:
                raise PduError(f"Length {length} leaves no room for the Dictionary ID")
            dict_id, at = bytes(data[5:9]), 9
        return cls(ptype, bytes(data[at:-4]), int.from_bytes(data[-4:], "big"), stream, encoding, dict_id)


class BlockPacker:
    """PDUs into 126-byte block payloads, back to back (``pdu-filler``,
    ``pdu-length-within-block``). ``push`` queues whole PDUs; ``take`` gives
    the next block, filled out with filler when fewer bytes are queued."""

    def __init__(self) -> None:
        self._buf = bytearray()

    def push(self, pdu: Pdu) -> None:
        if len(self._buf) % BLOCK_BYTES == BLOCK_BYTES - 1:
            self._buf.append(0)                 # a Length never straddles a block: one filler byte
        self._buf += pdu.encode()

    @property
    def queued(self) -> int:
        """Bytes queued (whole PDUs, any filler byte between them included)."""
        return len(self._buf)

    def take(self) -> bytes | None:
        """The next block's payload, or None when nothing is queued. A last
        partial block is completed with zeros, which start at a PDU boundary,
        so they read as filler."""
        if not self._buf:
            return None
        out = bytes(self._buf[:BLOCK_BYTES]).ljust(BLOCK_BYTES, b"\0")
        del self._buf[:BLOCK_BYTES]
        return out


def pack_blocks(pdus) -> list[bytes]:
    """The block payloads carrying ``pdus`` back to back, the last block
    completed with filler: a session stream from block boundary to block
    boundary, or one broadcast frame's complete PDUs."""
    p = BlockPacker()
    for pdu in pdus:
        p.push(pdu)
    out = []
    while (b := p.take()) is not None:
        out.append(b)
    return out


def blocks_needed(pdus) -> int:
    """Data blocks ``pack_blocks`` needs for ``pdus``."""
    return len(pack_blocks(pdus))


class BlockParser:
    """Block payloads, in sequence order, back into PDUs. ``feed`` returns the
    PDUs completed by a block; a PDU whose fields are malformed comes back as
    a ``PduError`` (its Length still framed it, so parsing continues)."""

    def __init__(self) -> None:
        self._pdu = bytearray()
        self._need = 0                          # bytes still missing from the PDU in progress

    @property
    def in_pdu(self) -> bool:
        return self._need > 0

    def feed(self, block: bytes) -> list[Pdu | PduError]:
        if len(block) != BLOCK_BYTES:
            raise PduError(f"a block payload is {BLOCK_BYTES} bytes, got {len(block)}")
        out: list[Pdu | PduError] = []
        pos = 0
        while pos < BLOCK_BYTES:
            if self._need:
                n = min(self._need, BLOCK_BYTES - pos)
                self._pdu += block[pos:pos + n]
                pos += n
                self._need -= n
                if not self._need:
                    try:
                        out.append(Pdu.decode(bytes(self._pdu)))
                    except PduError as e:
                        out.append(e)
                continue
            if BLOCK_BYTES - pos == 1:
                break                           # one byte left at a PDU boundary: filler
            length = int.from_bytes(block[pos:pos + 2], "big")
            if length == 0:
                break                           # filler: the rest of the block
            self._pdu = bytearray(block[pos:pos + 2])
            self._need = length
            pos += 2
        return out


def unpack_blocks(blocks) -> list[Pdu | PduError]:
    """Every PDU completed in ``blocks`` (a PDU still open at the end is left out)."""
    p = BlockParser()
    return [x for b in blocks for x in p.feed(bytes(b))]


def block_bits(payload: bytes) -> np.ndarray:
    """A block payload's 1,008 information bits, bytes in order, each most
    significant bit first (``data-block-byte-order``)."""
    if len(payload) != BLOCK_BYTES:
        raise PduError(f"a block payload is {BLOCK_BYTES} bytes")
    return np.unpackbits(np.frombuffer(payload, dtype=np.uint8))


def bits_to_payload(bits: np.ndarray) -> bytes:
    """The 126 payload bytes of a data block's information bits (CRC excluded)."""
    return np.packbits(np.asarray(bits, dtype=np.uint8)[:BLOCK_BYTES * 8]).tobytes()


# --- broadcast fragmentation (ADR 0009 broadcast-message-fragments) ----------------


def max_body(n_blocks: int, dictionary: bool = False) -> int:
    """The largest body one PDU can carry alone in ``n_blocks`` data blocks."""
    return n_blocks * BLOCK_BYTES - 2 - 3 - (4 if dictionary else 0) - 4


@dataclass(frozen=True)
class Fragment:
    """One frame of a broadcast message: its PDU and header flag bits 6
    (``first``) and 2 (``more``)."""

    first: bool
    more: bool
    pdu: Pdu


def fragment(type: int, body: bytes, slice_bytes: int, stream: int = 0) -> list[Fragment]:
    """A raw message as one frame per slice of at most ``slice_bytes`` (one
    complete PDU each, same Type and Stream). A message that fits one slice is
    a single frame with flag bit 6 set and bit 2 clear (a complete message);
    otherwise the first frame sets both, middle frames bit 2 only and the last
    neither (``first-fragment-flag``). The frames go out on consecutive frame
    counters."""
    if slice_bytes < 1:
        raise PduError("a slice carries at least one byte")
    slices = [body[i:i + slice_bytes] for i in range(0, len(body), slice_bytes)] or [b""]
    n = len(slices)
    return [Fragment(i == 0, i < n - 1, Pdu.raw(type, s, stream)) for i, s in enumerate(slices)]


class Reassembler:
    """Broadcast messages back from their frames, per source
    (``broadcast-message-fragments``): a message starts at a frame with flag
    bit 6 set; each frame whose counter is one higher appends its body; the
    frame with flag bit 2 clear completes it. A gap (a missing counter, or a
    new first fragment) discards the partial message, and a continuation
    without a first fragment held is discarded. Identical repeats (flag bit 5)
    are resolved before this. ``feed`` returns the completed message as one
    PDU of the first fragment's Type and Stream, or None."""

    def __init__(self) -> None:
        self._open: dict[object, tuple[int, Pdu, bytearray]] = {}

    def feed(self, source, counter: int, first: bool, more: bool, pdu: Pdu) -> Pdu | None:
        held = self._open.pop(source, None)
        if first:
            if not more:
                return pdu                      # a complete message
            self._open[source] = (counter, pdu, bytearray(pdu.body))
            return None
        if held is None or (held[0] + 1) % (1 << 16) != counter:
            return None                         # a continuation with no start held, or a gap
        _, head, body = held
        if (pdu.type, pdu.stream) != (head.type, head.stream):
            return None                         # every slice has the first one's Type and Stream
        body += pdu.body
        if more:
            self._open[source] = (counter, head, body)
            return None
        return Pdu.raw(head.type, bytes(body), head.stream)
