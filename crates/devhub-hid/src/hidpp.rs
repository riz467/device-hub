//! Logitech HID++ 2.0 — G PRO X SUPERLIGHT / PRO Wireless 系 (LIGHTSPEED 受信機・有線)。
//!
//! G HUB の代わりに、公開されている HID++ 2.0 の仕様 (Solaar / libratbag と同じ知見) で話す。
//! 実機 PRO X SUPERLIGHT (受信機 046D:C547・wpid 4093・FW MPM25.01 B0018) で
//! feature 一覧とオンボードプロファイルの中身を読んで確認した (docs/PROTOCOL_NOTES.md §Logitech)。
//!
//! # ワイヤ形式
//!
//! - 受信機は vendor usage page 0xFF00 に 2 本のコレクションを持つ。
//!   usage 1 = short (report 0x10, 7B)、usage 2 = long (report 0x11, 20B)。
//!   Windows では別ハンドル。要求はすべて long で送り、応答も long で来る。
//!   ただし HID++ 1.0 のエラー (0x8F: 未ペア・到達不能) と受信機レジスタ要求は short。
//! - 要求 `[0x11, dev, featIdx, fn<<4 | swid, params…]`、応答は先頭 4B が同じ。
//!   エラーは `[0x11, dev, 0xFF, featIdx, fn<<4|swid, code]`。
//! - `dev` は受信機経由ならペア番号 (1..6)、有線・受信機自身は 0xFF。
//! - `swid` は 0x0A — G HUB 等の他ソフトの応答と取り違えないため。
//!
//! # 設定の持たせ方
//!
//! G HUB を消しても設定が残るよう、**オンボードプロファイル (0x8100) に書く**。
//! プロファイルは 255B のセクタ 1 個 (format 4、末尾 2B が CRC-CCITT)。
//! 読む → 変更 → CRC を付け直す → 書く → 読み戻して照合、の順。
//! 書く前に元のセクタを `%APPDATA%\DeviceHub\hidpp-backup\` に保存する。
//! 体感をすぐ変えるため、DPI とポーリングは 0x2201 / 0x8060 でも即時に反映する。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SWID: u8 = 0x0A;
const SHORT: u8 = 0x10;
const LONG: u8 = 0x11;
const RECEIVER: u8 = 0xFF;
/// ボタン数 (format 4 は 16 枠あるが PRO 系は 5 個)。
const BUTTONS: usize = 5;

#[allow(dead_code)]
pub mod feat {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FW_VERSION: u16 = 0x0003;
    pub const NAME: u16 = 0x0005;
    pub const BATTERY_STATUS: u16 = 0x1000;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    pub const ADJ_DPI: u16 = 0x2201;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const ONBOARD: u16 = 0x8100;
}

/// 0x8100 の関数番号。
mod ob {
    pub const GET_INFO: u8 = 0;
    pub const SET_MODE: u8 = 1;
    pub const GET_MODE: u8 = 2;
    pub const SET_PROFILE: u8 = 3;
    pub const GET_PROFILE: u8 = 4;
    pub const MEM_READ: u8 = 5;
    pub const MEM_ADDR_WRITE: u8 = 6;
    pub const MEM_WRITE: u8 = 7;
    pub const MEM_WRITE_END: u8 = 8;
    pub const GET_DPI_INDEX: u8 = 11;
    pub const SET_DPI_INDEX: u8 = 12;
    pub const MODE_ONBOARD: u8 = 1;
    pub const MODE_HOST: u8 = 2;
}

/// ペア先の wpid / 有線 PID → (表示名, model id)。
fn model_lookup(id: u16) -> Option<(&'static str, &'static str)> {
    match id {
        0x4093 | 0xC094 => Some(("PRO X SUPERLIGHT", "logi-pro-x-superlight")),
        0x4079 | 0xC088 => Some(("PRO Wireless", "logi-pro-wireless")),
        0x409F | 0xC09B => Some(("PRO X SUPERLIGHT 2", "logi-pro-x-superlight-2")),
        _ => None,
    }
}

// ==================================================================== プロファイル (純粋関数)

/// オンボードプロファイル format 4 のセクタ。テストできるよう I/O から切り離す。
///
/// | off | 長さ | 内容 |
/// |---|---|---|
/// | 0 | 1 | レポート間隔 ms (1=1000Hz, 2=500, 4=250, 8=125) |
/// | 1 | 1 | 既定 DPI ステージ index |
/// | 2 | 1 | DPI シフト (スナイパー) の index |
/// | 3 | 10 | DPI × 5 (LE16、0 / 0xFFFF は未使用) |
/// | 13 | 3 | プロファイル色 RGB |
/// | 16 | 1 | 電源モード |
/// | 17 | 1 | 角度補正 |
/// | 32 | 64 | ボタン × 16 (4B) |
/// | 96 | 64 | G-shift ボタン × 16 (4B) |
/// | 160 | 48 | 名前 (UTF-16LE) |
/// | 208 | 44 | ライティング × 4 (11B) |
/// | len-2 | 2 | CRC-CCITT (BE) |
pub mod profile {
    pub const DPI_OFF: usize = 3;
    pub const DPI_SLOTS: usize = 5;
    pub const BUTTON_OFF: usize = 32;
    pub const NAME_OFF: usize = 160;
    pub const NAME_LEN: usize = 48;

