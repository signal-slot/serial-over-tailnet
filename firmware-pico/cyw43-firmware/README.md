# CYW43439 firmware

The Wi-Fi chip on the Raspberry Pi Pico 2 W runs these blobs, which the
firmware embeds:

- `43439A0.bin`: the chip's firmware.
- `43439A0_clm.bin`: its country location matrix.
- `nvram_rp2040.bin`: the NVRAM settings for the Pico W module (made from
  `nvram_rp2040.txt`).

They were copied unchanged from the `cyw43-firmware` directory of
https://github.com/embassy-rs/embassy at commit
1c5f29e0256d1853daeb1de866d53489614d3a6f, which took them from
https://github.com/georgerobotics/cyw43-driver/tree/main/firmware.

They are not under this repository's MIT licence: they are distributed under
the Infineon Permissive Binary License 1.0, in
`LICENSE-permissive-binary-license-1.0.txt`.
