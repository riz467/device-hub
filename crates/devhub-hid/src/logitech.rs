//! Logicool / Logitech HID++ 2.0 マウス (LIGHTSPEED 受信機経由)。
//!
//! 実機解析: docs/LOGITECH_PROX_SUPERLIGHT.md (PRO X SUPERLIGHT, 046d:c547 / 無線 PID 0x4093)。
//!
//! - チャネル: 受信機 if2 `FF00:02` の HID++ long report (0x11, 20 byte)。
//!   `[0x11, device index, feature index, (function << 4) | software id, params…]`。
//!   応答は同じ先頭 4 byte で返る。エラーは `[0x11, dev, 0xFF, feat, fn|sw, code]`。
//! - マウスは受信機の device index 1。スリープ中は何も返らない (= タイムアウト → asleep)。
//! - 機能は FeatureSet (0x0001) の自己申告で列挙し、ID → index を覚えておく。
//!   caps は機種表ではなく、申告された機能 (2201 DPI / 8060 レート / 8100 オンボード / 1004 電池)
//!   から組み立てる。
//! - **段階 2 = オンボードプロファイルへの書き込み**。DPI ステージ・既定ステージ・
//!   ポーリングレート・ボタン割り当ては、現在のプロファイル sector (ユーザー領域 < 0x0100 のみ、
//!   ROM は書かない) を読み → 該当 byte だけ変え → CRC を付け直し → memoryWrite (8100 fn6/7/8) →
//!   読み戻して一致確認 → setCurrentProfile で再読込、の順で書く。
//!   - 最初の書き込みの前に、ディレクトリ (sector 0) と全ユーザー sector を
//!     `%APPDATA%\DeviceHub\backups\logitech\` に退避する。退避できなければ書かない。
//!   - 中身が変わらない変更はフラッシュに書かない。現在ステージの切替は RAM 上の
//!     setCurrentDpiIndex (fn 0xC) だけで、フラッシュに書かない。
//!   - 退避ファイルは `restore_backup` (example `logirestore`) で書き戻せる。
//!   - reset / DFU / onboard↔host モード切替は呼ばない。
//! - 通知 (スリープ・再接続・電池) は別ハンドルで同じ受信機の short/long コレクションを開いて拾う。
//!   Windows の HID は開いたハンドルごとに入力レポートの写しを受け取るので、要求/応答の
//!   ハンドルと取り合わない。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 自分の要求を他アプリ・デバイス発の通知と見分ける software id (1..=15。0 は通知)。
const SW: u8 = 0x0D;
/// 1 往復の待ち。無線経由でも通常 10〜30ms で返る。
const TIMEOUT: Duration = Duration::from_millis(500);

/// HID++ 2.0 の機能 ID (使うものだけ)。
mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FW_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const WIRELESS_STATUS: u16 = 0x1D4B;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
}

type Events = Option<Arc<dyn Fn(HubEvent) + Send + Sync>>;

/// これ以上の sector は ROM (工場出荷プロファイル)。書かない。
const ROM_SECTOR: u16 = 0x0100;
/// profile format 4 の配置 (docs/LOGITECH_PROX_SUPERLIGHT.md §4)。
const OFF_RATE: usize = 0x00;
const OFF_DEFAULT_DPI: usize = 0x01;
const OFF_DPI: usize = 0x03;
const DPI_SLOTS: usize = 5;
const OFF_BUTTONS: usize = 0x20;

// ---------------------------------------------------------------- 純粋な解析 (テスト対象)

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// CRC-16/CCITT-FALSE (init 0xFFFF, poly 0x1021)。オンボードプロファイル sector の末尾 2 byte (BE)。
pub(crate) fn crc16(d: &[u8]) -> u16 {
    let mut c: u16 = 0xFFFF;
    for &b in d {
        c ^= (b as u16) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x1021 } else { c << 1 };
        }
    }
    c
}

/// 2201 getSensorDpiList の応答 (`[sensor, list…]`) → (min, max, step)。
/// list は BE16 の並びで 0 終端。`0xE000 | step` は「直前の値から次の値まで step 刻み」。
pub(crate) fn parse_dpi_list(r: &[u8]) -> Option<(u32, u32, u32)> {
    let mut vals: Vec<u32> = Vec::new();
    let mut step: Option<u32> = None;
    let mut i = 1;
    while i + 1 < r.len() {
        let v = be16(&r[i..]);
        if v == 0 {
            break;
        }
        if v >= 0xE000 {
            step = Some((v & 0x1FFF) as u32);
        } else {
            vals.push(v as u32);
        }
        i += 2;
    }
    let min = *vals.iter().min()?;
    let max = *vals.iter().max()?;
    let step = step.unwrap_or_else(|| {
        // 離散リスト: 隣り合う差の最小値 (1 個だけなら 50)。
        let mut s = vals.clone();
        s.sort_unstable();
        s.windows(2).map(|w| w[1] - w[0]).filter(|d| *d > 0).min().unwrap_or(50)
    });
    Some((min, max, step.max(1)))
}