    /// CRC-CCITT (初期値 0xFFFF、多項式 0x1021)。実機セクタで一致を確認済み。
    pub fn crc16(b: &[u8]) -> u16 {
        let mut c: u16 = 0xFFFF;
        for &x in b {
            c ^= (x as u16) << 8;
            for _ in 0..8 {
                c = if c & 0x8000 != 0 {
                    (c << 1) ^ 0x1021
                } else {
                    c << 1
                };
            }
        }
        c
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct Profile {
        pub raw: Vec<u8>,
    }

    impl Profile {
        /// セクタを受け取る。`check_crc` が false なら ROM 既定 (CRC 無し) も通す。
        pub fn parse(raw: &[u8], check_crc: bool) -> Result<Self, String> {
            if raw.len() < NAME_OFF + NAME_LEN + 2 {
                return Err(format!("profile sector too short ({}B)", raw.len()));
            }
            if check_crc {
                let n = raw.len();
                let stored = u16::from_be_bytes([raw[n - 2], raw[n - 1]]);
                let calc = crc16(&raw[..n - 2]);
                if stored != calc {
                    return Err(format!(
                        "profile CRC mismatch (stored {stored:04x}, calc {calc:04x})"
                    ));
                }
            }
            Ok(Self { raw: raw.to_vec() })
        }

        pub fn report_interval_ms(&self) -> u8 {
            self.raw[0]
        }
        pub fn set_report_interval_ms(&mut self, ms: u8) {
            self.raw[0] = ms;
        }

        pub fn default_index(&self) -> usize {
            self.raw[1] as usize
        }
        pub fn shift_index(&self) -> usize {
            self.raw[2] as usize
        }
        pub fn set_shift_index(&mut self, i: usize) {
            self.raw[2] = i as u8;
        }

        /// 有効な DPI ステージ (先頭から、未使用枠で打ち切り)。
        pub fn stages(&self) -> Vec<u32> {
            let mut out = vec![];
            for i in 0..DPI_SLOTS {
                let o = DPI_OFF + i * 2;
                let v = u16::from_le_bytes([self.raw[o], self.raw[o + 1]]);
                if v == 0 || v == 0xFFFF {
                    break;
                }
                out.push(v as u32);
            }
            out
        }

        /// ステージを差し替える (1..=5 個)。残り枠は 0 埋め (G HUB と同じ)。
        pub fn set_stages(&mut self, dpis: &[u32], default: usize) {
            let n = dpis.len().clamp(1, DPI_SLOTS);
            let vals = dpis
                .iter()
                .take(n)
                .map(|&d| d.min(u16::MAX as u32 - 1) as u16)
                .chain(std::iter::repeat(0));
            for (slot, v) in self.raw[DPI_OFF..DPI_OFF + DPI_SLOTS * 2]
                .chunks_mut(2)
                .zip(vals)
            {
                slot.copy_from_slice(&v.to_le_bytes());
            }
            self.raw[1] = default.min(n - 1) as u8;
            if self.shift_index() >= n {
                self.set_shift_index(0);
            }
        }

        pub fn button(&self, i: usize) -> [u8; 4] {
            let o = BUTTON_OFF + i * 4;
            [
                self.raw[o],
                self.raw[o + 1],
                self.raw[o + 2],
                self.raw[o + 3],
            ]
        }
        pub fn set_button(&mut self, i: usize, b: [u8; 4]) {
            let o = BUTTON_OFF + i * 4;
            self.raw[o..o + 4].copy_from_slice(&b);
        }

        pub fn name(&self) -> String {
            let b = &self.raw[NAME_OFF..NAME_OFF + NAME_LEN];
            let units: Vec<u16> = b
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0 && u != 0xFFFF)
                .collect();
            String::from_utf16_lossy(&units)
        }

        /// 末尾 2B に CRC を付け直したセクタ。
        pub fn finalize(&mut self) -> &[u8] {
            let n = self.raw.len();
            let c = crc16(&self.raw[..n - 2]);
            self.raw[n - 2..].copy_from_slice(&c.to_be_bytes());
            &self.raw
        }
    }
}

// ==================================================================== ボタン (純粋関数)

/// 4B のボタン割当 (format 4) の解釈。
///
/// - `80 01 hh ll` マウスボタン (BE16 マスク: 1=左 2=右 4=中 8=戻る 16=進む)
/// - `80 02 mm kk` キー (mm=修飾, kk=HID usage)
/// - `80 03 hh ll` コンシューマ (BE16 usage)
/// - `90 ff 00 dd` 機能 (ff: 1/2=チルト 3/4/5=DPI 上/下/循環 7=DPI シフト 0x10/0x11=スクロール下/上)
/// - それ以外 (マクロ等) は `Unsupported` として往復保存する。
pub mod button {
    use devhub_core::model::*;

    const CONSUMER: &[(MediaFn, u16)] = &[
        (MediaFn::PlayPause, 0x00CD),
        (MediaFn::Stop, 0x00B7),
        (MediaFn::Next, 0x00B5),
        (MediaFn::Prev, 0x00B6),
        (MediaFn::VolumeUp, 0x00E9),
        (MediaFn::VolumeDown, 0x00EA),
        (MediaFn::Mute, 0x00E2),
        (MediaFn::Calculator, 0x0192),
        (MediaFn::MyComputer, 0x0194),
        (MediaFn::Browser, 0x0196),
        (MediaFn::Mail, 0x018A),
        (MediaFn::BrightnessUp, 0x006F),
        (MediaFn::BrightnessDown, 0x0070),
        (MediaFn::Refresh, 0x0227),
    ];

    fn mask_of(f: MouseFn) -> Option<u16> {
        Some(match f {
            MouseFn::Left => 0x0001,
            MouseFn::Right => 0x0002,
            MouseFn::Middle => 0x0004,
            MouseFn::Back => 0x0008,
            MouseFn::Forward => 0x0010,
            _ => return None,
        })
    }

