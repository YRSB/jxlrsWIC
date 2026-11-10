"""Generates the JPEG XL test fixtures committed under `tests/fixtures`.

Run from the repository root on a machine with Pillow and libjxl's `cjxl`:

    python tests/fixtures/generate.py

The generated files are committed so CI never needs an encoder. Every fixture is deliberately
tiny (3x3) and uses fully saturated colours, which keeps handedness assertions exact.
"""

from __future__ import annotations

import shutil
import struct
import subprocess
import sys
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
TMP = HERE / "_build"

# Fully saturated primaries, one per pixel, row-major.
PRIMARIES = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 0),
    (0, 255, 255),
    (255, 0, 255),
    (0, 0, 0),
    (255, 255, 255),
    (128, 64, 32),
]


def find_tool(name: str) -> str:
    for candidate in (name, f"{name}.exe"):
        found = shutil.which(candidate)
        if found:
            return found
    raise SystemExit(f"{name} not found on PATH; install libjxl first")


def encode(cjxl: str, source: Path, output: Path, *args: str) -> None:
    subprocess.run(
        [cjxl, "-d", "0", "-e", "1", *args, str(source), str(output)],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def png(path: Path, mode: str, data: bytes, size=(3, 3)) -> Path:
    image = Image.frombytes(mode, size, data)
    image.save(path)
    return path


def bgra8_data() -> bytes:
    out = bytearray()
    for red, green, blue in PRIMARIES:
        out += bytes((blue, green, red, 255))
    return bytes(out)


def rgba8_alpha_data() -> bytes:
    return bytes(
        (
            255, 0, 0, 255,
            0, 255, 0, 128,
            0, 0, 255, 0,
            255, 255, 0, 255,
            0, 255, 255, 64,
            255, 0, 255, 255,
            0, 0, 0, 255,
            255, 255, 255, 255,
            128, 64, 32, 255,
        )
    )


def rgb16_data() -> bytes:
    out = bytearray()
    for red, green, blue in PRIMARIES:
        out += struct.pack(">HHH", red * 257, green * 257, blue * 257)
    return bytes(out)


def rgb16_primaries_data() -> bytes:
    """16-bit primaries in big-endian order, as PAM expects."""
    out = bytearray()
    for red, green, blue in PRIMARIES:
        out += struct.pack(">HHH", red * 257, green * 257, blue * 257)
    return bytes(out)


def main() -> int:
    cjxl = find_tool("cjxl")
    djxl = find_tool("djxl")
    TMP.mkdir(exist_ok=True)

    # 8-bit sRGB, opaque primaries. Lossless modular keeps the exact sample values.
    source = png(TMP / "srgb8.png", "RGBA", bgra8_data())
    encode(cjxl, source, HERE / "srgb8_lossless.jxl", "-m", "1")
    encode(cjxl, source, HERE / "srgb8_var_dct.jxl", "-m", "0", "-e", "3")

    # Embedded ICC: extract libjxl's own synthesized sRGB profile (BSD licensed, so it can be
    # committed) and tag a second lossless encode with it verbatim.
    subprocess.run(
        [djxl, str(HERE / "srgb8_lossless.jxl"), str(TMP / "unused.png"),
         f"--icc_out={HERE / 'embedded.icc'}"],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    encode(
        cjxl,
        source,
        HERE / "icc_embedded.jxl",
        "-m",
        "1",
        "-x",
        f"icc_pathname={HERE / 'embedded.icc'}",
    )

    # 8-bit sRGB with a real alpha channel.
    source = png(TMP / "alpha.png", "RGBA", rgba8_alpha_data())
    encode(cjxl, source, HERE / "alpha_lossless.jxl", "-m", "1")

    # 16-bit integer samples, encoded from a PAM carrying 16-bit precision.
    pam = TMP / "rgb16.pam"
    pam_contents = (
        b"P7\nWIDTH 3\nHEIGHT 3\nDEPTH 3\nMAXVAL 65535\nTUPLTYPE RGB\nENDHDR\n"
        + rgb16_primaries_data()
    )
    pam.write_bytes(pam_contents)
    encode(cjxl, pam, HERE / "rgb16_lossless.jxl", "-m", "1")

    # HDR: a 16-bit Rec.2100 PQ image, the shape real HDR photographs take. The decoder exposes
    # it as 64bppRGBAHalf so HDR fidelity survives into Direct2D.
    encode(
        cjxl,
        pam,
        HERE / "hdr_pq.jxl",
        "-m",
        "1",
        "-x",
        "color_space=RGB_D65_202_Rel_PeQ",
        "--intensity_target=10000",
    )

    shutil.rmtree(TMP, ignore_errors=True)
    for path in sorted(HERE.glob("*.jxl")):
        print(f"{path.name}: {path.stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

