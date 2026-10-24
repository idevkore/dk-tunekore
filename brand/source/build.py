"""Writes the tuneKore logo (TUNE plus the K> icon), the icon and the app icons into brand/."""
import os, sys
from glyphs import *

OUT = sys.argv[1]
INK, LIGHT, VIOLET, LIME, WHITE = "#121217", "#F7F7F8", "#8051FF", "#B7FF00", "#FFFFFF"
GAP_STEM, GAP_ROUND = 3.4, 2.9

def layout():
    """TUNE in capitals, then the K> icon, one cut-width apart."""
    x = 0.0; parts = []
    for g, gap in [(cap_T(), 2.4), (cap_U(), 3.4), (cap_N(), 3.4), (cap_E(), CUT)]:
        parts.append(move(g, x)); x += bounds(g)[2] + gap
    return U(*parts), x

def svg(view, body, title, w=None):
    x0, y0, x1, y1 = view
    vw, vh = x1 - x0, y1 - y0
    w = w or vw * 640 / max(vw, vh) if vw >= vh else w
    size = f' width="{640:.0f}" height="{640*vh/vw:.3f}"' if vw >= vh else f' width="{640*vw/vh:.3f}" height="640"'
    return (f'<svg xmlns="http://www.w3.org/2000/svg"{size} viewBox="{x0:.3f} {y0:.3f} {vw:.3f} {vh:.3f}" fill="none" role="img" aria-label="{title}">\n'
            f'<title>{title}</title>\n{body}</svg>\n')

def path(id_, fill, p): return f'<path id="{id_}" fill="{fill}" d="{d_of(p)}"/>\n'

os.makedirs(OUT, exist_ok=True)

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

open(f"{OUT}/tunekore-mark.svg", "w").write(svg((0, 0, mb[2], mb[3]), mark_at(1, 0, 0), "tuneKore"))

def tile_icon(name, ground):
    # Apple's 1024 grid: 824 rounded tile, mark 66% of the tile wide (as in devKore's tile), centered
    s = 0.66 * 824 / mb[2]
    body = (f'<rect id="tile" x="100" y="100" width="824" height="824" rx="185.4" fill="{ground}"/>\n'
            + mark_at(s, 512 - mb[2]*s/2, 512 - mb[3]*s/2))
    open(f"{OUT}/{name}.svg", "w").write(
        '<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024" fill="none" role="img" aria-label="tuneKore">\n'
        f'<title>tuneKore</title>\n{body}</svg>\n')

tile_icon("tunekore-app-icon", INK)
tile_icon("tunekore-app-icon-light", WHITE)

# Favicon: full-bleed rounded square, mark 76% wide
s2 = 76 / mb[2]
fav = f'<rect id="tile" width="100" height="100" rx="18" fill="{INK}"/>\n' + mark_at(s2, 50 - mb[2]*s2/2, 50 - mb[3]*s2/2)
open(f"{OUT}/tunekore-favicon.svg", "w").write(
    '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 100 100" fill="none" role="img" aria-label="tuneKore">\n'
    f'<title>tuneKore</title>\n{fav}</svg>\n')

# tuneKore logo: TUNE, then the K> icon
tune, x_k = layout()
lk_v, lk_l = move(KV, x_k), move(ARROW, x_k)
b = bounds(U(tune, lk_v, lk_l)); view = (0, 0, b[2], max(b[3], 31.79))
variants = {
    "tunekore-logo-color": (LIGHT, VIOLET, LIME),          # on dark grounds
    "tunekore-logo-color-on-light": (INK, VIOLET, LIME),
    "tunekore-logo-white": (WHITE, WHITE, WHITE),
    "tunekore-logo-ink": (INK, INK, INK),
}
for name, (c1, c2, c3) in variants.items():
    body = path("TUNE", c1, tune) + path("K", c2, lk_v) + path("chevron", c3, lk_l)
    open(f"{OUT}/{name}.svg", "w").write(svg(view, body, "tuneKore"))