    /// `stages` と `shift_index` は DPI シフトの表示値を引くために使う。
    pub fn decode(b: [u8; 4], stages: &[u32], shift_index: usize) -> ButtonAction {
        let raw = || ButtonAction::Unsupported {
            ty: b[0],
            params: b[1..].to_vec(),
        };
        match (b[0], b[1]) {
            (0xFF, 0xFF) => ButtonAction::Disabled,
            (0x80, 0x00) | (0x90, 0x00) => ButtonAction::Disabled,
            (0x80, 0x01) => match u16::from_be_bytes([b[2], b[3]]) {
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
            (0x80, 0x03) => {
                let u = u16::from_be_bytes([b[2], b[3]]);
                CONSUMER
                    .iter()
                    .find(|(_, c)| *c == u)
                    .map(|(m, _)| ButtonAction::Media(*m))
                    .unwrap_or_else(raw)
            }
            (0x90, 0x01) => ButtonAction::Mouse(MouseFn::ScrollLeft),
            (0x90, 0x02) => ButtonAction::Mouse(MouseFn::ScrollRight),
            (0x90, 0x03) => ButtonAction::Dpi(DpiFn::Up),
            (0x90, 0x04) => ButtonAction::Dpi(DpiFn::Down),
            (0x90, 0x05) => ButtonAction::Dpi(DpiFn::Cycle),
            (0x90, 0x07) => ButtonAction::Dpi(DpiFn::Shift(
                stages
                    .get(shift_index)
                    .or(stages.first())
                    .copied()
                    .unwrap_or(400),
            )),
            (0x90, 0x10) => ButtonAction::Mouse(MouseFn::ScrollDown),
            (0x90, 0x11) => ButtonAction::Mouse(MouseFn::ScrollUp),
            _ => raw(),
        }
    }

    /// 割当 → 4B。DPI シフトは「一番近いステージ」の index を返す (プロファイルの byte 2 に書く)。
    pub fn encode(a: &ButtonAction, stages: &[u32]) -> Result<([u8; 4], Option<usize>), String> {
        Ok(match a {
            ButtonAction::Disabled => ([0x90, 0x00, 0x00, 0x00], None),
            ButtonAction::Mouse(MouseFn::ScrollLeft) => ([0x90, 0x01, 0, 0], None),
            ButtonAction::Mouse(MouseFn::ScrollRight) => ([0x90, 0x02, 0, 0], None),
            ButtonAction::Mouse(MouseFn::ScrollDown) => ([0x90, 0x10, 0, 0], None),
            ButtonAction::Mouse(MouseFn::ScrollUp) => ([0x90, 0x11, 0, 0], None),
            ButtonAction::Mouse(f) => {
                let m = mask_of(*f).ok_or("このボタン動作はオンボードに保存できません")?;
                let [h, l] = m.to_be_bytes();
                ([0x80, 0x01, h, l], None)
            }
            ButtonAction::Key(k) => {
                if k.usage > 0xFF {
                    return Err("このキーはオンボードに保存できません".into());
                }
                ([0x80, 0x02, k.mods, k.usage as u8], None)
            }
            ButtonAction::Media(m) => {
                let u = CONSUMER
                    .iter()
                    .find(|(x, _)| x == m)
                    .map(|(_, u)| *u)
                    .ok_or("未対応のメディアキー")?;
                let [h, l] = u.to_be_bytes();
                ([0x80, 0x03, h, l], None)
            }
            ButtonAction::Dpi(DpiFn::Up) => ([0x90, 0x03, 0, 0], None),
            ButtonAction::Dpi(DpiFn::Down) => ([0x90, 0x04, 0, 0], None),
            ButtonAction::Dpi(DpiFn::Cycle) => ([0x90, 0x05, 0, 0], None),
            ButtonAction::Dpi(DpiFn::Shift(d)) => {
                let idx = stages
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, s)| (**s as i64 - *d as i64).abs())
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                ([0x90, 0x07, 0, 0], Some(idx))
            }
            ButtonAction::Unsupported { ty, params } if params.len() == 3 => {
                ([*ty, params[0], params[1], params[2]], None)
            }
            _ => return Err("この割り当てはオンボードに保存できません".into()),
        })
    }
}

// ==================================================================== 構造

/// 0x8100 getInfo の内容。
#[derive(Clone, Copy, Debug)]
struct OnboardInfo {
    profile_format: u8,
    sector_size: u16,
}

pub struct HidppDriver {
    long: Mutex<hidapi::HidDevice>,
    short: Option<Mutex<hidapi::HidDevice>>,
    /// HID++ のデバイス番号 (受信機経由 1..6 / 有線 0xFF)。
    index: u8,
    /// ペア先 wpid (受信機経由のみ)。バックアップのファイル名に使う。
    wpid: u16,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// feature id → index。
    feats: Mutex<HashMap<u16, u8>>,
    onboard: Mutex<Option<OnboardInfo>>,
    /// このセッションで既にバックアップしたセクタ。
    backed_up: Mutex<HashSet<u16>>,
}

type Events = Option<Arc<dyn Fn(HubEvent) + Send + Sync>>;
type DetectSink = Arc<Mutex<HashMap<String, (String, String)>>>;

