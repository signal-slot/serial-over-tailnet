#!/usr/bin/env bash
# Builds the flash images (bootloader, partition table, app, and the three
# merged into one) for a module variant. Run from firmware/ after building
# with the same variant:
#
#   ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;variants/n8r2.defaults" cargo build --release
#   tools/mkimage.sh n8r2 [profile] [outdir]
#
# The variant's defaults file (variants/<variant>.defaults) gives the flash
# size, which picks the partition table; the firmware must have been built
# with the same file.
set -euo pipefail
variant=${1:?variant such as n16r8}
profile=${2:-release}
out=${3:-dist/$variant}
defaults=variants/$variant.defaults
[ -f "$defaults" ] || { echo "unknown variant $variant (see variants/)" >&2; exit 1; }
flash_mb=$(sed -n 's/^CONFIG_ESPTOOLPY_FLASHSIZE="\([0-9]*\)MB"/\1/p' "$defaults")
[ -n "$flash_mb" ] || { echo "$defaults does not set CONFIG_ESPTOOLPY_FLASHSIZE" >&2; exit 1; }
case $flash_mb in
  4) partitions=partitions-4m.csv ;;
  *) partitions=partitions.csv ;;
esac
flash_mode=dio
target=${CARGO_TARGET_DIR:-target}/xtensa-esp32s3-espidf/$profile
py=$(ls -d .embuild/espressif/python_env/*/bin/python | head -1)
idf=.embuild/espressif/esp-idf/v5.3.3
mkdir -p "$out"
"$py" "$idf/components/partition_table/gen_esp32part.py" "$partitions" "$out/partition-table.bin" >/dev/null
"$py" -m esptool --chip esp32s3 elf2image --flash_mode "$flash_mode" --flash_freq 80m --flash_size "${flash_mb}MB" \
  -o "$out/app.bin" "$target/usb-serial-over-tailscale-firmware" >/dev/null
cp "$target/bootloader.bin" "$out/bootloader.bin"
"$py" -m esptool --chip esp32s3 merge_bin --flash_mode "$flash_mode" --flash_freq 80m --flash_size "${flash_mb}MB" \
  -o "$out/usb-serial-over-tailscale-$variant.bin" \
  0x0 "$out/bootloader.bin" 0x8000 "$out/partition-table.bin" 0x10000 "$out/app.bin" >/dev/null
ls -l "$out"
