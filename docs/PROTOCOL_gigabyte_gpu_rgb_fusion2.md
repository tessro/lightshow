# Gigabyte RTX 40-series RGB Fusion 2 (GPU-side I²C) protocol notes

The RGB controller on Gigabyte AORUS / GAMING / AERO RTX 40-series cards
is an ITE chip sitting on one of the GPU's internal I²C buses. It speaks
a different (simpler) protocol than the motherboard's USB-attached
sibling chip: 8-byte SMBus block writes, no large reports, no
addressable strips.

## Card identification

Per-card identification is done via PCI vendor/device + subsystem IDs
combined with an I²C address.

| Card                      | PCI dev | Subsys dev | I²C addr |
| ------------------------- | ------- | ---------- | -------- |
| RTX 4090 GAMING OC 24G    | `2684`  | `40BF`     | `0x71`   |
| AORUS RTX 4090 MASTER 24G | `2684`  | `21D1`     | `0x71`   |
| RTX 4090 AERO OC 24G      | `2684`  | `40BD`     | `0x71`   |

PCI vendor `0x10DE` (NVIDIA), subsystem vendor `0x1458` (Gigabyte).

## Transport: NVAPI I²C

Windows has no userspace I²C API for GPU buses; we have to use NVIDIA's
proprietary `nvapi64.dll`. The DLL exports a single symbol,
`nvapi_QueryInterface(u32 function_id) -> void*`. Each NVAPI function
has a magic 32-bit ID; you fetch its pointer at runtime and cast.

Function IDs we need (these are stable across driver versions):

| Function                      | ID           |
| ----------------------------- | ------------ |
| `NvAPI_Initialize`            | `0x0150E828` |
| `NvAPI_Unload`                | `0xD22BDD7E` |
| `NvAPI_EnumPhysicalGPUs`      | `0xE5AC921F` |
| `NvAPI_GPU_GetPCIIdentifiers` | `0x2DDFB66E` |
| `NvAPI_I2CWriteEx`            | `0x283AC65A` |

All NVAPI functions return `NvAPI_Status` (an `i32`, 0 = success,
negative = error).

### Handle types

```c
typedef void* NvPhysicalGpuHandle;
```

Opaque. Treat as `*mut c_void` in Rust.

### `NvAPI_EnumPhysicalGPUs`

```c
NvAPI_Status NvAPI_EnumPhysicalGPUs(
    NvPhysicalGpuHandle handles[64],  // out
    uint32_t*           count          // out
);
```

Up to 64 GPUs. We iterate them.

### `NvAPI_GPU_GetPCIIdentifiers`

```c
NvAPI_Status NvAPI_GPU_GetPCIIdentifiers(
    NvPhysicalGpuHandle handle,
    uint32_t*           device_id,         // out: 0x10DE2684 (high=vendor, low=dev)
    uint32_t*           subsystem_id,      // out: 0x14584034 etc.
    uint32_t*           revision_id,       // out
    uint32_t*           external_device_id // out (unused for us)
);
```

The packed layout of `device_id` and `subsystem_id` is
**`(device << 16) | vendor`** — device in the high half, vendor in
the low half. So Gigabyte 4090 GAMING OC matches when
`device_id == 0x2684_10DE` AND `subsystem_id == 0x40BF_1458`.

### `NvAPI_I2CWriteEx`

```c
NvAPI_Status NvAPI_I2CWriteEx(
    NvPhysicalGpuHandle handle,
    NV_I2C_INFO_V3*     args,
    uint32_t*           unknown_out_perhaps_speed   // can be NULL
);
```

`NV_I2C_INFO_V3` is the heart of the call — version-fielded as is
standard for NVAPI:

```c
typedef struct {
    uint32_t version;          // 0x30000 | sizeof(struct) — see VERSION below
    uint32_t display_mask;     // 0 — talking to an internal bus, not a display
    uint8_t  is_ddc_port;      // 0
    uint8_t  device_address;   // 7-bit << 1 form: e.g. 0xE2 for 0x71
    uint8_t* register_address; // pointer to register-byte buffer
    uint32_t register_size;    // typically 0
    uint8_t* data;             // pointer to data bytes to send
    uint32_t size;             // length of `data`
    uint32_t i2c_speed;        // legacy field; set to 0xFFFF (= use default)
    uint8_t  i2c_speed_khz;    // 4 = 100 kHz "default", 1 = 10 kHz "slow"
    uint8_t  port_id;          // I²C port number on the GPU — see below
    uint32_t is_port_id_set;   // 1 if `port_id` is meaningful, else 0
} NV_I2C_INFO_V3;
```

