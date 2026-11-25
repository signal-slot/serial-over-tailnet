#!/bin/sh
# Fails if a Rust function in an Xtensa firmware ELF realigns its stack
# pointer right after `entry` with a plain add.
#
# The Xtensa compiler does this for a frame that holds a value aligned to
# more than 16 bytes (std's `mpsc` channels hold some aligned to 64). The
# windowed ABI allows the stack pointer to move only with `movsp`: when the
# caller's registers have already been spilled, they are later restored from
# the wrong place, and the thread returns to a garbage address. This crashed
# the ESP32-S3 about once in 100 bridge connections.
#
# Usage, from firmware/: tools/check-stack-realign.sh <elf>
set -eu

elf=${1:?usage: $0 <elf>}
objdump=$(ls -d .embuild/espressif/tools/esp-clang/*/esp-clang/bin/llvm-objdump 2>/dev/null | sort -V | tail -1)
[ -x "$objdump" ] || { echo "no llvm-objdump in .embuild (build the firmware first)" >&2; exit 1; }

# Rust symbols (v0 mangling, `_R...`) whose first instructions after `entry`
# add to a1. (The C code's GCC does not do this; LLVM mis-decodes some of its
# data as such instructions, so C symbols are not looked at.)
found=$("$objdump" -d --mcpu=esp32s3 --no-show-raw-insn -j .flash.text -j .iram0.text "$elf" 2>/dev/null |
    awk '/^[0-9a-f]+ <.*>:$/ { fn = $2; since = 99 }
         /\tentry\t/ { since = 0; next }
         { since++ }
         since < 8 && /\t(add|add\.n|addi|sub)\ta1, a1, / && fn ~ /^<_R/ { print fn }' |
    sort -u)

if [ -n "$found" ]; then
    echo "these functions realign the stack pointer without movsp (an over-aligned local, such as an mpsc channel being created):" >&2
    echo "$found" >&2
    exit 1
fi
echo "stack realignment: none"
