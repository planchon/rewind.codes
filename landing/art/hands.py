"""Generates ../hero-data.js, the two hands of the hero, from Michelangelo's Creation of Adam.

The fresco (public domain) is downloaded from Wikimedia Commons. Both arms are cut out with
GrabCut, seeded by hand-drawn polygons. God's hand becomes the human: a weighted Voronoi stipple
(Lloyd relaxation), denser where the fresco is lit. Adam's hand becomes the agent: a Delaunay
mesh over nodes placed on its contour, on its inner edges (fingers, muscles) and inside it, finer
towards the fingertip. Each node keeps its graph distance to the fingertip, for the pulse that
main.js sends through the mesh.

    python3 -m venv .venv && .venv/bin/pip install numpy scipy opencv-python-headless
    .venv/bin/python art/hands.py            # from landing/, writes hero-data.js

The output is deterministic (fixed seeds). The script is not deployed (see ../.vercelignore).
"""

import collections
import heapq
import json
import pathlib
import urllib.request

import cv2
import numpy as np
from scipy.spatial import Delaunay, cKDTree

HERE = pathlib.Path(__file__).resolve().parent
OUT = HERE.parent / "hero-data.js"
CACHE = HERE / ".cache"
FRESCO_URL = (
    "https://upload.wikimedia.org/wikipedia/commons/5/5b/"
    "Michelangelo_-_Creation_of_Adam_%28cropped%29.jpg"
)
CROP = (420, 520, 2060, 900)  # x0, y0, x1, y1 in the 3524x1599 fresco: both arms
SPARK = (914.0, 207.0)  # between the two index fingers, in crop coordinates
TIP = (905, 213)  # Adam's index fingertip: where the pulse starts

# Rough outlines (probably foreground) and strokes certainly on the arm, for GrabCut.
ARMS = {
    "adam": {
        "poly": [(130, 188), (220, 188), (300, 203), (400, 203), (480, 183), (560, 158), (640, 132),
                 (690, 110), (740, 113), (800, 126), (850, 143), (882, 163), (908, 185), (922, 205),
                 (920, 227), (902, 248), (880, 262), (850, 260), (828, 258), (806, 256), (796, 238),
                 (778, 226), (750, 218), (720, 218), (690, 228), (650, 244), (600, 268), (560, 292),
                 (520, 307), (470, 316), (400, 320), (300, 342), (200, 348), (130, 348)],
        "fg": [([(160, 270), (300, 275), (420, 262), (520, 232), (620, 196), (700, 160), (760, 158)], 16),
               ([(760, 158), (820, 178), (868, 198), (902, 212)], 6),
               ([(800, 185), (830, 236)], 6), ([(830, 190), (856, 236)], 6), ([(852, 195), (878, 238)], 6)],
    },
    "god": {
        "poly": [(910, 197), (940, 185), (990, 178), (1040, 158), (1100, 143), (1200, 120), (1300, 90),
                 (1400, 53), (1545, 25), (1560, 25), (1560, 142), (1500, 152), (1400, 182), (1300, 202),
                 (1200, 214), (1120, 242), (1092, 282), (1072, 302), (1040, 314), (1000, 314), (984, 292),
                 (984, 247), (960, 217), (935, 216), (910, 212)],
        "fg": [([(926, 204), (1000, 198)], 5),
               ([(1000, 198), (1060, 195), (1150, 182), (1250, 162), (1350, 132), (1450, 102), (1540, 82)], 14),
               ([(1012, 222), (1010, 288)], 6), ([(1042, 232), (1046, 286)], 6)],
    },
}


def fresco():
    CACHE.mkdir(exist_ok=True)
    path = CACHE / "creation-of-adam.jpg"
    if not path.exists():
        req = urllib.request.Request(FRESCO_URL, headers={"User-Agent": "rewind-landing-art/1.0"})
        path.write_bytes(urllib.request.urlopen(req).read())
    x0, y0, x1, y1 = CROP
    return cv2.imread(str(path))[y0:y1, x0:x1]


