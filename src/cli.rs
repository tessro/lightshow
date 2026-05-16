use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lightshow", version, about = "Zero drama case lighting")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,

    /// Emit machine-readable JSON instead of human output.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Discover RGB devices across all backends.
    List,

    /// Dump every USB HID device we can see (diagnostic).
    Scan,

    /// Dump the raw HID report descriptor for one device, plus a summary of
    /// the feature/input/output report IDs and their sizes (diagnostic).
    Descriptor {
        /// `backend:key` of the device to inspect (from `lightshow list`).
        #[arg(long)]
        device: String,
    },

    /// Send an arbitrary feature-report payload to the Aura device. Used to
    /// probe the command space during reverse engineering.
    RawWrite {
        /// Substring of the device id from `lightshow list`.
        #[arg(long)]
        device: String,
        /// Hex bytes of the payload (without the leading report ID). Spaces
        /// or commas between bytes are optional. Examples: `00 00 00`, `ff,ff,ff`.
        #[arg(long)]
        payload: String,
    },

    /// For each detected ENE controller, read its device-name string and
    /// dump a register range across all chips so you can spot per-chip
    /// differences (diagnostic).
    EneProbe,

    /// Read a feature report from the device and dump it as hex+ASCII
    /// (diagnostic — used during protocol reverse engineering).
    Probe {
        /// Substring of the device id from `lightshow list`.
        #[arg(long)]
        device: String,
        /// Feature report ID to read (hex, e.g. `cc`).
        #[arg(long, default_value = "cc")]
        report_id: String,
        /// Number of bytes to request, including the leading report-id byte.
        #[arg(long, default_value_t = 64)]
        len: usize,
    },

    /// Paint device(s) a single static color.
    Set {
        /// Hex color (e.g. `ff8800` or `#ff8800`).
        #[arg(long)]
        color: String,

        /// Limit to a single device by `backend:key`. Without this, every
        /// detected device gets the same color.
        #[arg(long)]
        device: Option<String>,
    },
}
