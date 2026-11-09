#!/usr/bin/env python3
"""Writes board.kicad_sch from design.py.

Every pin gets a global label at its end (or a no-connect mark), so the schematic has no
wires: the nets are exactly the names in design.py. Check it with

    kicad-cli sch erc board.kicad_sch
"""
import copy
import os
import sys
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import design  # noqa: E402
from sexpr import Sym, dump, find, first, parse  # noqa: E402

SYMBOLS = "/usr/share/kicad/symbols"
ROOT = "6f1f0c0a-0000-4000-8000-000000000001"
G = 2.54

_libs = {}


def uid(*key):
    """A stable uuid for a thing, so regenerating does not churn the file."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "usb-serial-over-tailscale/board/" + "/".join(map(str, key))))


def lib_symbol(lib_id):
    """The library's definition of `lib_id`, flattened and renamed for a schematic."""
    libname, name = lib_id.split(":")
    if libname == "local":
        return local_symbol(name)
    if libname not in _libs:
        _libs[libname] = parse(open(f"{SYMBOLS}/{libname}.kicad_sym").read())
    by = {s[1]: s for s in find(_libs[libname], "symbol")}
    sym = copy.deepcopy(by[name])
    ext = first(sym, "extends")
    if ext:
        base = copy.deepcopy(by[ext[1]])
        props = {p[1]: p for p in find(sym, "property")}
        out = [Sym("symbol"), name]
        for item in base[2:]:
            if isinstance(item, list) and item[0] == "property" and item[1] in props:
                out.append(props.pop(item[1]))
            elif isinstance(item, list) and item[0] == "symbol":
                item[1] = name + item[1][len(ext[1]):]
                out.append(item)
            else:
                out.append(item)
        sym = out
    sym[1] = lib_id
    return sym


def local_symbol(name):
    pins = design.LOCAL_SYMBOLS[name]
    left = [p for p in pins if p[3] == "L"]
    right = [p for p in pins if p[3] == "R"]
    rows = max(len(left), len(right))
    half_h = (rows + 1) * G / 2
    half_w = 4 * G

    def prop(key, value, y, hide=False):
        eff = [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]]]
        if hide:
            eff.append([Sym("hide"), Sym("yes")])
        return [Sym("property"), key, value, [Sym("at"), 0, y, 0], eff]

    body = [Sym("symbol"), f"{name}_0_1",
            [Sym("rectangle"), [Sym("start"), -half_w, half_h], [Sym("end"), half_w, -half_h],
             [Sym("stroke"), [Sym("width"), 0.254], [Sym("type"), Sym("default")]],
             [Sym("fill"), [Sym("type"), Sym("background")]]]]
    unit = [Sym("symbol"), f"{name}_1_1"]
    for side, x, angle in ((left, -half_w - G, 0), (right, half_w + G, 180)):
        for i, (number, pname, etype, _) in enumerate(side):
            y = half_h - (i + 1) * G
            unit.append([Sym("pin"), Sym(etype), Sym("line"), [Sym("at"), x, y, angle], [Sym("length"), G],
                         [Sym("name"), pname, [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]]]],
                         [Sym("number"), number, [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]]]]])
    return [Sym("symbol"), f"local:{name}",
            [Sym("exclude_from_sim"), Sym("no")], [Sym("in_bom"), Sym("yes")], [Sym("on_board"), Sym("yes")],
            prop("Reference", "U", half_h + G), prop("Value", name, -half_h - G),
            prop("Footprint", "", 0, True), prop("Datasheet", "", 0, True),
            body, unit]


def pins_of(sym):
    """[(number, x, y, angle)] of a library symbol (unit 1, y up)."""
    out = []
    for sub in find(sym, "symbol"):
        for p in find(sub, "pin"):
            at = first(p, "at")
            angle = int(float(at[3])) if len(at) > 3 else 0
            out.append((first(p, "number")[1], float(at[1]), float(at[2]), angle))
    return out


def extent(sym):
    pins = pins_of(sym)
    xs = [p[1] for p in pins] or [0]
    ys = [p[2] for p in pins] or [0]
    return min(xs), max(xs), min(ys), max(ys)


def text_effects(justify=None, hide=False):
    eff = [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]]]
    if justify:
        eff.append([Sym("justify"), Sym(justify)])
    if hide:
        eff.append([Sym("hide"), Sym("yes")])
    return eff


def label(net, x, y, pin_angle, key):
    # The label reads away from the symbol body.
    angle, justify = {0: (180, "right"), 180: (0, "left"), 90: (270, "right"), 270: (90, "left")}[pin_angle]
    return [Sym("global_label"), net, [Sym("shape"), Sym("passive")], [Sym("at"), x, y, angle],
            text_effects(justify), [Sym("uuid"), uid("label", *key)]]


def snap(v):
    """Onto the 2.54 mm grid, where KiCad expects pin ends."""
    return round(round(v / G) * G, 4)


