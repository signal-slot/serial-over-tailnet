#!/usr/bin/env python3
"""Writes what JLCPCB takes for a board made and assembled, into fab/:

    fab/gerbers.zip     the layers and the drill files
    fab/bom.csv         Comment, Designator, Footprint, LCSC Part #
    fab/cpl.csv         Designator, Mid X, Mid Y, Layer, Rotation

from board.kicad_pcb (as gen_pcb.py and route.py left it) and design.py. It refuses a
board that does not pass KiCad's design rules check.
"""
import collections
import csv
import json
import math
import os
import re
import shutil
import subprocess
import sys
import zipfile

import pcbnew

# The board is the one in the directory given as the argument, this script's own by
# default.
SCRIPTS = os.path.dirname(os.path.abspath(__file__))
HERE = os.path.abspath(sys.argv[1]) if len(sys.argv) > 1 else SCRIPTS
sys.path.insert(0, HERE)
import design  # noqa: E402

BOARD = os.path.join(HERE, "board.kicad_pcb")
FAB = os.path.join(HERE, "fab")
OUTER = "F.Paste,B.Paste,F.SilkS,B.SilkS,F.Mask,B.Mask,Edge.Cuts"


def run(*args):
    result = subprocess.run(["kicad-cli", *args], capture_output=True, text=True)
    if result.returncode:
        raise SystemExit(f"kicad-cli {' '.join(args)}\n{result.stdout}{result.stderr}")
    return result.stdout


# JLCPCB places a part by its own footprint (jlc_align.py reads those footprints and
# writes jlc_transforms.json: for each part number, the rotation and the origin of
# JLCPCB's footprint in KiCad's footprint frame). The centroid file must therefore give
# JLCPCB's origin and JLCPCB's rotation, not KiCad's.
TRANSFORMS = json.load(open(os.path.join(SCRIPTS, "jlc_transforms.json")))
FOOTPRINTS = "/usr/share/kicad/footprints"


def rotate(x, y, degrees):
    """Counter-clockwise on the screen (y down), as KiCad's orientation is."""
    a = math.radians(degrees)
    return x * math.cos(a) + y * math.sin(a), -x * math.sin(a) + y * math.cos(a)


def mirror_x(x, y):
    return -x, y


def mirror_y(x, y):
    return x, -y


def library_pads(fp):
    lib, name = str(fp.GetFPID().GetLibNickname()), str(fp.GetFPID().GetLibItemName())
    where = HERE if lib == "local" else FOOTPRINTS
    original = pcbnew.FootprintLoad(f"{where}/{lib}.pretty", name)
    return [(p.GetNumber(), pcbnew.ToMM(p.GetPosition().x), pcbnew.ToMM(p.GetPosition().y))
            for p in original.Pads() if p.GetNumber()]


def placement(fp, code):
    """(x, y, rotation, check) for JLCPCB, and the largest distance between a pad of
    JLCPCB's footprint, placed as the file says, and the pad of KiCad's it must cover.

    KiCad's footprint sits at `pos` with orientation `a`, mirrored when on the bottom;
    the transform is read off the pads themselves (library pad to board pad), so no
    assumption about how KiCad flips is needed. JLCPCB's footprint is rotated by `theta`
    from KiCad's with its origin at `origin` (KiCad frame). On the top the rotation to
    write is a - theta. On the bottom the written angle is the part's rotation as seen
    from the bottom: JLCPCB rotates its footprint by it and then mirrors the result
    about the vertical axis (seen from the top). The angle is found here by trying the
    four right angles and keeping the one that puts JLCPCB's pads on KiCad's. The
    convention is what two placement previews showed (2026-11-09): parts whose JLCPCB
    footprint is turned 0° or 180° from KiCad's matched at 0°, and the three turned 90°
    (SOT-583, UQFN-10, SOT-25) were 180° out when the mirror was applied before the
    rotation instead of after it."""
    t = TRANSFORMS[code]
    theta, (ox, oy) = t["rotation"], t["origin"]
    pos = fp.GetPosition()
    px, py = pcbnew.ToMM(pos.x), pcbnew.ToMM(pos.y)
    a = fp.GetOrientationDegrees()
    bottom = fp.IsFlipped()
    # Pad numbers repeat (the module's ground pads, the switch's tabs): a transformed
    # library pad is compared with the nearest board pad of the same number.
    board = [(p.GetNumber(), pcbnew.ToMM(p.GetPosition().x), pcbnew.ToMM(p.GetPosition().y))
             for p in fp.Pads() if p.GetNumber()]
    local = library_pads(fp)

    def nearest(n, x, y):
        return min(math.hypot(x - bx, y - by) for m, bx, by in board if m == n)

    # KiCad's transform of local points, found from the pads: rotation, with a mirror
    # before or after it when on the bottom.
    def kicad_transform(mirror):
        def f(x, y):
            if mirror and mirror[1] == "before":
                x, y = mirror[0](x, y)
            x, y = rotate(x, y, a)
            if mirror and mirror[1] == "after":
                x, y = mirror[0](x, y)
            return px + x, py + y
        return f
    candidates = [(m, when) for m in (mirror_x, mirror_y) for when in ("before", "after")] if bottom else [None]
    best = None
    for mirror in candidates:
        f = kicad_transform(mirror)
        err = max(nearest(n, *f(x, y)) for n, x, y in local)
        if best is None or err < best[0]:
            best = (err, f)
    if best[0] > 0.01:
        raise SystemExit(f"{fp.GetReference()}: KiCad's footprint transform not understood ({best[0]:.3f} mm)")
    x, y = best[1](ox, oy)

    # JLCPCB's pads, placed as the file would say, on KiCad's pads: the largest miss.
    aliases = {"C2286": {"1": "2", "2": "1"}}.get(code, {})
    by_position = t["paired_by"] == "positions"       # the numbers mean nothing then

    def miss(rotation):
        worst = 0.0
        for number, jx, jy in t["jlc_pads"]:
            rx, ry = rotate(jx, jy, rotation)
            if bottom:
                rx, ry = mirror_x(rx, ry)
            gx, gy = x + rx, y + ry
            names = re.findall(r"[A-Z]+\d+|\d+|[A-Z]+", aliases.get(number, number)) or [number]
            targets = [] if by_position else [(bx, by) for n, bx, by in board if n in names]
            targets = targets or [(bx, by) for _, bx, by in board]
            worst = max(worst, min(math.hypot(gx - bx, gy - by) for bx, by in targets))
        return worst

    if bottom:
        rotation = min((0.0, 90.0, 180.0, 270.0), key=miss)
    else:
        rotation = (a - theta) % 360.0
    return x, y, rotation, miss(rotation)