/// 8060 getReportRateList の bitmask (bit n = (n+1) ms) → Hz の昇順。
pub(crate) fn parse_rate_list(mask: u8) -> Vec<u32> {
    let mut v: Vec<u32> = (0..8)
        .filter(|n| mask & (1 << n) != 0)
        .map(|n| 1000 / (n + 1))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Consumer page usage → MediaFn (HID Usage Tables の標準値)。
fn media_of_usage(u: u16) -> Option<MediaFn> {
    Some(match u {
        0xCD => MediaFn::PlayPause,
        0xB7 => MediaFn::Stop,
        0xB5 => MediaFn::Next,
        0xB6 => MediaFn::Prev,
        0xE9 => MediaFn::VolumeUp,
        0xEA => MediaFn::VolumeDown,
        0xE2 => MediaFn::Mute,
        0x192 => MediaFn::Calculator,
        0x194 => MediaFn::MyComputer,
        0x196 => MediaFn::Browser,
        0x18A => MediaFn::Mail,
        0x6F => MediaFn::BrightnessUp,
        0x70 => MediaFn::BrightnessDown,
        0x227 => MediaFn::Refresh,
        _ => return None,
    })
}

/// MediaFn → Consumer page usage (`media_of_usage` の逆)。
fn usage_of_media(m: MediaFn) -> Option<u16> {
    (0..0x300u16).find(|&u| media_of_usage(u) == Some(m))
}

/// ButtonAction → オンボードプロファイルのボタン 4 byte。表せないものは None。
pub(crate) fn encode_button(a: &ButtonAction) -> Option<[u8; 4]> {
    Some(match a {
        ButtonAction::Mouse(m) => {
            let mask: u16 = match m {
                MouseFn::Left => 0x0001,
                MouseFn::Right => 0x0002,
                MouseFn::Middle => 0x0004,
                MouseFn::Back => 0x0008,
                MouseFn::Forward => 0x0010,
                _ => return None,
            };
            [0x80, 0x01, (mask >> 8) as u8, mask as u8]
        }
        ButtonAction::Key(k) => [0x80, 0x02, k.mods, u8::try_from(k.usage).ok()?],
        ButtonAction::Media(m) => {
            let u = usage_of_media(*m)?;
            [0x80, 0x03, (u >> 8) as u8, u as u8]
        }
        ButtonAction::Disabled => [0xFF; 4],
        // 読んだまま書き戻す (特殊機能・マクロ等)。
        ButtonAction::Unsupported { ty, params } if params.len() == 3 => {
            [*ty, params[0], params[1], params[2]]
        }
        _ => return None,
    })
}

/// sector の CRC (末尾 2 byte, BE) を付け直す。
pub(crate) fn seal(sector: &mut [u8]) {
    let n = sector.len();
    let c = crc16(&sector[..n - 2]);
    sector[n - 2..].copy_from_slice(&c.to_be_bytes());
}

/// DPI ステージ (1..=5 個) と既定ステージを書き込む。空き枠は 0 (G HUB と同じ)。
pub(crate) fn set_dpis(sector: &mut [u8], dpis: &[u32], default: usize) {
    for k in 0..DPI_SLOTS {
        let v = dpis.get(k).copied().unwrap_or(0) as u16;
        sector[OFF_DPI + 2 * k..OFF_DPI + 2 * k + 2].copy_from_slice(&v.to_le_bytes());
    }
    sector[OFF_DEFAULT_DPI] = default.min(dpis.len().saturating_sub(1)) as u8;
}

/// ポーリングレート (Hz) → byte 0 (= 報告間隔 ms)。
pub(crate) fn set_rate(sector: &mut [u8], hz: u32) {
    sector[OFF_RATE] = (1000 / hz.max(1)).clamp(1, 255) as u8;
}

pub(crate) fn set_button(sector: &mut [u8], slot: usize, b: [u8; 4]) {
    sector[OFF_BUTTONS + 4 * slot..OFF_BUTTONS + 4 * slot + 4].copy_from_slice(&b);
}

/// sector 群を fixtures と同じ書式 (`-- sector 0xNNNN …` + 16 byte/行) にする。
pub(crate) fn dump_sectors(sectors: &[(u16, Vec<u8>)]) -> String {
    let mut s = String::new();
    for (n, d) in sectors {
        let len = d.len();
        let (stored, calc) = if len > 2 {
            (be16(&d[len - 2..]), crc16(&d[..len - 2]))
        } else {
            (0, 1)
        };
        s += &format!(
            "-- sector {n:#06x} ({len} bytes) crc stored {stored:04x} calc {calc:04x} {}\n",
            if stored == calc { "OK" } else { "NG" }
        );
        for (i, row) in d.chunks(16).enumerate() {
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            s += &format!("{:04x}: {}\n", i * 16, hex.join(" "));
        }
    }
    s
}

/// `dump_sectors` (と logiprobe の出力) を読み戻す。
pub(crate) fn parse_dump(txt: &str) -> Vec<(u16, Vec<u8>)> {
    let mut out: Vec<(u16, Vec<u8>)> = Vec::new();
    let mut on = false;
    for line in txt.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("-- sector ") {
            let num = rest.split_whitespace().next().unwrap_or("");
            on = match u16::from_str_radix(num.trim_start_matches("0x"), 16) {
                Ok(n) => {
                    out.push((n, Vec::new()));
                    true
                }
                Err(_) => false,
            };
            continue;
        }
        if !on {
            continue;
        }
        let Some((off, hex)) = t.split_once(": ") else {
            on = false;
            continue;
        };
        if usize::from_str_radix(off, 16).is_err() {
            on = false;
            continue;
        }
        let bytes: Option<Vec<u8>> = hex.split_whitespace().map(|h| u8::from_str_radix(h, 16).ok()).collect();
        match (bytes, out.last_mut()) {
            (Some(b), Some((_, d))) => d.extend(b),
            _ => on = false,
        }
    }
    out
}

/// オンボードプロファイルのボタン 4 byte → ButtonAction。
/// 確実に分かっている型 (0x80 = HID: マウス/キー/コンシューマ) だけ解釈し、
/// それ以外 (0x90 特殊機能・マクロ等) は生バイトのまま `Unsupported` で持つ。
pub(crate) fn decode_button(b: &[u8]) -> ButtonAction {
    let raw = || ButtonAction::Unsupported {
        ty: b[0],
        params: b[1..4].to_vec(),
    };
    if b[..4] == [0xFF; 4] {
        return ButtonAction::Disabled;
    }
    match (b[0], b[1]) {
        (0x80, 0x01) => match be16(&b[2..4]) {
            0x0001 => ButtonAction::Mouse(MouseFn::Left),
            0x0002 => ButtonAction::Mouse(MouseFn::Right),
            0x0004 => ButtonAction::Mouse(MouseFn::Middle),
            0x0008 => ButtonAction::Mouse(MouseFn::Back),
            0x0010 => ButtonAction::Mouse(MouseFn::Forward),
            _ => raw(),
        },
        (0x80, 0x02) => ButtonAction::Key(KeyStroke {
            mods: b[2],
            usage: b[3] as u16,
        }),
        (0x80, 0x03) => media_of_usage(be16(&b[2..4]))
            .map(ButtonAction::Media)
            .unwrap_or_else(raw),
        _ => raw(),
    }
}

/// オンボードプロファイル sector の中身 (段階 1 で使う分だけ)。
#[derive(Debug, PartialEq)]
pub(crate) struct Profile {
    /// 0 以外の DPI ステージ (並び順そのまま)。
    pub dpis: Vec<u32>,
    /// 既定ステージ (byte 1)。
    pub default_stage: usize,
    pub buttons: Vec<ButtonAction>,
    /// byte 0 (報告間隔 ms) から。
    pub rate_hz: Option<u32>,
}

/// profile format 4 (PRO X SUPERLIGHT) の sector を解釈する。CRC が合わなければ None
/// (読み途中で値が変わった/形式違いの可能性 — 推測で埋めない)。
pub(crate) fn parse_profile(sector: &[u8], buttons: usize) -> Option<Profile> {
    let n = sector.len();
    if n < 0x20 + 4 * buttons || n < 2 {
        return None;
    }
    if crc16(&sector[..n - 2]) != be16(&sector[n - 2..]) {
        return None;
    }
    let dpis: Vec<u32> = (0..DPI_SLOTS)
        .map(|k| u16::from_le_bytes([sector[OFF_DPI + 2 * k], sector[OFF_DPI + 1 + 2 * k]]) as u32)
        .filter(|&d| d != 0 && d != 0xFFFF)
        .collect();
    let buttons = (0..buttons)
        .map(|k| decode_button(&sector[OFF_BUTTONS + 4 * k..OFF_BUTTONS + 4 + 4 * k]))
        .collect();
    let rate_ms = sector[OFF_RATE];
    Some(Profile {
        default_stage: sector[OFF_DEFAULT_DPI] as usize,
        dpis,
        buttons,
        rate_hz: (rate_ms > 0).then(|| 1000 / rate_ms as u32),
    })
}

