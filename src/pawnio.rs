//! FFI bindings to `PawnIOLib.dll` plus a thin SMBus client.
//!
//! See `docs/PROTOCOL_gskill_ddr5.md` §"Transport: PawnIO + SmbusPIIX4"
//! for the protocol-level design. This module:
//!
//! - dynamically loads `PawnIOLib.dll`,
//! - loads a pawn-bytecode module blob (typically `SmbusPIIX4.bin`),
//! - exposes named IOCTLs (`ioctl_smbus_xfer`, `ioctl_piix4_port_sel`,
//!   `ioctl_set_sleep_mode`, `ioctl_identity`) as Rust methods.

use std::ffi::{CString, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};
use libloading::Library;

pub type Hresult = i32;
pub type Handle = *mut c_void;

const S_OK: Hresult = 0;

// I²C transaction size codes (Linux i2c_smbus_xfer convention).
const I2C_SMBUS_BYTE_DATA: u8 = 2;
const I2C_SMBUS_WORD_DATA: u8 = 3;

const I2C_SMBUS_READ: u8 = 1;
const I2C_SMBUS_WRITE: u8 = 0;

#[allow(dead_code)]
#[repr(u64)]
pub enum SleepMode {
    AlwaysBusy = 0,
    ShortBusy = 1,
    AlwaysSleep = 2,
}

type FnOpen = unsafe extern "system" fn(*mut Handle) -> Hresult;
type FnLoad = unsafe extern "system" fn(Handle, *const u8, usize) -> Hresult;
type FnExecute = unsafe extern "system" fn(
    Handle,
    *const i8,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> Hresult;
type FnClose = unsafe extern "system" fn(Handle) -> Hresult;

pub struct Pawnio {
    _lib: Library,
    open_fn: FnOpen,
    load_fn: FnLoad,
    execute_fn: FnExecute,
    close_fn: FnClose,
}

impl Pawnio {
    /// Load `PawnIOLib.dll`. The PawnIO installer puts it in
    /// `C:\Program Files\PawnIO\`, normally on PATH, but a fresh install
    /// hasn't been picked up by an existing shell. Try a handful of well-
    /// known paths before giving up.
    pub fn load() -> Result<Self> {
        const CANDIDATES: &[&str] = &[
            "PawnIOLib.dll",
            r"C:\Program Files\PawnIO\PawnIOLib.dll",
            r"C:\Program Files (x86)\PawnIO\PawnIOLib.dll",
        ];
        let mut last_err: Option<libloading::Error> = None;
        let lib = CANDIDATES
            .iter()
            .find_map(|path| match unsafe { Library::new(*path) } {
                Ok(lib) => Some(lib),
                Err(e) => {
                    last_err = Some(e);
                    None
                }
            })
            .ok_or_else(|| {
                let e = last_err.expect("at least one attempt");
                anyhow::anyhow!(
                    "load PawnIOLib.dll (is PawnIO installed? https://pawnio.eu): {e}"
                )
            })?;

        unsafe fn sym<F>(lib: &Library, name: &[u8]) -> Result<F> {
            let s: libloading::Symbol<F> =
                // SAFETY: caller passes a real PawnIOLib.dll Library.
                unsafe { lib.get(name) }
                    .with_context(|| {
                        format!(
                            "PawnIOLib.dll missing export `{}`",
                            String::from_utf8_lossy(name).trim_end_matches('\0'),
                        )
                    })?;
            // SAFETY: we transfer the function pointer out of the Symbol
            // borrow. Library is held in Pawnio._lib so the pointer remains
            // valid for the lifetime of Pawnio.
            Ok(unsafe { std::ptr::read(&*s) })
        }

        Ok(Self {
            open_fn: unsafe { sym(&lib, b"pawnio_open\0")? },
            load_fn: unsafe { sym(&lib, b"pawnio_load\0")? },
            execute_fn: unsafe { sym(&lib, b"pawnio_execute\0")? },
            close_fn: unsafe { sym(&lib, b"pawnio_close\0")? },
            _lib: lib,
        })
    }

    /// Open a driver handle and load the given pawn-bytecode module into it.
    pub fn open_with_module(&self, module_path: &Path) -> Result<Module<'_>> {
        let blob = std::fs::read(module_path)
            .with_context(|| format!("read pawn module {}", module_path.display()))?;
        let mut handle: Handle = std::ptr::null_mut();
        let hr = unsafe { (self.open_fn)(&mut handle) };
        check(hr, "pawnio_open")?;
        let hr = unsafe { (self.load_fn)(handle, blob.as_ptr(), blob.len()) };
        if let Err(e) = check(hr, "pawnio_load") {
            unsafe {
                let _ = (self.close_fn)(handle);
            }
            return Err(e);
        }
        Ok(Module {
            pawnio: self,
            handle,
        })
    }
}

