//! Logicool ドライバを UI と同じ経路 (connect → scan → driver → info/caps/read/battery/apply) で
//! 実機に当てる検証用。apply は段階 1 では何も送らずエラーになることを確認する。
//! 使い方: `cargo run -p devhub-hid --example logiread` (マウスを動かしてから)

use devhub_core::model::*;

fn main() {
    let hub = devhub_hid::connect().expect("hid");
    hub.watch(Box::new(|ev| println!("  (event) {ev:?}")));
    let list = hub.scan().expect("scan");
    let Some(d) = list.iter().find(|d| d.driver == DriverKind::Logitech) else {
        println!("Logicool デバイスが見つからない ({} 台検出)", list.len());
        return;
    };
    println!("scan: {} / {} / supported={} id={}", d.name, d.model, d.supported, d.id);
    let drv = hub.driver(&d.id).expect("driver");
    let info = drv.info();
    println!(
        "info: name={:?} model={:?} fw={:?} asleep={} battery={:?}",
        info.name, info.model, info.firmware, info.asleep, info.battery
    );
    let caps = drv.caps();
    if let Some(m) = &caps.mouse {
        println!(
            "caps.mouse: dpi {}..{} step {} stages {}..{} buttons {:?}",
            m.dpi_min,
            m.dpi_max,
            m.dpi_step,
            m.min_stages,
            m.max_stages,
            m.buttons.iter().map(|b| b.name.as_str()).collect::<Vec<_>>()
        );
    } else {
        println!("caps.mouse: None (マウスが寝ていて機能を列挙できていない)");
    }
    for s in &caps.settings {
        println!("caps.setting: {} {:?} enabled_when={:?}", s.id, s.kind, s.enabled_when);
    }
    println!("caps.battery: {}", caps.battery);
    match drv.read() {
        Ok(st) => {
            println!("read.settings: {:?}", st.settings);
            if let Some(m) = &st.mouse {
                println!(
                    "read.mouse: stages {:?} active {} buttons {:?}",
                    m.stages.iter().map(|s| s.x).collect::<Vec<_>>(),
                    m.active_stage,
                    m.buttons
                );
            }
        }
        Err(e) => println!("read: ERR {e}"),
    }
    println!("battery: {:?}", drv.battery());
    let r = drv.apply(&Change::ActiveStage(0));
    println!("apply(ActiveStage 0): {r:?}");
}
