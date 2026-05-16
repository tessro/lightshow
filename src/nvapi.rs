//! Minimal FFI bindings to `nvapi64.dll`.
//!
//! See `docs/PROTOCOL_gigabyte_gpu_rgb_fusion2.md` for the protocol-level
//! design notes. This module only exposes what the GPU backend needs:
//! init, enumerate physical GPUs, read PCI identifiers, and write I²C.
//!
//! NVAPI exports a single symbol, `nvapi_QueryInterface(u32 id) -> void*`,
//! that returns the address of every other function. The function IDs are
//! 32-bit magic numbers stable across driver versions.

use std::ffi::c_void;
use std::mem;

use anyhow::{Context, Result, bail};
use libloading::Library;

pub type NvStatus = i32;
pub type NvPhysicalGpuHandle = *mut c_void;

const FN_INITIALIZE: u32 = 0x0150_E828;
const FN_UNLOAD: u32 = 0xD22B_DD7E;
const FN_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const FN_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2DDF_B66E;
const FN_I2C_WRITE_EX: u32 = 0x283A_C65A;

pub const MAX_PHYSICAL_GPUS: usize = 64;

#[repr(u32)]
#[allow(dead_code)]
pub enum I2cSpeed {
    Default = 0,
    Khz3 = 1,
    Khz10 = 2,
    Khz33 = 3,
    Khz100 = 4,
    Khz200 = 5,
    Khz400 = 6,
}

#[repr(C)]
pub struct NvI2cInfoV3 {
    pub version: u32,
    pub display_mask: u32,
    pub is_ddc_port: u8,
    pub i2c_dev_address: u8,
    pub i2c_reg_address: *mut u8,
    pub reg_addr_size: u32,
    pub data: *mut u8,
    pub size: u32,
    pub i2c_speed: u32,
    pub i2c_speed_khz: u32,
    pub port_id: u8,
    pub is_port_id_set: u32,
}

impl NvI2cInfoV3 {
    /// Default-initialized struct ready for an `I2CWriteEx` of `size` bytes
    /// to a 7-bit I²C device address.
    pub fn for_write(dev_addr_7bit: u8, data: &mut [u8], port_id: u8) -> Self {
        let size = mem::size_of::<Self>() as u32;
        Self {
            version: (3u32 << 16) | size,
            display_mask: 0,
            is_ddc_port: 0,
            i2c_dev_address: dev_addr_7bit << 1, // NVAPI wants the 8-bit form
            i2c_reg_address: std::ptr::null_mut(),
            reg_addr_size: 0,
            data: data.as_mut_ptr(),
            size: data.len() as u32,
            i2c_speed: 0xFFFF, // legacy "use default" sentinel
            i2c_speed_khz: I2cSpeed::Khz100 as u32,
            port_id,
            is_port_id_set: 1,
        }
    }
}

type FnQueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
type FnInitialize = unsafe extern "C" fn() -> NvStatus;
type FnUnload = unsafe extern "C" fn() -> NvStatus;
type FnEnumPhysicalGpus =
    unsafe extern "C" fn(*mut NvPhysicalGpuHandle, *mut u32) -> NvStatus;
type FnGpuGetPciIdentifiers = unsafe extern "C" fn(
    NvPhysicalGpuHandle,
    *mut u32,
    *mut u32,
    *mut u32,
    *mut u32,
) -> NvStatus;
type FnI2cWriteEx =
    unsafe extern "C" fn(NvPhysicalGpuHandle, *mut NvI2cInfoV3, *mut u32) -> NvStatus;

pub struct Nvapi {
    // Library is held to keep the function pointers alive; never dropped
    // until Nvapi is dropped.
    _lib: Library,
    initialize: FnInitialize,
    unload: FnUnload,
    enum_physical_gpus: FnEnumPhysicalGpus,
    gpu_get_pci_identifiers: FnGpuGetPciIdentifiers,
    i2c_write_ex: FnI2cWriteEx,
    initialized: bool,
}