pub fn open(
    long: hidapi::HidDevice,
    short: Option<hidapi::HidDevice>,
    mut info: DeviceInfo,
    events: Events,
    detected: DetectSink,
) -> HubResult<HidppDriver> {
    let wireless = info.transport == Transport::Wireless24G;
    let mut d = HidppDriver {
        long: Mutex::new(long),
        short: short.map(Mutex::new),
        index: RECEIVER,
        wpid: info.pid,
        info: Mutex::new(info.clone()),
        caps: default_caps(&info),
        events,
        feats: Mutex::new(HashMap::new()),
        onboard: Mutex::new(None),
        backed_up: Mutex::new(HashSet::new()),
    };
    if wireless {
        // 受信機のペアリング表 (レジスタ 0xB5) — マウスが寝ていても受信機が答える。
        let (idx, wpid) = d
            .find_paired()
            .ok_or_else(|| HubError::Protocol("受信機にペアリング済みのマウスがありません".into()))?;
        d.index = idx;
        d.wpid = wpid;
        if let Some(fw) = d.receiver_firmware() {
            info.dongle_firmware = Some(fw);
        }
    }
    if let Some((name, model)) = model_lookup(d.wpid) {
        info.name = name.into();
        info.model = model.into();
        detected
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(info.id.0.clone(), (model.into(), name.into()));
    }
    *d.info.lock().unwrap_or_else(|p| p.into_inner()) = info.clone();
    // 起きていれば実機から caps を組み立てる (寝ていれば既定値のまま)。
    if d.ping().is_ok() {
        d.caps = d.probe_caps(&info);
    } else {
        d.info.lock().unwrap_or_else(|p| p.into_inner()).asleep = wireless;
    }
    Ok(d)
}

impl HidppDriver {
    fn emit(&self, ev: HubEvent) {
        if let Some(cb) = &self.events {
            cb(ev);
        }
    }

    fn set_asleep(&self, asleep: bool) {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if i.transport != Transport::Wireless24G || i.asleep == asleep {
            return;
        }
        i.asleep = asleep;
        let info = i.clone();
        drop(i);
        self.emit(HubEvent::Info(info.id.clone(), Box::new(info.clone())));
        if !asleep {
            // 起きた — 寝ている間に開いた画面を実機の値で描き直してもらう。
            self.emit(HubEvent::StateChanged(info.id));
        }
    }

    // ---------------------------------------------------------------- 低レベル I/O

