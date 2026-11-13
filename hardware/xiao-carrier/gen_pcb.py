#!/usr/bin/env python3
"""Writes board.kicad_pcb from design.py and layout.py: the outline, every footprint in
its place with its nets, the labels. Tracks and ground fills are made by route.py.
"""
import json
import os
import sys

import pcbnew

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import design  # noqa: E402
import layout  # noqa: E402
from layout import LENGTH, WIDTH  # noqa: E402

FOOTPRINTS = "/usr/share/kicad/footprints"
# Silkscreen JLCPCB prints: lines 0.15 mm and up, text 1 mm and up.
SILK_WIDTH = 0.15
SILK_TEXT = 1.0
OX, OY = 100.0, 100.0          # where the board sits on KiCad's sheet
BOARD = os.path.join(HERE, "board.kicad_pcb")
# The XIAO covers the top down to y = 21; its lands end 3.25 mm from the board's edges.
XIAO_INSIDE, XIAO_END = 3.5, 21.3
# Supply tracks are wider than signals (a few mA at most flow here).
SUPPLY_NETS = ["+3V3", "VREF"]
SUPPLY_WIDTH = 0.3


def mm(v):
    return pcbnew.FromMM(v)


def at(x, y):
    return pcbnew.VECTOR2I(mm(OX + x), mm(OY + y))


def save(board, path):
    """Writes the board and its project file. The footprints are edited after loading
    (silkscreen off the board is dropped), so the check's note that they differ from
    the library is switched off."""
    pcbnew.SaveBoard(path, board)
    project = path[:-len(".kicad_pcb")] + ".kicad_pro"
    settings = json.load(open(project))
    settings["board"]["design_settings"]["rule_severities"]["lib_footprint_mismatch"] = "ignore"
    json.dump(settings, open(project, "w"), indent=2, ensure_ascii=False)


def rules(board):
    """Design rules well inside what JLCPCB makes as a standard 2-layer board."""
    ds = board.GetDesignSettings()
    ds.m_CopperEdgeClearance = mm(0.25)
    ds.m_MinClearance = mm(0.15)
    ds.m_TrackMinWidth = mm(0.15)
    ds.m_ViasMinSize = mm(0.6)
    ds.m_MinThroughDrill = mm(0.3)
    ds.m_ViasMinAnnularWidth = mm(0.13)
    ds.m_HoleClearance = mm(0.25)
    ds.m_HoleToHoleMin = mm(0.5)
    settings = ds.m_NetSettings
    default = settings.GetDefaultNetclass()
    default.SetClearance(mm(0.2))
    default.SetTrackWidth(mm(0.2))
    default.SetViaDiameter(mm(0.6))
    default.SetViaDrill(mm(0.3))
    supply = pcbnew.NETCLASS("Supply")
    supply.SetClearance(mm(0.2))
    supply.SetTrackWidth(mm(SUPPLY_WIDTH))
    supply.SetViaDiameter(mm(0.6))
    supply.SetViaDrill(mm(0.3))
    settings.SetNetclass("Supply", supply)
    for net in SUPPLY_NETS:
        settings.SetNetclassPatternAssignment(net, "Supply")


def text(board, string, x, y, layer):
    label = pcbnew.PCB_TEXT(board)
    label.SetText(string)
    label.SetPosition(at(x, y))
    label.SetLayer(layer)
    label.SetMirrored(layer == pcbnew.B_SilkS)
    label.SetTextSize(pcbnew.VECTOR2I(mm(SILK_TEXT), mm(SILK_TEXT)))
    label.SetTextThickness(mm(SILK_WIDTH))
    board.Add(label)