fn button_names(n: usize) -> Vec<ButtonSlot> {
    const STD: [&str; 5] = ["左クリック", "右クリック", "ホイールクリック", "戻る", "進む"];
    (0..n)
        .map(|k| ButtonSlot {
            name: STD.get(k).map(|s| s.to_string()).unwrap_or_else(|| format!("ボタン {}", k + 1)),
        })
        .collect()
}

fn hz_label(hz: u32) -> String {
    if hz >= 1000 && hz.is_multiple_of(1000) {
        format!("{}K", hz / 1000)
    } else {
        hz.to_string()
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// 現在時刻 (UTC) → ("YYYYMMDD", "HHMMSS")。退避ファイル名用 (日付ライブラリを足さない)。
fn utc_stamp() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // days → 年月日 (H. Hinnant, civil_from_days)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (
        format!("{y:04}{m:02}{d:02}"),
        format!("{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60),
    )
}

fn hidpp_error(code: u8) -> &'static str {
    match code {
        1 => "unknown",
        2 => "invalid argument",
        3 => "out of range",
        4 => "hardware error",
        5 => "internal",
        6 => "invalid feature index",
        7 => "invalid function",
        8 => "busy",
        9 => "unsupported",
        _ => "?",
    }
}

// ---------------------------------------------------------------- ドライバ

/// 機能列挙で分かった本体の性質 (caps の材料)。
#[derive(Clone, Debug, Default)]
struct Probe {
    dpi: Option<(u32, u32, u32)>,
    rates: Vec<u32>,
    /// OnboardProfiles: (ボタン数, sector size)。
    onboard: Option<(usize, usize)>,
    battery: bool,
}

struct Shared {
    dev: Mutex<hidapi::HidDevice>,
    /// 受信機経由なら 1 (ペア slot)、直結なら 0xFF。
    index: u8,
    info: Mutex<DeviceInfo>,
    caps: Mutex<Capabilities>,
    /// 機能 ID → index。空 = 未列挙 (マウスが寝ていた)。
    feats: Mutex<HashMap<u16, u8>>,
    probe: Mutex<Probe>,
    events: Events,
    /// sector の読み書き (memoryRead / memoryWrite 列) を通しで直列化する。
    /// fn6→fn7…→fn8 の途中に別スレッドの memoryRead が割り込まないように。
    op: Mutex<()>,
    /// このセッションで退避したファイル。None = まだ (最初の書き込み前に作る)。
    backup: Mutex<Option<std::path::PathBuf>>,
}

pub struct LogitechDriver {
    sh: Arc<Shared>,
    stop: Arc<AtomicBool>,
}

pub fn open(dev: hidapi::HidDevice, info: DeviceInfo, events: Events) -> HubResult<LogitechDriver> {
    dev.set_blocking_mode(false).ok();
    let index = if info.transport == Transport::Wired { 0xFF } else { 1 };
    let sh = Arc::new(Shared {
        dev: Mutex::new(dev),
        index,
        info: Mutex::new(info),
        caps: Mutex::new(build_caps(&Probe::default())),
        feats: Mutex::new(HashMap::new()),
        probe: Mutex::new(Probe::default()),
        events,
        op: Mutex::new(()),
        backup: Mutex::new(None),
    });
    let stop = Arc::new(AtomicBool::new(false));
    spawn_notify(sh.clone(), stop.clone());
    // 寝ていれば失敗するだけ — 覚醒通知か次の info()/read() で取り直す。
    let _ = detect(&sh);
    Ok(LogitechDriver { sh, stop })
}

impl Drop for LogitechDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// HID++ 2.0 要求 1 往復。応答の params (16 byte) を返す。
fn call(sh: &Shared, feat_index: u8, func: u8, params: &[u8]) -> HubResult<Vec<u8>> {
    let fs = (func << 4) | SW;
    let dev_index = sh.index;
    let mut w = [0u8; 20];
    w[0] = 0x11;
    w[1] = dev_index;
    w[2] = feat_index;
    w[3] = fs;
    w[4..4 + params.len()].copy_from_slice(params);
    let dev = sh.dev.lock().unwrap_or_else(|p| p.into_inner());
    // 前の要求の遅延応答など、溜まっている古いレポートを捨てる。
    let mut b = [0u8; 64];
    while dev.read_timeout(&mut b, 0).unwrap_or(0) > 0 {}
    dev.write(&w).map_err(|e| HubError::Io(format!("HID++ write: {e}")))?;
    let t0 = Instant::now();
    while t0.elapsed() < TIMEOUT {
        let n = match dev.read_timeout(&mut b, 20) {
            Ok(n) => n,
            Err(e) => return Err(HubError::Io(format!("HID++ read: {e}"))),
        };
        if n < 7 || b[0] != 0x11 || b[1] != dev_index {
            continue;
        }
        if b[2] == feat_index && b[3] == fs {
            return Ok(b[4..20.min(n)].to_vec());
        }
        if b[2] == 0xFF && b[3] == feat_index && b[4] == fs {
            return Err(HubError::Protocol(format!(
                "HID++ error {:#04x} ({})",
                b[5],
                hidpp_error(b[5])
            )));
        }
    }
    Err(HubError::Disconnected)
}

fn feat_index(sh: &Shared, id: u16) -> Option<u8> {
    sh.feats.lock().unwrap_or_else(|p| p.into_inner()).get(&id).copied()
}

/// 機能 ID で呼ぶ。本体が持たない機能は Unsupported。
fn fcall(sh: &Shared, id: u16, func: u8, params: &[u8]) -> HubResult<Vec<u8>> {
    let i = feat_index(sh, id).ok_or(HubError::Unsupported("この機種に無い HID++ 機能"))?;
    let r = call(sh, i, func, params);
    match &r {
        Err(HubError::Disconnected) => set_asleep(sh, true),
        // 応答が返った = 起きている。覚醒通知 (0x41/1D4B) を取りこぼしても
        // 定期の電池取得などで asleep が解ける (変化時だけ Info を送る)。
        Ok(_) => set_asleep(sh, false),
        Err(_) => {}
    }
    r
}

