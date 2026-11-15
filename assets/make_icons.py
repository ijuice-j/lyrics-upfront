"""Build the app's .ico files from the Lyrics UpFront mark (logo.png).

    python assets/make_icons.py

Writes, next to this script:
  app.ico         the icon: exe, shortcuts, installer, Settings window, tray
                  while a song plays
  app-paused.ico  the same mark greyed out, for the tray while paused or idle

Every size is downscaled from the 512px master with Lanczos, so small sizes
stay as sharp as the artwork allows. Images are stored as PNG inside the
.ico, which Windows 10/11 read at every size and which keeps the exe small.
build.rs embeds both files; nothing here runs during a normal build.
Needs Pillow.
"""

from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def paused(im: Image.Image) -> Image.Image:
    """Grey, slightly dimmed copy: the blue plate turns slate, the wave light grey."""
    r, g, b, a = im.split()
    lum = Image.merge("RGB", (r, g, b)).convert("L")
    lum = lum.point(lambda v: round(v * 0.6 + 60))
    return Image.merge("RGBA", (lum, lum, lum, a))


def write_ico(master: Image.Image, path: Path) -> None:
    frames = [master.resize((s, s), Image.Resampling.LANCZOS) for s in SIZES]
    frames[-1].save(path, format="ICO", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])


def main() -> None:
    master = Image.open(HERE / "logo.png").convert("RGBA")
    if master.size != (512, 512):
        master = master.resize((512, 512), Image.Resampling.LANCZOS)
    write_ico(master, HERE / "app.ico")
    write_ico(paused(master), HERE / "app-paused.ico")
    for name in ("app.ico", "app-paused.ico"):
        print(name, (HERE / name).stat().st_size, "bytes")


if __name__ == "__main__":
    main()