    /// 受信した 1 レポート (long を優先、short は待たずに覗く)。
    fn recv_any(&self, wait_ms: i32) -> Option<Vec<u8>> {
        if let Some(s) = &self.short {
            let mut buf = [0u8; 20];
            if let Ok(n) = s
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .read_timeout(&mut buf, 0)
            {
                if n > 0 {
                    return Some(buf[..n].to_vec());
                }
            }
        }
        let mut buf = [0u8; 20];
        match self
            .long
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .read_timeout(&mut buf, wait_ms)
        {
            Ok(n) if n > 0 => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    /// HID++ 2.0 要求 1 件。応答の params (16B) を返す。
    fn raw(&self, feat_idx: u8, func: u8, params: &[u8], timeout_ms: u64) -> HubResult<[u8; 16]> {
        let fsw = (func << 4) | SWID;
        let mut pkt = [0u8; 20];
        pkt[0] = LONG;
        pkt[1] = self.index;
        pkt[2] = feat_idx;
        pkt[3] = fsw;
        let n = params.len().min(16);
        pkt[4..4 + n].copy_from_slice(&params[..n]);
        for attempt in 0..2 {
            self.long
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .write(&pkt)
                .map_err(|e| HubError::Io(format!("hid++ write: {e}")))?;
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            while Instant::now() < deadline {
                let Some(r) = self.recv_any(20) else { continue };
                if r.len() < 4 || r[1] != self.index {
                    continue;
                }
                // HID++ 1.0 エラー (受信機が返す: 3=到達不能 等) → 寝ている/電源 OFF。
                if r[0] == SHORT && r[2] == 0x8F && r.get(3) == Some(&feat_idx) {
                    self.set_asleep(true);
                    return Err(HubError::Protocol(
                        "マウスが応答しません — 動かして起こしてください".into(),
                    ));
                }
                if r[0] != LONG || r.len() < 20 {
                    continue;
                }
                if r[2] == 0xFF && r[3] == feat_idx && r[4] == fsw {
                    return Err(HubError::Protocol(format!(
                        "HID++ error 0x{:02X} (feature idx {feat_idx}, fn {func})",
                        r[5]
                    )));
                }
                if r[2] == feat_idx && r[3] == fsw {
                    self.set_asleep(false);
                    let mut out = [0u8; 16];
                    out.copy_from_slice(&r[4..20]);
                    return Ok(out);
                }
            }
            if attempt == 0 {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        self.set_asleep(true);
        Err(HubError::Protocol(
            "マウスが応答しません — 動かして起こしてください".into(),
        ))
    }

    /// feature id → index (キャッシュ付き)。
    fn index_of(&self, fid: u16) -> HubResult<u8> {
        if let Some(i) = self
            .feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&fid)
        {
            return Ok(*i);
        }
        let r = self.raw(0, 0, &fid.to_be_bytes(), 1000)?;
        if r[0] == 0 {
            return Err(HubError::Unsupported("HID++ feature"));
        }
        self.feats
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(fid, r[0]);
        Ok(r[0])
    }

    fn has(&self, fid: u16) -> bool {
        self.index_of(fid).is_ok()
    }

    fn call(&self, fid: u16, func: u8, params: &[u8]) -> HubResult<[u8; 16]> {
        let i = self.index_of(fid)?;
        self.raw(i, func, params, 1000)
    }

    /// IRoot.ping — 起きているかの確認。
    fn ping(&self) -> HubResult<()> {
        self.raw(0, 1, &[0, 0, 0xA5], 600).map(|_| ())
    }

    /// 受信機の HID++ 1.0 レジスタ読み (short 要求)。long/short どちらの応答も受ける。
    fn receiver_reg(&self, sub: u8, reg: u8, p0: u8) -> Option<Vec<u8>> {
        let s = self.short.as_ref()?;
        s.lock()
            .unwrap_or_else(|p| p.into_inner())
            .write(&[SHORT, RECEIVER, sub, reg, p0, 0, 0])
            .ok()?;
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            let Some(r) = self.recv_any(20) else { continue };
            if r.len() < 5 || r[1] != RECEIVER {
                continue;
            }
            if r[2] == 0x8F && r[3] == sub && r[4] == reg {
                return None;
            }
            if r[2] == sub && r[3] == reg {
                return Some(r);
            }
        }
        None
    }

    /// ペアリング表の最初のマウス → (デバイス番号, wpid)。
    fn find_paired(&self) -> Option<(u8, u16)> {
        let mut first = None;
        for n in 0..6u8 {
            let Some(r) = self.receiver_reg(0x83, 0xB5, 0x20 + n) else {
                continue;
            };
            if r.len() < 12 || r[4] != 0x20 + n {
                continue;
            }
            let wpid = u16::from_be_bytes([r[7], r[8]]);
            let kind = r[11] & 0x0F; // 2 = mouse
            if kind == 2 {
                return Some((n + 1, wpid));
            }
            first.get_or_insert((n + 1, wpid));
        }
        first
    }

    /// 受信機ファームウェア (レジスタ 0xF1)。
    fn receiver_firmware(&self) -> Option<String> {
        let a = self.receiver_reg(0x81, 0xF1, 0x01)?;
        let b = self.receiver_reg(0x81, 0xF1, 0x02)?;
        Some(format!("{:02X}.{:02X}.B{:02X}{:02X}", a[5], a[6], b[5], b[6]))
    }

    // ---------------------------------------------------------------- 機能

    fn firmware(&self) -> Option<String> {
        let r = self.call(feat::FW_VERSION, 0, &[]).ok()?;
        for e in 0..r[0].min(4) {
            let f = self.call(feat::FW_VERSION, 1, &[e]).ok()?;
            if f[0] == 0 {
                // 0 = 本体アプリケーション。
                let pfx: String = f[1..4].iter().map(|&c| c as char).collect();
                return Some(format!(
                    "{}{:02X}.{:02X}.B{:02X}{:02X}",
                    pfx.trim(),
                    f[4],
                    f[5],
                    f[6],
                    f[7]
                ));
            }
        }
        None
    }

    fn onboard_info(&self) -> HubResult<OnboardInfo> {
        if let Some(i) = *self.onboard.lock().unwrap_or_else(|p| p.into_inner()) {
            return Ok(i);
        }
        let r = self.call(feat::ONBOARD, ob::GET_INFO, &[])?;
        let info = OnboardInfo {
            profile_format: r[1],
            sector_size: u16::from_be_bytes([r[7], r[8]]),
        };
        if info.profile_format != 4 && info.profile_format != 5 {
            return Err(HubError::Unsupported("onboard profile format"));
        }
        if !(64..=1024).contains(&info.sector_size) {
            return Err(HubError::Protocol(format!(
                "unexpected sector size {}",
                info.sector_size
            )));
        }
        *self.onboard.lock().unwrap_or_else(|p| p.into_inner()) = Some(info);
        Ok(info)
    }

    /// セクタ 1 個を読む (16B ずつ。最後は size-16 から読んで重ねる)。
    fn read_sector(&self, sector: u16) -> HubResult<Vec<u8>> {
        let size = self.onboard_info()?.sector_size as usize;
        let mut out = vec![0u8; size];
        let mut off = 0usize;
        while off < size {
            let o = off.min(size - 16);
            let [sh, sl] = sector.to_be_bytes();
            let [oh, ol] = (o as u16).to_be_bytes();
            let r = self.call(feat::ONBOARD, ob::MEM_READ, &[sh, sl, oh, ol])?;
            out[o..o + 16].copy_from_slice(&r);
            off = o + 16;
        }
        Ok(out)
    }

    /// 書き込み前の保険: 元のセクタをファイルに残す (セッションで 1 回)。
    fn backup_sector(&self, sector: u16, data: &[u8]) {
        let mut done = self.backed_up.lock().unwrap_or_else(|p| p.into_inner());
        if !done.insert(sector) {
            return;
        }
        let Some(base) = std::env::var_os("APPDATA") else {
            return;
        };
        let dir = std::path::Path::new(&base)
            .join("DeviceHub")
            .join("hidpp-backup");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = std::fs::write(
            dir.join(format!("{:04x}-sector{:04x}-{ts}.bin", self.wpid, sector)),
            data,
        );
    }

    /// セクタを書いて読み戻しで照合する。`data` は CRC 込みの完成品。
    fn write_sector(&self, sector: u16, data: &[u8]) -> HubResult<()> {
        let current = self.read_sector(sector)?;
        if current == data {
            return Ok(());
        }
        self.backup_sector(sector, &current);
        let [sh, sl] = sector.to_be_bytes();
        let [lh, ll] = (data.len() as u16).to_be_bytes();
        self.call(feat::ONBOARD, ob::MEM_ADDR_WRITE, &[sh, sl, 0, 0, lh, ll])?;
        for chunk in data.chunks(16) {
            let mut b = [0u8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            self.call(feat::ONBOARD, ob::MEM_WRITE, &b)?;
        }
        self.call(feat::ONBOARD, ob::MEM_WRITE_END, &[])?;
        let back = self.read_sector(sector)?;
        if back != data {
            return Err(HubError::Protocol(
                "オンボードメモリの読み戻しが一致しません".into(),
            ));
        }
        Ok(())
    }

    /// プロファイルディレクトリ (セクタ 0) → 有効なプロファイルのセクタ番号。
    fn enabled_profiles(&self) -> HubResult<Vec<u16>> {
        let dir = self.read_sector(0)?;
        let mut out = vec![];
        for e in dir.chunks(4) {
            if e.len() < 4 || (e[0] == 0xFF && e[1] == 0xFF) {
                break;
            }
            if e[2] != 0 {
                out.push(u16::from_be_bytes([e[0], e[1]]));
            }
        }
        Ok(out)
    }

    fn current_profile_sector(&self) -> HubResult<u16> {
        let r = self.call(feat::ONBOARD, ob::GET_PROFILE, &[])?;
        Ok(u16::from_be_bytes([r[0], r[1]]))
    }

    fn load_current(&self) -> HubResult<(u16, profile::Profile)> {
        let sector = self.current_profile_sector()?;
        let raw = self.read_sector(sector)?;
        let p = profile::Profile::parse(&raw, true).map_err(HubError::Protocol)?;
        Ok((sector, p))
    }

    /// 現在のプロファイルを編集して保存し、ファームに読み直させる。
    fn edit_current(&self, f: impl FnOnce(&mut profile::Profile) -> HubResult<()>) -> HubResult<()> {
        let mode = self.call(feat::ONBOARD, ob::GET_MODE, &[])?[0];
        if mode != ob::MODE_ONBOARD {
            // ホストモードのままだとプロファイルが使われない (G HUB が残した状態)。
            self.call(feat::ONBOARD, ob::SET_MODE, &[ob::MODE_ONBOARD])?;
        }
        let (sector, mut p) = self.load_current()?;
        f(&mut p)?;
        let data = p.finalize().to_vec();
        self.write_sector(sector, &data)?;
        let [h, l] = sector.to_be_bytes();
        self.call(feat::ONBOARD, ob::SET_PROFILE, &[h, l])?;
        Ok(())
    }

    fn dpi_spec(&self) -> (u32, u32, u32) {
        match &self.caps.mouse {
            Some(m) => (m.dpi_min, m.dpi_max, m.dpi_step.max(1)),
            None => (100, 25600, 50),
        }
    }

    fn clamp_dpi(&self, v: u32) -> u32 {
        let (lo, hi, step) = self.dpi_spec();
        let v = v.clamp(lo, hi);
        lo + ((v - lo) + step / 2) / step * step
    }

    /// 体感用: 0x2201 で現在 DPI を即時に変える (失敗しても保存済みなので無視)。
    fn live_dpi(&self, dpi: u32) {
        let [h, l] = (dpi as u16).to_be_bytes();
        let _ = self.call(feat::ADJ_DPI, 3, &[0, h, l]);
    }

    fn probe_caps(&self, info: &DeviceInfo) -> Capabilities {
        let mut caps = default_caps(info);
        let Some(m) = caps.mouse.as_mut() else {
            return caps;
        };
        // DPI 範囲: [lo, (0xE000|step), hi, 0] の BE16 列。
        if let Ok(r) = self.call(feat::ADJ_DPI, 1, &[0]) {
            let vals: Vec<u16> = r[1..]
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .take_while(|&v| v != 0)
                .collect();
            let plain: Vec<u32> = vals
                .iter()
                .filter(|&&v| v < 0xE000)
                .map(|&v| v as u32)
                .collect();
            if let (Some(lo), Some(hi)) = (plain.iter().min(), plain.iter().max()) {
                m.dpi_min = *lo;
                m.dpi_max = *hi;
            }
            if let Some(step) = vals.iter().find(|&&v| v >= 0xE000) {
                m.dpi_step = (step & 0x1FFF).max(1) as u32;
            }
        }
        // ポーリング: bit n = (n+1) ms。
        if let Ok(r) = self.call(feat::REPORT_RATE, 0, &[]) {
            let mut opts: Vec<i64> = (0..8)
                .filter(|n| r[0] & (1 << n) != 0)
                .map(|n| 1000 / (n + 1) as i64)
                .collect();
            opts.sort();
            if !opts.is_empty() {
                if let Some(s) = caps.settings.iter_mut().find(|s| s.id.starts_with("polling")) {
                    s.kind = SettingKind::Choice {
                        options: opts.iter().map(|&hz| ChoiceOpt::new(hz, hz_label(hz))).collect(),
                    };
                }
            }
        }
        if let Ok(list) = self.enabled_profiles() {
            caps.profiles = (list.len() > 1).then_some(ProfileCaps {
                count: list.len() as u8,
                renamable: false,
            });
        }
        caps
    }
}

fn hz_label(hz: i64) -> String {
    if hz >= 1000 {
        format!("{}K", hz / 1000)
    } else {
        hz.to_string()
    }
}

fn polling_id(t: Transport) -> &'static str {
    if t == Transport::Wired {
        "polling_wired"
    } else {
        "polling_wireless"
    }
}

/// マウスが寝ていて実機に聞けないときの既定 (PRO X SUPERLIGHT の実測値)。
fn default_caps(info: &DeviceInfo) -> Capabilities {
    use section::*;
    let rates = [125i64, 250, 500, 1000];
    Capabilities {
        settings: vec![
            SettingSpec::choice(
                polling_id(info.transport),
                PERFORMANCE,
                "ポーリングレート",
                rates.iter().map(|&hz| ChoiceOpt::new(hz, hz_label(hz))).collect(),
            )
            .help("マウスの報告頻度 (Hz)。オンボードメモリに保存します"),
            SettingSpec::toggle("onboard_mode", SYSTEM, "オンボードメモリモード").help(
                "ON: 設定をマウス本体に保存して使う (G HUB 無しでも保持)。OFF はホストソフト任せ",
            ),
        ],
        actions: vec![ActionSpec::new("restore_rom", SYSTEM, "プロファイルを工場出荷時に戻す")
            .help("DPI・ポーリング・ボタンを本体 ROM の既定値に戻します")
            .danger("現在のプロファイルを工場出荷時の内容で上書きします")],
        profiles: None,
        mouse: Some(MouseCaps {
            dpi_min: 100,
            dpi_max: 25600,
            dpi_step: 50,
            min_stages: 1,
            max_stages: profile::DPI_SLOTS,
            xy_split: false,
            stage_colors: false,
            stage_toggle: false,
            buttons: ["左ボタン", "右ボタン", "ホイールクリック", "サイド (戻る)", "サイド (進む)"]
                .iter()
                .map(|n| ButtonSlot { name: (*n).into() })
                .collect(),
            button_kinds: vec![
                ButtonActionKind::Mouse,
                ButtonActionKind::Key,
                ButtonActionKind::Media,
                ButtonActionKind::Dpi,
                ButtonActionKind::Disabled,
            ],
            require_left_click: true,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        }),
        keyboard: None,
        lighting: vec![],
        battery: true,
        peq: None,
        firmware_feed: None,
    }
}

// ==================================================================== DeviceDriver

impl DeviceDriver for HidppDriver {
    fn info(&self) -> DeviceInfo {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if i.firmware.is_none() {
            if let Some(fw) = self.firmware() {
                i.firmware = Some(fw.clone());
                self.info.lock().unwrap_or_else(|p| p.into_inner()).firmware = Some(fw);
            }
        }
        if let Ok(b) = self.battery() {
            i.battery = Some(b);
            i.asleep = false;
        } else {
            i.asleep = i.transport == Transport::Wireless24G;
        }
        i.supported = true;
        i
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        self.ping()?;
        let mode = self.call(feat::ONBOARD, ob::GET_MODE, &[])?[0];
        let (sector, p) = self.load_current()?;
        let stages = p.stages();
        let n = stages.len().max(1);
        let active = self
            .call(feat::ONBOARD, ob::GET_DPI_INDEX, &[])
            .map(|r| r[0] as usize)
            .unwrap_or(p.default_index())
            .min(n - 1);
        let buttons = (0..BUTTONS)
            .map(|i| button::decode(p.button(i), &stages, p.shift_index()))
            .collect();
        let mut settings = BTreeMap::new();
        let ms = p.report_interval_ms().max(1) as i64;
        let transport = self.info.lock().unwrap_or_else(|p| p.into_inner()).transport;
        settings.insert(
            polling_id(transport).into(),
            SettingValue::Choice(1000 / ms),
        );
        settings.insert(
            "onboard_mode".into(),
            SettingValue::Bool(mode == ob::MODE_ONBOARD),
        );
        let profile = match self.enabled_profiles() {
            Ok(list) if list.len() > 1 => ProfileState {
                active: list.iter().position(|s| *s == sector).unwrap_or(0) as u8,
                names: list
                    .iter()
                    .map(|s| {
                        self.read_sector(*s)
                            .ok()
                            .and_then(|r| profile::Profile::parse(&r, false).ok())
                            .map(|p| p.name())
                            .unwrap_or_default()
                    })
                    .collect(),
            },
            _ => ProfileState {
                active: 0,
                names: vec![],
            },
        };
        Ok(DeviceState {
            profile,
            settings,
            mouse: Some(MouseState {
                stages: stages
                    .iter()
                    .map(|&x| DpiStage {
                        x,
                        y: None,
                        color: Rgb::default(),
                        enabled: true,
                    })
                    .collect(),
                active_stage: active,
                buttons,
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting: BTreeMap::new(),
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } if id.starts_with("polling") => {
                let hz = value.as_choice().clamp(125, 1000);
                let ms = (1000 / hz) as u8;
                self.edit_current(|p| {
                    p.set_report_interval_ms(ms);
                    Ok(())
                })?;
                let _ = self.call(feat::REPORT_RATE, 2, &[ms]);
                Ok(())
            }
            Change::Setting { id, value } if id == "onboard_mode" => {
                let m = if value.as_bool() {
                    ob::MODE_ONBOARD
                } else {
                    ob::MODE_HOST
                };
                self.call(feat::ONBOARD, ob::SET_MODE, &[m]).map(|_| ())
            }
            Change::DpiStages { stages, active } => {
                if stages.is_empty() {
                    return Err(HubError::Invalid("DPI ステージが空です".into()));
                }
                let dpis: Vec<u32> = stages
                    .iter()
                    .take(profile::DPI_SLOTS)
                    .map(|s| self.clamp_dpi(s.x))
                    .collect();
                let active = (*active).min(dpis.len() - 1);
                self.edit_current(|p| {
                    p.set_stages(&dpis, active);
                    Ok(())
                })?;
                let _ = self.call(feat::ONBOARD, ob::SET_DPI_INDEX, &[active as u8]);
                self.live_dpi(dpis[active]);
                Ok(())
            }
            Change::ActiveStage(a) => {
                let (_, p) = self.load_current()?;
                let stages = p.stages();
                let a = (*a).min(stages.len().saturating_sub(1));
                let _ = self.call(feat::ONBOARD, ob::SET_DPI_INDEX, &[a as u8]);
                if let Some(d) = stages.get(a) {
                    self.live_dpi(*d);
                }
                Ok(())
            }
            Change::Button { slot, action } => {
                if *slot >= BUTTONS {
                    return Err(HubError::Invalid("button slot".into()));
                }
                self.edit_current(|p| {
                    let (b, shift) =
                        button::encode(action, &p.stages()).map_err(HubError::Invalid)?;
                    p.set_button(*slot, b);
                    if let Some(i) = shift {
                        p.set_shift_index(i);
                    }
                    Ok(())
                })
            }
            Change::Profile(i) => {
                let list = self.enabled_profiles()?;
                let s = *list
                    .get(*i as usize)
                    .ok_or_else(|| HubError::Invalid("profile".into()))?;
                let [h, l] = s.to_be_bytes();
                self.call(feat::ONBOARD, ob::SET_PROFILE, &[h, l]).map(|_| ())
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "restore_rom" => {
                let sector = self.current_profile_sector()?;
                // ROM 既定はユーザー領域 + 0x0100 に置かれている (実機で確認: 0x0001 ↔ 0x0101)。
                let rom = self.read_sector(0x0100 | (sector & 0xFF))?;
                let mut p = profile::Profile::parse(&rom, false).map_err(HubError::Protocol)?;
                let data = p.finalize().to_vec();
                self.write_sector(sector, &data)?;
                let [h, l] = sector.to_be_bytes();
                self.call(feat::ONBOARD, ob::SET_PROFILE, &[h, l])?;
                Ok(ActionOutcome {
                    message: Some("工場出荷時のプロファイルに戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        if self.has(feat::UNIFIED_BATTERY) {
            // getStatus: [残量 %, レベル, 充電状態 (1=充電中 2=低速充電 3=満充電), 外部電源]
            let r = self.call(feat::UNIFIED_BATTERY, 1, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1..=3),
            });
        }
        if self.has(feat::BATTERY_STATUS) {
            let r = self.call(feat::BATTERY_STATUS, 0, &[])?;
            return Ok(Battery {
                percent: Some(r[0].min(100)),
                charging: matches!(r[2], 1..=4),
            });
        }
        Err(HubError::Unsupported("battery"))
    }
}

// ==================================================================== テスト

#[cfg(test)]
mod tests {
    use super::*;

    const SECTOR_DIR: &[u8] =
        include_bytes!("../fixtures/logitech_pro_x_superlight/sector_0000.bin");
    const SECTOR_P1: &[u8] = include_bytes!("../fixtures/logitech_pro_x_superlight/sector_0001.bin");
    const SECTOR_ROM: &[u8] =
        include_bytes!("../fixtures/logitech_pro_x_superlight/sector_0101.bin");

    #[test]
    fn crc_matches_device_sectors() {
        for s in [SECTOR_DIR, SECTOR_P1] {
            let n = s.len();
            assert_eq!(
                profile::crc16(&s[..n - 2]),
                u16::from_be_bytes([s[n - 2], s[n - 1]])
            );
        }
    }

    #[test]
    fn parses_user_profile() {
        let p = profile::Profile::parse(SECTOR_P1, true).unwrap();
        assert_eq!(p.report_interval_ms(), 1);
        assert_eq!(p.stages(), vec![800]);
        assert_eq!(p.default_index(), 0);
        assert_eq!(p.name(), "PROFILE_NAME_DEFAULT");
        let st = p.stages();
        let names: Vec<_> = (0..5)
            .map(|i| button::decode(p.button(i), &st, p.shift_index()))
            .collect();
        assert_eq!(
            names,
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
    fn rom_profile_has_five_stages_without_crc() {
        assert!(profile::Profile::parse(SECTOR_ROM, true).is_err());
        let p = profile::Profile::parse(SECTOR_ROM, false).unwrap();
        assert_eq!(p.stages(), vec![400, 800, 1600, 3200, 6400]);
        assert_eq!(p.default_index(), 1);
    }

    #[test]
    fn edit_and_finalize_roundtrip() {
        let mut p = profile::Profile::parse(SECTOR_P1, true).unwrap();
        p.set_stages(&[400, 800, 1600], 2);
        p.set_report_interval_ms(2);
        let data = p.finalize().to_vec();
        let q = profile::Profile::parse(&data, true).unwrap();
        assert_eq!(q.stages(), vec![400, 800, 1600]);
        assert_eq!(q.default_index(), 2);
        assert_eq!(q.report_interval_ms(), 2);
        // DPI 表より後ろ (色・ボタン・名前・ライティング) は触らない。
        let n = data.len();
        assert_eq!(&data[13..n - 2], &SECTOR_P1[13..n - 2]);
    }

    #[test]
    fn unchanged_profile_reencodes_identically() {
        let mut p = profile::Profile::parse(SECTOR_P1, true).unwrap();
        assert_eq!(p.finalize(), SECTOR_P1);
    }

    #[test]
    fn button_codec_roundtrip() {
        let st = [400, 800, 1600];
        let cases = [
            ButtonAction::Mouse(MouseFn::Left),
            ButtonAction::Mouse(MouseFn::Forward),
            ButtonAction::Mouse(MouseFn::ScrollUp),
            ButtonAction::Mouse(MouseFn::ScrollLeft),
            ButtonAction::Key(KeyStroke {
                mods: mods::LCTRL,
                usage: 0x06,
            }),
            ButtonAction::Media(MediaFn::VolumeUp),
            ButtonAction::Dpi(DpiFn::Cycle),
            ButtonAction::Dpi(DpiFn::Up),
            ButtonAction::Disabled,
        ];
        for a in cases {
            let (b, _) = button::encode(&a, &st).unwrap();
            assert_eq!(button::decode(b, &st, 0), a, "{a:?} -> {b:02x?}");
        }
        let (b, shift) = button::encode(&ButtonAction::Dpi(DpiFn::Shift(700)), &st).unwrap();
        assert_eq!(shift, Some(1));
        assert_eq!(button::decode(b, &st, 1), ButtonAction::Dpi(DpiFn::Shift(800)));
        // 解釈できない割当 (マクロ等) は往復保存できる。
        let raw = [0x01, 0x23, 0x45, 0x67];
        let a = button::decode(raw, &st, 0);
        assert_eq!(button::encode(&a, &st).unwrap().0, raw);
        assert!(button::encode(&ButtonAction::Mouse(MouseFn::DoubleClick), &st).is_err());
    }
}
