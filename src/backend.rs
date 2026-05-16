use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn from_hex(s: &str) -> Result<Self, ColorParseError> {
        let s = s.strip_prefix('#').unwrap_or(s);
        if s.len() != 6 {
            return Err(ColorParseError::BadLength);
        }
        let r = u8::from_str_radix(&s[0..2], 16).map_err(|_| ColorParseError::BadHex)?;
        let g = u8::from_str_radix(&s[2..4], 16).map_err(|_| ColorParseError::BadHex)?;
        let b = u8::from_str_radix(&s[4..6], 16).map_err(|_| ColorParseError::BadHex)?;
        Ok(Self { r, g, b })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ColorParseError {
    #[error("hex color must be 6 digits (e.g. ff8800)")]
    BadLength,
    #[error("invalid hex digit")]
    BadHex,
}

#[derive(Debug, Clone, Serialize)]
pub struct Zone {
    pub name: String,
    pub led_count: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: DeviceId,
    pub vendor: &'static str,
    pub name: String,
    pub zones: Vec<Zone>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceId {
    pub backend: &'static str,
    pub key: String,
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.backend, self.key)
    }
}

pub trait Backend {
    fn name(&self) -> &'static str;

    fn enumerate(&self) -> anyhow::Result<Vec<Device>>;

    fn set_static(&self, device: &Device, color: Color) -> anyhow::Result<()>;
}
