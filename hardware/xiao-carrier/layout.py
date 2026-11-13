"""Where every part sits.

Coordinates are millimetres, seen from the top, the XIAO's USB-C connector at the top
edge (y = 0), x from the left edge. The XIAO covers the top side down to y = 21. Past
its end come the parts, then the header: all on the top, so one side is assembled and
the bottom is flat but for the solder jumper.
"""
T, B = "top", "bottom"
WIDTH, LENGTH = 20.0, 31.7

PLACED = {
    "U1": (10.0, 10.5, 0, T),
    # the header: through the board, body on the top, pins over the end; pin 1 is the
    # right-hand hole, so the holes read G, TX, RX, V from the left
    "J1": (13.81, 27.6, 270, T),
    # the TX translator and its parts, above the header's TX pin
    "R3": (2.6, 22.5, 0, T), "C1": (2.6, 23.9, 0, T),
    "U2": (5.4, 23.2, 0, T), "R1": (7.6, 24.4, 90, T),
    # the RX translator and its parts, above the header's RX and V pins
    "R4": (9.6, 22.5, 0, T), "U3": (12.2, 23.2, 0, T), "R2": (10.1, 24.4, 90, T),
    "C2": (14.6, 22.6, 90, T), "R5": (14.6, 24.8, 90, T),
    # V halved for D1
    "R6": (17.4, 22.5, 0, T), "R7": (17.4, 23.7, 0, T), "C3": (17.4, 24.9, 0, T),
    # the jumper from 3.3 V to V, on the bottom
    "JP1": (16.5, 24.0, 90, B),
}
REFS = set(PLACED)
