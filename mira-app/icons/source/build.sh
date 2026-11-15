#!/usr/bin/env bash
# Regenerates every icon in ../ from the three SVG plates beside this script.
#
# Needs rsvg-convert (librsvg) and python3. Run it from anywhere.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
icons=$(dirname "$here")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

render() { rsvg-convert -w "$2" -h "$2" "$here/$1.svg" -o "$3"; }

# Linux desktop icons.
render small 32 "$icons/32x32.png"
render square 128 "$icons/128x128.png"
render square 256 "$icons/128x128@2x.png"
render square 512 "$icons/icon.png"

# macOS: Apple's grid at every size, each rendered from the plate rather than scaled from
# another, and every entry a PNG, which macOS has read since 10.7. Written in type order, so
# an unchanged icon is unchanged on disk.
for size in 16 32 64 128 256 512 1024; do
    render macos "$size" "$work/macos-$size.png"
done
python3 - "$icons/icon.icns" "$work" <<'EOF'
import struct, sys

out, work = sys.argv[1], sys.argv[2]
# Each entry's type and the pixels it holds; the @2x types share a size with a 1x one.
types = [
    ("ic07", 128), ("ic08", 256), ("ic09", 512), ("ic10", 1024), ("ic11", 32),
    ("ic12", 64), ("ic13", 256), ("ic14", 512), ("icp4", 16), ("icp5", 32),
]
body = b""
for kind, size in sorted(types):
    image = open(f"{work}/macos-{size}.png", "rb").read()
    body += kind.encode() + struct.pack(">I", 8 + len(image)) + image
open(out, "wb").write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
EOF

# Windows: the small plate up to 48px, where the detailed one smears, and the detailed
# one above. Every entry is a PNG; Windows has read those since Vista, and they are a
# fraction of the size of bitmaps.
sizes=(16 24 32 48 64 256)
for size in "${sizes[@]}"; do
    if ((size <= 48)); then plate=small; else plate=square; fi
    render "$plate" "$size" "$work/$size.png"
done
python3 - "$icons/icon.ico" "${sizes[@]/#/$work/}" <<'EOF'
import struct, sys

out, sources = sys.argv[1], [f"{path}.png" for path in sys.argv[2:]]
images = [open(path, "rb").read() for path in sources]
header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
directory = b""
for image in images:
    width, height = struct.unpack(">II", image[16:24])
    # A dimension of 256 is written as 0.
    directory += struct.pack("<BBBBHHII", width % 256, height % 256, 0, 0, 1, 32, len(image), offset)
    offset += len(image)
open(out, "wb").write(header + directory + b"".join(images))
EOF
