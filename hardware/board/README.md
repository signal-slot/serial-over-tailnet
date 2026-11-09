# Adapter board

A board of our own for the adapter, around the ESP32-S3-MINI-1-N4R2 module, which carries
the Japanese radio certification (技適 201-230385) that the YD-ESP32-S3 boards we tested
on do not.

*The design has not been built or tested yet. Everything below about how it behaves is
what the schematic says, not what a board did.*

[日本語](README-ja.md)

## What it is

- 35.1 × 26.7 mm, four layers, parts on both sides, 68 parts, all placed by JLCPCB
  (Standard assembly: the module and the TUSB320 are "Standard only" parts there, and
  the board has parts on both sides).
- Two identical USB-C ports, one at each end. Either one can be the port the PC side
  plugs into; the other then powers and talks to the target. A TUSB320LAI per port reads
  the cable's orientation and role over I2C, an LM66200 picks whichever port delivers
  5 V, and a TS3USB221 switches the module's USB to the other port. The LM66200's status
  output sets the switch's default, so the board routes the right way even before the
  firmware runs. 5 V to the target goes through an AP22653 current limiter (490 mA) per
  port, which the firmware turns on while that port's detector sees a sink attached.
- A 2.54 mm right-angle pin header (on the top, pins out of the long edge) for a
  target's UART: G, TX, RX, V. TX and RX go through two SN74LVC1T45 level translators
  whose target side is supplied by VREF, so a 1.8 V or 3.3 V target works; 330 Ω in
  series keeps a TX-to-TX miswire harmless. VREF comes from the header's V pin, or from the board's
  3.3 V when the slide switch next to the header is on `3V3` (for a target wired with
  three wires only). The module measures VREF on IO1.
- USB ESD protection (USBLC6-2SC6) on both ports, a BOOT button, a red LED, test pads for
  the module's UART0, EN, 5 V, 3.3 V and ground.
- The module's antenna sits over a 5 mm strip of bare board, without copper on any layer;
  the copper starts 0.3 mm below the antenna area. Espressif prefers the antenna beyond
  the board edge; the range with this arrangement is to be measured.

Parts, with JLCPCB numbers, are in `design.py`. The pin assignment of the module is there
too, chosen by which side of the module each signal's part sits on.

## Files

