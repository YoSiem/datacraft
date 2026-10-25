"""Export every DataCraft application icon from the imagegen master (requires Pillow)."""
import argparse
import io
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
MASTER = ROOT / "assets" / "icon-master.png"


def rounded(size: int, inset: int = 0) -> Image.Image:
    """macOS icon shape (Apple's grid: corner radius 22.4 % of the square) as an alpha mask."""
    mask = Image.new("L", (size, size), 0)
    radius = round((size - 2 * inset) * 0.224)
    ImageDraw.Draw(mask).rounded_rectangle((inset, inset, size - 1 - inset, size - 1 - inset), radius, fill=255)
    return mask


def inner_tile(master: Image.Image) -> Image.Image:
    """The master's navy content, inside its cyan frame (found by scanning from the edges)."""
    w, h = master.size
    def cyan(x, y):
        r, g, b, a = master.getpixel((x, y))
        return a > 200 and g > 200 and b > 200 and r < 80
    left = max(x for x in range(w // 2) if cyan(x, h // 2))
    right = min(x for x in range(w // 2, w) if cyan(x, h // 2))
    top = max(y for y in range(h // 2) if cyan(w // 2, y))
    bottom = min(y for y in range(h // 2, h) if cyan(w // 2, y))
    return master.crop((left + 1, top + 1, right, bottom))


def macos_icon(master: Image.Image) -> Image.Image:
    """The tile on Apple's grid: an 824 px rounded square centred on 1024 px, filled to its edges,
    with the cyan frame redrawn along it. macOS 26 shrinks an .icns that leaves that shape partly
    transparent onto a grey plate, so the closed app looked smaller in the Dock than the open one."""
    scale = 4  # drawn large and reduced, for smooth edges
    canvas, body = 1024 * scale, 824 * scale
    frame = round(body * 0.03)
    icon = Image.new("RGBA", (body, body), (0, 242, 252, 255))
    content = inner_tile(master).resize((body - 2 * frame, body - 2 * frame), Image.Resampling.LANCZOS)
    content_mask = rounded(body, frame).crop((frame, frame, body - frame, body - frame))
    icon.paste(content, (frame, frame), content_mask)
    out = Image.new("RGBA", (canvas, canvas), (0, 0, 0, 0))
    out.paste(icon, ((canvas - body) // 2, (canvas - body) // 2), rounded(body))
    return out.resize((1024, 1024), Image.Resampling.LANCZOS)


def generate() -> dict[str, bytes]:
    with Image.open(MASTER) as source:
        if source.width != source.height or source.width < 1024:
            raise ValueError("The application icon master must be square and at least 1024 px.")
        master = source.convert("RGBA")
    def draw(size):
        return master.resize((size, size), Image.Resampling.LANCZOS)
    mac = macos_icon(master)
    output = {}
    for name, size, fmt, options in [
        ("icon.png", 512, "PNG", {}),
        ("icon-32.png", 32, "PNG", {}),
        ("icon-64.png", 64, "PNG", {}),
        ("icon.ico", 256, "ICO", {"sizes": [(s, s) for s in SIZES], "append_images": [draw(s) for s in SIZES[:-1]]}),
        ("DataCraft.icns", 1024, "ICNS", {}),
        ("icon-macos.png", 512, "PNG", {}),
    ]:
        buffer = io.BytesIO()
        image = mac.resize((size, size), Image.Resampling.LANCZOS) if name in ("DataCraft.icns", "icon-macos.png") else draw(size)
        image.save(buffer, format=fmt, **options)
        output[name] = buffer.getvalue()
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check that committed assets match the generator")
    args = parser.parse_args()
    for name, content in generate().items():
        path = ROOT / "assets" / name
        if args.check:
            if not path.is_file() or path.read_bytes() != content:
                parser.error(f"{path} is out of date; run python scripts/make_icon.py")
        else:
            path.write_bytes(content)
    print("Application icons are up to date." if args.check else "Exported PNG, ICO and ICNS application icons from icon-master.png.")


if __name__ == "__main__":
    main()
