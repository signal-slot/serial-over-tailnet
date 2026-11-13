#!/usr/bin/env python3
"""Routes board.kicad_pcb with Freerouting and writes the result back.

    python3 gen_pcb.py                      # footprints, rules
    python3 route.py export work/board.dsn
    java -Djava.awt.headless=true -jar freerouting.jar --gui.enabled=false -de work/board.dsn -do routing.ses -mp 20 -mt 1
    python3 route.py import routing.ses     # board.kicad_pcb again, with tracks, vias, fills

routing.ses is kept: the router's result is not reproducible, so the board is rebuilt
from the session file, not by routing again. The session file is read here, not through
KiCad's own importer, which wants its GUI.
"""
import os
import sys

import pcbnew

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "board"))    # sexpr.py
sys.path.insert(0, HERE)
import gen_pcb  # noqa: E402
from sexpr import find, first, parse  # noqa: E402

BOARD = gen_pcb.BOARD
MIN_WIDTH = pcbnew.FromMM(0.15)
# Ground fills keep this far from other copper (KiCad's default, 0.5 mm, leaves little
# of a fill on a board this small).
FILL_CLEARANCE = 0.25


def export(dsn):
    board = pcbnew.LoadBoard(BOARD)
    if not pcbnew.ExportSpecctraDSN(board, dsn):
        raise SystemExit("KiCad could not write the DSN file")
    print(dsn)


def import_session(ses):
    session = parse(open(ses).read())
    routes = first(session, "routes")
    resolution = first(routes, "resolution")           # (resolution um 10): 10 units per um
    unit, per = str(resolution[1]), float(resolution[2])
    to_nm = {"um": 1000.0, "mil": 25400.0, "mm": 1e6, "inch": 25.4e6}[unit] / per

    # From a fresh board: removing the tracks of a routed one through the Python API
    # leaves the bindings broken.
    gen_pcb.main()
    board = pcbnew.LoadBoard(BOARD)
    layers = {board.GetLayerName(i): i for i in board.GetEnabledLayers().CuStack()}
    vias = {}
    for padstack in find(first(routes, "library_out") or [], "padstack"):
        name = padstack[1]                                # Via[0-1]_600:300_um
        size = name.split("_")[1].split(":")
        vias[name] = (float(size[0]) * 1000, float(size[1]) * 1000)

    def point(x, y):
        # Specctra's y points up, KiCad's down.
        return pcbnew.VECTOR2I(int(round(float(x) * to_nm)), int(round(-float(y) * to_nm)))

    tracks = n_vias = 0
    for net in find(first(routes, "network_out"), "net"):
        info = board.FindNet(net[1])
        if info is None:
            raise SystemExit(f"the session names a net the board lacks: {net[1]}")
        for wire in find(net, "wire"):
            path = first(wire, "path")
            layer, width, coords = path[1], float(path[2]), path[3:]
            pts = [point(coords[i], coords[i + 1]) for i in range(0, len(coords), 2)]
            for a, b in zip(pts, pts[1:]):
                if a == b:
                    continue
                t = pcbnew.PCB_TRACK(board)
                t.SetStart(a)
                t.SetEnd(b)
                # The router necks a track down at a pad; the board's rules (and
                # JLCPCB) want 0.15 mm and up. KiCad's check then says whether the
                # wider track still clears its neighbours.
                t.SetWidth(max(int(round(width * to_nm)), MIN_WIDTH))
                t.SetLayer(layers[layer])
                t.SetNet(info)
                board.Add(t)
                tracks += 1
        for via in find(net, "via"):
            diameter, drill = vias[via[1]]
            where = point(via[2], via[3])
            if any(t.Type() == pcbnew.PCB_VIA_T and t.GetPosition() == where for t in board.GetTracks()):
                continue
            v = pcbnew.PCB_VIA(board)
            v.SetPosition(where)
            v.SetViaType(pcbnew.VIATYPE_THROUGH)
            v.SetWidth(pcbnew.F_Cu, int(diameter))
            v.SetDrill(int(drill))
            v.SetLayerPair(pcbnew.F_Cu, pcbnew.B_Cu)
            v.SetNet(info)
            board.Add(v)
            n_vias += 1

    # Ground on the bottom, around what the router made. The top has none: most of it
    # is under the XIAO, where gen_pcb.py keeps copper out.
    ground = board.FindNet("GND")
    box = board.GetBoardEdgesBoundingBox()
    for layer in (pcbnew.B_Cu,):
        zone = pcbnew.ZONE(board)
        zone.SetLayer(layer)
        zone.SetNet(ground)
        zone.SetZoneName("GND")
        # Thermal reliefs: a small part with one end on solid copper tombstones, and a
        # hand-soldered pad on solid copper is hard to heat.
        zone.SetPadConnection(pcbnew.ZONE_CONNECTION_THERMAL)
        zone.SetMinThickness(pcbnew.FromMM(0.2))
        zone.SetLocalClearance(pcbnew.FromMM(FILL_CLEARANCE))
        zone.SetIslandRemovalMode(pcbnew.ISLAND_REMOVAL_MODE_ALWAYS)
        outline = zone.Outline()
        outline.NewOutline()
        for x, y in ((box.GetLeft(), box.GetTop()), (box.GetRight(), box.GetTop()),
                     (box.GetRight(), box.GetBottom()), (box.GetLeft(), box.GetBottom())):
            outline.Append(x, y)
        board.Add(zone)
    pcbnew.ZONE_FILLER(board).Fill(board.Zones())
    gen_pcb.save(board, BOARD)
    print(f"{BOARD}: {tracks} track segments, {n_vias} vias")


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] not in ("export", "import"):
        raise SystemExit(__doc__)
    (export if sys.argv[1] == "export" else import_session)(sys.argv[2])
