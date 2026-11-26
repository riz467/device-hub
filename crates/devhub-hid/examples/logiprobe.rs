//! Logitech HID++ 読み取り専用プローブ (LIGHTSPEED 受信機 / PRO X SUPERLIGHT 系の解析用)。
//!
//! - 受信機 (device index 0xFF): HID++ 1.0 のレジスタ読み出し (0x81 / 0x83) のみ。
//! - マウス (device index 1..=6): HID++ 2.0 の get 系関数のみ。
//!
//! set 系・memoryWrite・DFU・reset などの書き込み関数は一切呼ばない。
//! 使い方: `cargo run -p devhub-hid --example logiprobe [--profile-dump]`
//! (マウスがスリープ中だと応答しないので、実行前に少し動かしておく)

use hidapi::{HidApi, HidDevice};
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
/// 自分の要求を他アプリ (G HUB 等) の要求と見分けるための software id (1..=15)。
const SW: u8 = 0x0D;
const TIMEOUT: Duration = Duration::from_millis(600);

struct Port {
    long: HidDevice,
    short: Option<HidDevice>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn feature_name(id: u16) -> &'static str {
    match id {
        0x0000 => "Root",
        0x0001 => "FeatureSet",
        0x0002 => "FeatureInfo",
        0x0003 => "DeviceFwVersion",
        0x0005 => "DeviceName",
        0x0007 => "DeviceFriendlyName",
        0x0008 => "KeepAlive",
        0x00C2 => "DfuControlSigned",
        0x00C3 => "DfuControlSecure",
        0x00D0 => "Dfu",
        0x1000 => "BatteryStatus",
        0x1001 => "BatteryVoltage",
        0x1004 => "UnifiedBattery",
        0x1300 => "LedControl",
        0x1602 => "PasswordAuthentication",
        0x1802 => "DeviceReset",
        0x1803 => "GpioAccess",
        0x1805 => "OobState",
        0x1806 => "ConfigurableDeviceProperties",
        0x1811 => "?(0x1811)",
        0x1815 => "HostsInfo",
        0x18A1 => "?(0x18A1)",
        0x1890 => "RfTest",
        0x18B1 => "?(0x18B1)",
        0x1B04 => "ReprogControlsV4",
        0x1D4B => "WirelessDeviceStatus",
        0x1DF3 => "EquadDjDebugInfo",
        0x1E00 => "EnableHiddenFeatures",
        0x1E02 => "?(0x1E02)",
        0x1E22 => "?(0x1E22)",
        0x1EB0 => "?(0x1EB0)",
        0x1F1F => "?(0x1F1F)",
        0x2200 => "MousePointer",
        0x2201 => "AdjustableDpi",
        0x2202 => "ExtendedAdjustableDpi",
        0x2250 => "AnalysisMode",
        0x8060 => "AdjustableReportRate",
        0x8061 => "ExtendedAdjustableReportRate",
        0x8070 => "ColorLedEffects",
        0x8071 => "RgbEffects",
        0x8081 => "PerKeyLighting",
        0x8090 => "ModeStatus",
        0x8100 => "OnboardProfiles",
        0x8110 => "MouseButtonSpy",
        0x8111 => "LatencyMonitoring",
        _ => "?",
    }
}

impl Port {
    fn open(api: &HidApi) -> Port {
        let mut long = None;
        let mut short = None;
        println!("== 046d の HID コレクション");
        for d in api.device_list().filter(|d| d.vendor_id() == VID) {
            println!(
                "  {:04x}:{:04x} if={} up={:04x} u={:02x} {:?}",
                d.vendor_id(),
                d.product_id(),
                d.interface_number(),
                d.usage_page(),
                d.usage(),
                d.product_string()
            );
            if d.usage_page() == 0xFF00 && d.usage() == 0x02 && long.is_none() {
                long = Some(d.clone());
            }
            if d.usage_page() == 0xFF00 && d.usage() == 0x01 && short.is_none() {
                short = Some(d.clone());
            }
        }
        let long = long.expect("HID++ long コレクション (up=ff00 u=02) が見つからない");
        println!(
            "  -> HID++ long: pid {:04x} if={}",
            long.product_id(),
            long.interface_number()
        );
        let long = long.open_device(api).expect("open long");
        let short = short.and_then(|s| s.open_device(api).ok());
        let p = Port { long, short };
        p.drain();
        p
    }

    fn drain(&self) {
        let mut b = [0u8; 64];
        while self.long.read_timeout(&mut b, 10).unwrap_or(0) > 0 {}
        if let Some(s) = &self.short {
            while s.read_timeout(&mut b, 10).unwrap_or(0) > 0 {}
        }
    }

