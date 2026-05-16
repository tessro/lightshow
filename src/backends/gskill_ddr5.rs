//! G.Skill Trident Z5 DDR5 RGB backend.
//!
//! Talks to the ENE chip on each DIMM via PawnIO + the `SmbusPIIX4.bin`
//! pawn module. Protocol is documented in `docs/PROTOCOL_gskill_ddr5.md`.
//!
//! Setup the user must do once:
//!   1. Install PawnIO from https://pawnio.eu (and run lightshow elevated).
//!   2. Place `SmbusI801.bin` (Intel chipsets) and/or `SmbusPIIX4.bin` (AMD)
//!      somewhere this backend can find it. Default search paths
//!      (first hit wins per name):
//!        - `$LIGHTSHOW_PAWNIO_MODULES_DIR` env var (directory)
//!        - `%LOCALAPPDATA%\lightshow\`
//!        - `<exe dir>`
//!        - `C:\Program Files\PawnIO\Modules\`
//!        - the repo root (where `cargo run` is invoked)

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

use crate::backend::{Backend, Color, Device, DeviceId, Zone};
use crate::pawnio::{Module, Pawnio, SleepMode};

// SMBus addresses the ENE chip's I2C_ADDRESS register accepts when remapping.
// Sorted ascending; the order only matters as "which target gets assigned first"
// during a fresh remap, which has no functional consequence — detection scans
// the whole pool regardless of order.
const REMAP_POOL: &[u8] = &[
    0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x4F, 0x66, 0x67, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77,
];

// ENE register addresses (16-bit, big-endian-on-wire).
const ENE_REG_MICRON_CHECK: u16 = 0x1030;
const ENE_REG_COLORS_EFFECT: u16 = 0x8010; // 15 bytes (5 LEDs × RGB)
const ENE_REG_DIRECT: u16 = 0x8020;
const ENE_REG_MODE: u16 = 0x8021;
const ENE_REG_APPLY: u16 = 0x80A0;
const ENE_REG_SLOT_INDEX: u16 = 0x80F8;
const ENE_REG_I2C_ADDRESS: u16 = 0x80F9;
const ENE_REG_COLORS_EFFECT_V2: u16 = 0x8160; // 30 bytes (10 LEDs × RGB)

const ENE_MODE_STATIC: u8 = 0x01;
const ENE_APPLY_VAL: u8 = 0x01;
const ENE_DEFAULT_ADDR: u8 = 0x77;
const ENE_MAX_DIMM_SLOTS: u8 = 8;

pub struct GSkillDdr5;

impl GSkillDdr5 {
    pub fn new() -> Self {
        Self
    }
}

