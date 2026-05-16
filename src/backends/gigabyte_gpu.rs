//! Gigabyte RTX 40-series RGB Fusion 2 backend (GPU-side ITE chip).
//!
//! Sends 8-byte SMBus block writes to the RGB controller chip on the GPU's
//! internal I²C bus, reached via NVAPI's `NvAPI_I2CWriteEx`. Protocol and
//! NVAPI plumbing are documented in
//! `docs/PROTOCOL_gigabyte_gpu_rgb_fusion2.md`.

use anyhow::{Context, Result, bail};

use crate::backend::{Backend, Color, Device, DeviceId, Zone};
use crate::nvapi::{Nvapi, NvPhysicalGpuHandle};

const NVIDIA_VEN: u32 = 0x10DE;
const GIGABYTE_SUB_VEN: u32 = 0x1458;

// Recognized cards: (NVIDIA device id, Gigabyte subsystem device id, friendly name).
const KNOWN_CARDS: &[(u32, u32, &str)] = &[
    (0x2684, 0x40BF, "Gigabyte GeForce RTX 4090 GAMING OC"),
    (0x2684, 0x21D1, "AORUS GeForce RTX 4090 MASTER"),
    (0x2684, 0x40BD, "Gigabyte GeForce RTX 4090 AERO OC"),
];

// I²C address of the RGB controller on Gigabyte 40-series cards (7-bit form).
const I2C_ADDR: u8 = 0x71;
// All 40-series cards use I²C port 1.
const I2C_PORT: u8 = 1;

// Register/opcode constants — see PROTOCOL_gigabyte_gpu_rgb_fusion2.md.
const REG_COLOR_SINGLE: u8 = 0x40;
const REG_MODE: u8 = 0x88;
const REG_COLOR_LEFT_MID: u8 = 0xB0; // zones 0 and 1
const REG_COLOR_RIGHT: u8 = 0xB1; // zone 2

const MODE_STATIC: u8 = 0x01;
const SPEED_NORMAL: u8 = 0x02;
const BRIGHTNESS_MAX: u8 = 0x63; // 99

const NUM_ZONES: u8 = 4;

pub struct GigabyteGpu;

impl GigabyteGpu {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for GigabyteGpu {
    fn name(&self) -> &'static str {
        "gigabyte-gpu"
    }

    fn enumerate(&self) -> Result<Vec<Device>> {
        let mut nvapi = match Nvapi::load() {
            Ok(n) => n,
            Err(e) => {
                // Treat absence of NVAPI as "no devices" rather than an error
                // so a system without NVIDIA hardware doesn't fail `list`.
                eprintln!("note: NVAPI unavailable: {e:#}");
                return Ok(Vec::new());
            }
        };
        nvapi.initialize().context("NvAPI_Initialize")?;
        let handles = nvapi.enum_physical_gpus()?;
        let mut devices = Vec::new();
        for (idx, handle) in handles.iter().enumerate() {
            let (device_id, subsystem_id, _rev, _ext) = nvapi.gpu_pci_identifiers(*handle)?;
            let pci_vendor = device_id & 0xFFFF;
            let pci_device = device_id >> 16;
            let sub_vendor = subsystem_id & 0xFFFF;
            let sub_device = subsystem_id >> 16;
            if pci_vendor != NVIDIA_VEN || sub_vendor != GIGABYTE_SUB_VEN {
                continue;
            }
            let Some((_, _, name)) = KNOWN_CARDS
                .iter()
                .find(|(d, s, _)| *d == pci_device && *s == sub_device)
            else {
                continue;
            };
            devices.push(Device {
                id: DeviceId {
                    backend: "gigabyte-gpu",
                    // `nvapi-gpu-{idx}` is stable across runs as long as the
                    // GPU enumeration order is stable (it is in practice).
                    key: format!("nvapi-{idx}"),
                },
                vendor: "Gigabyte",
                name: (*name).to_string(),
                zones: vec![Zone {
                    name: "all".into(),
                    led_count: 0,
                }],
            });
        }
        Ok(devices)
    }

    fn set_static(&self, device: &Device, color: Color) -> Result<()> {
        if device.id.backend != "gigabyte-gpu" {
            bail!("device {} is not a Gigabyte GPU device", device.id);
        }
        let idx: usize = device
            .id
            .key
            .strip_prefix("nvapi-")
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| anyhow::anyhow!("malformed device key {}", device.id.key))?;

        let mut nvapi = Nvapi::load().context("load NVAPI")?;
        nvapi.initialize()?;
        let handles = nvapi.enum_physical_gpus()?;
        let handle = *handles
            .get(idx)
            .ok_or_else(|| anyhow::anyhow!("GPU index {idx} no longer enumerates"))?;

        for zone in 0..NUM_ZONES {
            write_mode(&nvapi, handle, zone, color)?;
            write_color(&nvapi, handle, zone, color)?;
        }
        Ok(())
    }
}

fn write_mode(
    nvapi: &Nvapi,
    handle: NvPhysicalGpuHandle,
    zone: u8,
    _color: Color,
) -> Result<()> {
    let mut pkt: [u8; 8] = [
        REG_MODE,
        MODE_STATIC,
        SPEED_NORMAL,
        BRIGHTNESS_MAX,
        0x00, // mystery flag — 0 for static
        zone + 1,
        0x00,
        0x00,
    ];
    nvapi.i2c_write(handle, I2C_ADDR, I2C_PORT, &mut pkt)
}

fn write_color(
    nvapi: &Nvapi,
    handle: NvPhysicalGpuHandle,
    zone: u8,
    c: Color,
) -> Result<()> {
    let mut pkt: [u8; 8] = match zone {
        // Zones 0 and 1 share register 0xB0, which carries 6 color bytes
        // (zone-0 RGB then zone-1 RGB). For "all zones same color" we send
        // the same triplet twice.
        0 | 1 => [REG_COLOR_LEFT_MID, MODE_STATIC, c.r, c.g, c.b, c.r, c.g, c.b],
        2 => [REG_COLOR_RIGHT, MODE_STATIC, c.r, c.g, c.b, 0, 0, 0],
        _ => [REG_COLOR_SINGLE, c.r, c.g, c.b, zone + 1, 0, 0, 0],
    };
    nvapi.i2c_write(handle, I2C_ADDR, I2C_PORT, &mut pkt)
}