/// `info.asleep` を変え、変わったときだけ `HubEvent::Info` を送る。
fn set_asleep(sh: &Shared, asleep: bool) {
    let mut i = sh.info.lock().unwrap_or_else(|p| p.into_inner());
    if i.asleep == asleep {
        return;
    }
    i.asleep = asleep;
    let info = i.clone();
    drop(i);
    if let Some(ev) = &sh.events {
        ev(HubEvent::Info(info.id.clone(), Box::new(info)));
    }
}

/// 機能を列挙し、名前・ファーム・caps を確定する。済んでいれば何もしない。
fn detect(sh: &Shared) -> HubResult<()> {
    if !sh.feats.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
        return Ok(());
    }
    // Root.getFeature(FeatureSet) — 寝ていればここでタイムアウト。
    let fs_idx = match call(sh, 0, 0, &[(feat::FEATURE_SET >> 8) as u8, feat::FEATURE_SET as u8]) {
        Ok(r) if r[0] != 0 => r[0],
        Ok(_) => return Err(HubError::Protocol("FeatureSet が無い (HID++ 2.0 ではない)".into())),
        Err(e) => {
            if matches!(e, HubError::Disconnected) {
                set_asleep(sh, true);
            }
            return Err(e);
        }
    };
    let count = call(sh, fs_idx, 0, &[])?[0];
    let mut map = HashMap::new();
    map.insert(feat::ROOT, 0u8);
    for i in 1..=count {
        let r = call(sh, fs_idx, 1, &[i])?;
        map.insert(be16(&r), i);
    }
    *sh.feats.lock().unwrap_or_else(|p| p.into_inner()) = map;

    // ---- 名前
    let name = (|| -> HubResult<String> {
        let len = fcall(sh, feat::DEVICE_NAME, 0, &[])?[0] as usize;
        let mut s = Vec::new();
        while s.len() < len {
            let r = fcall(sh, feat::DEVICE_NAME, 1, &[s.len() as u8])?;
            let take = (len - s.len()).min(r.len());
            if take == 0 {
                break;
            }
            s.extend_from_slice(&r[..take]);
        }
        Ok(String::from_utf8_lossy(&s).trim_end_matches('\0').trim().to_string())
    })()
    .ok()
    .filter(|s| !s.is_empty());

    // ---- ファーム (entity type 0 = 本体アプリ)。表記は "MPM 25.01.B0018"。
    let firmware = (|| -> HubResult<Option<String>> {
        let n = fcall(sh, feat::FW_VERSION, 0, &[])?[0];
        for e in 0..n {
            let r = fcall(sh, feat::FW_VERSION, 1, &[e])?;
            if r[0] == 0 {
                let prefix = String::from_utf8_lossy(&r[1..4]).trim().to_string();
                return Ok(Some(format!(
                    "{prefix} {:02x}.{:02x}.B{:04x}",
                    r[4],
                    r[5],
                    be16(&r[6..8])
                )));
            }
        }
        Ok(None)
    })()
    .ok()
    .flatten();

    // ---- caps の材料
    let mut probe = Probe {
        battery: feat_index(sh, feat::UNIFIED_BATTERY).is_some()
            || feat_index(sh, feat::BATTERY_STATUS).is_some(),
        ..Probe::default()
    };
    if let Ok(r) = fcall(sh, feat::ADJUSTABLE_DPI, 1, &[0]) {
        probe.dpi = parse_dpi_list(&r);
    }
    if let Ok(r) = fcall(sh, feat::REPORT_RATE, 0, &[]) {
        probe.rates = parse_rate_list(r[0]);
    }
    if let Ok(r) = fcall(sh, feat::ONBOARD_PROFILES, 0, &[]) {
        // [memModel, profFmt, macroFmt, profiles, oob, buttons, sectors, size(BE16), …]
        probe.onboard = Some((r[5] as usize, be16(&r[7..9]) as usize));
    }
    *sh.caps.lock().unwrap_or_else(|p| p.into_inner()) = build_caps(&probe);
    *sh.probe.lock().unwrap_or_else(|p| p.into_inner()) = probe;

    // ---- 名札。model が変わると UI は caps を取り直す (store.rs の renamed)。
    let mut i = sh.info.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(n) = name {
        i.model = format!("logi-{}", slug(&n));
        i.name = n;
    }
    i.firmware = firmware.or(i.firmware.take());
    i.asleep = false;
    let info = i.clone();
    drop(i);
    if let Some(ev) = &sh.events {
        ev(HubEvent::Info(info.id.clone(), Box::new(info)));
    }
    Ok(())
}

fn build_caps(p: &Probe) -> Capabilities {
    let mut caps = Capabilities::empty();
    caps.battery = p.battery;
    if !p.rates.is_empty() {
        let opts = p
            .rates
            .iter()
            .map(|&hz| ChoiceOpt::new(hz as i64, hz_label(hz)))
            .collect();
        caps.settings.push(
            SettingSpec::choice("polling_wireless", section::PERFORMANCE, "ポーリングレート", opts)
                .help("マウスのデータ更新頻度 (Hz)。オンボードプロファイルに保存されます"),
        );
    }
    if let Some((min, max, step)) = p.dpi {
        let buttons = p.onboard.map(|(b, _)| b).unwrap_or(5);
        caps.mouse = Some(MouseCaps {
            dpi_min: min,
            dpi_max: max,
            dpi_step: step,
            min_stages: 1,
            // オンボードプロファイルの DPI 枠は 5 個 (format 4)。無ければ現在値 1 個だけ。
            max_stages: if p.onboard.is_some() { 5 } else { 1 },
            xy_split: false,
            stage_colors: false,
            stage_toggle: false,
            buttons: button_names(buttons),
            button_kinds: vec![
                ButtonActionKind::Mouse,
                ButtonActionKind::Key,
                ButtonActionKind::Media,
                ButtonActionKind::Disabled,
            ],
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        });
    }
    caps
}

impl LogitechDriver {
    fn ensure_detected(&self) -> HubResult<()> {
        detect(&self.sh)
    }

    /// 現在のオンボードプロファイルの sector 番号 (8100 getCurrentProfile)。
    fn current_sector(&self) -> HubResult<u16> {
        let r = fcall(&self.sh, feat::ONBOARD_PROFILES, 4, &[])?;
        let sector = be16(&r[0..2]);
        if sector == 0 {
            return Err(HubError::Protocol("現在のプロファイルが不明".into()));
        }
        Ok(sector)
    }

    /// 現在のオンボードプロファイル sector を丸ごと読む。
    fn read_current_sector(&self, size: usize) -> HubResult<Vec<u8>> {
        let sector = self.current_sector()?;
        self.read_sector(sector, size)
    }

