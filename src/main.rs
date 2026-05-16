mod backend;
mod backends;
mod cli;
mod nvapi;
mod pawnio;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::backend::Color;
use crate::cli::{Cli, Cmd};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::List => run_list(cli.json),
        Cmd::Scan => run_scan(cli.json),
        Cmd::Descriptor { device } => run_descriptor(&device, cli.json),
        Cmd::Probe {
            device,
            report_id,
            len,
        } => run_probe(&device, &report_id, len),
        Cmd::RawWrite { device, payload } => run_raw_write(&device, &payload),
        Cmd::EneProbe => run_ene_probe(),
        Cmd::Set { color, device } => {
            let color = Color::from_hex(&color).context("parsing --color")?;
            run_set(color, device.as_deref(), cli.json)
        }
    }
}

fn run_scan(as_json: bool) -> Result<()> {
    let api = hidapi::HidApi::new().context("init hidapi")?;
    #[derive(serde::Serialize)]
    struct Entry {
        vid: String,
        pid: String,
        usage_page: String,
        usage: String,
        interface: i32,
        manufacturer: String,
        product: String,
        path: String,
    }
    let entries: Vec<Entry> = api
        .device_list()
        .map(|d| Entry {
            vid: format!("{:#06x}", d.vendor_id()),
            pid: format!("{:#06x}", d.product_id()),
            usage_page: format!("{:#06x}", d.usage_page()),
            usage: format!("{:#06x}", d.usage()),
            interface: d.interface_number(),
            manufacturer: d.manufacturer_string().unwrap_or("").to_string(),
            product: d.product_string().unwrap_or("").to_string(),
            path: d.path().to_string_lossy().into_owned(),
        })
        .collect();
    if as_json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        println!(
            "{:<7} {:<7} {:<7} {:<7} {:<3} {:<24} {}",
            "VID", "PID", "UsgPg", "Usage", "If#", "Manufacturer", "Product"
        );
        for e in &entries {
            println!(
                "{:<7} {:<7} {:<7} {:<7} {:<3} {:<24} {}",
                e.vid, e.pid, e.usage_page, e.usage, e.interface, e.manufacturer, e.product
            );
        }
    }
    Ok(())
}

fn run_list(as_json: bool) -> Result<()> {
    let backends = backends::all();
    let mut all = Vec::new();
    for b in &backends {
        match b.enumerate() {
            Ok(devs) => all.extend(devs),
            Err(e) => eprintln!("warning: {} enumeration failed: {e:#}", b.name()),
        }
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }
    if all.is_empty() {
        println!("No devices detected.");
        return Ok(());
    }
    for d in &all {
        println!("{}  [{}]  {}", d.id, d.vendor, d.name);
        for z in &d.zones {
            let led_label = if z.led_count == 0 {
                "?".to_string()
            } else {
                z.led_count.to_string()
            };
            println!("    zone {:<10} ({} LEDs)", z.name, led_label);
        }
    }
    Ok(())
}

fn run_descriptor(device_filter: &str, as_json: bool) -> Result<()> {
    let device = resolve_device(device_filter)?;
    let path = device
        .id
        .key
        .as_str();
    let api = hidapi::HidApi::new().context("init hidapi")?;
    let hid = api
        .open_path(std::ffi::CString::new(path.as_bytes())?.as_c_str())
        .with_context(|| format!("open device {}", device.id))?;
    let mut buf = vec![0u8; 4096];
    let n = hid
        .get_report_descriptor(&mut buf)
        .context("get_report_descriptor")?;
    buf.truncate(n);
    let reports = parse_hid_descriptor_reports(&buf);
    if as_json {
        #[derive(serde::Serialize)]
        struct Out<'a> {
            raw_hex: String,
            reports: &'a [ReportInfo],
        }
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&Out {
                raw_hex: hex,
                reports: &reports
            })?
        );
    } else {
        println!("raw descriptor ({} bytes):", buf.len());
        for chunk in buf.chunks(16) {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            println!("  {}", hex.join(" "));
        }
        println!();
        println!("{:<7} {:<7} {:<10} {}", "Kind", "ID", "Bits", "Bytes");
        for r in &reports {
            println!(
                "{:<7} {:#04x}   {:<10} {}",
                r.kind, r.id, r.total_bits, r.total_bytes
            );
        }
    }
    Ok(())
}