pub struct Module<'a> {
    pawnio: &'a Pawnio,
    handle: Handle,
}

impl Module<'_> {
    fn execute(
        &self,
        name: &str,
        input: &[u64],
        output: &mut [u64],
    ) -> Result<usize> {
        let cname = CString::new(name).context("ioctl name")?;
        let mut returned: usize = 0;
        let hr = unsafe {
            (self.pawnio.execute_fn)(
                self.handle,
                cname.as_ptr(),
                input.as_ptr(),
                input.len(),
                output.as_mut_ptr(),
                output.len(),
                &mut returned,
            )
        };
        check(hr, name)?;
        Ok(returned)
    }

    pub fn set_sleep_mode(&self, mode: SleepMode) -> Result<()> {
        let input = [mode as u64];
        let mut output = [];
        self.execute("ioctl_set_sleep_mode", &input, &mut output)?;
        Ok(())
    }

    /// AMD chipsets expose two SMBus ports; DDR5 DIMMs live on port 0.
    pub fn piix4_port_sel(&self, port: u64) -> Result<()> {
        let input = [port];
        let mut output = [0u64; 1];
        self.execute("ioctl_piix4_port_sel", &input, &mut output)?;
        Ok(())
    }

    /// Send an SMBus transaction. `data` is read/written based on `size_code`.
    /// On read, the returned bytes are placed back into the appropriate slots
    /// of `data`. The first byte of `data` for block reads is the length.
    pub fn smbus_xfer(
        &self,
        addr: u8,
        read_write: u8,
        command: u8,
        size_code: u8,
        data: &mut [u8; 32],
    ) -> Result<()> {
        let mut input = [0u64; 9];
        input[0] = addr as u64;
        input[1] = read_write as u64;
        input[2] = command as u64;
        input[3] = size_code as u64;
        // Pack 32 bytes of `data` into input[4..8].
        for (i, chunk) in data.chunks(8).enumerate() {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            input[4 + i] = u64::from_le_bytes(buf);
        }
        let mut output = [0u64; 5];
        self.execute("ioctl_smbus_xfer", &input, &mut output)?;
        // Unpack the 32-byte response back into `data`.
        for (i, slot) in data.chunks_mut(8).enumerate() {
            let bytes = output[i].to_le_bytes();
            slot.copy_from_slice(&bytes[..slot.len()]);
        }
        Ok(())
    }

    /// Convenience: SMBus write-byte-data.
    pub fn write_byte_data(&self, addr: u8, command: u8, value: u8) -> Result<()> {
        let mut data = [0u8; 32];
        data[0] = value;
        self.smbus_xfer(addr, I2C_SMBUS_WRITE, command, I2C_SMBUS_BYTE_DATA, &mut data)
    }

    /// Convenience: SMBus write-word-data (little-endian on the wire).
    pub fn write_word_data(&self, addr: u8, command: u8, value: u16) -> Result<()> {
        let mut data = [0u8; 32];
        data[..2].copy_from_slice(&value.to_le_bytes());
        self.smbus_xfer(addr, I2C_SMBUS_WRITE, command, I2C_SMBUS_WORD_DATA, &mut data)
    }

    /// Convenience: SMBus read-byte-data, returns the value byte.
    pub fn read_byte_data(&self, addr: u8, command: u8) -> Result<u8> {
        let mut data = [0u8; 32];
        self.smbus_xfer(addr, I2C_SMBUS_READ, command, I2C_SMBUS_BYTE_DATA, &mut data)?;
        Ok(data[0])
    }

}

impl Drop for Module<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = (self.pawnio.close_fn)(self.handle);
            }
        }
    }
}

fn check(hr: Hresult, what: &str) -> Result<()> {
    if hr == S_OK {
        return Ok(());
    }
    let hint = match hr as u32 {
        0x80070005 => " (E_ACCESSDENIED — run from an elevated/Administrator shell)",
        _ => "",
    };
    bail!("{what} failed: HRESULT = 0x{:08X}{hint}", hr as u32)
}
