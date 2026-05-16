# G.Skill Trident Z5 DDR5 RGB protocol notes

The RGB controller on G.Skill Trident Z5 DDR5 modules is an ENE chip
sitting on the host SMBus alongside (not behind) the DIMM's SPD5 hub.

## Transport: PawnIO + SmbusPIIX4

Userspace cannot issue IN/OUT instructions, so SMBus access on Windows
needs a kernel driver. **PawnIO** is a signed kernel driver that
loads sandboxed bytecode modules and presents them as IOCTLs.

- **Driver:** PawnIO (https://pawnio.eu) — Microsoft-signed, works
  with HVCI / Secure Boot. User installs it once.
- **User-mode library:** `PawnIOLib.dll` (LGPL-2.1) — exposes the
  open/load/execute/close API. We FFI to it via libloading.
- **Module:** `SmbusPIIX4.bin` (GPL-2.0) — a pawn-bytecode module
  that exposes the AMD chipset (PIIX4-family) SMBus controller's
  IO ports via well-named IOCTLs.

### PawnIOLib API

```c
HRESULT pawnio_version (ULONG*  version);
HRESULT pawnio_open    (HANDLE* handle);
HRESULT pawnio_load    (HANDLE handle, const u8* blob, size_t size);
HRESULT pawnio_execute (HANDLE handle, const char* name,
                        const u64* in, size_t in_count,
                        u64* out, size_t out_count, size_t* return_size);
HRESULT pawnio_close   (HANDLE handle);
```

`HRESULT` is a 32-bit signed int; `S_OK = 0`.

### SmbusPIIX4 IOCTLs

| IOCTL name             | in (u64×N)                  | out (u64×N)                         |
| ---------------------- | --------------------------- | ----------------------------------- |
| `ioctl_identity`       | `[0]`                       | `[name_lo, name_hi, pci_ids]`       |
| `ioctl_set_sleep_mode` | `[mode]` (0/1/2)            | (none)                              |
| `ioctl_piix4_port_sel` | `[port]` (0 or 1)           | `[status]` _(ignored on success)_   |
| `ioctl_smbus_xfer`     | see "SMBus xfer call" below | `[ec, byte, word_lo, word_hi, len]` |

#### SMBus xfer input layout

The 9-element `u64` input array for `ioctl_smbus_xfer`:

| Index  | Meaning                                             |
| ------ | --------------------------------------------------- |
| `0`    | 7-bit device address                                |
| `1`    | 1 = read, 0 = write                                 |
| `2`    | command/register byte                               |
| `3`    | size code (SMBus transaction type — see below)      |
| `4..8` | `i2c_smbus_data` union: bytes/word/block (32 bytes) |

Transaction size codes (matches Linux `i2c_smbus_xfer` `size` arg):

| Code | Name                       | Use                            |
| ---- | -------------------------- | ------------------------------ |
| `1`  | `I2C_SMBUS_BYTE`           | one data byte, no register     |
| `2`  | `I2C_SMBUS_BYTE_DATA`      | one data byte at a register    |
| `3`  | `I2C_SMBUS_WORD_DATA`      | one 16-bit value at a register |
| `5`  | `I2C_SMBUS_BLOCK_DATA`     | 1..32 bytes prefixed by length |
| `8`  | `I2C_SMBUS_I2C_BLOCK_DATA` | raw N-byte block               |

For our purposes we only need WORD_DATA and BYTE_DATA writes plus
BYTE_DATA reads.

### Port selection

AMD chipsets expose two SMBus ports (primary at IO base + 0x0,
secondary at +0x20). DDR5 DIMMs live on port 0. After loading the
module, call `ioctl_piix4_port_sel` with `0` once.

### Sleep mode

Some AMD chipsets need a small delay between SMBus transactions.
Set this once via `ioctl_set_sleep_mode` with one of:

- `0` `PAWNIO_SLEEPMODE_ALWAYSBUSY` — no sleep (fastest, can hang)
- `1` `PAWNIO_SLEEPMODE_SHORTBUSY` — short delay
- `2` `PAWNIO_SLEEPMODE_ALWAYSSLEEP` — always sleep (safest)

We use `2` to start.

### Global SMBus mutex

Multiple processes (us, Armoury Crate, other RGB tools) may hit the
SMBus at once. Coordinate by acquiring the named mutex
`Global\Access_SMBUS.HTP.Method` before each transaction and releasing
after. Windows API: `CreateMutexA` → `WaitForSingleObject` →
`ReleaseMutex` → `CloseHandle`.

## ENE chip register protocol

The ENE controller on each G.Skill DIMM uses **16-bit register
addressing** layered over SMBus. To access an ENE register:

```
WRITE register `reg`:
  smbus_write_word(addr, 0x00, byteswap(reg))    # latch high+low addr
  smbus_write_byte(addr, 0x01, value)            # write byte value

READ register `reg`:
  smbus_write_word(addr, 0x00, byteswap(reg))    # latch
  return smbus_read_byte(addr, 0x81)             # read byte
```

The `byteswap` of `reg` is `((reg << 8) & 0xFF00) | ((reg >> 8) & 0x00FF)`.

### Address remap procedure

By default, every G.Skill DIMM listens on address `0x77`. To
control multiple DIMMs independently, each must be remapped to a
unique address. The remap persists until power cycle.

If another program (Armoury Crate, iCUE) has already remapped, skip
this and just use the remapped addresses.

Procedure:

```
target_pool = [0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x4F, 0x66, 0x67,
               0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77]
# Order is arbitrary — pool membership is what the chip accepts.
idx = -1
for slot in 0..8:
    # Probe whether any DIMM is still on 0x77
    if smbus_write_quick(0x77) fails: break

    # Find the next unused target address
    loop:
        idx += 1
        if idx >= len(target_pool): break out of remap entirely
        if smbus_write_quick(target_pool[idx]) succeeds:
            continue   # address taken, try the next one
        break   # found a free one

    # Tell the DIMM at 0x77 what its new slot # and address should be
    ene_write(0x77, 0x80F8, slot)                       # slot index
    ene_write(0x77, 0x80F9, target_pool[idx] << 1)      # new I²C addr (shifted)
```

### Detection / validation

After remap, validate each candidate address by reading register
range `0xA0..0xB0`: each byte must equal `address - 0xA0` (i.e.
0x00, 0x01, 0x02, ...). If the pattern matches, it's a real ENE
chip. Also read 16 bytes at `0x1030` and reject if the string is
`"Micron"` — that's a Crucial DIMM with incompatible firmware.

### Key control registers

| Register | Name             | Purpose                                          |
| -------- | ---------------- | ------------------------------------------------ |
| `0x1000` | DEVICE_NAME      | 16-byte ASCII identification                     |
| `0x1C00` | CONFIG_TABLE     | Read 64 bytes; offset `0x02` is LED count        |
| `0x8000` | COLORS_DIRECT    | Direct-mode colors, 15 bytes (5 LEDs × RGB)      |
| `0x8010` | COLORS_EFFECT    | Effect-mode colors, 15 bytes                     |
| `0x8020` | DIRECT_ENABLE    | 1 = direct mode (we drive every frame), 0 = mode |
| `0x8021` | MODE             | 0=off, 1=static, 2=breathing, …                  |
| `0x8022` | SPEED            | 0=fastest .. 4=slowest                           |
| `0x80A0` | APPLY            | Write `0x01` to commit recent register writes    |
| `0x80F8` | SLOT_INDEX       | (remap only)                                     |
| `0x80F9` | I2C_ADDRESS      | (remap only)                                     |
| `0x8100` | COLORS_DIRECT_V2 | 30 bytes — newer chip variant (10 LEDs × RGB)    |
| `0x8160` | COLORS_EFFECT_V2 | 30 bytes — newer chip variant                    |

### LED count

The LED count is read from config table byte `0x1C02`. Typical
Trident Z5 module has **5 LEDs** per DIMM.

V2-color registers (`0x8100` / `0x8160`) hold 30 bytes (10 LEDs).
Whether a given chip uses V1 or V2 registers depends on firmware;
the safe approach is to write _both_ register banks for the same color
and let the chip use whichever it understands. For static color all
LEDs same, the data is just the color triplet repeated.

## Minimum sequence for `set --color RRGGBB` on every DIMM

For each remapped DIMM address:

1. **Set mode to STATIC**: `ene_write(addr, 0x8021, 0x01)`
2. **Write effect colors**: 30 bytes of repeating `[R, G, B]` at
   `0x8010` (V1) **and** `0x8160` (V2) — written byte-by-byte via the
   ENE protocol since SMBus doesn't natively do >32-byte writes.
3. **Apply**: `ene_write(addr, 0x80A0, 0x01)`

We deliberately use _effect_ mode (not direct mode) so the chip drives
the LEDs after we exit, rather than waiting for us to clock in a new
frame every refresh tick.

### Color byte order

ENE chips use **R, B, G** order per LED triplet in registers `0x8000+`,
`0x8010+`, `0x8100+`, and `0x8160+`: byte 0 = red, byte 1 = blue,
byte 2 = green. Writing `[R, G, B]` (the naive order) produces wrong
colors — e.g., `ff0055` (magenta) comes out as `ff5500` (orange).

## What we deliberately leave for later

- DDR4 RAM, motherboard ENE, and ENE GPU controllers (different
  detection paths even though the protocol is similar).
- LED count discovery from the config table (we paint all 5 LEDs +
  the V2 10-LED variant, which covers both layouts; reading the
  actual count would let us trim writes).
- Per-DIMM differentiation (paint all DIMMs same color at v0.1).
- Effects beyond static.

## Open questions

- Some DDR5 G.Skill firmware revisions reportedly fail the ENE
  detection register-range test (`0xA0..0xAF` read as `0..F`). If
  detection fails for the user's specific kit, we may need to
  bypass the validation or use a different match heuristic
  (e.g. read `0x1000` and look for `"G.SKILL"`).
- Behavior of the global SMBus mutex when Armoury Crate is also
  running: should be cooperative, but worth testing whether
  Armoury Crate immediately re-asserts its preferred color after
  we write.