def placements(board):
    """(reference, x, y, on the bottom, rotation, check) as JLCPCB reads them: y up, as
    in the Gerbers."""
    codes = {p[0]: p[4] for p in design.PARTS if p[4]}
    for fp in board.GetFootprints():
        ref = fp.GetReference()
        if ref not in codes:
            continue
        x, y, rotation, check = placement(fp, codes[ref])
        yield ref, x, -y, fp.IsFlipped(), rotation, check


def main():
    report = os.path.join(HERE, "drc.rpt")
    subprocess.run(["kicad-cli", "pcb", "drc", "--severity-error", "-o", report, BOARD], capture_output=True)
    text = open(report).read()
    if "Found 0 DRC violations" not in text or "Found 0 unconnected pads" not in text:
        raise SystemExit(f"the board does not pass the design rules check: see {report}")

    shutil.rmtree(FAB, ignore_errors=True)
    gerbers = os.path.join(FAB, "gerbers")
    os.makedirs(gerbers)
    copper = pcbnew.LoadBoard(BOARD).GetCopperLayerCount()
    layers = ",".join(["F.Cu"] + [f"In{i}.Cu" for i in range(1, copper - 1)] + ["B.Cu", OUTER])
    run("pcb", "export", "gerbers", "--layers", layers, "--no-protel-ext", "-o", gerbers + "/", BOARD)
    run("pcb", "export", "drill", "--format", "excellon", "--excellon-units", "mm",
        "--excellon-separate-th", "-o", gerbers + "/", BOARD)
    with zipfile.ZipFile(os.path.join(FAB, "gerbers.zip"), "w", zipfile.ZIP_DEFLATED) as z:
        for name in sorted(os.listdir(gerbers)):
            z.write(os.path.join(gerbers, name), name)

    # Parts JLCPCB places: everything with a part number (test pads have none).
    placed = {p[0]: p for p in design.PARTS if p[4]}
    groups = collections.OrderedDict()
    for ref, _, value, footprint, lcsc, _ in placed.values():
        # One line per part number; the first part's value names it.
        groups.setdefault(lcsc, (value, footprint.split(":")[1], []))[2].append(ref)
    with open(os.path.join(FAB, "bom.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["Comment", "Designator", "Footprint", "LCSC Part #"])
        for lcsc, (value, footprint, refs) in groups.items():
            w.writerow([value, ",".join(refs), footprint, lcsc])

    seen = set()
    board = pcbnew.LoadBoard(BOARD)
    checks = []
    with open(os.path.join(FAB, "cpl.csv"), "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["Designator", "Mid X", "Mid Y", "Layer", "Rotation"])
        for ref, x, y, bottom, rotation, check in placements(board):
            seen.add(ref)
            w.writerow([ref, f"{x:.4f}mm", f"{y:.4f}mm", "Bottom" if bottom else "Top", f"{rotation:.0f}"])
            checks.append((check, ref, bottom, rotation))
    missing = set(placed) - seen
    if missing:
        raise SystemExit(f"no position for {sorted(missing)}")
    checks.sort(reverse=True)
    with open(os.path.join(FAB, "placement-check.txt"), "w") as f:
        f.write("JLCPCB's footprint of each part, placed as cpl.csv says, against KiCad's pads:\n"
                "largest distance between a pad and the pad it must cover (mm).\n\n")
        for check, ref, bottom, rotation in checks:
            f.write(f"{ref:5} {'bottom' if bottom else 'top':6} rot {rotation:5.1f}  {check:.3f}\n")
    # JLCPCB's drawing of a part and the maker's differ by a few tenths at most (the
    # slide switch's tabs: 0.36 mm); anything larger is a wrong angle or origin.
    bad = [c for c in checks if c[0] > 0.5]
    if bad:
        raise SystemExit("JLCPCB's footprint does not land on KiCad's pads for "
                         + ", ".join(f"{ref} ({check:.2f} mm)" for check, ref, _, _ in bad))
    print(f"{FAB}: gerbers.zip ({len(os.listdir(gerbers))} files), bom.csv ({len(groups)} lines, "
          f"{len(placed)} parts), cpl.csv ({len(seen)} parts)")


if __name__ == "__main__":
    main()