def segment(img, arm):
    h, w = img.shape[:2]
    mask = np.full((h, w), cv2.GC_BGD, np.uint8)
    cv2.fillPoly(mask, [np.array(arm["poly"], np.int32)], cv2.GC_PR_FGD)
    for pts, thickness in arm["fg"]:
        cv2.polylines(mask, [np.array(pts, np.int32)], False, cv2.GC_FGD, thickness)
    bgd, fgd = np.zeros((1, 65)), np.zeros((1, 65))
    cv2.grabCut(img, mask, None, bgd, fgd, 8, cv2.GC_INIT_WITH_MASK)
    m = np.where((mask == cv2.GC_FGD) | (mask == cv2.GC_PR_FGD), 255, 0).astype(np.uint8)
    m = cv2.morphologyEx(m, cv2.MORPH_OPEN, np.ones((3, 3), np.uint8))
    n, lab, stats, _ = cv2.connectedComponentsWithStats(m)
    if n > 1:
        m = np.where(lab == 1 + np.argmax(stats[1:, cv2.CC_STAT_AREA]), 255, 0).astype(np.uint8)
    n, lab, stats, _ = cv2.connectedComponentsWithStats(255 - m)
    for i in range(1, n):
        if stats[i, cv2.CC_STAT_AREA] < 400:
            m[lab == i] = 255
    return m > 0


def smooth(e0, e1, x):
    t = np.clip((np.asarray(x, np.float32) - e0) / (e1 - e0), 0, 1)
    return t * t * (3 - 2 * t)


def normalize_in(g, mask):
    lo, hi = np.percentile(g[mask], 2), np.percentile(g[mask], 99.5)
    return np.clip((g - lo) / (hi - lo), 0, 1)


def silhouette(mask):
    m = mask.astype(np.uint8)
    s = (m - cv2.erode(m, np.ones((3, 3), np.uint8))).astype(np.float32)
    s = cv2.GaussianBlur(s, (0, 0), 1.2)
    return s / (s.max() + 1e-6)


def lloyd(rng, mask, dens, n, iters):
    ys, xs = np.nonzero(mask & (dens > 0))
    w = dens[ys, xs]
    idx = rng.choice(len(xs), n, replace=False, p=w / w.sum())
    pts = np.stack([xs[idx], ys[idx]], 1).astype(np.float64) + rng.random((n, 2)) - 0.5
    pix = np.stack([xs, ys], 1).astype(np.float64)
    for _ in range(iters):
        _, lab = cKDTree(pts).query(pix, workers=-1)
        sw = np.bincount(lab, weights=w, minlength=n)
        sx = np.bincount(lab, weights=w * pix[:, 0], minlength=n)
        sy = np.bincount(lab, weights=w * pix[:, 1], minlength=n)
        ok = sw > 0
        pts[ok, 0], pts[ok, 1] = sx[ok] / sw[ok], sy[ok] / sw[ok]
    return pts


def human_stipple(rng, tone, mask):
    width = mask.shape[1]
    fade = 1 - smooth(1380, 1555, np.arange(width))[None, :]
    lit = normalize_in(tone, mask) ** 1.7
    dens = (0.05 + 0.95 * lit + 0.8 * silhouette(mask)) * fade
    dens[~mask] = 0
    pts = lloyd(rng, mask, dens, 9000, 45)
    out = []
    for x, y in pts:
        xi, yi = int(round(x)), int(round(y))
        if 1 - smooth(1380, 1555, x) <= 0.01:
            continue
        t = lit[yi, xi] if mask[yi, xi] else 0
        out += [round(x * 2), round(y * 2), round(float(t) * 15)]
    return out


