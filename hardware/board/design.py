"""The adapter board, described once: parts, footprints, JLCPCB part numbers and nets.

`gen_sch.py` turns this into the KiCad schematic. Every pin of every part is named here:
a net name, or NC for a pin deliberately left open.

Two USB-C ports, A and B, are built the same. Either takes the charger or PC and the other
the target; the TUSB320LAI of each port tells which is which, and the LM66200 passes the
5 V of the port that supplies it. A 4-pin connector carries a UART at the target's voltage.
"""

NC = None

# Reference designs and datasheets the values come from:
#   ESP32-S3-MINI-1 datasheet v1.7, figure 9-1: 22 uF + 0.1 uF on 3V3, EN 10 k / 1 uF.
#   TUSB320LAI (SLLSEO4): VBUS_DET through 900 k (855..920 k), ID / INT_N pulled up with 200 k,
#       ADDR low = I2C address 0x47, high = 0x67, PORT open = dual role.
#   LM66200 (SLVSG04): ST is open drain, high while VIN1 supplies the output.
#   AP22653 (SOT26): 1 IN, 2 GND, 3 EN (high = on), 4 FAULT, 5 ILIM, 6 OUT; 49.9 k = 490 mA.
#   TS3USB221 (RSE): S low = D to 1D, S high = D to 2D, OE low = on.
#   SN74LVC1T45: DIR (referenced to VCCA) high = A to B, low = B to A; either supply at
#       GND isolates both sides.

USB_C = "Connector:USB_C_Receptacle_USB2.0_16P"
USB_C_FP = "Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12"
R0402 = "Resistor_SMD:R_0402_1005Metric"
C0402 = "Capacitor_SMD:C_0402_1005Metric"
C0603 = "Capacitor_SMD:C_0603_1608Metric"


def usb_c(port):
    return {
        "A4": f"VBUS_{port}", "A9": f"VBUS_{port}", "B4": f"VBUS_{port}", "B9": f"VBUS_{port}",
        "A1": "GND", "A12": "GND", "B1": "GND", "B12": "GND", "SH": "GND",
        "A5": f"CC1_{port}", "B5": f"CC2_{port}",
        "A6": f"DP_{port}", "B6": f"DP_{port}", "A7": f"DN_{port}", "B7": f"DN_{port}",
        "A8": NC, "B8": NC,
    }


def tusb320(port, addr):
    return {
        "1": f"CC1_{port}", "2": f"CC2_{port}", "3": NC, "4": f"VDET_{port}", "5": addr,
        "6": "TUSB_INT", "7": "I2C_SDA", "8": "I2C_SCL", "9": f"ID_{port}",
        "10": "GND", "11": "GND", "12": "+3V3",
    }


def ap22653(port):
    return {"1": "+5V", "2": "GND", "3": f"VBUS_EN_{port}", "4": f"FAULT_{port}",
            "5": f"ILIM_{port}", "6": f"VBUS_{port}"}


def usblc6(port):
    return {"1": f"DP_{port}", "6": f"DP_{port}", "3": f"DN_{port}", "4": f"DN_{port}",
            "5": f"VBUS_{port}", "2": "GND"}


ESP = {str(n): "GND" for n in [1, 2, 42, 43] + list(range(46, 66))}
# The GPIOs are chosen by where the pin is on the module, so that the tracks are short:
# pins 4-15 are on the side of port A, 16-30 along the edge with the UART connector,
# 31-45 on the side of port B. (ADC2, GPIO11-20, is not used: Wi-Fi takes it.)
ESP.update({
    "3": "+3V3", "45": "EN", "4": "BOOT",
    "23": "USB_DN", "24": "USB_DP",
    "39": "TXD0", "40": "RXD0",
    # port A side
    "5": "VREF_SENSE",      # IO1, ADC1_CH0
    "6": "LED",             # IO2
    "8": "ID_A",            # IO4
    "9": "FAULT_A",         # IO5
    "10": "VBUS_EN_A",      # IO6
    "12": "I2C_SDA",        # IO8
    "13": "I2C_SCL",        # IO9
    "14": "TUSB_INT",       # IO10
    # the edge with the USB switch and the UART
    "25": "MUX_SEL",        # IO21; reads PWR_ST through R11 while it is an input
    "28": "UART_TX",        # IO33
    "29": "UART_RX",        # IO34
    # port B side (IO35-37 are free on this module: its PSRAM is quad, not octal)
    "34": "ID_B",           # IO38
    "35": "FAULT_B",        # IO39
    "36": "VBUS_EN_B",      # IO40
})
ESP["11"] = "PWR_ST"        # IO7: which port supplies the 5 V, readable at any time
for n in [7, 15, 16, 17, 18, 19, 20, 21, 22, 26, 27, 30, 31, 32, 33, 37, 38, 41, 44]:
    ESP[str(n)] = NC        # IO3 IO11-IO18 IO26 IO47 IO48 IO35-IO37 IO41 IO42 IO45 IO46

