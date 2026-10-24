"""Writes the musiKore logo, the K> icon and the app icons into brand/."""
import os, sys
from glyphs import *

OUT = sys.argv[1]
INK, LIGHT, VIOLET, LIME, WHITE = "#121217", "#F7F7F8", "#8051FF", "#B7FF00", "#FFFFFF"
GAP_STEM, GAP_ROUND = 3.4, 2.9

def layout():
    m = glyph_m(); u = glyph_u(); s = glyph_s(); i = glyph_i()
    x = 0.0; parts = []
    parts.append(move(m, x)); x = bounds(m)[2] + GAP_STEM
    parts.append(move(u, x)); x += bounds(u)[2] + GAP_ROUND
    parts.append(move(s, x - bounds(s)[0])); x += bounds(s)[2] - bounds(s)[0] + GAP_ROUND
    parts.append(move(i, x)); x += bounds(i)[2] + CUT
    musi = U(*parts)
    v, l = kore()
    dx = x - 2.568
    return musi, move(v, dx, -0.115), move(l, dx, -0.115)

def svg(view, body, title, w=None):
    x0, y0, x1, y1 = view
    vw, vh = x1 - x0, y1 - y0
    w = w or vw * 640 / max(vw, vh) if vw >= vh else w
    size = f' width="{640:.0f}" height="{640*vh/vw:.3f}"' if vw >= vh else f' width="{640*vw/vh:.3f}" height="640"'
    return (f'<svg xmlns="http://www.w3.org/2000/svg"{size} viewBox="{x0:.3f} {y0:.3f} {vw:.3f} {vh:.3f}" fill="none" role="img" aria-label="{title}">\n'
            f'<title>{title}</title>\n{body}</svg>\n')

def path(id_, fill, p): return f'<path id="{id_}" fill="{fill}" d="{d_of(p)}"/>\n'

os.makedirs(OUT, exist_ok=True)
musi, kv, kl = layout()
b = bounds(U(musi, kv, kl)); view = (0, 0, b[2], max(b[3], 31.77))
variants = {
    "musikore-logo-color": (LIGHT, VIOLET, LIME),          # on dark grounds
    "musikore-logo-color-on-light": (INK, VIOLET, LIME),
    "musikore-logo-white": (WHITE, WHITE, WHITE),
    "musikore-logo-ink": (INK, INK, INK),
}
for name, (c1, c2, c3) in variants.items():
    body = path("musi", c1, musi) + path("Kore", c2, kv) + path("chevron", c3, kl)
    open(f"{OUT}/{name}.svg", "w").write(svg(view, body, "musiKore"))

# K> icon: the devKore icon-v2 shapes (DK.Web/brand/devkore/icon-v2) without the
# lime sliver in the K. The violet K's chevron sits over the lime chevron, leaving a
# diamond between them; the lime is cut where the violet covers it, so nothing overlaps.
K_TRIS = U(poly([(0.038, 9.808), (0.038, 0), (9.77, 0)]), poly([(9.808, 31.236), (0, 31.236), (0, 21.467)]))
K_CHEV = poly([(28.484, 0), (12.869, 15.615), (28.484, 31.236), (16.771, 31.236), (1.156, 15.615), (16.771, 0)])
ARROW = D(poly([(16.771, 0), (28.484, 0), (44.105, 15.615), (28.484, 31.236), (16.771, 31.236), (32.386, 15.615)]), K_CHEV)
KV = U(K_TRIS, K_CHEV)
mb = bounds(U(KV, ARROW))   # 0, 0, 44.105, 31.236

def mark_at(scale, tx, ty):
    t = (scale, 0, 0, scale, tx, ty)
    return path("K", VIOLET, xf(KV, t)) + path("chevron", LIME, xf(ARROW, t))

open(f"{OUT}/musikore-mark.svg", "w").write(svg((0, 0, mb[2], mb[3]), mark_at(1, 0, 0), "musiKore"))

def tile_icon(name, ground):
    # Apple's 1024 grid: 824 rounded tile, mark 66% of the tile wide (as in devKore's tile), centered
    s = 0.66 * 824 / mb[2]
    body = (f'<rect id="tile" x="100" y="100" width="824" height="824" rx="185.4" fill="{ground}"/>\n'
            + mark_at(s, 512 - mb[2]*s/2, 512 - mb[3]*s/2))
    open(f"{OUT}/{name}.svg", "w").write(
        '<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024" fill="none" role="img" aria-label="musiKore">\n'
        f'<title>musiKore</title>\n{body}</svg>\n')

tile_icon("musikore-app-icon", INK)
tile_icon("musikore-app-icon-light", WHITE)

# Favicon: full-bleed rounded square, mark 76% wide
s2 = 76 / mb[2]
fav = f'<rect id="tile" width="100" height="100" rx="18" fill="{INK}"/>\n' + mark_at(s2, 50 - mb[2]*s2/2, 50 - mb[3]*s2/2)
open(f"{OUT}/musikore-favicon.svg", "w").write(
    '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 100 100" fill="none" role="img" aria-label="musiKore">\n'
    f'<title>musiKore</title>\n{fav}</svg>\n')
print("logo view", [round(v, 3) for v in view], "mark", [round(v, 3) for v in mb])
print(ascii(U(musi, kv, kl), step=0.75, yr=(-1, 33)))