    /// 1 回分の受信 (long → short の順に短く待つ)。
    fn read_any(&self) -> Option<Vec<u8>> {
        let mut b = [0u8; 64];
        if let Ok(n) = self.long.read_timeout(&mut b, 20) {
            if n > 0 {
                return Some(b[..n].to_vec());
            }
        }
        if let Some(s) = &self.short {
            if let Ok(n) = s.read_timeout(&mut b, 5) {
                if n > 0 {
                    return Some(b[..n].to_vec());
                }
            }
        }
        None
    }

    /// HID++ 2.0 要求。Ok(params 16 byte) / Err(エラー説明)。
    fn call(&self, dev: u8, feat: u8, func: u8, params: &[u8]) -> Result<Vec<u8>, String> {
        let fs = (func << 4) | SW;
        let mut w = [0u8; 20];
        w[0] = 0x11;
        w[1] = dev;
        w[2] = feat;
        w[3] = fs;
        w[4..4 + params.len()].copy_from_slice(params);
        self.long.write(&w).map_err(|e| format!("write: {e}"))?;
        let t0 = Instant::now();
        while t0.elapsed() < TIMEOUT {
            let Some(r) = self.read_any() else { continue };
            if r.len() >= 20 && r[0] == 0x11 && r[1] == dev && r[2] == feat && r[3] == fs {
                return Ok(r[4..20].to_vec());
            }
            if r.len() >= 6 && r[0] == 0x11 && r[1] == dev && r[2] == 0xFF && r[3] == feat && r[4] == fs {
                return Err(format!("HID++2.0 error 0x{:02x}", r[5]));
            }
            if r.len() >= 6 && r[0] == 0x10 && r[1] == dev && r[2] == 0x8F && r[3] == feat && r[4] == fs {
                return Err(format!("HID++1.0 error 0x{:02x}", r[5]));
            }
            println!("    (通知) {}", hex(&r));
        }
        Err("timeout".into())
    }

    /// HID++ 1.0 レジスタ読み出し (受信機)。sub = 0x81 (short) / 0x83 (long)。
    fn read_reg(&self, dev: u8, sub: u8, reg: u8, p: [u8; 3]) -> Result<Vec<u8>, String> {
        let w = [0x10, dev, sub, reg, p[0], p[1], p[2]];
        match &self.short {
            Some(s) => s.write(&w).map_err(|e| format!("write: {e}"))?,
            None => return Err("short コレクション無し".into()),
        };
        let t0 = Instant::now();
        while t0.elapsed() < TIMEOUT {
            let Some(r) = self.read_any() else { continue };
            if r.len() >= 4 && (r[0] == 0x10 || r[0] == 0x11) && r[1] == dev && r[2] == sub && r[3] == reg {
                return Ok(r[4..].to_vec());
            }
            if r.len() >= 6 && r[0] == 0x10 && r[1] == dev && r[2] == 0x8F && r[3] == sub && r[4] == reg {
                return Err(format!("HID++1.0 error 0x{:02x}", r[5]));
            }
            println!("    (通知) {}", hex(&r));
        }
        Err("timeout".into())
    }
}