impl Nvapi {
    /// Load `nvapi64.dll` and resolve the function pointers we need. Does
    /// NOT call `NvAPI_Initialize` — call [`initialize`] separately.
    pub fn load() -> Result<Self> {
        let lib = unsafe { Library::new("nvapi64.dll") }
            .context("load nvapi64.dll (is the NVIDIA driver installed?)")?;

        let query_sym: libloading::Symbol<FnQueryInterface> =
            unsafe { lib.get(b"nvapi_QueryInterface") }
                .context("nvapi64.dll missing nvapi_QueryInterface export")?;
        let query: FnQueryInterface = *query_sym;

        unsafe fn resolve<F: Sized>(
            query: FnQueryInterface,
            id: u32,
            name: &'static str,
        ) -> Result<F> {
            // SAFETY: caller passes a valid query_interface pointer obtained
            // from the still-loaded nvapi64.dll.
            let p = unsafe { query(id) };
            if p.is_null() {
                bail!("nvapi_QueryInterface returned null for {name} (id {id:#010x})");
            }
            // SAFETY: NVAPI guarantees the returned pointer, when non-null,
            // is a function with the layout the caller asks for via F. We
            // can't statically verify this; it's documented behavior of the
            // NVAPI ABI and is the only way to dispatch through a void* table.
            Ok(unsafe { mem::transmute_copy::<*mut c_void, F>(&p) })
        }

        unsafe {
            Ok(Self {
                initialize: resolve(query, FN_INITIALIZE, "NvAPI_Initialize")?,
                unload: resolve(query, FN_UNLOAD, "NvAPI_Unload")?,
                enum_physical_gpus: resolve(
                    query,
                    FN_ENUM_PHYSICAL_GPUS,
                    "NvAPI_EnumPhysicalGPUs",
                )?,
                gpu_get_pci_identifiers: resolve(
                    query,
                    FN_GPU_GET_PCI_IDENTIFIERS,
                    "NvAPI_GPU_GetPCIIdentifiers",
                )?,
                i2c_write_ex: resolve(query, FN_I2C_WRITE_EX, "NvAPI_I2CWriteEx")?,
                _lib: lib,
                initialized: false,
            })
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        let rc = unsafe { (self.initialize)() };
        if rc != 0 {
            bail!("NvAPI_Initialize failed: {rc}");
        }
        self.initialized = true;
        Ok(())
    }

    /// Enumerate up to `MAX_PHYSICAL_GPUS` GPUs. Returns the valid prefix.
    pub fn enum_physical_gpus(&self) -> Result<Vec<NvPhysicalGpuHandle>> {
        let mut handles: [NvPhysicalGpuHandle; MAX_PHYSICAL_GPUS] =
            [std::ptr::null_mut(); MAX_PHYSICAL_GPUS];
        let mut count: u32 = 0;
        let rc = unsafe { (self.enum_physical_gpus)(handles.as_mut_ptr(), &mut count) };
        if rc != 0 {
            bail!("NvAPI_EnumPhysicalGPUs failed: {rc}");
        }
        Ok(handles[..count as usize].to_vec())
    }

    /// Returns `(device_id, subsystem_id, revision_id, ext_device_id)` where
    /// device_id and subsystem_id are packed as `(device << 16) | vendor`.
    pub fn gpu_pci_identifiers(
        &self,
        handle: NvPhysicalGpuHandle,
    ) -> Result<(u32, u32, u32, u32)> {
        let mut device_id = 0u32;
        let mut subsystem_id = 0u32;
        let mut revision_id = 0u32;
        let mut ext_device_id = 0u32;
        let rc = unsafe {
            (self.gpu_get_pci_identifiers)(
                handle,
                &mut device_id,
                &mut subsystem_id,
                &mut revision_id,
                &mut ext_device_id,
            )
        };
        if rc != 0 {
            bail!("NvAPI_GPU_GetPCIIdentifiers failed: {rc}");
        }
        Ok((device_id, subsystem_id, revision_id, ext_device_id))
    }

    /// Write `data` to a 7-bit I²C device address on the given GPU's I²C
    /// `port_id`. The data buffer is sent raw with no separate register byte.
    pub fn i2c_write(
        &self,
        handle: NvPhysicalGpuHandle,
        dev_addr_7bit: u8,
        port_id: u8,
        data: &mut [u8],
    ) -> Result<()> {
        let mut info = NvI2cInfoV3::for_write(dev_addr_7bit, data, port_id);
        let mut unknown_out: u32 = 0;
        let rc = unsafe { (self.i2c_write_ex)(handle, &mut info, &mut unknown_out) };
        if rc != 0 {
            bail!(
                "NvAPI_I2CWriteEx(addr=0x{dev_addr_7bit:02x}, port={port_id}) failed: {rc}"
            );
        }
        Ok(())
    }
}

impl Drop for Nvapi {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                let _ = (self.unload)();
            }
        }
    }
}