| File | What |
|---|---|
| `design.py` | The design, once: every part with its footprint, JLCPCB number and the net of every pin. |
| `layout.py` | Where every part sits, in millimetres. |
| `gen_sch.py` | Writes `board.kicad_sch`, `local.kicad_sym` and `sym-lib-table` from `design.py`. |
| `gen_pcb.py` | Writes `board.kicad_pcb` from `design.py` and `layout.py`: outline, footprints, rules, the ground plane. No tracks. |
| `route.py` | `export` writes the Specctra file for Freerouting; `import` rebuilds the board from `gen_pcb.py` and adds the router's tracks, widens the 3.3 V tracks where there is room, pours ground on every layer and stitches the pours with vias. |
| `routing.ses` | Freerouting's result. Kept because routing again gives a different result. |
| `fab.py` | Writes `fab/`: `gerbers.zip`, `bom.csv`, `cpl.csv` and `placement-check.txt` for JLCPCB. Refuses a board that fails KiCad's check, or a part whose JLCPCB footprint would not land on its pads. |
| `jlc_align.py`, `jlc_transforms.json` | JLCPCB's footprint for every part number, matched to KiCad's: the rotation and origin `fab.py` writes. The script needs the network; the result is kept. |
| `local.pretty/`, `local.kicad_sym` | The footprint of the slide switch (from the maker's drawing) and the symbols KiCad's libraries lack. |
| `board.kicad_pcb`, `board.kicad_sch`, `board.kicad_pro` | The result, as KiCad 10 opens it. |
| `sexpr.py` | A small S-expression reader and writer the scripts share. |

## Rebuilding

Needs KiCad 10 with its Python module (`import pcbnew`), KiCad's symbol and footprint
libraries, and Java for Freerouting (tested with 2.5.0).

```bash
python3 gen_sch.py && kicad-cli sch erc -o erc.rpt board.kicad_sch     # 0 errors, 0 warnings
python3 route.py import routing.ses                                    # board.kicad_pcb
kicad-cli pcb drc --severity-all -o drc.rpt board.kicad_pcb            # 0 violations, 0 unconnected
python3 fab.py                                                         # fab/
```

To route again after a change to the parts or the layout:

```bash
python3 gen_pcb.py
python3 route.py export work/board.dsn
java -jar freerouting.jar -de work/board.dsn -do routing.ses -mp 60 -mt 1
python3 route.py import routing.ses
```

Freerouting does not finish every time (on this board it finished on about half of the
attempts, and some of those left a piece of ground fill unconnected). `BOARD_LENGTH=35.4
python3 gen_pcb.py` moves the parts for another board length, which is the cheap way to
get another attempt. Check with `kicad-cli pcb drc` and keep the session file that passes.

## Firmware

The firmware is built for this board with its own variant and feature (the default
build is for the YD-ESP32-S3 development board):

```bash
cd firmware
ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/adapter.defaults" cargo build --release --features board-adapter
tools/mkimage.sh adapter release        # dist/adapter/
```

What it does on this board (`firmware/src/board.rs`, `uart_link.rs`): it reads which port
supplies the 5 V (the PC's), sends the module's USB to the other port, fixes the roles of
the two detectors over I2C (the PC's port a sink, the target's a source) and turns the
target port's 5 V on while a sink is attached there and off when it leaves. In setup mode
the USB stays at the PC's port, where the console is, and no 5 V goes out. The UART header
is bridged on the first TCP port plus 8, above the interface numbers a USB device can
have, at the configured baud rate. `status` shows which port powers the board, the role
each port's detector reports, the 5 V switches' fault outputs, whether the PC's port
advertises only 500 mA (the module and a target together can exceed that), and the VREF
voltage.

## Ordering from JLCPCB

Upload `fab/gerbers.zip`, choose 4 layers, 1.6 mm, ENIG (there is a 0.4 mm pitch part),
Standard assembly for both sides, then upload `fab/bom.csv` and `fab/cpl.csv`.

JLCPCB places a part by its own footprint, whose origin and zero rotation differ from
KiCad's part by part (two SOT-23-6 parts in its library face different ways). `cpl.csv`
is therefore computed from JLCPCB's footprints: `jlc_align.py` reads them for every part
number (EasyEDA's public API; the result is `jlc_transforms.json`) and matches their pads
to KiCad's, and `fab.py` writes JLCPCB's origin and rotation for each part and then checks
that JLCPCB's pads, placed as written, cover KiCad's pads (`fab/placement-check.txt`: the
largest miss per part, 0.36 mm at most, which is the difference between JLCPCB's drawing
of the slide switch and the maker's). For bottom parts the angle is the part's
rotation as seen from the bottom (JLCPCB rotates its footprint, then mirrors it); two
placement previews on 2026-11-09 settled that, the second one through the three parts
whose JLCPCB footprint is turned 90° from KiCad's. The
LED's pads are matched so that JLCPCB's cathode (pin 2 in its symbol) lands on KiCad's.
Look at the placement preview before paying anyway; it is the only view of what JLCPCB
will do.

Rough cost, from JLCPCB's price lists in November 2026 and not from a quote: about 160 USD
for five boards (Standard assembly on both sides, about 10 USD of parts per board, a
loading fee for each of the dozen "extended" part types, hand soldering for the header).

## Design rules

Within JLCPCB's published capabilities for a 4-layer board: 0.15 mm tracks (0.09 mm
where the router had to neck down to a pad; JLCPCB's minimum is 0.09), 0.127 mm
clearance, 0.3 mm drill / 0.5 mm vias, 0.25 mm from copper to the board edge (JLCPCB asks
0.2). Supply tracks are 0.25 mm.

## Not verified

- No board has been made. The circuit, the firmware's control of it and the radio range
  with the antenna over the board are untested.
- How the Japanese certification must be shown on a product that embeds the module.
- The slide switch's positions: from the maker's drawing the knob towards the `3V3` label
  selects 3.3 V. The firmware can read the result on IO1 either way.
- `cpl.csv` has been checked against JLCPCB's footprints numerically, not yet against a
  placement preview made from this version of the file (see Ordering).
- The 22 µF 6.3 V capacitor on 3.3 V is worth about half its rating at that bias; the
  module's recommendation assumes the full 22 µF. If brown-outs show, add a second one.
- The USB data pairs are autorouted: the two lines of port B differ in length by about
  18 mm and change layers. Full speed (12 Mb/s) does not mind; it is not a high-speed
  layout.
