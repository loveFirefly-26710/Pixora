#!/usr/bin/env python3
"""生成一张合规的最小 PNG，给冒烟测试当上传素材。

用法：python make-test-png.py <输出路径> [边长]
"""

import struct
import sys
import zlib


def chunk(tag: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


def make_png(size: int) -> bytes:
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)
    row = b"\x00" + bytes([0x20, 0x80, 0x40]) * size
    raw = row * size
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


def main() -> int:
    if len(sys.argv) < 2:
        print("用法：python make-test-png.py <输出路径> [边长]", file=sys.stderr)
        return 2
    path = sys.argv[1]
    size = int(sys.argv[2]) if len(sys.argv) > 2 else 8
    with open(path, "wb") as handle:
        handle.write(make_png(size))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
