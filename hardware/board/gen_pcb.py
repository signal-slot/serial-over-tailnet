#!/usr/bin/env python3
"""Writes board.kicad_pcb from design.py: the outline, every footprint in its place with
its nets, and the ground fills. Tracks are not made here.

Coordinates are millimetres, seen from the top, x along the board (USB-C port A on the
left edge, port B on the right) and y down. y = 0 is where the copper starts: the board
goes on ANTENNA mm above it, bare, under the module's antenna, so that the antenna does not
stick out over the edge. No layer has copper there (the module's footprint carries the
keep-out).
"""
import json
import os
import sys

import pcbnew

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import design  # noqa: E402

import layout  # noqa: E402

FOOTPRINTS = "/usr/share/kicad/footprints"
# The board is LENGTH x (HEIGHT + ANTENNA) mm. BOARD_LENGTH=<mm> tries another length.
LENGTH, HEIGHT = float(os.environ.get("BOARD_LENGTH", 35.2)), 21.7
ANTENNA = 5.0                 # bare board above y = 0, under the antenna
# The module's antenna area (5.05 mm from its top edge, which sits 0.15 mm inside the
# board edge) ends at y = 0.2; the copper fills start below it, above the module's first
# row of pads at y = 0.75.
COPPER_FROM = 0.5
# Silkscreen JLCPCB prints: lines 0.15 mm and up, text 1 mm and up.
SILK_WIDTH = 0.15
SILK_TEXT = 1.0
OX, OY = 100.0, 100.0          # where the board sits on KiCad's sheet
B = "bottom"

def placed(ref):
    return layout.placed(ref, LENGTH)


def mm(v):
    return pcbnew.FromMM(v)


def save(board, path):
    """Writes the board and its project file.

    KiCad writes the project file with the board. The footprints are edited after loading
    (silkscreen off the board is dropped), so the check's note that they differ from the
    library is switched off there."""
    pcbnew.SaveBoard(path, board)
    project = path[:-len(".kicad_pcb")] + ".kicad_pro"
    settings = json.load(open(project))
    settings["board"]["design_settings"]["rule_severities"]["lib_footprint_mismatch"] = "ignore"
    json.dump(settings, open(project, "w"), indent=2, ensure_ascii=False)


def at(x, y):
    return pcbnew.VECTOR2I(mm(OX + x), mm(OY + y))


# Nets that carry supply current get wider tracks.
POWER_NETS = ["+5V", "VBUS_A", "VBUS_B"]
POWER_WIDTH = 0.25
# 3.3 V is routed, not a plane (the second inner layer also carries signals), and it goes
# to 28 pins, most of them pull-ups. The router finishes when it may route it as thin as
# a signal and mostly does not when it must be POWER_WIDTH everywhere (0, 2, 0, 2, 1 and 0
# connections left on six board lengths against 11, 7, 8, 12, 21 and 0). route.py widens
# it to POWER_WIDTH afterwards wherever the neighbours allow.
THIN_SUPPLY, THIN_WIDTH = "+3V3", 0.15


def rules(board):
    """Design rules within what JLCPCB makes as a standard 4-layer board."""
    ds = board.GetDesignSettings()
    ds.m_CopperEdgeClearance = mm(0.25)     # JLCPCB: 0.2 mm from a routed edge
    ds.m_MinClearance = mm(0.1)
    ds.m_TrackMinWidth = mm(0.09)       # the router necks down to 0.09 at fine-pitch pads
    ds.m_ViasMinSize = mm(0.45)
    ds.m_MinThroughDrill = mm(0.2)
    ds.m_ViasMinAnnularWidth = mm(0.1)
    ds.m_HoleClearance = mm(0.2)
    ds.m_HoleToHoleMin = mm(0.25)
    settings = ds.m_NetSettings
    default = settings.GetDefaultNetclass()
    default.SetClearance(mm(0.127))
    default.SetTrackWidth(mm(0.15))
    default.SetViaDiameter(mm(0.5))
    default.SetViaDrill(mm(0.3))
    power = pcbnew.NETCLASS("Power")
    power.SetClearance(mm(0.127))
    power.SetTrackWidth(mm(POWER_WIDTH))
    power.SetViaDiameter(mm(0.5))
    power.SetViaDrill(mm(0.3))
    settings.SetNetclass("Power", power)
    for net in POWER_NETS:
        settings.SetNetclassPatternAssignment(net, "Power")
    thin = pcbnew.NETCLASS("ThinSupply")
    thin.SetClearance(mm(0.127))
    thin.SetTrackWidth(mm(THIN_WIDTH))
    thin.SetViaDiameter(mm(0.5))
    thin.SetViaDrill(mm(0.3))
    settings.SetNetclass("ThinSupply", thin)
    settings.SetNetclassPatternAssignment(THIN_SUPPLY, "ThinSupply")


