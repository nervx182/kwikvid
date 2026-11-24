"""Report whether a BMP screenshot shows the spike's test pattern.

Usage: python3 has_test_pattern.py shot.bmp [label]
Looks for the yellow (235, 235, 16) and cyan (16, 235, 235) colour bars that
every preview path draws. Prints one line; never fails the job.
"""
import struct
import sys


def main() -> None:
    path = sys.argv[1]
    label = sys.argv[2] if len(sys.argv) > 2 else path
    try:
        data = open(path, "rb").read()
        off, = struct.unpack_from("<I", data, 10)
        w, h = struct.unpack_from("<ii", data, 18)
        bpp, = struct.unpack_from("<H", data, 28)
        if bpp not in (24, 32):
            raise ValueError(f"unsupported bpp {bpp}")
        step = bpp // 8
        stride = (w * step + 3) & ~3
        yellow = cyan = samples = 0
        for y in range(0, abs(h), 4):
            row = off + y * stride
            for x in range(0, w, 4):
                b, g, r = data[row + x * step : row + x * step + 3]
                samples += 1
                if r > 200 and g > 200 and b < 70:
                    yellow += 1
                elif b > 200 and g > 200 and r < 70:
                    cyan += 1
        visible = yellow > samples * 0.005 and cyan > samples * 0.005
        print(f"SCREENSHOT {label}: test pattern visible={'yes' if visible else 'no'} "
              f"(yellow {100 * yellow / samples:.1f}%, cyan {100 * cyan / samples:.1f}%, {w}x{abs(h)})")
    except Exception as e:  # noqa: BLE001 - diagnostics only
        print(f"SCREENSHOT {label}: could not analyse ({e})")


main()
