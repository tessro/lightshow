//! Gigabyte Aorus motherboard RGB Fusion 2.0 (ITE USB) backend.
//!
//! Implementation follows `docs/PROTOCOL_gigabyte_rgb_fusion2.md`. All
//! command bytes, offsets, and sequences come from that file — read it
//! before changing this module.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hidapi::{HidApi, HidDevice};

use crate::backend::{Backend, Color, Device, DeviceId, Zone};

const ITE_VID: u16 = 0x048D;
const FUSION2_PIDS: &[u16] = &[0x8297, 0x8950, 0x5702, 0x5711];
const FUSION2_USAGE_PAGE: u16 = 0xFF89;
const FUSION2_USAGE: u16 = 0x00CC;

const PACKET_LEN: usize = 64;
const REPORT_ID: u8 = 0xCC;

// Opcodes (byte 1 of every packet).
const OP_EFFECT_HEADER_BASE: u8 = 0x20; // 0x20..0x27 = effect for LED i, 0x20 with mask = all
const OP_APPLY: u8 = 0x28;
const OP_FREEZE_HW_EFFECTS: u8 = 0x32;
const OP_SET_LED_COUNT: u8 = 0x34;
const OP_LAMP_ARRAY: u8 = 0x48;

// Inside the effect packet.
const EFFECT_TYPE_STATIC: u8 = 0x01;
const FULL_BRIGHTNESS: u8 = 0xFF;

// ARGB header bytes — go in byte 1 of the strip-data packet.
const ARGB_HEADERS: &[u8] = &[0x58, 0x59, 0x62, 0x63];

// LEDs per strip we paint by default. 32 is the smallest length code the
// firmware supports and covers most fan setups (a 3-pack of typical ARGB
// fans is ~24 LEDs total).
const DEFAULT_LEDS_PER_HEADER: usize = 32;
const LEDS_PER_PACKET: usize = 19;
// Mask passed to opcode 0x32 to disable the firmware's effect engine on
// every ARGB header. The bits are zone-purpose markers, NOT 1<<led_index:
//   0x01  main effect zone / HDR_D_LED1
//   0x02  HDR_D_LED2
//   0x08  HDR_D_LED3
//   0x10  HDR_D_LED4   (bit 0x04 is unused by the firmware)
const ARGB_FREEZE_MASK: u8 = 0x01 | 0x02 | 0x08 | 0x10;

pub struct GigabyteMobo;

impl GigabyteMobo {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for GigabyteMobo {
    fn name(&self) -> &'static str {
        "gigabyte-mobo"
    }

    fn enumerate(&self) -> Result<Vec<Device>> {
        let api = HidApi::new().context("init hidapi")?;
        let mut devices = Vec::new();
        for info in api.device_list() {
            if info.vendor_id() != ITE_VID || !FUSION2_PIDS.contains(&info.product_id()) {
                continue;
            }
            if info.usage_page() != FUSION2_USAGE_PAGE || info.usage() != FUSION2_USAGE {
                continue;
            }
            let path = info.path().to_string_lossy().into_owned();
            let name = format!(
                "Gigabyte RGB Fusion 2.0 (ITE {:04x}:{:04x})",
                info.vendor_id(),
                info.product_id()
            );
            devices.push(Device {
                id: DeviceId {
                    backend: "gigabyte-mobo",
                    key: path,
                },
                vendor: "Gigabyte",
                name,
                zones: vec![Zone {
                    name: "all".into(),
                    led_count: 0,
                }],
            });
        }
        Ok(devices)
    }

    fn set_static(&self, device: &Device, color: Color) -> Result<()> {
        if device.id.backend != "gigabyte-mobo" {
            bail!("device {} is not a Gigabyte RGB Fusion 2 device", device.id);
        }
        let pid = pid_from_path(&device.id.key);
        let api = HidApi::new().context("init hidapi")?;
        let hid = api
            .open_path(std::ffi::CString::new(device.id.key.as_bytes())?.as_c_str())
            .with_context(|| format!("open {}", device.id))?;

        // 1. Take exclusive control away from Windows' LampArray driver.
        send_short(&hid, OP_LAMP_ARRAY, 0x00, 0x00)?;

        // 2. Drive non-addressable zones (onboard LEDs, basic 12V RGB headers)
        //    via a hardware "static" effect on all 8 effect zones.
        send_effect_all(&hid, pid, color)?;
        send_apply(&hid, pid)?;

        // 3. Hand the ARGB headers (5V addressable strips/fans) to per-pixel
        //    direct mode and paint every LED the same color.
        send_short(&hid, OP_FREEZE_HW_EFFECTS, ARGB_FREEZE_MASK, 0x00)?;
        sleep(Duration::from_millis(50));
        send_short(&hid, OP_SET_LED_COUNT, 0x00, 0x00)?; // LEDS_32 for all four headers
        for &hdr in ARGB_HEADERS {
            send_strip_uniform(&hid, hdr, color, DEFAULT_LEDS_PER_HEADER)?;
        }
        Ok(())
    }
}

