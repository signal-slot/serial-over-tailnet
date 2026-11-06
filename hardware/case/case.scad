// Case for the VCC-GND Studio YD-ESP32-S3 (YD-ESP32-23 2022-v1.3), with or without pin headers.
//
// One open-top box. The board slides in from the top, USB-C end up,
// and stops with the receptacles flush with the rim.
//
// Board coordinates follow the vendor's dimension drawing
// (github.com/vcc-gnd/YD-ESP32-S3, "ESP32-S3-Metric.pdf"):
// x across the board from the "3V3" pin row, y along it from the USB-C edge,
// z from the back of the case towards the component side.
// The model is built in these coordinates and stood up at the end (y = 0 on top).
//
// The flat back of the PCB rests on the back wall and its edges run in grooves.
// With headers (long pins on the back) the same box only grows where the two pin rows are:
// a channel behind each row for the header plastic and pins (the back is thickened to
// hold them), and a slot in the groove's front lip for the pin stubs on top.
//
// openscad -o yd-esp32-s3-case.stl case.scad
// openscad -D headers=true -o yd-esp32-s3-case-headers.stl case.scad

part = "case"; // "case", "assembly"
headers = false;

/* [Board] */
pcb_w = 27.94;
pcb_l = 57.15;
pcb_t = 1.6;
ant_l = 63.39;      // USB-C edge to the tip of the module antenna overhang
comp_h = 3.4;       // tallest part above the PCB (USB-C 3.26, WROOM-1 3.1)

/* [Pin headers] */
pin_rows_x = [1.27, 26.67];
pin_below = 8.5;    // header plastic (2.5) and long pins (6.0) below the PCB
stub_h = 2.0;       // short pin ends and solder above the PCB
hdr_w = 2.54;       // header plastic width
hdr_clr = 0.4;
stub_w = 2.0;       // short pin ends with their solder fillets

usb_x = [8.02, 19.92];  // native USB (target side), COM (power and log)
usb_w = 8.94;

/* [Case] */
wall = 1.6;
back_t = 1.2;
front_t = 1.4;
bottom_t = 1.6;
clr = 0.6;          // board edge to side wall; 0.3 was too tight on a real board
comp_x = [2.2, 25.7];  // components stay inside this band; the PCB edges outside it run in grooves
groove_clr = 0.4;   // extra thickness of the grooves over the PCB; 0.15 was too tight on a real board

$fn = 48;
e = 0.01;

z_pcb = back_t;                     // PCB bottom
z_top = z_pcb + pcb_t;              // PCB top
z_ceil = z_top + comp_h + 0.5;
z_pins = z_pcb - pin_below - hdr_clr;  // bottom of the header channels
H = z_ceil + front_t;

x0 = -clr - wall;
x1 = pcb_w + clr + wall;
y1 = ant_l + clr + bottom_t;

module box(p0, p1) { translate(p0) cube(p1 - p0); }

// text on the outside of the front, upright when the box stands USB end up
module label(xy, s, size) {
    translate([xy[0], xy[1]]) rotate(180)
        text(s, size = size, halign = "center", valign = "center", font = "Liberation Sans:style=Bold");
}

// header channel across x, from the side wall to just past the header plastic
function chan(x) = x < pcb_w / 2 ? [-clr, x + hdr_w / 2 + hdr_clr] : [x - hdr_w / 2 - hdr_clr, pcb_w + clr];

module case() {
    difference() {
        box([x0, 0, headers ? z_pins - back_t : 0], [x1, y1, H]);
        // groove for the PCB
        box([-clr, -1, z_pcb - groove_clr / 2], [pcb_w + clr, ant_l + clr, z_top + groove_clr / 2]);
        // components
        box([comp_x[0], -1, z_top], [comp_x[1], ant_l + clr, z_ceil]);
        if (headers) for (x = pin_rows_x) {
            c = chan(x);
            box([c[0], -1, z_pins], [c[1], ant_l + clr, z_pcb + e]);
            box([x - stub_w / 2, -1, z_top - e], [x + stub_w / 2, ant_l + clr, z_top + stub_h]);
        }
        // the module antenna overhangs the PCB end
        box([-clr, pcb_l, back_t], [pcb_w + clr, ant_l + clr, z_ceil]);
        translate([0, 0, H - 0.4]) linear_extrude(1) {
            label([usb_x[0], 4], "USB", 3);
            label([usb_x[1], 4], "COM", 3);
        }
    }
    // the PCB end rests here, which puts the USB-C receptacles flush with the rim
    box([3, pcb_l + 0.2, back_t - e], [pcb_w - 3, y1 - bottom_t + e, z_top - 0.4]);
}

module board() {
    color("#202020") box([0, 0.2, z_pcb], [pcb_w, pcb_l + 0.2, z_top]);
    if (headers) for (x = pin_rows_x) {
        color("#303030") box([x - 1.27, 0.64 + 0.2, z_pcb - 2.5], [x + 1.27, 56.52 + 0.2, z_pcb]);
        for (i = [0 : 21]) color("gold")
            translate([x - 0.32, 1.91 + 2.54 * i + 0.2 - 0.32, z_pcb - pin_below]) cube([0.64, 0.64, pin_below + pcb_t + 1.4]);
    }
    color("silver") box([4.9, 37.9, z_top], [23.1, ant_l + 0.2, z_top + 3.1]);
    for (x = usb_x) color("silver") box([x - usb_w / 2, 0.2, z_top], [x + usb_w / 2, 7.5, z_top + 3.26]);
}

// stand it up: USB-C end on top, antenna end on the bed
module upright() translate([0, 0, y1]) rotate([-90, 0, 0]) children();

if (part == "assembly") upright() { %case(); board(); }
else upright() case();