def place(items, lib_defs, ref, lib_id, value, footprint, lcsc, pins, x, y):
    x, y = snap(x), snap(y)
    sym = lib_defs.setdefault(lib_id, lib_symbol(lib_id))
    x0, x1, y0, y1 = extent(sym)
    inst = [Sym("symbol"), [Sym("lib_id"), lib_id], [Sym("at"), x, y, 0], [Sym("unit"), 1],
            [Sym("exclude_from_sim"), Sym("no")], [Sym("in_bom"), Sym("yes")], [Sym("on_board"), Sym("yes")],
            [Sym("dnp"), Sym("no")], [Sym("uuid"), uid("sym", ref)],
            [Sym("property"), "Reference", ref, [Sym("at"), x + x0, y - y1 - 2 * G, 0], text_effects("left")],
            [Sym("property"), "Value", value, [Sym("at"), x + x0, y - y1 - G, 0], text_effects("left")],
            [Sym("property"), "Footprint", footprint, [Sym("at"), x, y, 0], text_effects(hide=True)],
            [Sym("property"), "Datasheet", "", [Sym("at"), x, y, 0], text_effects(hide=True)],
            [Sym("property"), "LCSC", lcsc, [Sym("at"), x, y, 0], text_effects(hide=True)]]
    seen_points = set()
    numbers = set()
    for number, px, py, angle in pins_of(sym):
        numbers.add(number)
        inst.append([Sym("pin"), number, [Sym("uuid"), uid("pin", ref, number)]])
        if number not in pins:
            raise SystemExit(f"{ref}: pin {number} is not in design.py")
        net = pins[number]
        sx, sy = round(x + px, 4), round(y - py, 4)
        if (sx, sy, net) in seen_points:
            continue
        seen_points.add((sx, sy, net))
        if net is None:
            items.append([Sym("no_connect"), [Sym("at"), sx, sy], [Sym("uuid"), uid("nc", ref, number)]])
        else:
            items.append(label(net, sx, sy, angle, (ref, number)))
    extra = set(pins) - numbers
    if extra:
        raise SystemExit(f"{ref}: design.py names pins the symbol lacks: {sorted(extra)}")
    inst.append([Sym("instances"), [Sym("project"), "board",
                 [Sym("path"), f"/{ROOT}", [Sym("reference"), ref], [Sym("unit"), 1]]]])
    items.append(inst)


# Where the larger parts go on the sheet (mm); the small ones are laid out in rows below.
PLACED = {
    "U1": (300, 150),
    "J1": (60, 70), "U11": (60, 140), "U2": (140, 70), "U5": (140, 130),
    "J2": (60, 230), "U12": (60, 300), "U3": (140, 230), "U6": (140, 290),
    "U4": (220, 60), "U8": (220, 110), "U7": (220, 190),
    "U9": (420, 70), "U10": (420, 130), "J3": (500, 100),
    "SW1": (420, 200), "SW2": (500, 160), "D1": (420, 240),
}


def main():
    items, lib_defs = [], {}
    col = row = 0
    for ref, lib_id, value, footprint, lcsc, pins in design.PARTS:
        if ref in PLACED:
            x, y = PLACED[ref]
        else:
            x, y = 190 + col * 6 * G, 330 + row * 16 * G
            col += 1
            if col == 22:
                col, row = 0, row + 1
        place(items, lib_defs, ref, lib_id, value, footprint, lcsc, pins, x, y)
    for i, net in enumerate(design.POWER_FLAGS):
        place(items, lib_defs, f"#FLG{i + 1:02d}", "power:PWR_FLAG", "PWR_FLAG", "", "", {"1": net},
              420 + i * 8 * G, 300)
    sch = [Sym("kicad_sch"), [Sym("version"), 20231120], [Sym("generator"), "gen_sch.py"],
           [Sym("uuid"), ROOT], [Sym("paper"), "A2"],
           [Sym("title_block"), [Sym("title"), "usb-serial-over-tailscale adapter"],
            [Sym("comment"), 1, "Generated from design.py by gen_sch.py"]],
           [Sym("lib_symbols")] + [lib_defs[k] for k in sorted(lib_defs)]]
    sch += items
    sch.append([Sym("sheet_instances"), [Sym("path"), "/", [Sym("page"), "1"]]])
    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.join(here, "board.kicad_sch")
    open(out, "w").write(dump(sch) + "\n")
    # The symbols drawn here, as a project library, so KiCad can find them again.
    local = [Sym("kicad_symbol_lib"), [Sym("version"), 20231120], [Sym("generator"), "gen_sch.py"]]
    for name in sorted(design.LOCAL_SYMBOLS):
        sym = local_symbol(name)
        sym[1] = name
        local.append(sym)
    open(os.path.join(here, "local.kicad_sym"), "w").write(dump(local) + "\n")
    open(os.path.join(here, "sym-lib-table"), "w").write(
        '(sym_lib_table\n\t(version 7)\n\t(lib (name "local")(type "KiCad")(uri "${KIPRJMOD}/local.kicad_sym")(options "")(descr "Symbols of this board"))\n)\n')
    print(f"{out}: {len(design.PARTS)} parts")


if __name__ == "__main__":
    main()
