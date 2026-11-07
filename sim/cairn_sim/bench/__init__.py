"""Bench test kit (SPEC 14.3): test frames to a WAV, a recording back to a report.

``tx`` writes a WAV of Stage 1 transmitter frames plus a JSON manifest; play
it through radio A, record radio B, and ``rx`` decodes the recording offline
with the Stage 1 receiver, so no real-time modem is needed. See
``docs/bench-testing.md``.
"""