def main():
    board = pcbnew.BOARD()
    board.SetCopperLayerCount(2)
    rules(board)

    nets = {}
    for _, _, _, _, _, pins in design.PARTS:
        for net in pins.values():
            if net and net not in nets:
                item = pcbnew.NETINFO_ITEM(board, net)
                board.Add(item)
                nets[net] = item

    inside = pcbnew.BOX2I(at(0.3, 0.3), pcbnew.VECTOR2I(mm(WIDTH - 0.6), mm(LENGTH - 0.6)))
    missing = [p[0] for p in design.PARTS if p[0] not in layout.REFS]
    if missing:
        raise SystemExit(f"not placed: {missing}")
    for ref, _, value, footprint, lcsc, pins in design.PARTS:
        lib, name = footprint.split(":")
        where = HERE if lib == "local" else FOOTPRINTS
        fp = pcbnew.FootprintLoad(f"{where}/{lib}.pretty", name)
        if fp is None:
            raise SystemExit(f"{ref}: no footprint {footprint}")
        fp.SetFPID(pcbnew.LIB_ID(lib, name))
        fp.SetReference(ref)
        fp.SetValue(value)
        # No reference designators in the silkscreen: the parts are too close for them.
        fp.Reference().SetLayer(pcbnew.F_Fab)
        for item in fp.GraphicalItems():
            if item.GetLayer() in (pcbnew.F_SilkS, pcbnew.B_SilkS) and item.Type() == pcbnew.PCB_SHAPE_T:
                if item.GetWidth() < mm(SILK_WIDTH):
                    item.SetWidth(mm(SILK_WIDTH))
        board.Add(fp)
        x, y, rot, side = layout.PLACED[ref]
        fp.SetPosition(at(x, y))
        if side == layout.B:
            fp.Flip(fp.GetPosition(), pcbnew.FLIP_DIRECTION_LEFT_RIGHT)
        fp.SetOrientationDegrees(rot)
        seen = set()
        for pad in fp.Pads():
            number = pad.GetNumber()
            if not number:
                continue
            if number not in pins:
                raise SystemExit(f"{ref}: pad {number} is not in design.py")
            seen.add(number)
            if pins[number]:
                pad.SetNet(nets[pins[number]])
                # The translators' ground pins join the fill solid: a pin between two
                # tracks has room for one spoke only.
                if ref in ("U2", "U3"):
                    pad.SetLocalZoneConnection(pcbnew.ZONE_CONNECTION_FULL)
        # Silkscreen that would run off the board (the header's pins) goes to the
        # fabrication layer, which is not printed. (Removing an item through the Python
        # API breaks the bindings for the calls that follow.)
        for item in fp.GraphicalItems():
            if item.GetLayer() in (pcbnew.F_SilkS, pcbnew.B_SilkS) and not inside.Contains(item.GetBoundingBox()):
                item.SetLayer(pcbnew.F_Fab if item.GetLayer() == pcbnew.F_SilkS else pcbnew.B_Fab)
        lacking = set(pins) - seen
        if lacking:
            raise SystemExit(f"{ref}: footprint {name} has no pad for {sorted(lacking)}")


    # Under the XIAO the top layer carries nothing but the lands: the XIAO's underside
    # has bare pads (battery, JTAG), and solder mask alone would be all that kept them
    # from a track. The lands' holes take the nets to the bottom layer.
    keep = pcbnew.ZONE(board)
    keep.SetIsRuleArea(True)
    keep.SetZoneName("under the XIAO")
    keep.SetLayer(pcbnew.F_Cu)
    keep.SetDoNotAllowTracks(True)
    keep.SetDoNotAllowVias(True)
    keep.SetDoNotAllowZoneFills(True)
    keep.SetDoNotAllowPads(False)
    keep.SetDoNotAllowFootprints(False)
    outline = keep.Outline()
    outline.NewOutline()
    for x, y in [(XIAO_INSIDE, 0), (WIDTH - XIAO_INSIDE, 0), (WIDTH - XIAO_INSIDE, XIAO_END), (XIAO_INSIDE, XIAO_END)]:
        outline.Append(mm(OX + x), mm(OY + y))
    board.Add(keep)

    # The header's pins, named on the bottom beyond its holes (its body covers the top),
    # and what the solder jumper does.
    left = layout.PLACED["J1"][0] - 7.62
    for i, name in enumerate(("G", "TX", "RX", "V")):
        text(board, name, left + 2.54 * i, 30.1, pcbnew.B_SilkS)
    jumper = layout.PLACED["JP1"]
    text(board, "V=3V3", jumper[0] - 1.2, jumper[1] - 2.6, pcbnew.B_SilkS)

    corners = [(0, 0), (WIDTH, 0), (WIDTH, LENGTH), (0, LENGTH)]
    for i, a in enumerate(corners):
        seg = pcbnew.PCB_SHAPE(board)
        seg.SetShape(pcbnew.SHAPE_T_SEGMENT)
        seg.SetStart(at(*a))
        seg.SetEnd(at(*corners[(i + 1) % 4]))
        seg.SetLayer(pcbnew.Edge_Cuts)
        seg.SetWidth(mm(0.1))
        board.Add(seg)

    save(board, BOARD)
    print(f"{BOARD}: {len(design.PARTS)} footprints, {len(nets)} nets, board {WIDTH} x {LENGTH} mm")


if __name__ == "__main__":
    main()