# (reference, symbol, value, footprint, JLCPCB part, pins)
PARTS = [
    ("U1", "RF_Module:ESP32-S3-MINI-1", "ESP32-S3-MINI-1-N4R2", "RF_Module:ESP32-S2-MINI-1", "C3013941", ESP),
    ("J1", USB_C, "USB-C A", USB_C_FP, "C165948", usb_c("A")),
    ("J2", USB_C, "USB-C B", USB_C_FP, "C165948", usb_c("B")),
    ("U2", "Interface_USB:TUSB320", "TUSB320LAI", "Package_DFN_QFN:Texas_X2QFN-12_1.6x1.6mm_P0.4mm", "C132554", tusb320("A", "GND")),
    ("U3", "Interface_USB:TUSB320", "TUSB320LAI", "Package_DFN_QFN:Texas_X2QFN-12_1.6x1.6mm_P0.4mm", "C132554", tusb320("B", "+3V3")),
    ("U4", "local:LM66200", "LM66200", "Package_TO_SOT_SMD:SOT-583-8", "C3235556",
     {"1": "GND", "5": "GND", "2": "+5V", "7": "+5V", "3": "VBUS_B", "6": "VBUS_A", "4": "GND", "8": "PWR_ST"}),
    ("U5", "local:AP22653", "AP22653W6", "Package_TO_SOT_SMD:SOT-23-6", "C2158037", ap22653("A")),
    ("U6", "local:AP22653", "AP22653W6", "Package_TO_SOT_SMD:SOT-23-6", "C2158037", ap22653("B")),
    ("U7", "local:TS3USB221", "TS3USB221", "Package_DFN_QFN:Texas_UQFN-10_1.5x2mm_P0.5mm", "C130085",
     {"1": "DP_A", "2": "DN_A", "3": "DP_B", "4": "DN_B", "5": "GND", "6": "GND",
      "7": "USB_DN", "8": "USB_DP", "9": "MUX_SEL", "10": "+3V3"}),
    ("U8", "Regulator_Linear:AP2112K-3.3", "AP2112K-3.3", "Package_TO_SOT_SMD:SOT-23-5", "C51118",
     {"1": "+5V", "2": "GND", "3": "+5V", "4": NC, "5": "+3V3"}),
    ("U9", "Logic_LevelTranslator:SN74LVC1T45DCK", "SN74LVC1T45", "Package_TO_SOT_SMD:SOT-363_SC-70-6", "C9382",
     {"1": "+3V3", "2": "GND", "3": "UART_TX", "4": "TGT_TX", "5": "+3V3", "6": "VREF"}),
    ("U10", "Logic_LevelTranslator:SN74LVC1T45DCK", "SN74LVC1T45", "Package_TO_SOT_SMD:SOT-363_SC-70-6", "C9382",
     {"1": "+3V3", "2": "GND", "3": "UART_RX", "4": "TGT_RX", "5": "GND", "6": "VREF"}),
    ("U11", "Power_Protection:USBLC6-2SC6", "USBLC6-2SC6", "Package_TO_SOT_SMD:SOT-23-6", "C7519", usblc6("A")),
    ("U12", "Power_Protection:USBLC6-2SC6", "USBLC6-2SC6", "Package_TO_SOT_SMD:SOT-23-6", "C7519", usblc6("B")),
    ("J3", "Connector_Generic:Conn_01x04", "UART", "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Horizontal", "C32713263",
     {"4": "GND", "3": "J3_TX", "2": "J3_RX", "1": "VREF_EXT"}),   # pin 1 is the right-hand hole
    # VREF, the target side's supply of the level translators: the header's VREF pin, or
    # 3.3 V for a target wired with three wires only (GND, TX, RX). The common contact is
    # pin 2, the tabs are the cover.
    ("SW2", "local:MSK12C02", "VREF", "local:MSK12C02", "C431540",
     {"1": "+3V3", "2": "VREF", "3": "VREF_EXT", "4": "GND"}),
    ("SW1", "Switch:SW_Push", "BOOT", "Button_Switch_SMD:SW_Push_1P1T_XKB_TS-1187A", "C318884", {"1": "BOOT", "2": "GND"}),
    ("D1", "Device:LED", "red", "LED_SMD:LED_0603_1608Metric", "C2286", {"1": "GND", "2": "LED_A"}),
]


# JLCPCB part numbers by value (0402 1 % resistors; X5R/X7R capacitors), as found on
# jlcpcb.com/parts on 2026-11-07.
R_PARTS = {"100": "C25076", "330": "C25104", "470": "C25117", "1k": "C11702", "4.7k": "C25900",
           "10k": "C25744", "49.9k": "C25897", "100k": "C25741", "200k": "C25764", "910k": "C25800"}
C_PARTS = {"100n": "C1525", "1u": "C52923", "4.7u": "C19666", "10u": "C19702", "22u": "C59461"}


def r(ref, value, a, b):
    PARTS.append((ref, "Device:R", value, R0402, R_PARTS[value], {"1": a, "2": b}))


def c(ref, value, a, b="GND", fp=C0402):
    PARTS.append((ref, "Device:C", value, fp, C_PARTS[value], {"1": a, "2": b}))


