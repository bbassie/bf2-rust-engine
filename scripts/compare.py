"""Before/after comparison of two screenshot folders: pairs files by name, prints how much each
changed and writes a sheet with before | after | difference (amplified) per row.

    python scripts/compare.py target/scenarios/light/before/karkand target/scenarios/light/after/karkand
    python scripts/compare.py before/ after/ --out target/compare.png --only interior,soldier

A "changed" pixel differs by more than --threshold (0-255, default 12) in any channel. Use it
to confirm that a change did what it should and nothing else, e.g. that a lighting tweak left
the other maps alone. Needs Pillow (`pip install pillow`).
"""
import argparse
import glob
import os
import sys

from PIL import Image, ImageChops, ImageDraw, ImageStat

sys.path.insert(0, os.path.dirname(__file__))
from sheet import font, label  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    parser.add_argument('before')
    parser.add_argument('after')
    parser.add_argument('--out', help='sheet (default: <after>/_compare.png)')
    parser.add_argument('--threshold', type=int, default=12)
    parser.add_argument('--width', type=int, default=620, help='tile width')
    parser.add_argument('--only', help='comma-separated names to include')
    args = parser.parse_args()

    names = sorted(
        os.path.basename(f) for f in glob.glob(os.path.join(args.after, '*.png'))
        if not os.path.basename(f).startswith('_') and os.path.exists(os.path.join(args.before, os.path.basename(f)))
    )
    if args.only:
        wanted = set(args.only.split(','))
        names = [n for n in names if os.path.splitext(n)[0] in wanted]
    if not names:
        sys.exit('no screenshots with the same name in both folders')

    rows = []
    print(f'{"screenshot":32} {"mean diff":>9} {"changed":>8}')
    for name in names:
        a = Image.open(os.path.join(args.before, name)).convert('RGB')
        b = Image.open(os.path.join(args.after, name)).convert('RGB')
        if a.size != b.size:
            b = b.resize(a.size)
        diff = ImageChops.difference(a, b)
        mean = sum(ImageStat.Stat(diff).mean) / 3
        mask = diff.convert('L').point(lambda v: 255 if v > args.threshold else 0)
        changed = ImageStat.Stat(mask).mean[0] / 255 * 100
        print(f'{os.path.splitext(name)[0]:32} {mean:9.2f} {changed:7.1f}%')
        heat = diff.point(lambda v: min(255, v * 4))
        rows.append((name, a, b, heat, mean, changed))

    w = args.width
    h = round(w * rows[0][1].height / rows[0][1].width)
    gap = 6
    canvas = Image.new('RGB', (3 * w + 2 * gap, len(rows) * (h + gap) - gap), (40, 40, 40))
    draw = ImageDraw.Draw(canvas)
    fnt = font(15)
    for i, (name, a, b, heat, mean, changed) in enumerate(rows):
        y = i * (h + gap)
        for j, (im, tag) in enumerate([(a, 'before'), (b, 'after'), (heat, 'difference x4')]):
            canvas.paste(im.resize((w, h), Image.LANCZOS), (j * (w + gap), y))
            text = f'{os.path.splitext(name)[0]}: {tag}' if j == 0 else tag
            if j == 2:
                text += f'  {changed:.1f}% changed'
            label(draw, (j * (w + gap) + 8, y + 6), text, fnt)
    out = args.out or os.path.join(args.after, '_compare.png')
    canvas.save(out)
    print(f'{out}: {len(rows)} pairs')


if __name__ == '__main__':
    main()