    /// sector を丸ごと読む (16 byte ずつ)。
    fn read_sector(&self, sector: u16, size: usize) -> HubResult<Vec<u8>> {
        if size < 16 {
            return Err(HubError::Protocol("sector size が不正".into()));
        }
        let mut data = Vec::with_capacity(size);
        let mut off = 0usize;
        while data.len() < size {
            // offset は size-16 まで。末尾は重ねて読み、重なった分を切り詰める。
            let o = off.min(size - 16);
            let r = fcall(
                &self.sh,
                feat::ONBOARD_PROFILES,
                5,
                &[(sector >> 8) as u8, sector as u8, (o >> 8) as u8, o as u8],
            )?;
            data.truncate(o);
            data.extend_from_slice(&r[..16]);
            off += 16;
        }
        data.truncate(size);
        Ok(data)
    }

    /// オンボードモード (8100 getMode = 1) か。オンボード機能が無ければ false。
    fn onboard_mode(&self, probe: &Probe) -> HubResult<bool> {
        if probe.onboard.is_none() {
            return Ok(false);
        }
        Ok(fcall(&self.sh, feat::ONBOARD_PROFILES, 2, &[])?[0] == 1)
    }

    /// memoryAddrWrite (fn6) → memoryWrite ×n (fn7, 16 byte ずつ。端数は 0xFF 詰め) → memoryWriteEnd (fn8)。
    fn write_sector(&self, sector: u16, data: &[u8]) -> HubResult<()> {
        let len = data.len();
        fcall(
            &self.sh,
            feat::ONBOARD_PROFILES,
            6,
            &[(sector >> 8) as u8, sector as u8, 0, 0, (len >> 8) as u8, len as u8],
        )?;
        for chunk in data.chunks(16) {
            let mut b = [0xFFu8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            fcall(&self.sh, feat::ONBOARD_PROFILES, 7, &b)?;
        }
        fcall(&self.sh, feat::ONBOARD_PROFILES, 8, &[])?;
        Ok(())
    }

    /// 書いて読み戻し、一致するまで最大 2 回。寝た (Disconnected) ときは即中断。
    fn write_verified(&self, sector: u16, data: &[u8]) -> HubResult<()> {
        let mut last = String::new();
        for _ in 0..2 {
            match self.write_sector(sector, data) {
                Err(HubError::Disconnected) => return Err(HubError::Disconnected),
                Err(e) => last = e.to_string(),
                Ok(()) => {}
            }
            let back = self.read_sector(sector, data.len())?;
            if back == data {
                return Ok(());
            }
            if last.is_empty() {
                last = "読み戻した内容が一致しません".into();
            }
        }
        let bk = self.sh.backup.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Err(HubError::Protocol(format!(
            "sector {sector:#06x} の書き込みに失敗 ({last})。退避: {}",
            bk.map(|p| p.display().to_string()).unwrap_or_default()
        )))
    }

    /// 最初の書き込みの前に、ディレクトリ (sector 0) と並んでいる全ユーザー sector を退避する。
    fn ensure_backup(&self, size: usize) -> HubResult<std::path::PathBuf> {
        let mut slot = self.sh.backup.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = &*slot {
            return Ok(p.clone());
        }
        let dir = self.read_sector(0, size)?;
        let mut sectors = vec![(0u16, dir.clone())];
        // ディレクトリ: 4 byte ずつ [addr BE16, enabled, 0xFF]、0xFFFF で終わり。
        for e in dir[..size - 2].chunks_exact(4) {
            let addr = be16(e);
            if addr == 0xFFFF {
                break;
            }
            if addr != 0 && addr < ROM_SECTOR && !sectors.iter().any(|(n, _)| *n == addr) {
                sectors.push((addr, self.read_sector(addr, size)?));
            }
        }
        let cur = self.current_sector()?;
        if cur < ROM_SECTOR && !sectors.iter().any(|(n, _)| *n == cur) {
            sectors.push((cur, self.read_sector(cur, size)?));
        }
        let info = self.sh.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (date, time) = utc_stamp();
        let base = std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| HubError::Io("APPDATA が無いので退避できません".into()))?;
        let folder = base.join("DeviceHub").join("backups").join("logitech");
        std::fs::create_dir_all(&folder).map_err(|e| HubError::Io(format!("退避フォルダ: {e}")))?;
        let path = folder.join(format!("{}-{date}-{time}.txt", info.model));
        let body = format!(
            "# Device HUB: Logicool オンボードプロファイルの退避 (書き込み前の状態)\n\
             # device: {} / {} / fw {}\n# taken: {date} {time} UTC / current profile sector {cur:#06x}\n\
             # 書き戻し: cargo run -p devhub-hid --example logirestore -- \"<このファイル>\"\n{}",
            info.name,
            info.model,
            info.firmware.as_deref().unwrap_or("?"),
            dump_sectors(&sectors)
        );
        std::fs::write(&path, body).map_err(|e| HubError::Io(format!("退避ファイル: {e}")))?;
        *slot = Some(path.clone());
        Ok(path)
    }

    /// 現在のプロファイル sector を読み、`edit` で変え、CRC を付け直して書く。
    /// 変わらなければ書かない。書いたら同じプロファイルを選び直して本体に反映させ、
    /// 使用中のステージを元に戻す。
    fn modify_current(
        &self,
        probe: &Probe,
        edit: impl FnOnce(&mut Vec<u8>) -> HubResult<()>,
    ) -> HubResult<()> {
        let (buttons, size) = probe
            .onboard
            .ok_or(HubError::Unsupported("オンボードプロファイルが無い機種"))?;
        let _g = self.sh.op.lock().unwrap_or_else(|p| p.into_inner());
        if !self.onboard_mode(probe)? {
            return Err(HubError::Unsupported("host モード中の保存 (オンボードモードのみ対応)"));
        }
        let sector = self.current_sector()?;
        if sector >= ROM_SECTOR {
            return Err(HubError::Unsupported(
                "工場出荷 (ROM) プロファイル使用中の書き込み",
            ));
        }
        let cur = self.read_sector(sector, size)?;
        parse_profile(&cur, buttons).ok_or_else(|| {
            HubError::Protocol("現在のプロファイルの CRC が一致しないため書き込みません".into())
        })?;
        let mut new = cur.clone();
        edit(&mut new)?;
        seal(&mut new);
        if new == cur {
            return Ok(());
        }
        self.ensure_backup(size)?;
        let stage = fcall(&self.sh, feat::ONBOARD_PROFILES, 0xB, &[]).map(|r| r[0]).ok();
        self.write_verified(sector, &new)?;
        // 同じプロファイルを選び直すと本体が sector を読み直す (DPI 位置は既定に戻る)。
        fcall(
            &self.sh,
            feat::ONBOARD_PROFILES,
            3,
            &[(sector >> 8) as u8, sector as u8],
        )?;
        if let Some(s) = stage {
            let n = parse_profile(&new, buttons).map(|p| p.dpis.len()).unwrap_or(1);
            if (s as usize) < n {
                let _ = fcall(&self.sh, feat::ONBOARD_PROFILES, 0xC, &[s]);
            }
        }
        Ok(())
    }

