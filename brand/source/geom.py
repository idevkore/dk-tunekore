"""Path helpers for building the musiKore marks: SVG path strings in and out,
boolean ops, transforms, and a text preview so shapes can be checked without images."""
import math
import pathops
from fontTools.svgLib.path import parse_path
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.freetypePen import FreeTypePen
from fontTools.pens.boundsPen import BoundsPen

def P(d, t=(1, 0, 0, 1, 0, 0)):
    """Path from an SVG d string, optionally transformed (affine 6-tuple)."""
    p = pathops.Path()
    parse_path(d, TransformPen(p.getPen(), t))
    return p

def poly(pts):
    p = pathops.Path(); pen = p.getPen()
    pen.moveTo(pts[0])
    for q in pts[1:]: pen.lineTo(q)
    pen.closePath(); return p

def rect(x0, y0, x1, y1):
    return poly([(x0, y0), (x1, y0), (x1, y1), (x0, y1)])

def xf(path, t):
    out = pathops.Path(); path.draw(TransformPen(out.getPen(), t)); return out

def move(path, dx, dy=0.0): return xf(path, (1, 0, 0, 1, dx, dy))

def U(*paths):
    acc = pathops.Path()
    for p in paths: acc = pathops.op(acc, p, pathops.PathOp.UNION)
    return acc

def D(a, b): return pathops.op(a, b, pathops.PathOp.DIFFERENCE)

def bounds(path):
    bp = BoundsPen(None); path.draw(bp); return bp.bounds

def d_of(path, nd=3):
    pen = SVGPathPen(None, ntos=lambda v: (f"{v:.{nd}f}").rstrip("0").rstrip(".") if "." in f"{v:.{nd}f}" else f"{v:.{nd}f}")
    path.draw(pen); return pen.getCommands()

def offset_polygon(pts, r):
    """Offset a simple polygon outward by r with miter joins (same as an SVG stroke
    of width 2r unioned with the fill, when miters stay under the miter limit)."""
    n = len(pts)
    area = sum(pts[i][0]*pts[(i+1)%n][1] - pts[(i+1)%n][0]*pts[i][1] for i in range(n)) / 2
    sgn = 1 if area > 0 else -1   # y-down coords: positive area = clockwise on screen
    lines = []
    for i in range(n):
        (x0, y0), (x1, y1) = pts[i], pts[(i+1) % n]
        dx, dy = x1-x0, y1-y0; L = math.hypot(dx, dy)
        nx, ny = (dy/L, -dx/L) if sgn > 0 else (-dy/L, dx/L)   # outward normal
        lines.append(((x0+nx*r, y0+ny*r), (x1+nx*r, y1+ny*r)))
    out = []
    for i in range(n):
        (a, b), (c, d) = lines[i-1], lines[i]
        x1, y1 = a; x2, y2 = b; x3, y3 = c; x4, y4 = d
        den = (x1-x2)*(y3-y4) - (y1-y2)*(x3-x4)
        px = ((x1*y2-y1*x2)*(x3-x4) - (x1-x2)*(x3*y4-y3*x4)) / den
        py = ((x1*y2-y1*x2)*(y3-y4) - (y1-y2)*(x3*y4-y3*x4)) / den
        out.append((px, py))
    return out

def ascii(path, step=0.6, xr=None, yr=None):
    """Text rendering of a path (y down). '#' = filled."""
    b = bounds(path)
    x0, x1 = xr or (b[0]-1, b[2]+1); y0, y1 = yr or (b[1]-1, b[3]+1)
    W = int((x1-x0)/step); H = int((y1-y0)/(step*2))
    pen = FreeTypePen(None); path.draw(pen)
    # FreeType pen is y-up; flip so rows read top-down in our y-down space.
    a = pen.array(width=W, height=H, transform=(1/step, 0, 0, -1/(2*step), -x0/step, y1/(2*step)))
    rows = []
    for r in a[::-1] if False else a:
        rows.append("".join("#" if v > 0.5 else ("+" if v > 0.15 else " ") for v in r))
    return "\n".join(rows)

def smooth(path, spacing=1.4, corner_deg=28):
    """Turn a dense polygon into cubic curves: keep sharp corners, resample the runs
    between them evenly and fit Catmull-Rom splines through the samples."""
    from fontTools.pens.recordingPen import RecordingPen
    rec = RecordingPen(); path.draw(rec)
    contours, cur = [], []
    for op, args in rec.value:
        if op == "moveTo": cur = [args[0]]
        elif op == "lineTo": cur.append(args[0])
        elif op in ("closePath", "endPath"):
            if len(cur) > 2: contours.append(cur)
            cur = []
        else: raise ValueError("expected a polygon, got " + op)
    out = pathops.Path(); pen = out.getPen()
    for pts in contours:
        if math.dist(pts[0], pts[-1]) < 1e-6: pts = pts[:-1]
        n = len(pts)
        def turn(i):
            a, b, c = pts[i-1], pts[i], pts[(i+1) % n]
            a1 = math.atan2(b[1]-a[1], b[0]-a[0]); a2 = math.atan2(c[1]-b[1], c[0]-b[0])
            return abs((math.degrees(a2-a1) + 180) % 360 - 180)
        corners = [i for i in range(n) if turn(i) > corner_deg] or [0]
        pen.moveTo(pts[corners[0]])
        for ci, c0 in enumerate(corners):
            c1 = corners[(ci+1) % len(corners)]
            run = [pts[c0]]; j = c0
            while True:
                j = (j + 1) % n; run.append(pts[j])
                if j == c1: break
            L = [0.0]
            for k in range(1, len(run)): L.append(L[-1] + math.dist(run[k-1], run[k]))
            if L[-1] < 1e-9: continue
            N = max(1, round(L[-1] / spacing))
            if N == 1 or len(run) == 2:
                pen.lineTo(run[-1]); continue
            q, k = [], 0
            for s_ in [L[-1]*t/N for t in range(N+1)]:
                while k < len(L)-2 and L[k+1] < s_: k += 1
                seg = L[k+1]-L[k]; f = 0 if seg == 0 else (s_-L[k])/seg
                q.append((run[k][0] + (run[k+1][0]-run[k][0])*f, run[k][1] + (run[k+1][1]-run[k][1])*f))
            q = [q[0]] + q + [q[-1]]
            for t in range(1, len(q)-2):
                p0, p1, p2, p3 = q[t-1], q[t], q[t+1], q[t+2]
                pen.curveTo((p1[0]+(p2[0]-p0[0])/6, p1[1]+(p2[1]-p0[1])/6),
                            (p2[0]-(p3[0]-p1[0])/6, p2[1]-(p3[1]-p1[1])/6), p2)
        pen.closePath()
    return out

def iou(a, b, step=0.05):
    pa = FreeTypePen(None); a.draw(pa); pb = FreeTypePen(None); b.draw(pb)
    bb = bounds(U(a, b)); W = int((bb[2]-bb[0])/step)+4; H = int((bb[3]-bb[1])/step)+4
    t = (1/step, 0, 0, 1/step, -bb[0]/step+2, -bb[1]/step+2)
    A = pa.array(width=W, height=H, transform=t) > .5; Bm = pb.array(width=W, height=H, transform=t) > .5
    return (A & Bm).sum() / (A | Bm).sum()
