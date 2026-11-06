# 3D-printed case

A one-piece, open-top box for the VCC-GND Studio YD-ESP32-S3 (`YD-ESP32-23 2022-v1.3`).
The board slides in from the top, USB-C end up, and stops with the receptacles flush with the rim.

| File | Board | Outer size (W × D × H) |
| --- | --- | --- |
| `yd-esp32-s3-case.stl` | no pin headers | 32.3 × 8.1 × 65.6 mm |
| `yd-esp32-s3-case-headers.stl` | pin headers, long pins on the back | 32.3 × 17.0 × 65.6 mm |

Both are ready to print standing upright, with no supports.

- `case.scad`: OpenSCAD source; every dimension is a parameter at the top.

The board dimensions come from the vendor's drawing (`ESP32-S3-Metric.pdf` in [vcc-gnd/YD-ESP32-S3](https://github.com/vcc-gnd/YD-ESP32-S3)).
The flat back of the PCB rests on the back wall and its edges run in grooves.
The headers version is the same box, grown only where the two pin rows are: a channel behind each row for the header plastic and pins (the back is made thicker, keeping the outside a plain rectangle), and a slot in the front lip of each groove for the pin ends on top.
It assumes standard headers: 8.5 mm below the PCB (`pin_below`).

The front is plain apart from the `USB` and `COM` labels at the top.

## Printing

PLA or PETG, 0.2 mm layers, 3 walls.
Keep metal (and metallic filament) away from the antenna end, which is the bottom of the box.

## Adjusting

| Parameter | Default | When to change |
| --- | --- | --- |
| `groove_clr` | 0.4 | Groove thickness over the PCB. 0.15 was too tight on a real board. |
| `pin_below` | 8.5 | Header plastic plus pins below the PCB, if yours differ (headers). |
| `clr` | 0.6 | Gap between each board edge and the side wall. 0.3 was too tight on a real board. |

```bash
openscad -o yd-esp32-s3-case.stl case.scad
openscad -D headers=true -o yd-esp32-s3-case-headers.stl case.scad
openscad -D 'part="assembly"' case.scad   # preview with the board in place
```