def tp(ref, net):
    PARTS.append((ref, "Connector:TestPoint", net, "TestPoint:TestPoint_Pad_D1.0mm", "", {"1": net}))


# Module
r("R1", "10k", "+3V3", "EN")
c("C1", "1u", "EN")
r("R2", "10k", "+3V3", "BOOT")
c("C2", "22u", "+3V3", fp=C0603)
c("C3", "100n", "+3V3")
r("R24", "470", "LED", "LED_A")      # about 3 mA through a red LED
# Power
c("C4", "10u", "+5V", fp=C0603)
c("C5", "10u", "+3V3", fp=C0603)
c("C6", "1u", "VBUS_A")             # LM66200 inputs
c("C7", "1u", "VBUS_B")
# AP22653 outputs: 0.1 u plus a bulk capacitor at the pin (its datasheet), kept under the
# 10 u a Type-C sink may present (TUSB320 datasheet, table 14): the same port is a sink
# when it is the PC's.
c("C15", "100n", "VBUS_A")
c("C16", "4.7u", "VBUS_A", fp=C0603)
c("C17", "100n", "VBUS_B")
c("C18", "4.7u", "VBUS_B", fp=C0603)
c("C13", "100n", "+5V")
c("C14", "100n", "+5V")
r("R10", "100k", "+3V3", "PWR_ST")
r("R11", "10k", "PWR_ST", "MUX_SEL")
r("R12", "100k", "VBUS_EN_A", "GND")
r("R13", "100k", "VBUS_EN_B", "GND")
r("R14", "100k", "+3V3", "FAULT_A")
r("R15", "100k", "+3V3", "FAULT_B")
r("R16", "49.9k", "ILIM_A", "GND")
r("R17", "49.9k", "ILIM_B", "GND")
# USB-C role detection
r("R3", "910k", "VBUS_A", "VDET_A")
r("R4", "910k", "VBUS_B", "VDET_B")
r("R5", "4.7k", "+3V3", "I2C_SDA")
r("R6", "4.7k", "+3V3", "I2C_SCL")
r("R7", "200k", "+3V3", "TUSB_INT")
r("R8", "200k", "+3V3", "ID_A")
r("R9", "200k", "+3V3", "ID_B")
c("C8", "100n", "+3V3")
c("C9", "100n", "+3V3")
c("C10", "100n", "+3V3")
# UART at the target's voltage
c("C11", "100n", "+3V3")
c("C12", "100n", "VREF")
# 330 ohm keeps a TX-to-TX miswire under the translators' 24 mA output rating.
r("R18", "330", "TGT_TX", "J3_TX")
r("R19", "330", "J3_RX", "TGT_RX")
# Both translator inputs are held high while nothing drives them (the module's UART1
# until the firmware sets it up; the target when it is off or absent).
r("R20", "100k", "+3V3", "UART_RX")
r("R25", "100k", "+3V3", "UART_TX")
r("R21", "100k", "VREF", "TGT_RX")
# VREF halved for the ADC; the capacitor gives the converter a low-impedance source.
r("R22", "100k", "VREF", "VREF_SENSE")
r("R23", "100k", "VREF_SENSE", "GND")
c("C19", "100n", "VREF_SENSE")
# Test pads
tp("TP1", "TXD0")
tp("TP2", "RXD0")
tp("TP3", "GND")
tp("TP4", "+3V3")
tp("TP5", "+5V")
tp("TP6", "EN")

# Nets that are supplied from outside the board (through a connector), which KiCad's
# check cannot see as driven. +5V, +3V3 and the two VBUS have a part that drives them.
POWER_FLAGS = ["GND", "VREF"]

# Symbols not in KiCad's libraries: (pin number, name, electrical type, side).
LOCAL_SYMBOLS = {
    "MSK12C02": [("1", "A", "passive", "L"), ("3", "B", "passive", "L"),
                 ("2", "COM", "passive", "R"), ("4", "TAB", "passive", "R")],
    "LM66200": [("3", "VIN1", "power_in", "L"), ("6", "VIN2", "power_in", "L"), ("4", "~{ON}", "input", "L"),
                ("1", "GND", "power_in", "L"), ("5", "GND", "power_in", "L"),
                ("2", "VOUT", "power_out", "R"), ("7", "VOUT", "passive", "R"), ("8", "ST", "open_collector", "R")],
    "AP22653": [("1", "IN", "power_in", "L"), ("3", "EN", "input", "L"), ("2", "GND", "power_in", "L"),
                ("6", "OUT", "power_out", "R"), ("4", "~{FAULT}", "open_collector", "R"), ("5", "ILIM", "passive", "R")],
    "TS3USB221": [("8", "D+", "bidirectional", "L"), ("7", "D-", "bidirectional", "L"), ("9", "S", "input", "L"),
                  ("6", "~{OE}", "input", "L"), ("5", "GND", "power_in", "L"),
                  ("10", "VCC", "power_in", "R"), ("1", "1D+", "bidirectional", "R"), ("2", "1D-", "bidirectional", "R"),
                  ("3", "2D+", "bidirectional", "R"), ("4", "2D-", "bidirectional", "R")],
}