def agent_mesh(rng, gray, tone, mask):
    h, w = mask.shape
    lit = normalize_in(tone, mask)
    fade = smooth(150, 420, np.arange(w))

    def spacing(x):  # px between nodes: fine at the hand, coarse up the arm
        return 4.6 + 7.5 * (1 - smooth(320, 760, x))

    cell, grid, nodes, kinds = 4.0, {}, [], []

    def free(p, mind):
        kx, ky, r = int(p[0] // cell), int(p[1] // cell), int(np.ceil(mind / cell))
        for i in range(kx - r, kx + r + 1):
            for j in range(ky - r, ky + r + 1):
                for q in grid.get((i, j), ()):
                    if (q[0] - p[0]) ** 2 + (q[1] - p[1]) ** 2 < mind * mind:
                        return False
        return True

    def add(p, kind):
        grid.setdefault((int(p[0] // cell), int(p[1] // cell)), []).append(p)
        nodes.append(p)
        kinds.append(kind)

    # 1. Contour.
    contours, _ = cv2.findContours(mask.astype(np.uint8), cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_NONE)
    contour = max(contours, key=len)[:, 0, :].astype(np.float64)
    contour = cv2.GaussianBlur(contour.reshape(-1, 1, 2), (1, 9), 0).reshape(-1, 2)
    last = None
    for p in contour:
        if last is None or np.hypot(*(p - last)) >= spacing(p[0]) * 0.85:
            if p[0] >= 135:
                add(p, 0)
            last = p
    # 2. Inner edges: finger separations, muscles.
    inner = cv2.erode(mask.astype(np.uint8), np.ones((5, 5), np.uint8)) > 0
    edges = cv2.Canny((np.clip(cv2.GaussianBlur(gray, (0, 0), 1.0), 0, 1) * 255).astype(np.uint8), 28, 70)
    ey, ex = np.nonzero((edges > 0) & inner)
    for i in rng.permutation(len(ex)):
        p = np.array([ex[i], ey[i]], float)
        if p[0] >= 150 and free(p, spacing(p[0]) * 1.05):
            add(p, 1)
    # 3. Fill, denser where lit.
    ys, xs = np.nonzero(mask)
    wts = (0.25 + lit[ys, xs]) * fade[xs] + 0.02
    for i in rng.choice(len(xs), 60000, p=wts / wts.sum()):
        p = np.array([xs[i] + rng.random() - 0.5, ys[i] + rng.random() - 0.5])
        if free(p, spacing(p[0]) * (1.75 - 0.55 * lit[ys[i], xs[i]])):
            add(p, 2)

    nodes, kinds = np.array(nodes), np.array(kinds)
    keep = fade[np.clip(nodes[:, 0].round().astype(int), 0, w - 1)] > 0.02
    nodes, kinds = nodes[keep], kinds[keep]

    def inside(p):
        xi, yi = int(round(p[0])), int(round(p[1]))
        return 0 <= xi < w and 0 <= yi < h and mask[yi, xi]

    links = set()
    for s in Delaunay(nodes).simplices:
        for i in range(3):
            a, b = sorted((int(s[i]), int(s[(i + 1) % 3])))
            if (a, b) in links:
                continue
            pa, pb = nodes[a], nodes[b]
            if np.hypot(*(pa - pb)) > 2.6 * spacing((pa[0] + pb[0]) / 2):
                continue
            if all(inside(pa + (pb - pa) * t) for t in (0.2, 0.4, 0.6, 0.8)):
                links.add((a, b))
    links = sorted(links)

    # Graph distance from the fingertip (Dijkstra), for the pulse.
    adj = collections.defaultdict(list)
    for a, b in links:
        d = float(np.hypot(*(nodes[a] - nodes[b])))
        adj[a].append((b, d))
        adj[b].append((a, d))
    tip = int(np.argmin(np.hypot(nodes[:, 0] - TIP[0], nodes[:, 1] - TIP[1])))
    dist = np.full(len(nodes), np.inf)
    dist[tip] = 0
    heap = [(0.0, tip)]
    while heap:
        d0, u = heapq.heappop(heap)
        if d0 > dist[u]:
            continue
        for v, d in adj[u]:
            if d0 + d < dist[v]:
                dist[v] = d0 + d
                heapq.heappush(heap, (dist[v], v))
    dist[~np.isfinite(dist)] = dist[np.isfinite(dist)].max()

    out = []
    for (x, y), k, d in zip(nodes, kinds, dist):
        xi, yi = min(max(int(round(x)), 0), w - 1), min(max(int(round(y)), 0), h - 1)
        t = float(lit[yi, xi]) if mask[yi, xi] else 0.5
        out += [round(x * 2), round(y * 2), round(t * 15), int(k), round(float(d))]
    return out, [v for a, b in links for v in (a, b)]


def main():
    img = fresco()
    h, w = img.shape[:2]
    gray = cv2.cvtColor(img, cv2.COLOR_BGR2LAB)[..., 0].astype(np.float32) / 255
    gray = cv2.morphologyEx(gray, cv2.MORPH_CLOSE, np.ones((3, 3), np.uint8))  # soften the cracks
    tone = cv2.GaussianBlur(gray, (0, 0), 2.2)
    rng = np.random.default_rng(11)
    human = human_stipple(rng, tone, segment(img, ARMS["god"]))
    nodes, links = agent_mesh(rng, gray, tone, segment(img, ARMS["adam"]))
    data = {"w": w, "h": h, "spark": SPARK, "H": human, "N": nodes, "E": links}
    OUT.write_text(
        "// Generated by art/hands.py from Michelangelo, The Creation of Adam (c. 1512, public\n"
        "// domain). H = the human hand, stippled: x2, y2, tone (0-15), in half pixels of a 1640x380\n"
        "// frame. N = the agent's hand, a mesh: x2, y2, tone, kind (0 contour, 1 edge, 2 fill), graph\n"
        "// distance to the fingertip. E = mesh edges, pairs of node indexes.\n"
        "window.REWIND_HERO=" + json.dumps(data, separators=(",", ":")) + ";\n"
    )
    print(f"{OUT.name}: {len(human) // 3} dots, {len(nodes) // 5} nodes, {len(links) // 2} edges")


if __name__ == "__main__":
    main()
