#!/usr/bin/env python3
"""Routes board.kicad_pcb with Freerouting and writes the result back.

    python3 gen_pcb.py                      # footprints, rules, planes
    python3 route.py export work/board.dsn
    java -jar freerouting.jar -de work/board.dsn -do routing.ses -mp 60 -mt 1
    python3 route.py import routing.ses     # board.kicad_pcb again, with tracks, vias, fills

routing.ses is kept: the router's result is not reproducible (it depends on timing), so
the board is rebuilt from the session file, not by routing again. The session file is
read here, not through KiCad's own importer, which wants its GUI.
"""
import json
import os
import subprocess
import sys

import pcbnew

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gen_pcb  # noqa: E402
from gen_pcb import ANTENNA, COPPER_FROM, POWER_WIDTH, THIN_SUPPLY  # noqa: E402
from sexpr import find, first, parse  # noqa: E402

MIN_WIDTH = pcbnew.FromMM(0.09)
HERE = os.path.dirname(os.path.abspath(__file__))
BOARD = os.path.join(HERE, "board.kicad_pcb")


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
    # leaves the bindings broken (every removed track leaks and later calls fail).
    gen_pcb.main()
    board = pcbnew.LoadBoard(BOARD)
    layers = {board.GetLayerName(i): i for i in board.GetEnabledLayers().CuStack()}
    vias = {}
    for padstack in find(first(routes, "library_out") or [], "padstack"):
        name = padstack[1]                                # Via[0-3]_600:300_um
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
                # The router necks a track down to as little as 0.075 mm at a pad;
                # JLCPCB makes 0.09 mm and up. KiCad's check then says whether the
                # wider track still clears its neighbours.
                t.SetWidth(max(int(round(width * to_nm)), MIN_WIDTH))
                t.SetLayer(layers[layer])
                t.SetNet(info)
                board.Add(t)
                tracks += 1
        for via in find(net, "via"):
            diameter, drill = vias[via[1]]
            where = point(via[2], via[3])
            # gen_pcb.py's own vias (in the module's thermal pads) come back in the
            # session too; one is enough.
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

    wide, total = widen(board, THIN_SUPPLY, POWER_WIDTH)

    # Ground on every layer, around what the router made (gen_pcb.py made the one on In1).
    ground = board.FindNet("GND")
    box = board.GetBoardEdgesBoundingBox()
    have = {z.GetLayer() for z in board.Zones() if not z.GetIsRuleArea()}
    for layer in (pcbnew.F_Cu, pcbnew.In2_Cu, pcbnew.B_Cu):
        if layer in have:
            continue
        zone = pcbnew.ZONE(board)
        zone.SetLayer(layer)
        zone.SetNet(ground)
        zone.SetZoneName("GND")
        zone.SetPadConnection(pcbnew.ZONE_CONNECTION_THERMAL)
        zone.SetMinThickness(pcbnew.FromMM(0.15))
        zone.SetIslandRemovalMode(pcbnew.ISLAND_REMOVAL_MODE_ALWAYS)
        outline = zone.Outline()
        outline.NewOutline()
        # Not under the antenna: that stays bare on every layer.
        top = box.GetTop() + pcbnew.FromMM(ANTENNA + COPPER_FROM)
        for x, y in ((box.GetLeft(), top), (box.GetRight(), top),
                     (box.GetRight(), box.GetBottom()), (box.GetLeft(), box.GetBottom())):
            outline.Append(x, y)
        board.Add(zone)
    pcbnew.ZONE_FILLER(board).Fill(board.Zones())
    stitched = stitch(board)
    pcbnew.ZONE_FILLER(board).Fill(board.Zones())
    gen_pcb.save(board, BOARD)
    print(f"{BOARD}: {tracks} track segments, {n_vias} vias, {stitched} ground stitching vias, "
          f"{THIN_SUPPLY} is {POWER_WIDTH} mm wide over {wide:.1f} of its {total:.1f} mm")