fn crc16(d: &[u8]) -> u16 {
    let mut c: u16 = 0xFFFF;
    for &b in d {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
        }
    }
    c
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn main() {
    let dump = std::env::args().any(|a| a == "--profile-dump");
    let api = HidApi::new().expect("hidapi");
    let port = Port::open(&api);

    println!("\n== 受信機 (0xFF) HID++1.0 レジスタ (読み出しのみ)");
    for (name, sub, reg, p) in [
        ("0x02 接続数        ", 0x81u8, 0x02u8, [0u8; 3]),
        ("0xF1 fw (major)    ", 0x81, 0xF1, [0x01, 0, 0]),
        ("0xF1 fw (build)    ", 0x81, 0xF1, [0x02, 0, 0]),
        ("0xB5 受信機情報    ", 0x83, 0xB5, [0x03, 0, 0]),
        ("0xB5 ペア情報 #1   ", 0x83, 0xB5, [0x20, 0, 0]),
        ("0xB5 ペア拡張 #1   ", 0x83, 0xB5, [0x30, 0, 0]),
        ("0xB5 名前 #1       ", 0x83, 0xB5, [0x40, 0, 0]),
    ] {
        match port.read_reg(0xFF, sub, reg, p) {
            Ok(r) => println!("  {name} {}", hex(&r)),
            Err(e) => println!("  {name} {e}"),
        }
    }

    println!("\n== device index ごとの ping (Root.ping)");
    let mut target = None;
    for dev in [0xFFu8, 1, 2, 3, 4, 5, 6] {
        match port.call(dev, 0x00, 1, &[0, 0, 0x5A]) {
            Ok(r) => {
                println!("  dev {dev:#04x}: HID++ {}.{} (echo {:02x})", r[0], r[1], r[2]);
                if target.is_none() && dev != 0xFF && r[0] >= 2 {
                    target = Some(dev);
                }
            }
            Err(e) => println!("  dev {dev:#04x}: {e}"),
        }
    }
    if target.is_none() {
        // スリープ中のマウスは ping に答えない — 動かされるまで最大 5 分 dev 1 を叩き続ける。
        println!("  マウスを動かしてください (最大 5 分待機)...");
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(300) {
            if let Ok(r) = port.call(1, 0x00, 1, &[0, 0, 0x5A]) {
                println!("  dev 0x01: HID++ {}.{} (起床)", r[0], r[1]);
                target = Some(1);
                break;
            }
        }
    }
    let Some(dev) = target else {
        println!("\nHID++2.0 のマウスが応答しない (スリープ中なら動かしてから再実行)");
        return;
    };
    println!("\n== 対象 device index {dev}");

    // FeatureSet で全機能を列挙
    let fs_idx = port.call(dev, 0, 0, &[0x00, 0x01]).expect("getFeature(FeatureSet)")[0];
    let count = port.call(dev, fs_idx, 0, &[]).expect("FeatureSet.getCount")[0];
    let mut feats: Vec<(u8, u16)> = vec![(0, 0x0000)];
    println!("  機能数 {count} (Root を除く)");
    println!("  idx  id     type ver  name");
    for i in 0..=count {
        match port.call(dev, fs_idx, 1, &[i]) {
            Ok(r) => {
                let id = be16(&r);
                let ty = r[2];
                let flags = [
                    (0x80, "obsolete"),
                    (0x40, "hidden"),
                    (0x20, "engineering"),
                    (0x10, "manuf-deact"),
                    (0x08, "compl-deact"),
                ]
                .iter()
                .filter(|(m, _)| ty & m != 0)
                .map(|(_, n)| *n)
                .collect::<Vec<_>>()
                .join(",");
                println!("  {i:>3}  {id:04x}   {ty:02x}   {:>3}  {} {flags}", r[3], feature_name(id));
                feats.push((i, id));
            }
            Err(e) => println!("  {i:>3}  {e}"),
        }
    }
    let find = |id: u16| feats.iter().find(|(_, f)| *f == id).map(|(i, _)| *i);

    if let Some(i) = find(0x0005) {
        let len = port.call(dev, i, 0, &[]).map(|r| r[0]).unwrap_or(0);
        let mut name = Vec::new();
        while (name.len() as u8) < len {
            match port.call(dev, i, 1, &[name.len() as u8]) {
                Ok(r) => name.extend(r.iter().take((len as usize) - name.len()).copied()),
                Err(_) => break,
            }
        }
        let ty = port.call(dev, i, 2, &[]).map(|r| r[0]);
        println!("\n  DeviceName: {:?} / type {:?}", String::from_utf8_lossy(&name), ty);
    }
    if let Some(i) = find(0x0003) {
        let n = port.call(dev, i, 0, &[]).map(|r| r[0]).unwrap_or(0);
        println!("\n  DeviceFwVersion: entity {n}");
        for e in 0..n {
            if let Ok(r) = port.call(dev, i, 1, &[e]) {
                println!(
                    "    #{e} type {} {}{:02x}.{:02x} build {:04x} raw {}",
                    r[0],
                    String::from_utf8_lossy(&r[1..4]),
                    r[4],
                    r[5],
                    be16(&r[6..8]),
                    hex(&r)
                );
            }
        }
    }
    for (id, label, funcs) in [
        (0x1000u16, "BatteryStatus", &[0u8][..]),
        (0x1004, "UnifiedBattery caps/status", &[0, 1][..]),
        (0x1D4B, "WirelessDeviceStatus", &[][..]),
        (0x2201, "AdjustableDpi count", &[0][..]),
        (0x2202, "ExtendedAdjustableDpi count", &[0][..]),
        (0x8060, "ReportRate list/current", &[0, 1][..]),
        (0x8061, "ExtReportRate list/current", &[1, 2][..]),
        (0x8090, "ModeStatus", &[0][..]),
        (0x8110, "MouseButtonSpy count", &[0][..]),
    ] {
        let Some(i) = find(id) else { continue };
        println!("\n  {label} ({id:04x} @ idx {i})");
        for f in funcs {
            match port.call(dev, i, *f, &[]) {
                Ok(r) => println!("    fn{f}: {}", hex(&r)),
                Err(e) => println!("    fn{f}: {e}"),
            }
        }
    }
    if let Some(i) = find(0x2201) {
        for (f, label) in [(1u8, "dpiList"), (2, "currentDpi")] {
            match port.call(dev, i, f, &[0]) {
                Ok(r) => println!("    2201 fn{f} {label}(sensor0): {}", hex(&r)),
                Err(e) => println!("    2201 fn{f} {label}: {e}"),
            }
        }
    }
    if let Some(i) = find(0x2202) {
        for (f, args, label) in [
            (1u8, &[0u8][..], "capabilities(sensor0)"),
            (2, &[0, 0, 0][..], "dpiRanges(sensor0, X, page0)"),
            (5, &[0][..], "currentDpi(sensor0)"),
        ] {
            match port.call(dev, i, f, args) {
                Ok(r) => println!("    2202 fn{f} {label}: {}", hex(&r)),
                Err(e) => println!("    2202 fn{f} {label}: {e}"),
            }
        }
    }
    if let Some(i) = find(0x1B04) {
        let n = port.call(dev, i, 0, &[]).map(|r| r[0]).unwrap_or(0);
        println!("\n  ReprogControlsV4: {n} 個");
        for k in 0..n {
            if let Ok(r) = port.call(dev, i, 1, &[k]) {
                let cid = be16(&r);
                let rep = port
                    .call(dev, i, 2, &[r[0], r[1]])
                    .map(|x| hex(&x[..6]))
                    .unwrap_or_else(|e| e);
                println!("    #{k} cid {cid:#06x} tid {:#06x} flags {:02x} {:02x} pos {} grp {} gmask {:02x} | reporting {rep}",
                    be16(&r[2..4]), r[4], r[8], r[5], r[6], r[7]);
            }
        }
    }
    if let Some(i) = find(0x8100) {
        println!("\n  OnboardProfiles (8100 @ idx {i})");
        let desc = port.call(dev, i, 0, &[]);
        match &desc {
            Ok(r) => println!(
                "    desc: memModel {} profFmt {} macroFmt {} profiles {} oob {} buttons {} sectors {} sectorSize {} layout {:02x} info {:02x} | raw {}",
                r[0], r[1], r[2], r[3], r[4], r[5], r[6], be16(&r[7..9]), r[9], r[10], hex(r)
            ),
            Err(e) => println!("    desc: {e}"),
        }
        for (f, label) in [(2u8, "mode(1=onboard,2=host)"), (4, "currentProfile"), (0xB, "currentDpiIndex")] {
            match port.call(dev, i, f, &[]) {
                Ok(r) => println!("    fn{f} {label}: {}", hex(&r[..4])),
                Err(e) => println!("    fn{f} {label}: {e}"),
            }
        }
        if dump {
            let size = desc.as_ref().map(|r| be16(&r[7..9])).unwrap_or(0) as usize;
            let mut sectors = vec![0x0000u16];
            // ディレクトリ (sector 0) を読んでから、載っているプロファイル sector を読む。
            let mut si = 0;
            while si < sectors.len() {
                let sec = sectors[si];
                si += 1;
                let mut data = Vec::new();
                let mut off = 0usize;
                while off < size {
                    let o = off.min(size.saturating_sub(16));
                    match port.call(dev, i, 5, &[(sec >> 8) as u8, sec as u8, (o >> 8) as u8, o as u8]) {
                        Ok(r) => {
                            data.truncate(o);
                            data.extend_from_slice(&r);
                        }
                        Err(e) => {
                            println!("    sector {sec:#06x} off {o}: {e}");
                            break;
                        }
                    }
                    off += 16;
                }
                // 末尾 2 byte = CRC-16/CCITT-FALSE (init 0xFFFF, poly 0x1021) を big endian で、という想定を検証する。
                let crc_ok = if data.len() == size && size >= 2 {
                    let calc = crc16(&data[..size - 2]);
                    let stored = be16(&data[size - 2..]);
                    format!("crc stored {stored:04x} calc {calc:04x} {}", if calc == stored { "OK" } else { "NG" })
                } else {
                    "crc -".into()
                };
                println!("    -- sector {sec:#06x} ({} bytes) {crc_ok}", data.len());
                for (n, c) in data.chunks(16).enumerate() {
                    println!("    {:04x}: {}", n * 16, hex(c));
                }
                if sec == 0 {
                    for e in data.chunks(4) {
                        let s = be16(e);
                        if s == 0xFFFF || s == 0 {
                            break;
                        }
                        if !sectors.contains(&s) {
                            sectors.push(s);
                        }
                    }
                }
            }
        }
    }
    println!("\n-- done --");
}
