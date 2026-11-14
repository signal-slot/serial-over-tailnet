"""A carrier for a Seeed Studio XIAO (ESP32S3 or ESP32C3), described once: parts,
footprints, JLCPCB part numbers and nets.

The XIAO is soldered flat on the board by its castellated edge, or stands on pin headers
(its lands have holes). Past its end opposite the USB-C
connector the board carries two level translators and a 4-pin right-angle header with a
UART at the target's voltage: G, TX, RX, V, in the order of the adapter board's header
(../board). V is the target's supply (1.65 to 5.5 V); it powers the target side of the
translators and nothing else.

The pins are chosen to be free on both XIAOs:
    D4, D5  UART TX, RX   ESP32S3 GPIO5, GPIO6; ESP32C3 GPIO6, GPIO7 (no strapping pin)
    D1      V / 2         ESP32S3 GPIO2 (ADC1_CH1); ESP32C3 GPIO3 (ADC1_CH3)
D6 and D7 (UART0: the boot ROM prints there), D2 (GPIO3 on the ESP32S3) and D8, D9 (GPIO8,
GPIO9 on the ESP32C3) are strapping or boot pins and stay free.
"""

NC = None

# SN74LVC1T45: DIR (referenced to VCCA) high = A to B, low = B to A; either supply at GND
# isolates both sides.

R0402 = "Resistor_SMD:R_0402_1005Metric"
C0402 = "Capacitor_SMD:C_0402_1005Metric"

# The XIAO seen from the top with the USB-C connector up: 1-7 down the left edge (D0-D6),
# 8-14 up the right edge (D7-D10, 3V3, GND, 5V). The UART is where the firmware has it
# (feature board-xiao-s3): D10 = TX, D9 = RX.
XIAO = {
    "1": NC, "2": "VREF_SENSE", "3": NC, "4": NC, "5": NC, "6": NC, "7": NC,
    "8": NC, "9": NC, "10": "UART_RX", "11": "UART_TX", "12": "+3V3", "13": "GND", "14": NC,
}

# (reference, symbol, value, footprint, JLCPCB part, pins)
PARTS = [
    ("U1", "local:XIAO", "XIAO ESP32S3 / ESP32C3", "local:XIAO", "", XIAO),
    ("U2", "Logic_LevelTranslator:SN74LVC1T45DCK", "SN74LVC1T45", "Package_TO_SOT_SMD:SOT-363_SC-70-6", "C9382",
     {"1": "+3V3", "2": "GND", "3": "UART_TX", "4": "TGT_TX", "5": "+3V3", "6": "VREF"}),
    ("U3", "Logic_LevelTranslator:SN74LVC1T45DCK", "SN74LVC1T45", "Package_TO_SOT_SMD:SOT-363_SC-70-6", "C9382",
     {"1": "+3V3", "2": "GND", "3": "UART_RX", "4": "TGT_RX", "5": "GND", "6": "VREF"}),
    ("J1", "Connector_Generic:Conn_01x04", "UART", "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Horizontal", "C32713263",
     {"4": "GND", "3": "J1_TX", "2": "J1_RX", "1": "VREF"}),
    # For a target wired with three wires only (GND, TX, RX) at 3.3 V: bridging it makes
    # the XIAO's 3.3 V the target side's supply. Open as made. Never bridge it and wire V.
    ("JP1", "Jumper:SolderJumper_2_Open", "V = 3V3", "Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm", "",
     {"1": "+3V3", "2": "VREF"}),
]

# JLCPCB part numbers by value (0402 1 % resistors; X5R/X7R capacitors), as in ../board.
R_PARTS = {"330": "C25104", "100k": "C25741"}
C_PARTS = {"100n": "C1525"}


def r(ref, value, a, b):
    PARTS.append((ref, "Device:R", value, R0402, R_PARTS[value], {"1": a, "2": b}))


def c(ref, value, a, b="GND"):
    PARTS.append((ref, "Device:C", value, C0402, C_PARTS[value], {"1": a, "2": b}))


c("C1", "100n", "+3V3")
c("C2", "100n", "VREF")
# 330 ohm keeps a TX-to-TX miswire under the translators' 24 mA output rating.
r("R1", "330", "TGT_TX", "J1_TX")
r("R2", "330", "J1_RX", "TGT_RX")
# Both translator inputs are held high while nothing drives them (the XIAO's UART until
# the firmware sets it up; the target when it is off or absent).
r("R3", "100k", "+3V3", "UART_TX")
r("R4", "100k", "+3V3", "UART_RX")
r("R5", "100k", "VREF", "TGT_RX")
# V halved for the ADC (5.5 V becomes 2.75 V); the capacitor gives the converter a
# low-impedance source.
r("R6", "100k", "VREF", "VREF_SENSE")
r("R7", "100k", "VREF_SENSE", "GND")
c("C3", "100n", "VREF_SENSE")

# Nets supplied from outside the board, which KiCad's check cannot see as driven.
POWER_FLAGS = ["GND", "VREF", "+3V3"]

# Symbols not in KiCad's libraries: (pin number, name, electrical type, side).
LOCAL_SYMBOLS = {
    "XIAO": [("1", "D0", "bidirectional", "L"), ("2", "D1", "bidirectional", "L"), ("3", "D2", "bidirectional", "L"),
             ("4", "D3", "bidirectional", "L"), ("5", "D4", "bidirectional", "L"), ("6", "D5", "bidirectional", "L"),
             ("7", "D6", "bidirectional", "L"),
             ("14", "5V", "passive", "R"), ("13", "GND", "power_in", "R"), ("12", "3V3", "passive", "R"),
             ("11", "D10", "bidirectional", "R"), ("10", "D9", "bidirectional", "R"), ("9", "D8", "bidirectional", "R"),
             ("8", "D7", "bidirectional", "R")],
}
