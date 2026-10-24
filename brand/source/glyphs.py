"""tuneKore letters: the TUNE capitals, plus the earlier lowercase musi letters
built from the devKore wordmark's own strokes (dev coordinates, y down)."""
import math
from geom import *

B, X = 31.177, 5.503          # baseline, flat x-height
CUT = 3.5                     # the wordmark's cut gap (half of the mask's 7-unit stroke)

# devKore letters, for reference and reuse
# The devKore letters (DK.Web/brand/devkore/devkore-logo-color.svg), for side-by-side checks only.
import re, os
_ref = os.path.expanduser("~/dev/DK.Web/brand/devkore/devkore-logo-color.svg")
LOGO = dict(re.findall(r'<path id="([^"]+)"[^>]* d="([^"]+)"', open(_ref).read())) if os.path.exists(_ref) else {}

def arch(dx, W, k=1.0, Sr=8.9, ys=14.6, ysi=17.2, crotch=False):
    """One n-arch: the r's stem junction and shoulder (x-scaled by k from the junction),
    turned down into a right stem of width Sr whose outer edge is at W (local)."""
    jx_o, jx_i = 8.181, 8.872
    ox = lambda x: jx_o + (x - jx_o) * k     # outer shoulder x
    ix = lambda x: jx_i + (x - jx_i) * k     # inner shoulder x
    ax_o, ax_i = ox(15.768), ix(15.327)      # outer / inner apex
    xri = W - Sr
    a_o, h_o = W - ax_o, ys - 4.937
    a_i, h_i = xri - ax_i, ysi - 12.306
    if crotch:   # joining a stem with no flat top (m's middle stem): run the junction's
        # tangent back into the previous arch so the two shoulders meet in a clean V
        tx_, ty_ = ox(9.71) - 8.181, 6.994 - 10.26
        sx_, sy_ = 8.181 - 0.5*tx_, 10.26 - 0.5*ty_
        start = f"M{sx_} {B} L{sx_} {sy_} L8.181 10.26 "
    else:
        start = f"M{8.083} {B} L8.083 10.26 L8.181 10.26 "
    d = (start +
         f"C{ox(9.71)} 6.994 {ox(11.925)} 4.937 {ax_o} 4.937 "
         f"C{ax_o + 0.5674*a_o} 4.937 {W} {4.937 + 0.3663*h_o} {W} {ys} "
         f"L{W} {B} L{xri} {B} L{xri} {ysi} "
         f"C{xri} {12.306 + 0.4696*h_i} {ax_i + 0.709*a_i} 12.306 {ax_i} 12.306 "
         f"C{ix(11.191)} 12.306 {jx_i} 15.311 {jx_i} 20.34 L{jx_i} {B} Z")
    return move(P(d), dx)

def stem_left():
    return rect(0, X, 8.083, B)

def glyph_n(W=28.0, k=0.92):
    return U(stem_left(), arch(0, W, k))

def glyph_m(counter=9.3, k=0.84):
    W1 = 8.872 + counter + 8.9
    return U(stem_left(), arch(0, W1, k), arch(W1 - 8.872, W1, k, crotch=True))

def glyph_u(W=28.0, k=0.92):
    # a turned n: its stem junction becomes the u's spur, like the d's
    return xf(glyph_n(W, k), (-1, 0, 0, -1, W, B + X))

def glyph_i(w=8.9):
    t = w
    tri = poly([(0, 0), (t, 0), (0, t)])
    c = t + CUT * math.sqrt(2)               # stem's top edge: x + y = c
    stem = poly([(0, c), (w, c - w), (w, B), (0, B)])
    return U(tri, stem)

