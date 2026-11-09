#!/usr/bin/env python3
"""Where JLCPCB's footprint of each part sits relative to KiCad's.

JLCPCB places a part by its own footprint (EasyEDA's library): the centroid file's
position is that footprint's origin and the rotation is that footprint's. KiCad's
footprint for the same part has its origin and zero rotation elsewhere. This reads
JLCPCB's footprint for every part number in design.py (EasyEDA's public API, cached in
work/jlc/), matches its pads to KiCad's by pad number, and writes jlc_transforms.json:
for each part number the rotation (degrees, counter-clockwise on the screen) and the
origin (KiCad-local mm) that put JLCPCB's pads onto KiCad's, with the largest pad
position error left. fab.py reads that file.

    python3 jlc_align.py        # needs the network once; prints a table
"""
import json
import math
import os
import re
import subprocess
import sys

import pcbnew

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import design  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, "work", "jlc")
FOOTPRINTS = "/usr/share/kicad/footprints"
UNIT = 0.254                    # EasyEDA's unit is 10 mil
API = "https://easyeda.com/api/products/{}/components?version=6.4.19.5"


def jlc_pads(code):
    """[(number, x, y)] in mm from JLCPCB's footprint origin, y down (like KiCad)."""
    os.makedirs(CACHE, exist_ok=True)
    path = os.path.join(CACHE, f"{code}.json")
    if not os.path.exists(path):
        subprocess.run(["curl", "-s", "-m", "30", "-o", path, API.format(code)], check=True)
    data = json.load(open(path))["result"]
    package = data["packageDetail"]["dataStr"]
    ox, oy = package["head"]["x"], package["head"]["y"]
    pads = []
    for shape in package["shape"]:
        if not shape.startswith("PAD~"):
            continue
        f = shape.split("~")
        pads.append((f[8], (float(f[2]) - ox) * UNIT, (float(f[3]) - oy) * UNIT))
    return package["head"]["c_para"].get("package", ""), pads


def kicad_pads(footprint):
    lib, name = footprint.split(":")
    where = HERE if lib == "local" else FOOTPRINTS
    fp = pcbnew.FootprintLoad(f"{where}/{lib}.pretty", name)
    return [(p.GetNumber(), pcbnew.ToMM(p.GetPosition().x), pcbnew.ToMM(p.GetPosition().y))
            for p in fp.Pads() if p.GetNumber()]


def rotate(x, y, degrees):
    """Counter-clockwise on the screen (y down)."""
    a = math.radians(degrees)
    return x * math.cos(a) + y * math.sin(a), -x * math.sin(a) + y * math.cos(a)


# Where JLCPCB's pad numbers mean something else than KiCad's. The LED: KiCad's pad 1 is
# the cathode; in JLCPCB's library the symbol of C2286 draws the cathode bar at its pin
# 2 (its pin 1 is the anode), so JLCPCB's pad 2 must land on KiCad's pad 1.
PAD_ALIASES = {"C2286": {"1": "2", "2": "1"}}


def names(number, code=""):
    """JLCPCB writes a USB-C pad shared by two pins as 'A1B12'."""
    number = PAD_ALIASES.get(code, {}).get(number, number)
    parts = re.findall(r"[A-Z]+\d+|\d+|[A-Z]+", number)
    return parts if len(parts) > 1 else [number]


def align(kicad, jlc, code=""):
    """(rotation, origin, error, pads, how): the rotation and the JLCPCB origin in
    KiCad's frame that put JLCPCB's pads on KiCad's, the largest distance left between
    a pad and its partner, and how the pads were paired: by number, or by position
    when the two libraries number them differently (then the pattern must be the same
    whichever way round, as it is for a square push button)."""
    k_by = {}
    for n, x, y in kicad:
        k_by.setdefault(n, []).append((x, y))
    pairs = []                   # (kicad pad, jlc pad) by a number that is unique on both
    for n, x, y in jlc:
        for name in names(n, code):
            if name in k_by and len(k_by[name]) == 1 and sum(1 for m, _, _ in jlc if name in names(m, code)) == 1:
                pairs.append((k_by[name][0], (x, y)))
    how = "numbers"
    if len(pairs) < 2:
        if len(kicad) != len(jlc):
            return None
        how = "positions"
    best = None
    for degrees in (0, 90, 180, 270):
        if how == "numbers":
            rk = [rotate(x, y, degrees) for (x, y), _ in pairs]
            js = [j for _, j in pairs]
        else:
            # Centroids together, then each KiCad pad to the nearest JLCPCB pad.
            kc = (sum(x for _, x, _ in kicad) / len(kicad), sum(y for _, _, y in kicad) / len(kicad))
            jc = (sum(x for _, x, _ in jlc) / len(jlc), sum(y for _, _, y in jlc) / len(jlc))
            rk = [rotate(x - kc[0], y - kc[1], degrees) for _, x, y in kicad]
            js = [min(((x - jc[0], y - jc[1]) for _, x, y in jlc), key=lambda j: math.hypot(j[0] - rx, j[1] - ry))
                  for rx, ry in rk]
            rk = [(x + rotate(*kc, degrees)[0], y + rotate(*kc, degrees)[1]) for x, y in rk]
            js = [(x + jc[0], y + jc[1]) for x, y in js]
        tx = sum(jx - x for (x, _), (jx, _) in zip(rk, js)) / len(rk)
        ty = sum(jy - y for (_, y), (_, jy) in zip(rk, js)) / len(rk)
        err = max(math.hypot(x + tx - jx, y + ty - jy) for (x, y), (jx, jy) in zip(rk, js))
        if best is None or err < best[2]:
            # JLCPCB's origin (0, 0) in KiCad's frame: rotate(p, d) + t = 0.
            ox, oy = rotate(-tx, -ty, -degrees)
            best = (degrees, (round(ox, 3), round(oy, 3)), round(err, 3), len(rk), how)
    return best


def main():
    table = {}
    seen = set()
    for ref, _, value, footprint, code, _ in design.PARTS:
        if not code or code in seen:
            continue
        seen.add(code)
        package, jlc = jlc_pads(code)
        result = align(kicad_pads(footprint), jlc, code)
        if result is None:
            print(f"{code:10} {value:22} {package[:36]:36} cannot be matched")
            continue
        degrees, origin, err, n, how = result
        table[code] = {"rotation": degrees, "origin": list(origin), "error": err, "pads": n, "paired_by": how,
                       "package": package, "kicad": footprint.split(":")[1],
                       "jlc_pads": [[num, round(x, 3), round(y, 3)] for num, x, y in jlc]}
        flag = "" if err < 0.5 else "   <-- does not fit"
        print(f"{code:10} {value:22} {package[:36]:36} rot {degrees:3} origin ({origin[0]:+.2f}, {origin[1]:+.2f}) "
              f"err {err:.3f} over {n} pads by {how}{flag}")
    out = os.path.join(HERE, "jlc_transforms.json")
    json.dump(table, open(out, "w"), indent=1, sort_keys=True)
    print(out)


if __name__ == "__main__":
    main()