fn run_ene_probe() -> Result<()> {
    let entries = backends::gskill_ddr5::probe_diag()?;
    if entries.is_empty() {
        println!("No ENE controllers detected on SMBus.");
        return Ok(());
    }
    // Per-chip summary.
    for e in &entries {
        let name_ascii: String = e
            .name
            .iter()
            .map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '.' })
            .collect();
        let name_hex: Vec<String> = e.name.iter().map(|b| format!("{b:02x}")).collect();
        println!("0x{:02x}:", e.addr);
        println!("  name (ascii): \"{name_ascii}\"");
        println!("  name (hex):   {}", name_hex.join(" "));
        println!(
            "  effect-color LED0 (0x8010): R={:02x} G={:02x} B={:02x}",
            e.v1_first[0], e.v1_first[1], e.v1_first[2]
        );
        println!(
            "  effect-color V2 LED0 (0x8160): R={:02x} G={:02x} B={:02x}",
            e.v2_first[0], e.v2_first[1], e.v2_first[2]
        );
    }
    // Cross-DIMM diff: print only registers that differ between any pair.
    println!();
    println!("== cross-DIMM register diff ==");
    println!(
        "(only registers where at least one DIMM differs from the first are shown; \
         `--` = read failed)"
    );
    let header_addrs: Vec<String> = entries.iter().map(|e| format!("0x{:02x}", e.addr)).collect();
    println!("reg      {}", header_addrs.join("    "));
    let mut any = false;
    let n = entries[0].dump.len();
    for i in 0..n {
        let reg = entries[0].dump[i].0;
        let vals: Vec<Option<u8>> = entries.iter().map(|e| e.dump[i].1).collect();
        let first = vals[0];
        if !vals.iter().all(|v| *v == first) {
            any = true;
            let cells: Vec<String> = vals
                .iter()
                .map(|v| v.map(|b| format!("{b:02x}")).unwrap_or_else(|| "--".into()))
                .collect();
            println!("0x{reg:04x}   {}", cells.join("      "));
        }
    }
    if !any {
        println!("(no register in the scanned ranges differs across DIMMs)");
    }
    Ok(())
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',')
        .collect();
    if cleaned.len() % 2 != 0 {
        bail!("hex payload must have an even number of digits");
    }
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        bytes.push(
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .with_context(|| format!("invalid hex at byte {}", i / 2))?,
        );
    }
    Ok(bytes)
}

fn run_raw_write(device_filter: &str, payload_hex: &str) -> Result<()> {
    let payload = parse_hex_bytes(payload_hex)?;
    let device = resolve_device(device_filter)?;
    let api = hidapi::HidApi::new().context("init hidapi")?;
    let hid = api
        .open_path(std::ffi::CString::new(device.id.key.as_bytes())?.as_c_str())
        .with_context(|| format!("open {}", device.id))?;
    backends::gigabyte_mobo::raw_write(&hid, &payload)?;
    println!("wrote {} payload bytes to {}", payload.len(), device.id);
    Ok(())
}

fn run_probe(device_filter: &str, report_id_hex: &str, len: usize) -> Result<()> {
    let id = u8::from_str_radix(report_id_hex.trim_start_matches("0x"), 16)
        .context("--report-id must be hex like `cc` or `0xcc`")?;
    if len == 0 || len > 4096 {
        bail!("--len must be 1..=4096");
    }
    let device = resolve_device(device_filter)?;
    let api = hidapi::HidApi::new().context("init hidapi")?;
    let hid = api
        .open_path(std::ffi::CString::new(device.id.key.as_bytes())?.as_c_str())
        .with_context(|| format!("open {}", device.id))?;
    let mut buf = vec![0u8; len];
    buf[0] = id;
    let n = hid
        .get_feature_report(&mut buf)
        .with_context(|| format!("get_feature_report(0x{id:02x}, {len} bytes)"))?;
    buf.truncate(n);
    println!(
        "got {} bytes from feature report 0x{:02x} on {}:",
        n, id, device.id
    );
    for chunk in buf.chunks(16) {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '.' })
            .collect();
        println!("  {:<48}  |{ascii}|", hex.join(" "));
    }
    Ok(())
}