# ---- s: a centerline swept with the wordmark's contrast (thin horizontals, heavy verticals)
def catmull(pts, n=24):
    out = []
    P_ = [pts[0]] + pts + [pts[-1]]
    for i in range(1, len(P_) - 2):
        p0, p1, p2, p3 = P_[i-1], P_[i], P_[i+1], P_[i+2]
        for j in range(n):
            t = j / n; t2, t3 = t*t, t*t*t
            out.append(tuple(0.5*((2*p1[c]) + (-p0[c]+p2[c])*t + (2*p0[c]-5*p1[c]+4*p2[c]-p3[c])*t2
                                  + (-p0[c]+3*p1[c]-3*p2[c]+p3[c])*t3) for c in (0, 1)))
    out.append(pts[-1]); return out

S_SPINE = [(23.4, 10.05), (21.0, 8.6), (17.2, 7.88), (12.0, 7.86), (7.6, 8.36), (4.95, 9.95), (4.3, 12.45),
           (5.65, 14.95), (9.4, 16.85), (14.3, 18.6), (18.0, 20.45), (19.55, 23.55), (18.35, 26.85),
           (14.8, 28.6), (10.0, 28.8), (5.8, 28.45), (2.6, 27.6), (0.0, 26.4)]

def glyph_s(th=5.75, tv=8.5, bump=1.1, xcu=21.2, xcl=2.25, spine=S_SPINE, sx=0.95, raw=False):
    c = catmull([(x*sx, y) for x, y in spine], 30)
    xcu, xcl = xcu*sx, xcl*sx
    L_ = [0.0]
    for i in range(1, len(c)): L_.append(L_[-1] + math.dist(c[i-1], c[i]))
    mid = L_[-1] * 0.5
    outer, inner = [], []
    for i, (x, y) in enumerate(c):
        a, b = c[max(i-1, 0)], c[min(i+1, len(c)-1)]
        tx, ty = b[0]-a[0], b[1]-a[1]; n_ = math.hypot(tx, ty); tx, ty = tx/n_, ty/n_
        phi = abs(math.atan2(ty, tx)); s = abs(math.sin(phi))
        ease = min(1.0, min(L_[i], L_[-1]-L_[i]) / 9.0)   # contrast fades near the terminals
        t = th + (tv - th) * s * ease + bump * math.exp(-((L_[i]-mid)/7.0)**2)
        nx, ny = -ty, tx
        outer.append((x + nx*t/2, y + ny*t/2)); inner.append((x - nx*t/2, y - ny*t/2))
    body = pathops.simplify(poly(outer + inner[::-1]))
    body = D(body, rect(xcu, -10, 60, 16))        # upper-right terminal: vertical cut, like the e
    body = D(body, rect(-30, 19, xcl, 60))        # lower-left terminal
    return body if raw else smooth(body)

# ---- Kore, from the tight wordmark (its own coordinates; y is 0.115 lower than dev's)
TIGHT = {
 "o": "M31.111 18.4795C31.111 9.95603 37.5661 5.03027 46.0406 5.03027C54.515 5.03027 60.9757 9.95603 60.9757 18.4795C60.9757 27.0029 54.5205 31.8851 46.0406 31.8851C37.5607 31.8851 31.111 26.9594 31.111 18.4795ZM51.7936 18.4795C51.7936 14.6368 50.1172 11.2351 46.0242 11.2351C41.9312 11.2351 40.2603 14.6368 40.2603 18.4795C40.2603 22.3221 41.9367 25.6749 46.0242 25.6749C50.1118 25.6749 51.8099 22.2786 51.8099 18.4795H51.7936Z",
 "r": "M82.6055 12.7155C81.6239 12.4701 80.6115 12.3711 79.601 12.4216C75.4645 12.4216 73.1459 15.426 73.1459 20.4552V31.2918H64.274V5.61806H72.3566V10.3751H72.4546C73.984 7.10938 76.1992 5.052 80.0419 5.052C81.0514 5.06957 82.0579 5.16786 83.0518 5.34592L82.6055 12.7155Z",
 "e": "M92.0161 21.0429C92.5113 24.5916 95.2219 25.9741 99.5598 25.9741C102.295 25.9656 104.985 25.2813 107.392 23.982V30.2413C104.385 31.3682 101.193 31.922 97.9814 31.8741C89.507 31.8741 83.0518 26.8994 83.0518 18.4195C83.0518 9.93959 88.8157 5.01929 96.8003 5.01929C106.309 5.01929 109.76 11.7684 109.76 19.4046V21.0375L92.0161 21.0429ZM101.775 16.0192C101.775 13.0583 100.398 10.6472 96.8493 10.6472C93.6435 10.6472 92.1195 13.0583 91.9181 16.0192H101.775Z",
}
CHEV = [(33.2339, 0.115234), (17.6185, 15.7307), (33.2339, 31.3515), (21.521, 31.3515), (5.90552, 15.7307), (21.521, 0.115234)]
TRI_TOP = [(0.0380859, 9.92319), (0.0380859, 0.115234), (9.76984, 0.115234)]
TRI_BOT = [(9.80796, 31.3135), (0, 31.3135), (0, 21.5818)]
END = [(112.666, 0.115234), (128.287, 15.7307), (112.666, 31.3515), (124.379, 31.3515), (140, 15.7307), (124.379, 0.115234)]

