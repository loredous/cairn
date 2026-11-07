"""Protocol parameters, mirrored from docs/SPEC.md.

Every number the simulation uses comes from here, so a change to the
specification is a change to one file (SPEC 21.3 asks the same of the Rust
core's params.rs). Section references are to docs/SPEC.md.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

# --- 5.1 / 5.2 waveform ------------------------------------------------------

FS = 48_000                  # samples per second
TONE_SPACING = 100.0         # Hz
N_USEFUL = 480               # useful symbol, 10.0 ms
CP_SHORT = 60                # 1.25 ms
CP_LONG = 120                # 2.5 ms (repeater mode or on request)
FIRST_TONE_HZ = 350.0        # tone k sits at 350 + 100 k Hz
MAX_TONES = 56
BASE_TONES = 24              # preamble, header and control blocks
PILOT_TONES = (3, 20, 37, 52)  # 37 and 52 only when active
BAND_LOW_HZ = 300.0          # occupied band starts here

# Tone k is at (FIRST_TONE_HZ / TONE_SPACING + k) = 3.5 + k FFT bins of the
# 480-point useful symbol: every tone sits on a half bin, so each has a
# half-integer number of cycles per useful symbol and the cyclic prefix is
# the *negated* tail of the symbol (equivalently, a 50 Hz shift of an
# integer-bin OFDM symbol). See ofdm.py.
BIN_OFFSET = FIRST_TONE_HZ / TONE_SPACING  # 3.5


def tone_freq(k: int) -> float:
    return FIRST_TONE_HZ + TONE_SPACING * k


def pilots_for(width: int) -> tuple[int, ...]:
    return tuple(p for p in PILOT_TONES if p < width)


def data_tones_for(width: int, mask: frozenset[int] | None = None) -> tuple[int, ...]:
    """Data tones at a width, lowest first, minus pilots and masked tones."""
    pilots = set(pilots_for(width))
    masked = mask or frozenset()
    return tuple(k for k in range(width) if k not in pilots and k not in masked)


def band_high_hz(width: int) -> float:
    return tone_freq(width - 1) + TONE_SPACING / 2


# --- 6.1 preamble, 8.1 frame layout -------------------------------------------

PREAMBLE_SYMBOLS = 4          # S1, S1, S2, S2
PROBE_SYMBOLS = 2
HEADER_INFO_BITS = 144
CONTROL_INFO_BITS = 256
DATA_INFO_BITS = 1024         # 126 payload bytes + CRC-16
DATA_PAYLOAD_BITS = 1008
MAX_FRAME_S = 4.0

# --- 6.2 MCS ladder, A.3 code lengths -----------------------------------------


@dataclass(frozen=True)
class Rung:
    mcs: int
    bits_per_symbol: int      # constellation bits per tone-symbol
    code_rate: str
    coded_bits: int           # data block length on air (A.3)
    nominal_snr_db: float     # SPEC 6.2 "Nominal SNR" column

    @property
    def modulation(self) -> str:
        return {2: "QPSK", 4: "16-QAM", 6: "64-QAM", 8: "256-QAM"}[self.bits_per_symbol]


RUNGS: tuple[Rung, ...] = (
    Rung(0, 2, "1/2", 2048, 5.0),
    Rung(1, 2, "3/4", 1368, 8.0),
    Rung(2, 4, "1/2", 2048, 11.0),
    Rung(3, 4, "3/4", 1368, 15.0),
    Rung(4, 6, "2/3", 1536, 19.0),
    Rung(5, 6, "5/6", 1232, 23.0),
    Rung(6, 8, "3/4", 1368, 27.0),
)

# --- 6.3 / 20.2 LDPC codes ----------------------------------------------------

CODES_DIR = Path(__file__).resolve().parents[2] / "codes"


@dataclass(frozen=True)
class CodeSpec:
    name: str
    base_graph: int           # 1 or 2
    lifting: int              # Z
    info_bits: int            # after shortening (includes the CRC-16)
    coded_bits: tuple[int, ...]  # lengths used on air

    @property
    def set_index(self) -> int:
        """38.212 Table 5.3.2-1 lifting-size set index for Z."""
        # Z = a * 2^j; the set is identified by a, the odd part of Z
        # (powers of two belong to the a = 2 set).
        odd = self.lifting
        while odd % 2 == 0:
            odd //= 2
        return (2, 3, 5, 7, 9, 11, 13, 15).index(odd if odd != 1 else 2)


HEADER_CODE = CodeSpec("header", 2, 15, HEADER_INFO_BITS, (288, 432))
CONTROL_CODE = CodeSpec("control", 2, 26, CONTROL_INFO_BITS, (512,))
DATA_CODE = CodeSpec("data", 1, 48, DATA_INFO_BITS, (2048, 1536, 1368, 1232))

# --- 7.1 levels ---------------------------------------------------------------

CLIP_DB_ABOVE_RMS = 7.0       # clip-and-filter threshold at MCS 0-3 (the reference level)

# Clip level by rung: 7 dB above RMS at MCS 0-3, 8 dB at MCS 4, 9 dB at MCS 5,
# 10 dB at MCS 6 (Stage 1 finding: a fixed 7 dB clip leaves 64-QAM 5/6 and
# 256-QAM 3/4 at an error floor set by the clipper's own distortion). A frame
# is clipped as a whole at its data rung's level.
CLIP_DB_BY_MCS: tuple[float, ...] = (7.0, 7.0, 7.0, 7.0, 8.0, 9.0, 10.0)


def clip_db(mcs: int) -> float:
    """Clip-and-filter level (dB above RMS) for frames at rung ``mcs``."""
    return CLIP_DB_BY_MCS[mcs]


def clip_offset_db(mcs: int) -> float:
    """dB to add to an average-referenced SNR to put it on the 7 dB-clip scale.

    The output peak is fixed (-3 dBFS), so each dB of clip level above 7 dB
    costs a dB of average power on a peak-limited radio. A threshold measured
    at average power ``snr`` is reported as ``snr + clip_offset_db(mcs)``,
    the SNR a 7 dB-clipped signal at the same peak would need.
    """
    return clip_db(mcs) - CLIP_DB_ABOVE_RMS

PEAK_DBFS = -3.0              # output peak level
