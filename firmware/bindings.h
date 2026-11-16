// Extra ESP-IDF / component headers exposed to Rust as `esp_idf_sys::usb::*`.
// Only for chips with a USB host controller (not the ESP32-C3).
#include "soc/soc_caps.h"
#if SOC_USB_OTG_SUPPORTED
#include "usb/usb_host.h"
#include "usb/cdc_acm_host.h"
#endif