def widen(board, net, width):
    """Makes the tracks of `net` `width` mm wide wherever that breaks no rule.

    Every track of the net is widened, then the ones KiCad's check names in a violation go
    back to what the router made, until the check has nothing to say about them. Returns
    the length that is wide now and the length of all of the net's tracks, in mm.
    """
    code = board.FindNet(net).GetNetCode()
    target = pcbnew.FromMM(width)
    tracks = {t.m_Uuid.AsString(): t for t in board.GetTracks()
              if t.Type() != pcbnew.PCB_VIA_T and t.GetNetCode() == code}
    routed = {uuid: t.GetWidth() for uuid, t in tracks.items()}
    wide = {uuid for uuid, t in tracks.items() if t.GetWidth() < target}
    for uuid in wide:
        tracks[uuid].SetWidth(target)
    report = os.path.join(HERE, "work", "widen.json")
    os.makedirs(os.path.dirname(report), exist_ok=True)
    while wide:
        gen_pcb.save(board, BOARD)
        subprocess.run(["kicad-cli", "pcb", "drc", "--format", "json", "--severity-error", "-o", report, BOARD],
                       capture_output=True)
        named = {item["uuid"] for v in json.load(open(report))["violations"] for item in v["items"]}
        back = named & wide
        if not back:
            break
        for uuid in back:
            tracks[uuid].SetWidth(routed[uuid])
        wide -= back
    length = {uuid: pcbnew.ToMM(t.GetLength()) for uuid, t in tracks.items()}
    return (sum(length[u] for u, t in tracks.items() if t.GetWidth() >= target), sum(length.values()))


# A stitching via is 0.5 mm across. It goes only where all four layers are ground fill
# around it: the fills have already kept their distance from other nets, so the via only
# has to be inside them.
STITCH_PITCH = 1.0      # mm, the grid the vias are tried on
STITCH_IN_FILL = 0.3    # mm around the via's centre that must be ground fill
STITCH_PAD = 0.4        # mm from a pad's outline to the via's centre (no via in a pad)
STITCH_VIA = 0.9        # mm between the centres of two vias


def stitch(board):
    """Ties the ground fills of the four layers together with vias.

    The router puts signals on the inner layers too, which cuts the ground fills into
    pieces; the vias join the pieces through the other layers. A piece too small to hold
    a via stays as it was: KiCad's check says whether anything is left unconnected.
    """
    ground = board.FindNet("GND")
    code = ground.GetNetCode()
    layers = [pcbnew.F_Cu, pcbnew.In1_Cu, pcbnew.In2_Cu, pcbnew.B_Cu]
    zones = {layer: [] for layer in layers}
    for zone in board.Zones():
        if not zone.GetIsRuleArea() and zone.GetNetCode() == code:
            zones[zone.GetLayer()].append(zone)
    reach = pcbnew.FromMM(STITCH_IN_FILL)
    ring = [(0, 0)] + [(int(reach * dx), int(reach * dy)) for dx, dy in
                       ((1, 0), (-1, 0), (0, 1), (0, -1), (.7, .7), (.7, -.7), (-.7, .7), (-.7, -.7))]

    def filled(layer, x, y):
        return all(any(z.HitTestFilledArea(layer, pcbnew.VECTOR2I(x + dx, y + dy), 0) for z in zones[layer])
                   for dx, dy in ring)

    # What a via must stay away from: (x, y, distance between centres)
    keep = []
    for fp in board.GetFootprints():
        for pad in fp.Pads():
            size = pad.GetSize(pcbnew.F_Cu) if pad.GetAttribute() != pcbnew.PAD_ATTRIB_NPTH else pad.GetDrillSize()
            keep.append((pad.GetPosition().x, pad.GetPosition().y,
                         max(size.x, size.y) // 2 + pcbnew.FromMM(STITCH_PAD)))
    for track in board.GetTracks():
        if track.Type() == pcbnew.PCB_VIA_T:
            keep.append((track.GetPosition().x, track.GetPosition().y, pcbnew.FromMM(STITCH_VIA)))

    box = board.GetBoardEdgesBoundingBox()
    pitch = pcbnew.FromMM(STITCH_PITCH)
    added = 0
    y = box.GetTop() + pcbnew.FromMM(ANTENNA + COPPER_FROM) + pitch // 2
    while y < box.GetBottom():
        x = box.GetLeft() + pitch // 2
        while x < box.GetRight():
            if (all((x - kx) ** 2 + (y - ky) ** 2 >= d * d for kx, ky, d in keep)
                    and all(filled(layer, x, y) for layer in layers)):
                via = pcbnew.PCB_VIA(board)
                via.SetPosition(pcbnew.VECTOR2I(x, y))
                via.SetViaType(pcbnew.VIATYPE_THROUGH)
                via.SetWidth(pcbnew.F_Cu, pcbnew.FromMM(0.5))
                via.SetDrill(pcbnew.FromMM(0.3))
                via.SetLayerPair(pcbnew.F_Cu, pcbnew.B_Cu)
                via.SetNet(ground)
                board.Add(via)
                keep.append((x, y, pcbnew.FromMM(STITCH_VIA)))
                added += 1
            x += pitch
        y += pitch
    return added


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] not in ("export", "import"):
        raise SystemExit(__doc__)
    (export if sys.argv[1] == "export" else import_session)(sys.argv[2])