    /// このセッションで作った退避ファイル (まだ書いていなければ None)。
    pub fn backup_path(&self) -> Option<std::path::PathBuf> {
        self.sh.backup.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 現在のプロファイル sector の生バイト (検証用)。
    pub fn current_profile_bytes(&self) -> HubResult<Vec<u8>> {
        let size = self
            .sh
            .probe
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .onboard
            .map(|(_, s)| s)
            .ok_or(HubError::Unsupported("オンボードプロファイルが無い機種"))?;
        let _g = self.sh.op.lock().unwrap_or_else(|p| p.into_inner());
        self.read_current_sector(size)
    }

    /// 退避ファイル (`ensure_backup` の書式) をユーザー領域に書き戻す。
    /// CRC が合わない sector・ROM sector・サイズ違いがあれば何も書かずにエラー。
    pub fn restore_backup(&self, txt: &str) -> HubResult<String> {
        self.ensure_detected()?;
        let probe = self.sh.probe.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (_, size) = probe
            .onboard
            .ok_or(HubError::Unsupported("オンボードプロファイルが無い機種"))?;
        let sectors = parse_dump(txt);
        if sectors.is_empty() {
            return Err(HubError::Invalid("退避ファイルに sector がありません".into()));
        }
        for (n, d) in &sectors {
            if *n >= ROM_SECTOR || d.len() != size || crc16(&d[..size - 2]) != be16(&d[size - 2..]) {
                return Err(HubError::Invalid(format!(
                    "sector {n:#06x} が不正 (ROM / サイズ / CRC)。何も書いていません"
                )));
            }
        }
        let _g = self.sh.op.lock().unwrap_or_else(|p| p.into_inner());
        // 書き戻す前の状態も退避しておく (書き戻しの取り消し用)。
        let before = self.ensure_backup(size)?;
        let mut wrote = Vec::new();
        for (n, d) in &sectors {
            if self.read_sector(*n, size)? != *d {
                self.write_verified(*n, d)?;
                wrote.push(format!("{n:#06x}"));
            }
        }
        if let Ok(cur) = self.current_sector() {
            let _ = fcall(&self.sh, feat::ONBOARD_PROFILES, 3, &[(cur >> 8) as u8, cur as u8]);
        }
        Ok(if wrote.is_empty() {
            "変更なし (本体の内容は退避ファイルと同じ)".into()
        } else {
            format!(
                "書き戻した sector: {} (書き戻し前の状態: {})",
                wrote.join(", "),
                before.display()
            )
        })
    }

    fn read_mouse(&self, probe: &Probe) -> HubResult<MouseState> {
        let n_buttons = probe.onboard.map(|(b, _)| b).unwrap_or(5);
        // オンボードモード (8100 getMode = 1) なら、効いている設定はプロファイル sector にある。
        if let Some((buttons, size)) = probe.onboard {
            let mode = fcall(&self.sh, feat::ONBOARD_PROFILES, 2, &[])?[0];
            if mode == 1 {
                let sector = self.read_current_sector(size)?;
                let prof = parse_profile(&sector, buttons).ok_or_else(|| {
                    HubError::Protocol("オンボードプロファイルの CRC が一致しません".into())
                })?;
                let active = fcall(&self.sh, feat::ONBOARD_PROFILES, 0xB, &[])
                    .map(|r| r[0] as usize)
                    .unwrap_or(prof.default_stage);
                let stages: Vec<DpiStage> = prof
                    .dpis
                    .iter()
                    .map(|&x| DpiStage {
                        x,
                        y: None,
                        color: Rgb::default(),
                        enabled: true,
                    })
                    .collect();
                let active = active.min(stages.len().saturating_sub(1));
                return Ok(MouseState {
                    stages,
                    active_stage: active,
                    buttons: prof.buttons,
                    macros: vec![],
                    adaptive: None,
                });
            }
        }
        // host モード / オンボード無し: 現在 DPI 1 個だけ。ボタンは割り当てを読めないので標準配置として見せる。
        let r = fcall(&self.sh, feat::ADJUSTABLE_DPI, 2, &[0])?;
        let dpi = be16(&r[1..3]) as u32;
        let std = [MouseFn::Left, MouseFn::Right, MouseFn::Middle, MouseFn::Back, MouseFn::Forward];
        Ok(MouseState {
            stages: vec![DpiStage {
                x: dpi,
                y: None,
                color: Rgb::default(),
                enabled: true,
            }],
            active_stage: 0,
            buttons: (0..n_buttons)
                .map(|k| std.get(k).map(|m| ButtonAction::Mouse(*m)).unwrap_or(ButtonAction::Disabled))
                .collect(),
            macros: vec![],
            adaptive: None,
        })
    }
}

impl DeviceDriver for LogitechDriver {
    fn info(&self) -> DeviceInfo {
        let _ = self.ensure_detected();
        let mut i = self.sh.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if !i.asleep {
            if let Ok(b) = self.battery() {
                i.battery = Some(b);
            }
        }
        i
    }

