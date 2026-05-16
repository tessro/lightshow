# Gigabyte RGB Fusion 2.0 USB protocol notes

## Device identity

USB device on `ROOT_HUB30` (chipset USB), with two HID collections on
vendor usage page `0xFF89`:

| VID    | PIDs                           | Manufacturer  |
| ------ | ------------------------------ | ------------- |
| `048D` | `8297`, `8950`, `5702`, `5711` | ITE Tech Inc. |

Our target machine has PID `5702`. The two collections are:

| Collection | Usage    | Feature report | Length (incl. ID) | Role                                           |
| ---------- | -------- | -------------- | ----------------- | ---------------------------------------------- |
| `Col01`    | `0x0010` | `0x5A`         | 17                | Unused for RGB (keyboard backlight on some hw) |
| `Col02`    | `0x00CC` | `0xCC`         | 64                | **Primary LED control**                        |

All commands described below are sent to **Col02 as HID feature reports
of exactly 64 bytes**, with the first byte being report ID `0xCC`.

When `get_feature_report` is issued with no preceding command, the
device echoes a USB-style string descriptor (`cc 03` followed by
UTF-16LE "ITE DeviceInc."). This is a useful liveness check but
carries no protocol payload.

## Wire framing

Every packet is exactly **64 bytes**. The first byte is always
`0xCC` (report ID). The second byte is the command opcode. Remaining
62 bytes are command-specific payload, zero-filled when unused.

```
+----+----+----------------------------------------+
| CC | OP |   62 bytes of command payload          |
+----+----+----------------------------------------+
  0    1    2                                   63
```

Two transport conventions exist:

1. **Short commands**: payload is bytes 2 and 3 only (occasionally
   byte 4). Sent for state changes, applies, mode switches.
2. **Long commands**: payload occupies a structured layout — the
   "effect packet" below.

## Commands relevant to set-static-color

### Effect packet (opcode varies, used for per-LED static color)

This packet defines what a single LED zone does. For a static color we
set `effect_type = 1` (static). The byte layout at offsets `[0..64)`:

| Offset | Bytes | Field           | Notes                                    |
| ------ | ----- | --------------- | ---------------------------------------- |
| 0      | 1     | report_id       | `0xCC` always                            |
| 1      | 1     | header / opcode | See "Header / zone mapping" below        |
| 2      | 4     | zone_mask_lo    | LE u32. Bit `i` selects LED `i` (0..31). |
| 6      | 4     | zone_mask_hi    | LE u32. Always 0 for our usage.          |
| 10     | 1     | reserved        | 0                                        |
| 11     | 1     | effect_type     | `1` = static, others = animations        |
| 12     | 1     | max_brightness  | `0..=255`. Use `255` for full color.     |
| 13     | 1     | min_brightness  | 0 for static                             |
| 14     | 4     | color0          | **BGR0** byte order — see below          |
| 18     | 4     | color1          | 0 for static                             |
| 22     | 2     | period0         | 0 for static (fade-in)                   |
| 24     | 2     | period1         | 0 for static (fade-out)                  |
| 26     | 2     | period2         | 0 for static (hold)                      |
| 28     | 2     | period3         | 0 for static                             |
| 30     | 4     | effect_params   | 0 for static                             |
| 34     | 30    | padding         | 0                                        |

All multi-byte integers are **little-endian**.

#### Color byte order

The `color0` field is **BGR0** when read as 4 raw bytes:

```
byte 14 = B
byte 15 = G
byte 16 = R
byte 17 = 0
```

This is consistent with a little-endian `u32` value of the form
`0x00RRGGBB`.

#### Header / zone mapping

`header` (byte 1) plus `zone_mask_lo` (bytes 2–5) together select
which LEDs the effect applies to. For a per-LED write:

| Target LED index                        | header byte      | zone_mask_lo                             |
| --------------------------------------- | ---------------- | ---------------------------------------- |
| LED `i`, `i < 8`                        | `0x20 + i`       | `1 << i`                                 |
| LED `i`, `i ∈ {8,9,10}` (PID 5711 only) | `0x90 + (i - 8)` | `1 << i`                                 |
| All LEDs ("global")                     | `0x20`           | `0xFF` (PID 5702) or `0x07FF` (PID 5711) |

For a "paint everything one color" command on PID `0x5702` we use
`header = 0x20`, `zone_mask_lo = 0xFF`.

### Apply / commit (opcode `0x28`)

The effect packet alone stages the change but does not necessarily
commit it. To commit, send a short command:

```
CC 28 FF 00  00 00 ... (60 zeros)
```

For PID `0x5711`, the third byte is `0x07` instead of `0x00`:

```
CC 28 FF 07  00 00 ... (60 zeros)
```

