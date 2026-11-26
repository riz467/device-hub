//! Logicool 段階 2 (オンボードプロファイル書き込み) の実機往復テスト。
//!
//! 1. 元の状態を読む (sector の生バイトも控える)
//! 2. ポーリング 500Hz / DPI [400,800,1600] 既定 2 段目 / ステージ切替 / ボタン 4 (戻る) → 音量+ を書き、
//!    それぞれ read() で反映を確認
//! 3. 自動退避ファイルから restore_backup で書き戻し、sector が元とバイト一致することを確認
//!
//! Device HUB 本体は終了してから実行する。マウスを動かしてから:
//!   cargo run -p devhub-hid --example logie2e

use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use std::time::{Duration, Instant};

fn main() {
    let hub = devhub_hid::connect().expect("hid");
    let list = hub.scan().expect("scan");
    let d = list
        .iter()
        .find(|d| d.driver == DriverKind::Logitech)
        .expect("Logicool デバイスが見つかりません")
        .clone();
    let api = hidapi::HidApi::new().expect("hidapi");
    let dev = api
        .device_list()
        .find(|x| x.vendor_id() == d.vid && x.product_id() == d.pid && x.usage_page() == 0xFF00 && x.usage() == 0x02)
        .and_then(|x| x.open_device(&api).ok())
        .expect("HID++ long");
    let drv = devhub_hid::logitech::open(dev, d, None).expect("open");

    let t0 = Instant::now();
    while drv.info().asleep {
        if t0.elapsed() > Duration::from_secs(600) {
            panic!("マウスが起きません");
        }
        println!("スリープ中 — マウスを動かしてください");
        std::thread::sleep(Duration::from_secs(15));
    }
    let orig_bytes = drv.current_profile_bytes().expect("sector");
    let orig = drv.read().expect("read");
    let om = orig.mouse.clone().unwrap();
    println!(
        "元: rate {:?} dpi {:?} active {} buttons {:?}",
        orig.settings.get("polling_wireless"),
        om.stages.iter().map(|s| s.x).collect::<Vec<_>>(),
        om.active_stage,
        om.buttons
    );

    let mut ok = true;
    let mut check = |name: &str, cond: bool, detail: String| {
        println!("[{}] {name}: {detail}", if cond { "OK" } else { "NG" });
        ok &= cond;
    };

    // ---- ポーリング
    let t = Instant::now();
    drv.apply(&Change::Setting { id: "polling_wireless".into(), value: SettingValue::Choice(500) })
        .expect("apply rate");
    let el = t.elapsed();
    let st = drv.read().expect("read");
    check(
        "ポーリング 500Hz",
        st.settings.get("polling_wireless") == Some(&SettingValue::Choice(500)),
        format!("{:?} ({} ms, 退避 {:?})", st.settings.get("polling_wireless"), el.as_millis(), drv.backup_path()),
    );

    // ---- DPI ステージ
    let stage = |x| DpiStage { x, y: None, color: Rgb::default(), enabled: true };
    drv.apply(&Change::DpiStages { stages: vec![stage(400), stage(800), stage(1600)], active: 1 })
        .expect("apply dpi");
    let m = drv.read().expect("read").mouse.unwrap();
    let xs: Vec<u32> = m.stages.iter().map(|s| s.x).collect();
    check("DPI [400,800,1600] 使用中 2 段目", xs == [400, 800, 1600] && m.active_stage == 1, format!("{xs:?} active {}", m.active_stage));

    // ---- ステージ切替 (RAM のみ)
    let before = drv.current_profile_bytes().unwrap();
    drv.apply(&Change::ActiveStage(2)).expect("apply stage");
    let m = drv.read().expect("read").mouse.unwrap();
    let after = drv.current_profile_bytes().unwrap();
    check("ステージ切替 → 3 段目 (フラッシュ不変)", m.active_stage == 2 && before == after, format!("active {} sector 同一 {}", m.active_stage, before == after));

    // ---- ボタン
    drv.apply(&Change::Button { slot: 3, action: ButtonAction::Media(MediaFn::VolumeUp) }).expect("apply button");
    let m = drv.read().expect("read").mouse.unwrap();
    check("ボタン 4 → 音量+", m.buttons[3] == ButtonAction::Media(MediaFn::VolumeUp), format!("{:?}", m.buttons[3]));
    check("ステージは切替後の 3 段目のまま", m.active_stage == 2, format!("active {}", m.active_stage));

    // ---- 拒否されるべきもの (何も書かない)
    let r = drv.apply(&Change::Button { slot: 0, action: ButtonAction::Mouse(MouseFn::Right) });
    check("左クリック変更は拒否", r.is_err(), format!("{r:?}"));
    let r = drv.apply(&Change::DpiStages { stages: vec![stage(99999)], active: 0 });
    check("範囲外 DPI は拒否", r.is_err(), format!("{r:?}"));

    // ---- 退避から書き戻し
    let path = drv.backup_path().expect("退避ファイルがあるはず");
    let txt = std::fs::read_to_string(&path).expect("退避を読む");
    let msg = drv.restore_backup(&txt).expect("restore");
    println!("restore: {msg}");
    let back = drv.current_profile_bytes().unwrap();
    check("書き戻し後 sector が元とバイト一致", back == orig_bytes, format!("{} byte", back.len()));
    let st = drv.read().expect("read");
    let m = st.mouse.clone().unwrap();
    check(
        "書き戻し後の値",
        st.settings.get("polling_wireless") == orig.settings.get("polling_wireless")
            && m.stages.iter().map(|s| s.x).collect::<Vec<_>>() == om.stages.iter().map(|s| s.x).collect::<Vec<_>>()
            && m.buttons == om.buttons,
        format!("rate {:?} dpi {:?} active {}", st.settings.get("polling_wireless"), m.stages.iter().map(|s| s.x).collect::<Vec<_>>(), m.active_stage),
    );
    println!("退避ファイル: {}", path.display());
    println!("{}", if ok { "ALL OK" } else { "NG あり" });
}