def kore():
    """K (violet), o r e (violet, o and e cut 3.5 clear of the chevrons), end chevron (lime),
    in tight-wordmark coordinates. Returns (violet, lime)."""
    sh = lambda pts, dx: [(x + dx, y) for x, y in pts]
    k_chev = sh(CHEV, 2.568); end = sh(END, -6.945)
    K = U(poly(k_chev), poly(sh(TRI_TOP, 2.568)), poly(sh(TRI_BOT, 2.568)))
    o = D(P(TIGHT["o"]), poly(offset_polygon(k_chev, CUT)))
    e = D(P(TIGHT["e"]), poly(offset_polygon(end, CUT)))
    return U(K, o, P(TIGHT["r"]), e), poly(end)

def k_only():
    sh = lambda pts, dx: [(x + dx, y) for x, y in pts]
    return U(poly(CHEV), poly(TRI_TOP), poly(TRI_BOT))

# ---- TUNE: capitals for the tuneKore logo, cap height and weight matched to the K
CAP = 31.236      # the K's height
STEM = 8.9        # vertical strokes, as in the lowercase
BAR = 7.0         # horizontal strokes

def cap_T(w=27.0):
    x0 = (w - STEM) / 2
    return U(rect(0, 0, w, BAR), rect(x0, 0, x0 + STEM, CAP))

def cap_U(w=27.4, ym=13.0):
    a = w / 2; yb = CAP + 0.55; h = yb - ym           # bowl bottom overshoots like the o
    ai = a - STEM; yi = yb - 6.4; hi = yi - ym
    d = (f"M0 0 H{STEM} V{ym} "
         f"C{STEM} {ym + 0.5304*hi} {a - 0.709*ai} {yi} {a} {yi} "
         f"C{a + 0.709*ai} {yi} {w - STEM} {ym + 0.5304*hi} {w - STEM} {ym} "
         f"V0 H{w} V{ym} "
         f"C{w} {ym + 0.6337*h} {w - 0.4323*a} {yb} {a} {yb} "
         f"C{0.4323*a} {yb} 0 {ym + 0.6337*h} 0 {ym} Z")
    return P(d)

def cap_N(w=28.4, diag=10.6):
    return U(rect(0, 0, STEM, CAP), rect(w - STEM, 0, w, CAP),
             poly([(0, 0), (diag, 0), (w, CAP), (w - diag, CAP)]))

def cap_E(w=23.0, mid=21.0):
    y1 = (CAP - BAR) / 2
    return U(rect(0, 0, STEM, CAP), rect(0, 0, w, BAR),
             rect(0, y1, mid, y1 + BAR), rect(0, CAP - BAR, w, CAP))
