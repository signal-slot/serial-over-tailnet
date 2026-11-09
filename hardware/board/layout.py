"""Where every part sits.

Coordinates are millimetres, seen from the top, x along the board (USB-C port A on the
left edge, port B on the right) and y down from where the copper starts (gen_pcb.py).

The two ends are alike. On top each has its connector with, beside it, the part that
detects the cable's role and the ESD protection. On the bottom each port's 5 V switch is
under its own connector, and the part that picks the 5 V input sits between the two, so no
supply has to cross a row of parts. The module is in the middle with the rest under it.

The table is written for a board BASE mm long. placed() serves other lengths: the parts
of the left end stay, those of the right end move with the right edge and the middle ones
by half as much.
"""
T, B = "top", "bottom"
BASE = 36.0

# x, y, rotation, side: parts anchored to the left end
LEFT = {
    # 5.79 mm from the connector's rear slots to the board edge, as HRO's drawing has it
    "J1": (2.66, 7.0, 270, T),
    "U11": (2.7, 15.0, 0, T), "U2": (6.6, 14.3, 0, T), "C8": (6.6, 16.6, 0, T), "R3": (6.6, 17.7, 0, T),
    # bottom, under the connector: port A's 5 V switch, between the connector's holes
    "U5": (3.0, 7.0, 0, B), "R12": (6.4, 5.5, 0, B), "R14": (6.4, 6.5, 0, B), "R16": (6.4, 7.5, 0, B), "C14": (6.4, 8.5, 0, B),
    "C15": (3.0, 9.4, 0, B), "C16": (3.4, 4.4, 0, B),
    # bottom, beside the connector: BOOT button
    "SW1": (4.3, 15.7, 0, B),
}
# parts anchored to the right end (x measured as on a 36 mm board)
RIGHT = {
    "J2": (33.34, 7.0, 90, T),
    "U12": (33.3, 15.0, 0, T), "U3": (29.4, 14.3, 0, T), "C9": (29.4, 16.6, 0, T), "R4": (29.4, 17.7, 0, T),
    "U6": (33.0, 7.0, 0, B), "R13": (29.6, 5.5, 0, B), "R15": (29.6, 6.5, 0, B), "R17": (29.6, 7.5, 0, B), "C13": (29.6, 8.5, 0, B),
    "C17": (33.0, 9.4, 0, B), "C18": (32.6, 4.4, 0, B),
    "TP6": (27.6, 13.3, 0, B), "TP1": (29.6, 13.3, 0, B), "TP2": (31.6, 13.3, 0, B), "TP3": (33.6, 13.3, 0, B),
    "TP4": (35.0, 15.3, 0, B),
    # the VREF switch: on the bottom edge, its knob over the edge, next to the UART header
    "SW2": (31.5, 19.9, 0, B),
}
# parts that stay with the module, in the middle
MID = {
    "U1": (18.0, 5.3, 0, T),
    "D1": (11.0, 17.5, 0, T), "R24": (14.0, 17.5, 0, T),
    # bottom, first row: the 3.3 V regulator by the module's supply pin (pin 3, top of
    # its left side), then the part that picks the 5 V input between the two ports, the
    # module's reset parts, a test pad
    "U8": (10.6, 2.4, 0, B), "C5": (13.4, 2.3, 90, B), "C4": (15.2, 2.3, 90, B),
    "U4": (17.4, 2.2, 0, B), "C6": (19.6, 2.2, 90, B), "C7": (21.0, 2.2, 90, B), "TP5": (23.0, 1.6, 0, B),
    "R1": (25.0, 1.2, 0, B), "C1": (25.0, 2.3, 0, B), "R2": (25.0, 3.4, 0, B),
    "R10": (17.0, 4.5, 0, B), "R11": (19.1, 4.5, 0, B),
    # bottom, second row: the module's capacitors and the pull-ups
    "C2": (14.5, 5.9, 0, B), "C3": (14.5, 7.3, 0, B),
    "R5": (17.3, 5.9, 0, B), "R6": (17.3, 7.0, 0, B), "R7": (17.3, 8.1, 0, B), "R8": (19.4, 5.9, 0, B), "R9": (19.4, 7.0, 0, B),
    # bottom, third row: UART level shifters, USB switch, test pads
    "R25": (8.9, 9.6, 0, B), "R21": (10.5, 10.9, 0, B), "C19": (12.2, 9.4, 0, B), "R22": (13.6, 10.9, 90, B), "R23": (14.6, 10.9, 90, B), "R20": (15.6, 10.9, 90, B), "C11": (16.6, 10.9, 90, B),
    "C10": (18.4, 9.7, 0, B), "U7": (18.4, 12.0, 0, B),
    "R19": (20.6, 10.6, 90, B), "R18": (22.0, 10.6, 90, B),
    "U10": (11.0, 13.2, 0, B), "U9": (14.1, 13.2, 0, B), "C12": (16.4, 13.0, 90, B),
    # the UART header: through the board at the bottom edge, body on the top, pins over
    # the edge; pin 1 is at the right end, so the holes read G, TX, RX, V from the left
    "J3": (24.62, 17.7, 270, T),
}


def placed(ref, length):
    extra = length - BASE
    if ref in LEFT:
        return LEFT[ref]
    if ref in RIGHT:
        x, y, rot, side = RIGHT[ref]
        return x + extra, y, rot, side
    x, y, rot, side = MID[ref]
    return x + extra / 2, y, rot, side


REFS = set(LEFT) | set(RIGHT) | set(MID)