    fn caps(&self) -> Capabilities {
        self.sh.caps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        self.ensure_detected()?;
        let probe = self.sh.probe.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut settings = BTreeMap::new();
        if let Ok(r) = fcall(&self.sh, feat::REPORT_RATE, 1, &[]) {
            if r[0] > 0 {
                settings.insert(
                    "polling_wireless".into(),
                    SettingValue::Choice((1000 / r[0] as u32) as i64),
                );
            }
        }
        let mouse = if probe.dpi.is_some() {
            let _g = self.sh.op.lock().unwrap_or_else(|p| p.into_inner());
            Some(self.read_mouse(&probe)?)
        } else {
            None
        };
        set_asleep(&self.sh, false);
        Ok(DeviceState {
            profile: ProfileState::default(),
            settings,
            mouse,
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        self.ensure_detected()?;
        let probe = self.sh.probe.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match change {
            // RAM 上の切替だけ (フラッシュに書かない)。host モードはステージ 1 個なので何もしない。
            Change::ActiveStage(i) => {
                if self.onboard_mode(&probe)? {
                    fcall(&self.sh, feat::ONBOARD_PROFILES, 0xC, &[*i as u8])?;
                }
                Ok(())
            }
            Change::DpiStages { stages, active } => {
                let (min, max, _) = probe.dpi.ok_or(HubError::Unsupported("DPI"))?;
                let xs: Vec<u32> = stages.iter().map(|s| s.x).collect();
                if xs.is_empty() || xs.len() > DPI_SLOTS {
                    return Err(HubError::Invalid(format!("DPI ステージ数 {} (1〜{DPI_SLOTS})", xs.len())));
                }
                if let Some(bad) = xs.iter().find(|&&x| x < min || x > max) {
                    return Err(HubError::Invalid(format!("DPI {bad} ({min}〜{max})")));
                }
                let active = (*active).min(xs.len() - 1);
                if self.onboard_mode(&probe)? {
                    self.modify_current(&probe, |s| {
                        set_dpis(s, &xs, active);
                        Ok(())
                    })?;
                    fcall(&self.sh, feat::ONBOARD_PROFILES, 0xC, &[active as u8])?;
                } else {
                    // host モード: 現在 DPI だけ (本体の RAM。保存はされない)。
                    let d = xs[active] as u16;
                    fcall(&self.sh, feat::ADJUSTABLE_DPI, 3, &[0, (d >> 8) as u8, d as u8])?;
                }
                Ok(())
            }
            Change::Button { slot, action } => {
                let (buttons, _) = probe
                    .onboard
                    .ok_or(HubError::Unsupported("ボタン割り当て (オンボードプロファイルが無い機種)"))?;
                if *slot >= buttons {
                    return Err(HubError::Invalid(format!("ボタン {slot} (0〜{})", buttons - 1)));
                }
                if *slot == 0 && *action != ButtonAction::Mouse(MouseFn::Left) {
                    return Err(HubError::Invalid("左クリックは変更できません".into()));
                }
                let b = encode_button(action)
                    .ok_or(HubError::Unsupported("この割り当て (Logicool で表せない動作)"))?;
                self.modify_current(&probe, |s| {
                    set_button(s, *slot, b);
                    Ok(())
                })
            }
            Change::Setting { id, value } if id == "polling_wireless" => {
                let SettingValue::Choice(hz) = value else {
                    return Err(HubError::Invalid(format!("{value:?}")));
                };
                let hz = *hz as u32;
                if !probe.rates.contains(&hz) {
                    return Err(HubError::Invalid(format!("ポーリングレート {hz}Hz")));
                }
                if self.onboard_mode(&probe)? {
                    self.modify_current(&probe, |s| {
                        set_rate(s, hz);
                        Ok(())
                    })
                } else {
                    fcall(&self.sh, feat::REPORT_RATE, 2, &[(1000 / hz) as u8]).map(|_| ())
                }
            }
            _ => Err(HubError::Unsupported("この設定 (Logicool)")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        // 寝ている間に開いたドライバは機能表が空 — ここで列挙し直さないと
        // 電池取得が Unsupported で即終わり、本体に一度も問い合わせない (= asleep が解けない)。
        self.ensure_detected()?;
        if feat_index(&self.sh, feat::UNIFIED_BATTERY).is_some() {
            // getStatus → [残量%, level, 充電状態 (0 放電 / 1・2 充電中 / 3 満充電), …]
            let r = fcall(&self.sh, feat::UNIFIED_BATTERY, 1, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1 | 2),
            });
        }
        if feat_index(&self.sh, feat::BATTERY_STATUS).is_some() {
            // getBatteryLevelStatus → [残量%, 次の段階, 状態 (1..=3 充電系)]
            let r = fcall(&self.sh, feat::BATTERY_STATUS, 0, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1..=3),
            });
        }
        Err(HubError::Unsupported("battery"))
    }
}

// ---------------------------------------------------------------- 通知

/// 同じ受信機の HID++ short/long コレクションを別ハンドルで開き、
/// 接続 (0x41)・再接続 (1D4B)・電池 (1004)・設定変更 (2201/8060/8100) の通知を拾う。
fn spawn_notify(sh: Arc<Shared>, stop: Arc<AtomicBool>) {
    let (vid, pid, serial) = {
        let i = sh.info.lock().unwrap_or_else(|p| p.into_inner());
        (i.vid, i.pid, i.serial.clone())
    };
    std::thread::spawn(move || {
        let Ok(api) = hidapi::HidApi::new() else {
            return;
        };
        let open = |usage: u16| {
            api.device_list()
                .find(|d| {
                    d.vendor_id() == vid
                        && d.product_id() == pid
                        && d.usage_page() == 0xFF00
                        && d.usage() == usage
                        && serial
                            .as_deref()
                            .is_none_or(|s| s.is_empty() || d.serial_number() == Some(s))
                })
                .and_then(|d| d.open_device(&api).ok())
        };
        let short = open(0x01);
        let long = open(0x02);
        if short.is_none() && long.is_none() {
            return;
        }
        let mut b = [0u8; 64];
        while !stop.load(Ordering::Relaxed) {
            if let Some(s) = &short {
                if let Ok(n) = s.read_timeout(&mut b, 100) {
                    if n >= 5 && b[0] == 0x10 && b[1] == sh.index && b[2] == 0x41 {
                        // DJ 接続通知: b[4] bit6 = リンク未確立。
                        if b[4] & 0x40 != 0 {
                            set_asleep(&sh, true);
                        } else {
                            woke(&sh);
                        }
                    }
                }
            }
            if let Some(l) = &long {
                if let Ok(n) = l.read_timeout(&mut b, 100) {
                    // software id 0 = デバイス発の通知 (自分の要求の応答写しは SW が入っている)。
                    if n >= 8 && b[0] == 0x11 && b[1] == sh.index && b[3] & 0x0F == 0 {
                        on_event(&sh, b[2], &b[4..n.min(20)]);
                    }
                }
            }
        }
    });
}

fn woke(sh: &Shared) {
    if sh.feats.lock().unwrap_or_else(|p| p.into_inner()).is_empty() {
        // 開いた時に寝ていた: ここで列挙すれば Info (model 変化) で UI が caps を取り直す。
        let _ = detect(sh);
    } else {
        set_asleep(sh, false);
    }
}

fn on_event(sh: &Shared, fi: u8, p: &[u8]) {
    let is = |id: u16| feat_index(sh, id) == Some(fi);
    if is(feat::WIRELESS_STATUS) {
        woke(sh);
    } else if is(feat::UNIFIED_BATTERY) {
        let id = sh.info.lock().unwrap_or_else(|p| p.into_inner()).id.clone();
        if let Some(ev) = &sh.events {
            ev(HubEvent::Battery(
                id,
                Battery {
                    percent: Some(p[0].min(100)),
                    charging: matches!(p[2], 1 | 2),
                },
            ));
        }
    } else if is(feat::ONBOARD_PROFILES) || is(feat::ADJUSTABLE_DPI) || is(feat::REPORT_RATE) {
        // 本体の DPI ボタンなどで変わった — UI に読み直させる。
        let id = sh.info.lock().unwrap_or_else(|p| p.into_inner()).id.clone();
        if let Some(ev) = &sh.events {
            ev(HubEvent::StateChanged(id));
        }
    }
}

// ---------------------------------------------------------------- テスト

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/logitech/prox-superlight-c547.txt");