(There is also a long-form apply that uses byte 2 as a 32-bit zone
selector mask; we don't need it for "apply everything".)

### Persist to flash (opcode `0x5E`)

Optional. Writes current state to the controller's NVRAM so it
survives a power cycle.

```
CC 5E 00 00  00 00 ... (60 zeros)
```

We **do not** call this on every set — only when the user explicitly
asks to persist (a future `--persist` flag). Avoids burning flash
cycles during interactive use.

### Disable LampArray mode (opcode `0x48`)

Modern firmware exposes Microsoft's HID LampArray protocol on the same
HID interface. While LampArray is active, the firmware may override
or ignore our writes. Disable it explicitly before our first effect
write:

```
CC 48 00 00  00 00 ... (60 zeros)
```

Re-enable on shutdown if you want the system to respect LampArray
from other software:

```
CC 48 01 00  00 00 ... (60 zeros)
```

### Freeze/unfreeze hardware effects (opcode `0x32`)

The firmware runs hardware-side effect animations on certain zones by
default. To force a zone to honor our static color and not be
overwritten by the firmware's animation engine, send:

```
CC 32 <bitmask> 00  ... zeros
```

Where `<bitmask>` is the zones to _disable_ hardware effects on
(`0xFF` for all). Sleep ~50ms after this command before continuing.

## Minimum sequence for `set --color RRGGBB`

To paint everything reachable (both effect-driven onboard zones and
per-pixel ARGB headers) the same color:

```
1. CC 48 00 00 ...                              # disable LampArray
2. CC 20  FF 00 00 00  00 00 00 00  00 01 FF 00  BB GG RR 00  ...
                                                # effect packet:
                                                # header=0x20, zones=0xFF,
                                                # static, brightness 255, color
3. CC 28 FF 00 ...                              # apply effect (committed)
4. CC 32 1B 00 ...     (sleep 50ms)             # take all 4 ARGB headers off hw effects
5. CC 34 00 00 ...                              # set strip max length to 32 LEDs each
6. For each header in {0x58, 0x59, 0x62, 0x63}:
     For each chunk of up to 19 LEDs (until 32 LEDs have been sent):
       CC <hdr> <boffset_lo boffset_hi> <bcount> <GRB triplets...>
```

(All "..." are zero-fill out to 64 bytes.)

## Addressable strip data (ARGB headers)

The "effect packet" above only drives **non-addressable** zones —
onboard LEDs and basic 12V RGB headers. Anything plugged into one of
the motherboard's 5V ARGB headers (where WS2812-style addressable
fans/strips live) is driven instead by **per-pixel data** sent through
a different packet format.

### Strip packet layout (64 bytes)

| Offset | Bytes | Field      | Notes                                                |
| ------ | ----- | ---------- | ---------------------------------------------------- |
| 0      | 1     | report_id  | `0xCC`                                               |
| 1      | 1     | header     | One of `0x58 0x59 0x62 0x63` (see header map below)  |
| 2      | 2     | boffset    | LE u16 byte offset into the strip's frame buffer     |
| 4      | 1     | bcount     | Number of payload bytes in this packet (`= leds*3`)  |
| 5      | 57    | rgb_data   | Up to 19 RGB triplets, in the strip's calibrated order |
| 62     | 2     | padding    | 0                                                    |

To paint N LEDs of a strip, send `ceil(N / 19)` packets with
contiguous `boffset` values. Each packet may carry 1..19 LEDs; the
last one carries the remainder. After the last packet the controller
drives the strip — there is **no separate apply for strip data**.

### ARGB header bytes

| Header index | `header` byte | Typical motherboard label |
| ------------ | ------------- | ------------------------- |
| 1            | `0x58`        | `D_LED1` / `ARGB1`        |
| 2            | `0x59`        | `D_LED2` / `ARGB2`        |
| 3            | `0x62`        | `D_LED3` / `ARGB3` (PID `5711` only) |
| 4            | `0x63`        | `D_LED4` / `ARGB4` (PID `5711` only) |

### Color byte order is per-header calibration

Each ARGB header has a stored calibration that tells the controller
which byte position in each RGB triplet is R, G, B. The packed
calibration `u32` decodes as:

```
bo_r = (calibration >> 16) & 0xFF
bo_g = (calibration >>  8) & 0xFF
bo_b = (calibration >>  0) & 0xFF
```

`bo_r`/`bo_g`/`bo_b` are byte offsets *within a triplet* (0, 1, or 2).

For uncalibrated controllers and most WS2812B-class fans/strips the
order is **GRB** (`bo_r=1, bo_g=0, bo_b=2`). If a uniform red comes
out green/blue, the strip is using a different order — we'd need to
read the calibration via opcode `0x32`/`0x33` reads, which we have
not implemented.

### Hardware effects must be disabled for the targeted headers

Each ARGB header has a bit in the "effects disabled" mask managed by
opcode `0x32`. **The bits are zone-purpose markers, not `1<<led_index`:**

| Zone                           | Bit    |
| ------------------------------ | ------ |
| Main effect zone / HDR_D_LED1  | `0x01` |
| HDR_D_LED2                     | `0x02` |
| HDR_D_LED3                     | `0x08` |
| HDR_D_LED4                     | `0x10` |

Bit `0x04` is intentionally skipped — quirk of the firmware's bitmap.

To take per-pixel control of all four ARGB headers in one shot:

```
CC 32 1B 00  00 00 ... (60 zeros)    # 0x01 | 0x02 | 0x08 | 0x10
```

Sleep ~50ms after this.

### Optional: set max strip length (opcode `0x34`)

Tells the firmware how many LEDs to clock out per header. Packed as
two 4-bit length codes per byte:

```
CC 34 <d2 d1> <d4 d3>  00 00 ...
```

Where each nibble selects: `0`=32 LEDs, `1`=64, `2`=256, `3`=512,
`4`=1024. Without this command the firmware uses whatever was set
last (typically 32 from boot defaults). Setting `LEDS_32` for every
header is a safe baseline:

```
CC 34 00 00  00 00 ... (60 zeros)
```

## What we deliberately leave for later

- Per-LED addressing (the API paints all LEDs on a header the same
  color at v0.1).
- Hardware effect modes (pulse, wave, color cycle).
- Calibration packets (`0x33`, `0x34`, `0x47`, `0x5E`).
- Reading firmware info / current state.

## Open questions

- Do we need an init handshake before the first effect packet, or is
  `0x48 0x00` + `0x32 0xFF` enough on a cold open?
- Does the chipset retain state between USB device opens, or does
  hidapi's `open_path` reset it? Worth observing during testing.