def plane(board, net, layer):
    """A copper plane of `net` over the whole board on an inner layer."""
    zone = pcbnew.ZONE(board)
    zone.SetLayer(layer)
    zone.SetNet(net)
    zone.SetZoneName(net.GetNetname())
    # Thermal reliefs: a small passive with one end on solid copper and the other on a
    # thin track tombstones. The module's pads are connected solid (set per pad below).
    zone.SetPadConnection(pcbnew.ZONE_CONNECTION_THERMAL)
    zone.SetMinThickness(mm(0.15))
    outline = zone.Outline()
    outline.NewOutline()
    for x, y in [(0, COPPER_FROM), (LENGTH, COPPER_FROM), (LENGTH, HEIGHT), (0, HEIGHT)]:
        outline.Append(mm(OX + x), mm(OY + y))
    board.Add(zone)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    board = pcbnew.BOARD()
    board.SetCopperLayerCount(4)
    rules(board)

    nets = {}
    for _, _, _, _, _, pins in design.PARTS:
        for net in pins.values():
            if net and net not in nets:
                item = pcbnew.NETINFO_ITEM(board, net)
                board.Add(item)
                nets[net] = item

    missing = [p[0] for p in design.PARTS if p[0] not in layout.REFS]
    if missing:
        raise SystemExit(f"not placed: {missing}")
    for ref, _, value, footprint, lcsc, pins in design.PARTS:
        lib, name = footprint.split(":")
        where = here if lib == "local" else FOOTPRINTS
        fp = pcbnew.FootprintLoad(f"{where}/{lib}.pretty", name)
        if fp is None:
            raise SystemExit(f"{ref}: no footprint {footprint}")
        fp.SetFPID(pcbnew.LIB_ID(lib, name))
        fp.SetReference(ref)
        fp.SetValue(value)
        # No reference designators in the silkscreen: the parts are too close for them.
        # They are on the fabrication layer, which the assembly drawing uses.
        fp.Reference().SetLayer(pcbnew.F_Fab)
        for item in fp.GraphicalItems():
            if item.GetLayer() in (pcbnew.F_SilkS, pcbnew.B_SilkS) and item.Type() == pcbnew.PCB_SHAPE_T:
                if item.GetWidth() < mm(SILK_WIDTH):
                    item.SetWidth(mm(SILK_WIDTH))
        board.Add(fp)
        x, y, rot, side = placed(ref)
        fp.SetPosition(at(x, y))
        if side == B:
            fp.Flip(fp.GetPosition(), pcbnew.FLIP_DIRECTION_LEFT_RIGHT)
        fp.SetOrientationDegrees(rot)
        seen = set()
        for pad in fp.Pads():
            number = pad.GetNumber()
            if not number:
                continue        # a hole without a net
            if number not in pins:
                raise SystemExit(f"{ref}: pad {number} is not in design.py")
            seen.add(number)
            if pins[number]:
                pad.SetNet(nets[pins[number]])
            if ref == "U1":
                pad.SetLocalZoneConnection(pcbnew.ZONE_CONNECTION_FULL)
        lacking = set(pins) - seen
        if lacking:
            raise SystemExit(f"{ref}: footprint {name} has no pad for {sorted(lacking)}")

    # Silkscreen that would run off the board (the header's pins, the connectors' and the
    # module's outlines) is dropped: it could not be printed anyway.
    inside = pcbnew.BOX2I(at(0.3, -ANTENNA + 0.3), pcbnew.VECTOR2I(mm(LENGTH - 0.6), mm(HEIGHT + ANTENNA - 0.6)))
    for fp in board.GetFootprints():
        for item in list(fp.GraphicalItems()):
            if item.GetLayer() in (pcbnew.F_SilkS, pcbnew.B_SilkS) and not inside.Contains(item.GetBoundingBox()):
                fp.Remove(item)

    # Labels: the UART header's pins on the bottom, under its holes (the header's body is
    # on the top), the VREF switch's two positions on the top, by the edge (the switch is
    # on the bottom).
    header = placed("J3")
    switch = placed("SW2")
    left = header[0] - 7.62          # the header's pin 1 is its right-hand hole
    labels = [("G", left, 20.0, pcbnew.B_SilkS), ("TX", left + 2.54, 20.0, pcbnew.B_SilkS),
              ("RX", left + 5.08, 20.0, pcbnew.B_SilkS), ("V", left + 7.62, 20.0, pcbnew.B_SilkS),
              ("EXT", switch[0] - 3.9, 20.3, pcbnew.F_SilkS), ("3V3", switch[0] + 2.8, 17.9, pcbnew.F_SilkS)]
    for text, x, y, layer in labels:
        label = pcbnew.PCB_TEXT(board)
        label.SetText(text)
        label.SetPosition(at(x, y))
        label.SetLayer(layer)
        label.SetMirrored(layer == pcbnew.B_SilkS)
        label.SetTextSize(pcbnew.VECTOR2I(mm(SILK_TEXT), mm(SILK_TEXT)))
        label.SetTextThickness(mm(SILK_WIDTH))
        board.Add(label)

    # Ground vias in the module's nine centre pads, as its datasheet's land pattern has
    # them: heat and the RF return go to the inner ground.
    # Only where the bottom side under the pad is free of other parts' pads.
    module = board.FindFootprintByReference("U1")
    bottom_pads = [(p.GetPosition(), max(p.GetSize(pcbnew.B_Cu).x, p.GetSize(pcbnew.B_Cu).y))
                   for fp in board.GetFootprints() if fp.IsFlipped() for p in fp.Pads()]
    for pad in module.Pads():
        if pad.GetSize(pcbnew.F_Cu).x >= mm(1.0):
            centre = pad.GetPosition()
            if any((centre - pos).EuclideanNorm() < size // 2 + mm(0.6) for pos, size in bottom_pads):
                continue
            via = pcbnew.PCB_VIA(board)
            via.SetPosition(pad.GetPosition())
            via.SetViaType(pcbnew.VIATYPE_THROUGH)
            via.SetWidth(pcbnew.F_Cu, mm(0.5))
            via.SetDrill(mm(0.3))
            via.SetLayerPair(pcbnew.F_Cu, pcbnew.B_Cu)
            via.SetNet(nets["GND"])
            board.Add(via)

    corners = [(0, -ANTENNA), (LENGTH, -ANTENNA), (LENGTH, HEIGHT), (0, HEIGHT)]
    for i, a in enumerate(corners):
        seg = pcbnew.PCB_SHAPE(board)
        seg.SetShape(pcbnew.SHAPE_T_SEGMENT)
        seg.SetStart(at(*a))
        seg.SetEnd(at(*corners[(i + 1) % 4]))
        seg.SetLayer(pcbnew.Edge_Cuts)
        seg.SetWidth(mm(0.1))
        board.Add(seg)

    # Ground on the first inner layer when the router runs: it reaches ground pins with
    # vias to it. The router may also put signals there (keeping it off, as a power layer
    # or with a no-track area, left it unable to finish or to place a via at all), so
    # route.py adds ground fills on the other three layers afterwards and the four are
    # tied together by the ground vias.
    plane(board, nets["GND"], pcbnew.In1_Cu)

    out = os.path.join(here, "board.kicad_pcb")
    save(board, out)
    print(f"{out}: {len(design.PARTS)} footprints, {len(nets)} nets, board {LENGTH} x {HEIGHT + ANTENNA} mm")


if __name__ == "__main__":
    main()
