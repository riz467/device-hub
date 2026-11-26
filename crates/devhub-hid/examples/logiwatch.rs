//! Logitech 受信機の休眠/覚醒の観測 (読み取り専用)。
//!
//! HID++ short/long コレクションに届く全レポートを時刻付きで出し、
//! 2 秒ごとに device 1 へ Root.ping (読み取りのみ) を送って応答の有無を記録する。
//! 使い方: `cargo run -p devhub-hid --example logiwatch [秒数=240]`

use hidapi::HidApi;
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
const SW: u8 = 0x0E;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(240);
    let api = HidApi::new().expect("hidapi");
    let find = |u: u16| {
        api.device_list()
            .find(|d| d.vendor_id() == VID && d.usage_page() == 0xFF00 && d.usage() == u)
            .and_then(|d| d.open_device(&api).ok())
    };
    let long = find(0x02).expect("long");
    let short = find(0x01);
    let t0 = Instant::now();
    let mut next_ping = Instant::now();
    let mut waiting: Option<Instant> = None;
    let mut b = [0u8; 64];
    while t0.elapsed() < Duration::from_secs(secs) {
        let ts = t0.elapsed().as_secs_f32();
        if waiting.is_none() && Instant::now() >= next_ping {
            let mut w = [0u8; 20];
            w[0] = 0x11;
            w[1] = 1;
            w[2] = 0;
            w[3] = 0x10 | SW;
            w[6] = 0x5A;
            let _ = long.write(&w);
            waiting = Some(Instant::now());
        }
        if let Some(s) = &short {
            if let Ok(n) = s.read_timeout(&mut b, 10) {
                if n > 0 {
                    println!("{ts:7.2} short {}", hex(&b[..n]));
                }
            }
        }
        if let Ok(n) = long.read_timeout(&mut b, 10) {
            if n > 0 {
                let mine = n >= 4 && b[0] == 0x11 && b[1] == 1 && b[2] == 0 && b[3] == 0x10 | SW;
                if mine {
                    if let Some(w) = waiting.take() {
                        println!("{ts:7.2} ping OK ({} ms)", w.elapsed().as_millis());
                    }
                    next_ping = Instant::now() + Duration::from_secs(2);
                } else if !(n >= 4 && b[0] == 0x11 && b[3] & 0x0F != 0) {
                    // 他アプリの要求の応答写し (SW≠0) は省く。デバイス発の通知だけ出す。
                    println!("{ts:7.2} long  {}", hex(&b[..n]));
                }
            }
        }
        if let Some(w) = waiting {
            if w.elapsed() > Duration::from_millis(1500) {
                println!("{ts:7.2} ping timeout");
                waiting = None;
                next_ping = Instant::now() + Duration::from_secs(2);
            }
        }
    }
}
