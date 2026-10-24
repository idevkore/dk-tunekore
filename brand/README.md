# tuneKore brand

The tuneKore logo is **TUNE** in capitals followed by the K› icon: "TUNE K›".
The name is written tuneKore in text. The logo was rebranded from musiKore on
2026-10-24. Open `preview.html` to see everything on dark and light grounds.

| File | Use |
| --- | --- |
| `tunekore-logo-color.svg` | Main logo, on dark grounds (TUNE in light) |
| `tunekore-logo-color-on-light.svg` | Main logo, on light grounds (TUNE in ink) |
| `tunekore-logo-white.svg`, `tunekore-logo-ink.svg` | One-color versions |
| `tunekore-mark.svg` | The K› icon, the same on dark and light grounds |
| `tunekore-app-icon.svg` | App icon: the K› icon on an ink tile, on Apple's 1024 grid (824 tile) |
| `tunekore-app-icon-light.svg` | The same icon on a white tile, as an alternative |
| `app-icon/tuneKore.icns` | macOS app icon, built from `tunekore-app-icon.svg` |
| `tunekore-favicon.svg` | Small square tile for web and docs |
| `png/` | PNG exports of the above |

## Colors

| Name | Value | Role |
| --- | --- | --- |
| Ink | `#121217` | Ground, and TUNE on light |
| Light | `#F7F7F8` | TUNE on dark |
| Violet | `#8051FF` | The K |
| Lime | `#B7FF00` | The chevron only |

## How TUNE is built

No font is used, so the letters belong to the brand. They are drawn as plain
geometry, matched to the K:

- **Cap height** is the K's height (31.236 units).
- **Vertical strokes** are 8.9 units, the same as the devKore wordmark's
  stems. **Horizontal strokes** are 7.0.
- **U** has the same rounded bottom as the wordmark's "o", with the same small
  overshoot below the baseline.
- **N** has a straight diagonal from the top of its left stem to the foot of
  its right stem.
- **Spacing:** 2.4 after the T (its bar already leaves air), 3.4 between the
  other letters, and 3.5, the K's cut width, between the E and the K›.

`source/glyphs.py` also holds the earlier lowercase letters (m, u, s, i) from
the musiKore logo, in case a lowercase mark is wanted later.

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
cd brand && mkdir -p app-icon/tuneKore.iconset && for s in 16 32 128 256 512; do rsvg-convert -w $s tunekore-app-icon.svg -o app-icon/tuneKore.iconset/icon_${s}x${s}.png; rsvg-convert -w $((s*2)) tunekore-app-icon.svg -o app-icon/tuneKore.iconset/icon_${s}x${s}@2x.png; done && iconutil -c icns app-icon/tuneKore.iconset -o app-icon/tuneKore.icns
```

The PNGs in `png/` are exported the same way: logos at 2400 px wide, marks at
1200 px, and the icon at 1024 px.