/// Build and send a "short" packet: `[CC, op, a, b, 0, 0, ...]`.
fn send_short(hid: &HidDevice, op: u8, a: u8, b: u8) -> Result<()> {
    let mut buf = [0u8; PACKET_LEN];
    buf[0] = REPORT_ID;
    buf[1] = op;
    buf[2] = a;
    buf[3] = b;
    hid.send_feature_report(&buf)
        .with_context(|| format!("send_feature_report op=0x{op:02x}"))?;
    Ok(())
}

/// Build the effect packet that paints every onboard LED zone the same
/// static color, then send it. Layout per PROTOCOL.md §"Effect packet".
fn send_effect_all(hid: &HidDevice, pid: u16, color: Color) -> Result<()> {
    let mut buf = [0u8; PACKET_LEN];
    buf[0] = REPORT_ID;
    buf[1] = OP_EFFECT_HEADER_BASE; // 0x20 with mask = "all zones"

    // zone_mask_lo (LE u32) at bytes 2..6.
    let zone_mask: u32 = if pid == 0x5711 { 0x07FF } else { 0x00FF };
    buf[2..6].copy_from_slice(&zone_mask.to_le_bytes());
    // zone_mask_hi (bytes 6..10) stays 0.
    // reserved (byte 10) stays 0.

    buf[11] = EFFECT_TYPE_STATIC;
    buf[12] = FULL_BRIGHTNESS;
    // min_brightness (byte 13) stays 0.

    // color0 at bytes 14..18 in BGR0 order.
    buf[14] = color.b;
    buf[15] = color.g;
    buf[16] = color.r;
    buf[17] = 0;
    // color1, periods, params, padding all stay 0.

    hid.send_feature_report(&buf)
        .context("send effect packet")?;
    Ok(())
}

fn send_apply(hid: &HidDevice, pid: u16) -> Result<()> {
    // PID 5711 wants byte 3 = 0x07; everyone else gets 0x00.
    let b = if pid == 0x5711 { 0x07 } else { 0x00 };
    send_short(hid, OP_APPLY, 0xFF, b)
}

/// Paint `led_count` LEDs of one ARGB header to a uniform color. Chunks the
/// data into up to 19 LEDs per packet; the controller drives the strip after
/// it receives the full LED count (no separate apply).
fn send_strip_uniform(
    hid: &HidDevice,
    header: u8,
    color: Color,
    led_count: usize,
) -> Result<()> {
    let mut leds_sent: usize = 0;
    while leds_sent < led_count {
        let leds_in_pkt = (led_count - leds_sent).min(LEDS_PER_PACKET);
        let mut buf = [0u8; PACKET_LEN];
        buf[0] = REPORT_ID;
        buf[1] = header;
        let boffset = (leds_sent * 3) as u16;
        buf[2..4].copy_from_slice(&boffset.to_le_bytes());
        buf[4] = (leds_in_pkt * 3) as u8;
        // Default WS2812B byte order is GRB. Without calibration data from
        // the controller we just write GRB and hope the fans match. If they
        // come out wrong-colored, the calibration reads need to be added.
        for i in 0..leds_in_pkt {
            let off = 5 + i * 3;
            buf[off] = color.g;
            buf[off + 1] = color.r;
            buf[off + 2] = color.b;
        }
        hid.send_feature_report(&buf)
            .with_context(|| format!("strip write header=0x{header:02x} off={boffset}"))?;
        leds_sent += leds_in_pkt;
    }
    Ok(())
}

/// Extract the PID embedded in a Windows HID device path. Falls back to 0
/// (which is treated as "non-5711") if the path doesn't contain a PID
/// token, which means we'd send the more common 0x5702 variant of apply.
fn pid_from_path(path: &str) -> u16 {
    // Path looks like `\\?\HID#VID_048D&PID_5702&Col02#...`.
    let upper = path.to_ascii_uppercase();
    let Some(idx) = upper.find("PID_") else {
        return 0;
    };
    let hex = &upper[idx + 4..];
    let hex = hex.split(|c: char| !c.is_ascii_hexdigit()).next().unwrap_or("");
    u16::from_str_radix(hex, 16).unwrap_or(0)
}

/// Diagnostic helper used by the `raw-write` CLI subcommand.
pub fn raw_write(hid: &HidDevice, payload: &[u8]) -> Result<()> {
    if payload.len() > PACKET_LEN - 1 {
        bail!(
            "payload too long: {} bytes, max {}",
            payload.len(),
            PACKET_LEN - 1
        );
    }
    let mut buf = [0u8; PACKET_LEN];
    buf[0] = REPORT_ID;
    buf[1..1 + payload.len()].copy_from_slice(payload);
    hid.send_feature_report(&buf)
        .context("send_feature_report(0xCC)")?;
    Ok(())
}
