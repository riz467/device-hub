//! Logicool オンボードプロファイルの退避ファイルを本体に書き戻す。
//!
//! 退避ファイルは Device HUB が最初の書き込みの前に自動で作る
//! (`%APPDATA%\DeviceHub\backups\logitech\<model>-<日付>-<時刻>.txt`)。
//! logiprobe `--profile-dump` の出力も同じ書式なので読める。
//!
//! 使い方 (Device HUB 本体は終了してから。同じ受信機を 2 つのアプリで操作しない):
//!   cargo run -p devhub-hid --example logirestore -- "<退避ファイル>"
//!   cargo run -p devhub-hid --example logirestore -- --check "<退避ファイル>"   (書かずに中身の確認だけ)

use devhub_core::model::*;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let Some(path) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("使い方: logirestore [--check] <退避ファイル>");
        std::process::exit(2);
    };
    let txt = std::fs::read_to_string(path).expect("退避ファイルを読めません");

    let hub = devhub_hid::connect().expect("hid");
    let list = hub.scan().expect("scan");
    let Some(d) = list.iter().find(|d| d.driver == DriverKind::Logitech) else {
        eprintln!("Logicool デバイスが見つかりません");
        std::process::exit(1);
    };
    let api = hidapi::HidApi::new().expect("hidapi");
    let dev = api
        .device_list()
        .find(|x| {
            x.vendor_id() == d.vid && x.product_id() == d.pid && x.usage_page() == 0xFF00 && x.usage() == 0x02
        })
        .and_then(|x| x.open_device(&api).ok())
        .expect("HID++ long コレクションを開けません");
    let drv = devhub_hid::logitech::open(dev, d.clone(), None).expect("open");
    let info = devhub_core::backend::DeviceDriver::info(&drv);
    println!("対象: {} ({}) fw {:?} asleep={}", info.name, info.model, info.firmware, info.asleep);
    if info.asleep {
        eprintln!("マウスがスリープ中です。動かしてからやり直してください");
        std::process::exit(1);
    }
    if check {
        println!("--check: 書き込みはしません");
        return;
    }
    match drv.restore_backup(&txt) {
        Ok(msg) => println!("完了: {msg}"),
        Err(e) => {
            eprintln!("失敗: {e}");
            std::process::exit(1);
        }
    }
}
