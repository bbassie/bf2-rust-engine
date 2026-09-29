"""Contact sheet: tiles a folder's screenshots into one labelled image, so a scenario run can
be reviewed at a glance (one image to open instead of many).

    python scripts/sheet.py target/scenarios/viewmodel              # -> <dir>/_sheet.png
    python scripts/sheet.py target/scenarios/viewmodel --cols 2 --width 900
    python scripts/sheet.py a.png b.png c.png --out target/sheet.png

Needs Pillow (`pip install pillow`).
"""
import argparse
import glob
import os
import sys

from PIL import Image, ImageDraw, ImageFont


def font(size):
    for name in ('consola.ttf', 'DejaVuSansMono.ttf', 'arial.ttf'):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            pass
    return ImageFont.load_default()


def label(draw, xy, text, fnt):
    x, y = xy
    box = draw.textbbox((x, y), text, font=fnt)
    draw.rectangle((box[0] - 4, box[1] - 3, box[2] + 4, box[3] + 3), fill=(0, 0, 0))
    draw.text((x, y), text, fill=(255, 255, 255), font=fnt)


def sheet(files, cols, width, out):
    images = [Image.open(f).convert('RGB') for f in files]
    tile_h = max(round(width * im.height / im.width) for im in images)
    rows = (len(images) + cols - 1) // cols
    gap = 6
    canvas = Image.new('RGB', (cols * width + (cols - 1) * gap, rows * tile_h + (rows - 1) * gap), (40, 40, 40))
    draw = ImageDraw.Draw(canvas)
    fnt = font(max(14, width // 32))
    for i, (path, im) in enumerate(zip(files, images)):
        x, y = (i % cols) * (width + gap), (i // cols) * (tile_h + gap)
        canvas.paste(im.resize((width, round(width * im.height / im.width)), Image.LANCZOS), (x, y))
        label(draw, (x + 8, y + 6), os.path.splitext(os.path.basename(path))[0], fnt)
    canvas.save(out)
    return canvas.size


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('paths', nargs='+', help='a folder of PNGs, or PNG files')
    parser.add_argument('--cols', type=int, default=0, help='columns (default: 2-4 by count)')
    parser.add_argument('--width', type=int, default=0, help='tile width in pixels')
    parser.add_argument('--out', help='output file (default: <folder>/_sheet.png)')
    args = parser.parse_args()

    if len(args.paths) == 1 and os.path.isdir(args.paths[0]):
        folder = args.paths[0]
        files = sorted(f for f in glob.glob(os.path.join(folder, '*.png')) if not os.path.basename(f).startswith('_'))
        out = args.out or os.path.join(folder, '_sheet.png')
    else:
        files = args.paths
        out = args.out or 'sheet.png'
    if not files:
        sys.exit('no PNG files found')
    cols = args.cols or (2 if len(files) <= 4 else 3 if len(files) <= 9 else 4)
    # About 1900 px wide in total: readable labels, one image to look at.
    width = args.width or 1900 // cols
    w, h = sheet(files, cols, width, out)
    print(f'{out}: {len(files)} screenshots, {w}x{h}')


if __name__ == '__main__':
    main()