impl Backend for GSkillDdr5 {
    fn name(&self) -> &'static str {
        "gskill-ddr5"
    }

    fn enumerate(&self) -> Result<Vec<Device>> {
        // Loading PawnIO + the SMBus module is heavyweight and may probe a
        // bunch of addresses; do it lazily and surface why it gave up so
        // setup problems are debuggable (but don't fail `list`).
        let pawnio = match Pawnio::load() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("note: gskill-ddr5: load PawnIOLib.dll failed: {e:#}");
                return Ok(Vec::new());
            }
        };
        let (module, kind) = match open_smbus_module(&pawnio) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };
        configure(&module, kind)?;
        let addrs = remap_and_detect(&module)?;
        if addrs.is_empty() {
            eprintln!(
                "note: gskill-ddr5: PawnIO ({kind:?}) ready, no ENE controllers found"
            );
        }
        Ok(addrs
            .into_iter()
            .map(|addr| Device {
                id: DeviceId {
                    backend: "gskill-ddr5",
                    key: format!("ene-0x{addr:02x}"),
                },
                vendor: "G.Skill",
                name: "G.Skill DDR5 RGB (ENE)".into(),
                zones: vec![Zone {
                    name: "all".into(),
                    led_count: 0,
                }],
            })
            .collect())
    }

    fn set_static(&self, device: &Device, color: Color) -> Result<()> {
        if device.id.backend != "gskill-ddr5" {
            bail!("device {} is not a G.Skill DDR5 device", device.id);
        }
        let addr = device
            .id
            .key
            .strip_prefix("ene-0x")
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .ok_or_else(|| anyhow!("malformed device key `{}`", device.id.key))?;

        let pawnio = Pawnio::load()?;
        let (module, kind) = open_smbus_module(&pawnio)
            .ok_or_else(|| anyhow!("no SMBus pawn module loaded — see backend docs"))?;
        configure(&module, kind)?;

        // Turn off direct mode so the chip displays the effect registers
        // we're about to write. If we leave direct mode on, our effect-color
        // writes land but are ignored because the chip is showing the
        // direct-color registers (0x8000) instead.
        ene_write(&module, addr, ENE_REG_DIRECT, 0x00)?;
        ene_write(&module, addr, ENE_REG_MODE, ENE_MODE_STATIC)?;
        ene_write_color_block(&module, addr, ENE_REG_COLORS_EFFECT, color, 5)?;
        ene_write_color_block(&module, addr, ENE_REG_COLORS_EFFECT_V2, color, 10)?;
        ene_write(&module, addr, ENE_REG_APPLY, ENE_APPLY_VAL)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum SmbusKind {
    IntelI801,
    AmdPiix4,
}

fn module_filename(kind: SmbusKind) -> &'static str {
    match kind {
        SmbusKind::IntelI801 => "SmbusI801.bin",
        SmbusKind::AmdPiix4 => "SmbusPIIX4.bin",
    }
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("LIGHTSHOW_PAWNIO_MODULES_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let mut p = PathBuf::from(local_appdata);
        p.push("lightshow");
        dirs.push(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs.push(PathBuf::from(r"C:\Program Files\PawnIO\Modules"));
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    dirs
}

fn locate_module(kind: SmbusKind) -> Option<PathBuf> {
    let filename = module_filename(kind);
    search_dirs()
        .into_iter()
        .map(|d| d.join(filename))
        .find(|p| p.exists())
}

/// Try the Intel module first, then the AMD module. Returns the loaded
/// module and which kind succeeded. If neither loads, prints a diagnostic
/// and returns None.
fn open_smbus_module(pawnio: &Pawnio) -> Option<(Module<'_>, SmbusKind)> {
    let kinds = [SmbusKind::IntelI801, SmbusKind::AmdPiix4];
    let mut last_err: Option<String> = None;
    for kind in kinds {
        let Some(path) = locate_module(kind) else {
            continue;
        };
        match pawnio.open_with_module(&path) {
            Ok(m) => return Some((m, kind)),
            Err(e) => {
                last_err = Some(format!("{} → {e:#}", path.display()));
            }
        }
    }
    if let Some(e) = last_err {
        eprintln!("note: gskill-ddr5: SMBus pawn module load failed: {e}");
    } else {
        eprintln!(
            "note: gskill-ddr5: neither SmbusI801.bin nor SmbusPIIX4.bin found \
             (set $LIGHTSHOW_PAWNIO_MODULES_DIR to a directory containing them)"
        );
    }
    None
}

fn configure(module: &Module<'_>, kind: SmbusKind) -> Result<()> {
    match kind {
        SmbusKind::AmdPiix4 => {
            module.set_sleep_mode(SleepMode::AlwaysSleep)?;
            // DDR5 on AMD lives on SMBus port 0.
            module.piix4_port_sel(0)?;
        }
        SmbusKind::IntelI801 => {
            // I801 doesn't expose port_sel or set_sleep_mode IOCTLs.
        }
    }
    Ok(())
}

/// Run the G.Skill DIMM remap procedure (idempotent — skips if no DIMM
/// answers at 0x77), then probe every address in REMAP_POOL for an ENE
/// controller. Returns the addresses of valid controllers found.
fn remap_and_detect(module: &Module<'_>) -> Result<Vec<u8>> {
    let mut next_target = 0usize;
    for slot in 0..ENE_MAX_DIMM_SLOTS {
        // If nothing answers at 0x77, either we've already remapped every
        // populated DIMM, or there are no G.Skill DIMMs at all.
        if !probe(module, ENE_DEFAULT_ADDR) {
            break;
        }
        // Find an address in the pool that's currently free.
        let target = loop {
            if next_target >= REMAP_POOL.len() {
                break None;
            }
            let candidate = REMAP_POOL[next_target];
            next_target += 1;
            if !probe(module, candidate) {
                break Some(candidate);
            }
        };
        let Some(target) = target else {
            // Out of remap targets; stop trying to enumerate further DIMMs.
            break;
        };
        ene_write(module, ENE_DEFAULT_ADDR, ENE_REG_SLOT_INDEX, slot)?;
        ene_write(
            module,
            ENE_DEFAULT_ADDR,
            ENE_REG_I2C_ADDRESS,
            target << 1, // ENE wants the 8-bit address form
        )?;
    }

    // Now probe each pool address for a working ENE controller.
    let mut found = Vec::new();
    for &addr in REMAP_POOL {
        if probe_ene_controller(module, addr).unwrap_or(false) {
            found.push(addr);
        }
    }
    Ok(found)
}

fn probe(module: &Module<'_>, addr: u8) -> bool {
    // A successful byte-data read from register 0x00 is a reliable ACK
    // signal across SMBus controllers (more than write-quick, which the
    // pawn module's status code for is fuzzy).
    module.read_byte_data(addr, 0x00).is_ok()
}

fn probe_ene_controller(module: &Module<'_>, addr: u8) -> Result<bool> {
    if !probe(module, addr) {
        return Ok(false);
    }
    // ENE chips return [0x00, 0x01, ..., 0x0F] when register range
    // 0xA0..0xAF is read. Spot-check three values.
    for i in [0u8, 5, 15] {
        match module.read_byte_data(addr, 0xA0 + i) {
            Ok(v) if v == i => {}
            _ => return Ok(false),
        }
    }
    // Crucial (Micron) DIMMs use the same chip with different firmware;
    // skip them by looking for "Micron" in the device-name register.
    let mut name = [0u8; 6];
    for (offset, slot) in name.iter_mut().enumerate() {
        *slot = ene_read(module, addr, ENE_REG_MICRON_CHECK + offset as u16)?;
    }
    if &name == b"Micron" {
        return Ok(false);
    }
    Ok(true)
}

fn ene_write(module: &Module<'_>, addr: u8, reg: u16, value: u8) -> Result<()> {
    // Latch the 16-bit ENE register address (big-endian on the wire).
    let swapped = reg.swap_bytes();
    module.write_word_data(addr, 0x00, swapped)?;
    module.write_byte_data(addr, 0x01, value)
}

fn ene_read(module: &Module<'_>, addr: u8, reg: u16) -> Result<u8> {
    let swapped = reg.swap_bytes();
    module.write_word_data(addr, 0x00, swapped)?;
    module.read_byte_data(addr, 0x81)
}

/// Read an N-byte block starting at ENE register `start_reg`. Convenience
/// for diagnostics — uses one ene_read per byte (no block read support
/// at the SMBus level for ENE's 16-bit register space).
pub fn ene_read_block(
    module: &Module<'_>,
    addr: u8,
    start_reg: u16,
    out: &mut [u8],
) -> Result<()> {
    for (offset, slot) in out.iter_mut().enumerate() {
        *slot = ene_read(module, addr, start_reg + offset as u16)?;
    }
    Ok(())
}

/// Ranges read by `probe_diag` for diffing across DIMMs. Broad full-chip
/// scan to find any per-DIMM register that differs.
pub const DIAG_RANGES: &[(u16, u16)] = &[
    (0x0000, 0x2000), // full low-memory area — device info, config tables, anything else
    (0x8020, 0x8200), // control register area, V1 + V2 effect regions
];

/// Diagnostic helper used by the `gskill-probe` subcommand: probe each
/// detected ENE controller and return its 16-byte device-name string plus
/// a wide register dump for cross-DIMM diffing.
pub fn probe_diag() -> Result<Vec<DiagEntry>> {
    let pawnio = Pawnio::load()?;
    let (module, kind) = open_smbus_module(&pawnio)
        .ok_or_else(|| anyhow!("SMBus pawn module didn't load"))?;
    configure(&module, kind)?;
    let addrs = remap_and_detect(&module)?;
    let mut entries = Vec::new();
    for addr in addrs {
        let mut name = [0u8; 16];
        ene_read_block(&module, addr, 0x1000, &mut name).ok();
        // First-LED current effect color (both register banks) for quick
        // sanity-check of color writes.
        let mut v1_first = [0u8; 3];
        let mut v2_first = [0u8; 3];
        ene_read_block(&module, addr, 0x8010, &mut v1_first).ok();
        ene_read_block(&module, addr, 0x8160, &mut v2_first).ok();
        // Wide dump of (register, value) pairs for diffing.
        let mut dump: Vec<(u16, Option<u8>)> = Vec::new();
        for &(start, end) in DIAG_RANGES {
            for reg in start..end {
                dump.push((reg, ene_read(&module, addr, reg).ok()));
            }
        }
        entries.push(DiagEntry {
            addr,
            name,
            v1_first,
            v2_first,
            dump,
        });
    }
    Ok(entries)
}

pub struct DiagEntry {
    pub addr: u8,
    pub name: [u8; 16],
    pub v1_first: [u8; 3],
    pub v2_first: [u8; 3],
    pub dump: Vec<(u16, Option<u8>)>,
}

/// Write `led_count` color triplets to the ENE color register block. The
/// chip's on-wire byte order per triplet is **R, B, G** (not R, G, B) —
/// observed empirically when `ff0055` came out orange instead of magenta.
fn ene_write_color_block(
    module: &Module<'_>,
    addr: u8,
    start_reg: u16,
    color: Color,
    led_count: u16,
) -> Result<()> {
    for led in 0..led_count {
        let base = start_reg + led * 3;
        ene_write(module, addr, base, color.r)?;
        ene_write(module, addr, base + 1, color.b)?;
        ene_write(module, addr, base + 2, color.g)?;
    }
    Ok(())
}