    /// fixtures の `-- sector 0x000N` ダンプを 255 byte に戻す。
    fn sector(n: u16) -> Vec<u8> {
        parse_dump(FIXTURE)
            .into_iter()
            .find(|(k, _)| *k == n)
            .map(|(_, d)| d)
            .unwrap()
    }

    #[test]
    fn dump_roundtrip() {
        let all = parse_dump(FIXTURE);
        assert_eq!(all.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(parse_dump(&dump_sectors(&all)), all);
    }

    #[test]
    fn edit_dpis_rate_and_seal() {
        let mut s = sector(1);
        set_dpis(&mut s, &[400, 800, 1600], 1);
        set_rate(&mut s, 500);
        seal(&mut s);
        let p = parse_profile(&s, 5).expect("CRC を付け直したので読める");
        assert_eq!(p.dpis, vec![400, 800, 1600]);
        assert_eq!(p.default_stage, 1);
        assert_eq!(p.rate_hz, Some(500));
        assert_eq!(&s[3..13], &[0x90, 0x01, 0x20, 0x03, 0x40, 0x06, 0, 0, 0, 0]);
        // 触っていない部分 (ボタン・名前・G-shift 層) はそのまま。
        assert_eq!(&s[0x20..0xFD], &sector(1)[0x20..0xFD]);
        // 工場出荷の並びを書けば sector 2 と同じ DPI 部になる。
        let mut t = sector(1);
        set_dpis(&mut t, &[400, 800, 1600, 3200, 6400], 1);
        assert_eq!(&t[1..13], &sector(2)[1..13]);
    }

    #[test]
    fn edit_back_to_original_is_identical() {
        let orig = sector(1);
        let mut s = orig.clone();
        set_dpis(&mut s, &[800], 0);
        set_rate(&mut s, 1000);
        set_button(&mut s, 3, encode_button(&ButtonAction::Mouse(MouseFn::Back)).unwrap());
        seal(&mut s);
        assert_eq!(s, orig, "同じ値なら書き込みは発生しない");
    }

    #[test]
    fn button_encode_roundtrip() {
        let cases = [
            ButtonAction::Mouse(MouseFn::Left),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Key(KeyStroke { mods: 0x01, usage: 0x06 }),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Media(MediaFn::Calculator),
            ButtonAction::Disabled,
            ButtonAction::Unsupported { ty: 0x90, params: vec![5, 0, 0] },
        ];
        for a in cases {
            let b = encode_button(&a).unwrap();
            assert_eq!(decode_button(&b), a);
        }
        assert_eq!(encode_button(&ButtonAction::Mouse(MouseFn::ScrollUp)), None);
        assert_eq!(encode_button(&ButtonAction::Key(KeyStroke { mods: 0, usage: 0x100 })), None);
    }

    #[test]
    fn utc_stamp_shape() {
        let (d, t) = utc_stamp();
        assert_eq!((d.len(), t.len()), (8, 6));
        assert!(d.starts_with("20"));
    }

    #[test]
    fn crc_matches_fixture() {
        for n in 0..=5 {
            let s = sector(n);
            assert_eq!(s.len(), 255, "sector {n}");
            assert_eq!(crc16(&s[..253]), be16(&s[253..]), "sector {n}");
        }
    }

    #[test]
    fn active_profile_from_fixture() {
        // sector 1 = G HUB が書いた現用プロファイル: 800 DPI 1 段、標準 5 ボタン。
        let p = parse_profile(&sector(1), 5).unwrap();
        assert_eq!(p.dpis, vec![800]);
        assert_eq!(p.default_stage, 0);
        assert_eq!(
            p.buttons,
            vec![
                ButtonAction::Mouse(MouseFn::Left),
                ButtonAction::Mouse(MouseFn::Right),
                ButtonAction::Mouse(MouseFn::Middle),
                ButtonAction::Mouse(MouseFn::Back),
                ButtonAction::Mouse(MouseFn::Forward),
            ]
        );
    }

    #[test]
    fn factory_profile_from_fixture() {
        let p = parse_profile(&sector(2), 5).unwrap();
        assert_eq!(p.dpis, vec![400, 800, 1600, 3200, 6400]);
        assert_eq!(p.default_stage, 1);
    }

    #[test]
    fn corrupted_profile_is_rejected() {
        let mut s = sector(1);
        s[3] ^= 0xFF;
        assert!(parse_profile(&s, 5).is_none());
    }

    #[test]
    fn dpi_list_range() {
        // 実機応答: 100..25600 step 50
        let r = [0x00, 0x00, 0x64, 0xE0, 0x32, 0x64, 0x00, 0x00, 0x00];
        assert_eq!(parse_dpi_list(&r), Some((100, 25600, 50)));
        // 離散リスト
        let r = [0x00, 0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0x00, 0x00];
        assert_eq!(parse_dpi_list(&r), Some((400, 1600, 400)));
    }

    #[test]
    fn rate_list() {
        // 実機応答 0x8b = 1/2/4/8 ms
        assert_eq!(parse_rate_list(0x8b), vec![125, 250, 500, 1000]);
    }

    #[test]
    fn button_kinds() {
        assert_eq!(
            decode_button(&[0x80, 0x02, 0x01, 0x06]),
            ButtonAction::Key(KeyStroke { mods: 0x01, usage: 0x06 })
        );
        assert_eq!(
            decode_button(&[0x80, 0x03, 0x00, 0xE9]),
            ButtonAction::Media(MediaFn::VolumeUp)
        );
        assert!(matches!(
            decode_button(&[0x90, 0x05, 0x00, 0x00]),
            ButtonAction::Unsupported { ty: 0x90, .. }
        ));
    }

    #[test]
    fn caps_from_probe() {
        let c = build_caps(&Probe {
            dpi: Some((100, 25600, 50)),
            rates: vec![125, 250, 500, 1000],
            onboard: Some((5, 255)),
            battery: true,
        });
        let m = c.mouse.clone().unwrap();
        assert_eq!((m.dpi_min, m.dpi_max, m.dpi_step, m.max_stages), (100, 25600, 50, 5));
        assert_eq!(m.buttons.len(), 5);
        assert!(c.battery);
        let s = c.setting("polling_wireless").unwrap();
        assert!(s.enabled_when.is_none(), "段階 2 は書ける");
    }

    #[test]
    fn slug_name() {
        assert_eq!(slug("PRO X Wireless"), "pro-x-wireless");
    }
}
