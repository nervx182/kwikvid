# 0005 Permissive dependencies only

Status: accepted (2026-11-24)

Context: the project may go open source later and must not depend on proprietary GoPro material.

Decision: cargo deny enforces a permissive licence allowlist (deny.toml). FFmpeg only as an LGPL dynamic build without GPL codecs. Hardware/OS encoders (VideoToolbox) for H.264/HEVC. No Quik music, gauge templates or installer content in the repo.

Consequences: own beat detector; frame-blend slow-mo until an open interpolation model is integrated.