Total size on **x64 Windows** is 60 bytes (natural alignment with
8-byte pointer alignment forcing two 4-byte pad regions). The version
field is computed as:

```
version = (3u32 << 16) | sizeof_struct
        = 0x30000 | 60
        = 0x3003C
```

Don't hardcode 60 — compute it from `size_of::<NvI2cInfoV3>()` so
future struct changes stay correct.

This is the standard NVAPI "MAKE_NVAPI_VERSION" pattern.

#### Device address quirk

NVAPI's `device_address` is the **8-bit form** — the 7-bit I²C address
shifted left by 1 with the R/W bit slot zero. The card-list address
`0x71` (7-bit) becomes `0xE2` (8-bit) in this struct.

#### Port ID

GPUs expose multiple I²C ports. The RGB controller typically lives on
port 1 on Gigabyte 40-series cards. If a write to port 1 returns an
error, fall back to trying ports 0..7 with `is_port_id_set = 1`.
If `is_port_id_set = 0`, NVAPI auto-selects, but for non-display
devices auto-select usually fails.

## Wire protocol (over I²C, after transport works)

The controller is byte-addressable. Every write is an 8-byte block:

```
[reg, b1, b2, b3, b4, b5, b6, b7]
```

Where `reg` is the register-of-interest and the rest are data
specific to that register.

### Registers

| Register | Name             | Purpose                                              |
| -------- | ---------------- | ---------------------------------------------------- |
| `0x88`   | MODE             | Set effect mode + speed/brightness + zone selector   |
| `0x40`   | COLOR (zone ≥ 3) | Set RGB for a single zone with the zone-index suffix |
| `0xB0`   | COLOR_LEFT_MID   | Set RGB for both zones 0 and 1 (6 color bytes total) |
| `0xB1`   | COLOR_RIGHT      | Set RGB for zone 2 (3 color bytes, rest zero)        |
| `0xAA`   | SAVE             | Persist current config to chip flash                 |

### Modes

| Mode value | Name          |
| ---------- | ------------- |
| `0x01`     | Static        |
| `0x02`     | Breathing     |
| `0x03`     | Color cycle   |
| `0x04`     | Flashing      |
| `0x05`     | Gradient      |
| `0x06`     | Color shift   |
| `0x07`     | Wave          |
| `0x08`     | Dual flashing |
| `0x0B`     | Tricolor      |

We only need `0x01` for v0.1.

### Brightness / speed ranges

- Brightness: `0x00..0x63` (0–99)
- Speed: `0x00..0x05`; `0x02` is "normal" and works for non-animated modes too.

## Minimum sequence for static color on a single zone

For zone `z` (0..3) painted to color `(R, G, B)` at full brightness:

1. **Mode write** to register `0x88`:
   ```
   [0x88, 0x01, 0x02, 0x63, 0x00, z+1, 0x00, 0x00]
          mode  spd  brt  myst  zone+1
   ```
2. **Color write** — register depends on zone:
   - `z == 0` or `z == 1`: register `0xB0`. Writes both zone-0 and zone-1 colors:
     ```
     [0xB0, 0x01, R0, G0, B0, R1, G1, B1]
     ```
     (For "paint everything one color," set R0=R1=R, G0=G1=G, B0=B1=B.)
   - `z == 2`: register `0xB1`:
     ```
     [0xB1, 0x01, R, G, B, 0x00, 0x00, 0x00]
     ```
   - `z >= 3`: register `0x40`:
     ```
     [0x40, R, G, B, z+1, 0x00, 0x00, 0x00]
     ```

3. (Optional) **Save** to NVRAM:
   ```
   [0xAA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
   ```
   Skip for interactive use.

To "paint everything one color" on a typical 4090, write all four
zones in turn (z = 0, 1, 2, 3) with the same color.

## What we deliberately leave for later

- Reading current state / firmware version from the controller.
- Hardware effect modes (wave, breathing, color cycle).
- Per-zone differentiation (we paint all zones the same color at
  v0.1; some cards expose zones with different physical LEDs).
- Auto-detecting cards not yet in the recognized list — for that we'd
  need a probe mechanism (try I²C reads at common addresses, see
  what answers).

## Open questions

- Does port 1 work on all three 4090 SKUs, or does it differ per
  card? Probe-or-ask either way.
- Does `NvAPI_I2CWriteEx` require the GeForce driver to be in a
  particular state (e.g., a display attached)? Worth testing
  during implementation.
- The `mystery_flag` byte in the mode packet is 0 for most modes but
  becomes `0x08` for `MODE_GRADIENT` / `MODE_TRICOLOR` and the
  numberOfColors for `MODE_COLOR_SHIFT`. Not needed for static.
