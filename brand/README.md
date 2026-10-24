# musiKore brand

The musiKore logo is the devKore wordmark with "dev" replaced by "musi". "Kore"
is the original devkore.com wordmark (`DK.Web/public/images/brand/logo-kore-wordmark-tight.svg`):
the cut K, the "o" and "e" cut back 3.5 units clear of the chevrons, and the
lime chevron. Open `preview.html` to see everything on dark and light grounds.

| File | Use |
| --- | --- |
| `musikore-logo-color.svg` | Main logo, on dark grounds (musi in light) |
| `musikore-logo-color-on-light.svg` | Main logo, on light grounds (musi in ink) |
| `musikore-logo-white.svg`, `musikore-logo-ink.svg` | One-color versions |
| `musikore-mark.svg` | The K› icon, the same on dark and light grounds |
| `musikore-app-icon.svg` | App icon: the K› icon on an ink tile, on Apple's 1024 grid (824 tile) |
| `musikore-app-icon-light.svg` | The same icon on a white tile, as an alternative |
| `app-icon/musiKore.icns` | macOS app icon, built from `musikore-app-icon.svg` |
| `musikore-favicon.svg` | Small square tile for web and docs |
| `png/` | PNG exports of the above |

## Colors

| Name | Value | Role |
| --- | --- | --- |
| Ink | `#121217` | Ground, and "musi" on light |
| Light | `#F7F7F8` | "musi" on dark |
| Violet | `#8051FF` | Kore and the K |
| Lime | `#B7FF00` | The chevron only |

## How the letters are built

No font is used, so the letters belong to the brand. The "ore" letters match no
open font closely, so "musi" is drawn from them:

- **m** is the "r"'s stem, junction and shoulder, turned down into a right stem,
  twice. The second arch meets the first in a V.
- **u** is that arch turned 180°, so its bottom-right spur matches the "d".
- **s** follows the "e": thin horizontals (5.75), heavy sides (8.5) and
  vertical cuts at both ends.
- **i** has the K's corner triangle as its dot, cut from the stem by the same
  3.5 gap the "o" and "e" have. Its top lines up with the K.
- Spacing: 3.4 between straight sides, 2.9 next to a round side, and 3.5 (the
  cut width) between the "i" and the K.

## The K› icon

The icon is devKore's K› (`DK.Web/brand/devkore/icon-v2`) without the lime
sliver inside the K: the violet K's chevron sits over the lime chevron, with a
diamond between them. The lime is cut where the violet covers it, so the file
has no overlapping shapes. The arms run at 45°, the same as the wordmark's K.
(The PNG copy in the business folder is about 9% wider than this.)

## Rebuilding

`source/` has the scripts that drew these files. To change a letter, edit
`source/glyphs.py`, then:

```bash
python3 -m venv .venv && .venv/bin/pip install -r brand/source/requirements.txt
```

```bash
cd brand/source && ../../.venv/bin/python build.py ..
```

Then re-export the app icon (`rsvg-convert` is in Homebrew's `librsvg`;
`iconutil` comes with macOS):

```bash
cd brand && mkdir -p app-icon/musiKore.iconset && for s in 16 32 128 256 512; do rsvg-convert -w $s musikore-app-icon.svg -o app-icon/musiKore.iconset/icon_${s}x${s}.png; rsvg-convert -w $((s*2)) musikore-app-icon.svg -o app-icon/musiKore.iconset/icon_${s}x${s}@2x.png; done && iconutil -c icns app-icon/musiKore.iconset -o app-icon/musiKore.icns
```

The PNGs in `png/` are exported the same way: logos at 2400 px wide, marks at
1200 px, and the icon at 1024 px.
