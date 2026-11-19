//! Logitech HID++ ドライバ経路の実機検証 — hidpp::open → caps → read。
//! `--write` を付けると書き込み往復 (ポーリング・DPI・ボタン) を試し、最後に元へ戻す。
//!
//! `cargo run -p devhub-hid --example hidppread`
//! `cargo run -p devhub-hid --example hidppread -- --write`

use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use devhub_hid::hidpp;
use devhub_hid::transport;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn main() {
    let write = std::env::args().any(|a| a == "--write");
    let api = transport::open().expect("hidapi");
    let find = |usage: u16| {
        api.device_list()
            .find(|d| {
                d.vendor_id() == 0x046d
                    && [0xc547, 0xc539, 0xc54d, 0xc094, 0xc088].contains(&d.product_id())
                    && d.usage_page() == 0xff00
                    && d.usage() == usage
            })
            .cloned()
    };
    let long_i = find(2).expect("HID++ long collection (FF00:02)");
    let short = find(1).and_then(|d| d.open_device(&api).ok());
    let long = long_i.open_device(&api).expect("open long");
    let wired = [0xc094, 0xc088].contains(&long_i.product_id());
    let info = DeviceInfo {
        id: DeviceId("hidpp-probe".into()),
        kind: DeviceKind::Mouse,
        name: "Logitech?".into(),
        vendor: "Logitech".into(),
        model: "logi-hidpp".into(),
        vid: 0x046d,
        pid: long_i.product_id(),
        transport: if wired {
            Transport::Wired
        } else {
            Transport::Wireless24G
        },
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Hidpp,
        supported: false,
        asleep: false,
        battery: None,
        board_id: None,
    };
    let detected = Arc::new(Mutex::new(HashMap::new()));
    let d = hidpp::open(long, short, info, None, detected).expect("driver");

    let mut i = d.info();
    for _ in 0..15 {
        if !i.asleep {
            break;
        }
        println!("   (マウスが寝ています — 動かしてください)");
        std::thread::sleep(std::time::Duration::from_millis(800));
        i = d.info();
    }
    println!(
        "== info: name={} model={} fw={:?} dongle={:?} bat={:?} asleep={}",
        i.name, i.model, i.firmware, i.dongle_firmware, i.battery, i.asleep
    );

    let c = d.caps();
    let m = c.mouse.as_ref().unwrap();
    println!(
        "== caps: dpi {}..{} step {} stages<= {} buttons={} profiles={:?}",
        m.dpi_min,
        m.dpi_max,
        m.dpi_step,
        m.max_stages,
        m.buttons.len(),
        c.profiles
    );
    for s in &c.settings {
        println!("   setting {} {:?}", s.id, s.kind);
    }

    let s = d.read().expect("read");
    dump(&s);
    if !write {
        println!("OK (read only)");
        return;
    }

    let poll_id = s
        .settings
        .keys()
        .find(|k| k.starts_with("polling"))
        .cloned()
        .unwrap();
    let orig_poll = s.settings[&poll_id].clone();
    let orig_mouse = s.mouse.clone().unwrap();

    // 1) ポーリング 500Hz → 読み戻し
    println!("== apply {poll_id} -> 500");
    d.apply(&Change::Setting {
        id: poll_id.clone(),
        value: SettingValue::Choice(500),
    })
    .expect("apply polling");
    let s2 = d.read().expect("read2");
    assert_eq!(s2.settings[&poll_id], SettingValue::Choice(500));
    println!("   readback ok");

    // 2) DPI 3 段 (400/800/1600, active=1) → 読み戻し
    let st = |x| DpiStage {
        x,
        y: None,
        color: Rgb::default(),
        enabled: true,
    };
    println!("== apply DPI [400, 800, 1600] active=1");
    d.apply(&Change::DpiStages {
        stages: vec![st(400), st(800), st(1600)],
        active: 1,
    })
    .expect("apply dpi");
    let s3 = d.read().expect("read3");
    let got: Vec<u32> = s3.mouse.as_ref().unwrap().stages.iter().map(|s| s.x).collect();
    assert_eq!(got, vec![400, 800, 1600]);
    println!("   readback ok, active={}", s3.mouse.as_ref().unwrap().active_stage);

    // 3) サイド (進む) → キー 'a' → 読み戻し
    let a = ButtonAction::Key(KeyStroke { mods: 0, usage: 4 });
    println!("== apply button4 -> {a:?}");
    d.apply(&Change::Button {
        slot: 4,
        action: a.clone(),
    })
    .expect("apply button");
    let s4 = d.read().expect("read4");
    assert_eq!(s4.mouse.as_ref().unwrap().buttons[4], a);
    println!("   readback ok");

    // 元に戻す
    println!("== restore");
    d.apply(&Change::Button {
        slot: 4,
        action: orig_mouse.buttons[4].clone(),
    })
    .expect("restore button");
    d.apply(&Change::DpiStages {
        stages: orig_mouse.stages.clone(),
        active: orig_mouse.active_stage,
    })
    .expect("restore dpi");
    d.apply(&Change::Setting {
        id: poll_id.clone(),
        value: orig_poll,
    })
    .expect("restore polling");
    let s5 = d.read().expect("read5");
    dump(&s5);
    assert_eq!(s5.settings, s.settings);
    assert_eq!(s5.mouse, s.mouse);
    println!("OK (write roundtrip + restore)");
}

fn dump(s: &DeviceState) {
    let ms = s.mouse.as_ref().unwrap();
    println!(
        "== stages (active={}): {:?}",
        ms.active_stage,
        ms.stages.iter().map(|s| s.x).collect::<Vec<_>>()
    );
    for (k, b) in ms.buttons.iter().enumerate() {
        println!("   button {k}: {b:?}");
    }
    for (k, v) in &s.settings {
        println!("   {k} = {v:?}");
    }
    println!("   profile: {:?}", s.profile);
}
