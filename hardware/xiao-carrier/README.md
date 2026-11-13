# XIAO carrier

A small board that a Seeed Studio XIAO ESP32S3 (or ESP32C3) sits on.
Past the XIAO's end opposite the USB-C connector it has a 4-pin right-angle header with a UART at the target's voltage.

**Not made yet.** Nothing here has been tested on hardware.

| | |
|---|---|
| Size | 20.0 × 31.7 mm, 2 layers |
| Header | G, TX, RX, V (2.54 mm, right angle), the order of the adapter board in `../board` |
| V | the target's supply, 1.65 to 5.5 V; it powers the target side of the level translators and nothing else |
| XIAO pins | D4 = TX, D5 = RX, D1 = V / 2 (for the ADC) |
| Level translators | 2 × SN74LVC1T45, 330 Ω in series with TX and RX |
| Solder jumper | on the bottom, open as made; bridged, it connects V to the XIAO's 3.3 V for a 3.3 V target wired with G, TX, RX only |

The XIAO's lands have holes.
It is soldered flat on the board by its castellated edge, or stands on pin headers.
Under the XIAO the top layer has no copper but the lands, because the XIAO's underside has bare pads.

Never bridge the jumper and wire V to the target at the same time: that joins two supplies.

## Files

`design.py` is the single source (parts, JLCPCB part numbers, nets) and `layout.py` places the parts.
The scripts shared with the adapter board are in `../board`.

```bash
python3 ../board/gen_sch.py .            # board.kicad_sch
python3 gen_pcb.py                       # board.kicad_pcb: footprints, rules
python3 route.py export work/board.dsn
java -Djava.awt.headless=true -jar freerouting.jar --gui.enabled=false \
    -de work/board.dsn -do routing.ses -mp 20 -mt 1
python3 route.py import routing.ses      # tracks, vias, ground fill
kicad-cli pcb drc --severity-all -o drc.rpt board.kicad_pcb
python3 ../board/fab.py .                # fab/: gerbers.zip, bom.csv, cpl.csv
```

`routing.ses` is kept because routing again gives a different result.
JLCPCB assembles the top side; the XIAO is soldered on by hand.