fn resolve_device(filter: &str) -> Result<backend::Device> {
    let backends = backends::all();
    let mut matches = Vec::new();
    for b in &backends {
        let Ok(devs) = b.enumerate() else { continue };
        for d in devs {
            if d.id.to_string().contains(filter) {
                matches.push(d);
            }
        }
    }
    match matches.len() {
        0 => bail!("no device matched `{filter}`"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let ids: Vec<_> = matches.iter().map(|d| d.id.to_string()).collect();
            bail!("filter `{filter}` matched {n} devices:\n  {}", ids.join("\n  "))
        }
    }
}

#[derive(serde::Serialize)]
struct ReportInfo {
    kind: &'static str,
    id: u8,
    total_bits: u32,
    total_bytes: u32,
}

/// Minimal HID report descriptor parser: walks short items and accumulates the
/// (report_id × report_size × report_count) per main item (Input/Output/Feature).
/// Enough to learn which feature report IDs exist and how big each one is.
fn parse_hid_descriptor_reports(desc: &[u8]) -> Vec<ReportInfo> {
    use std::collections::BTreeMap;

    let mut out: BTreeMap<(&'static str, u8), u32> = BTreeMap::new();
    let mut report_id: u8 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut i = 0;
    while i < desc.len() {
        let prefix = desc[i];
        // Long item: prefix 0xFE
        if prefix == 0xFE {
            if i + 1 >= desc.len() {
                break;
            }
            let data_size = desc[i + 1] as usize;
            i += 3 + data_size;
            continue;
        }
        let size_code = prefix & 0x03;
        let data_size = match size_code {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4,
            _ => unreachable!(),
        };
        let item_type = (prefix >> 2) & 0x03; // 0=Main 1=Global 2=Local
        let tag = (prefix >> 4) & 0x0F;
        if i + 1 + data_size > desc.len() {
            break;
        }
        let data: u32 = match data_size {
            0 => 0,
            1 => desc[i + 1] as u32,
            2 => u16::from_le_bytes([desc[i + 1], desc[i + 2]]) as u32,
            4 => u32::from_le_bytes([desc[i + 1], desc[i + 2], desc[i + 3], desc[i + 4]]),
            _ => 0,
        };
        match item_type {
            0 => {
                // Main: 8=Input, 9=Output, B=Feature
                let kind = match tag {
                    0b1000 => Some("Input"),
                    0b1001 => Some("Output"),
                    0b1011 => Some("Feature"),
                    _ => None,
                };
                if let Some(k) = kind {
                    let bits = report_size * report_count;
                    *out.entry((k, report_id)).or_default() += bits;
                }
            }
            1 => {
                // Global
                match tag {
                    0b0111 => report_size = data,
                    0b1000 => report_id = data as u8,
                    0b1001 => report_count = data,
                    _ => {}
                }
            }
            _ => {}
        }
        i += 1 + data_size;
    }
    out.into_iter()
        .map(|((kind, id), bits)| ReportInfo {
            kind,
            id,
            total_bits: bits,
            total_bytes: bits.div_ceil(8),
        })
        .collect()
}

fn run_set(color: Color, device_filter: Option<&str>, as_json: bool) -> Result<()> {
    let backends = backends::all();
    let mut results: Vec<(String, Result<()>)> = Vec::new();
    let mut matched_any = false;
    for b in &backends {
        let devs = match b.enumerate() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("warning: {} enumeration failed: {e:#}", b.name());
                continue;
            }
        };
        for d in devs {
            if let Some(filter) = device_filter
                && !d.id.to_string().contains(filter)
            {
                continue;
            }
            matched_any = true;
            let id = d.id.to_string();
            let result = b.set_static(&d, color);
            results.push((id, result));
        }
    }
    if !matched_any {
        if let Some(f) = device_filter {
            bail!("no device matched `{f}`");
        }
        bail!("no devices detected");
    }
    if as_json {
        let payload: Vec<_> = results
            .iter()
            .map(|(id, r)| {
                serde_json::json!({
                    "device": id,
                    "ok": r.is_ok(),
                    "error": r.as_ref().err().map(|e| format!("{e:#}")),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for (id, r) in &results {
            match r {
                Ok(()) => println!("ok    {id}"),
                Err(e) => println!("error {id}  {e:#}"),
            }
        }
    }
    if results.iter().any(|(_, r)| r.is_err()) {
        bail!("one or more devices failed to update");
    }
    Ok(())
}
