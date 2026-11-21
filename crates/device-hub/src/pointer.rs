//! Windows のポインター設定 (加速 =「ポインターの精度を高める」・速度) の一時切替。
//!
//! プロセス連動ルール (`ProcRule::pointer`) から使う。ゲームが前面の間だけ加速を切り、
//! 外れたら元に戻す。
//!
//! - `SPIF_UPDATEINIFILE` を付けない = レジストリへ書かない。アプリが落ちても
//!   サインアウト/再起動で元の設定に戻る (常に「一時的な上書き」)。
//! - 元の値は最初の切替時に 1 回だけ覚え、`release` でその値に戻す。
//!   切替中にルールの種類が変わっても (加速オフ ↔ 1:1) 元の値は上書きしない。

use crate::prefs::PointerMode;
use std::sync::Mutex;

/// Windows の速度スライダーの標準値 (11 段階の 6 段目) — 移動量が 1:1 になる。
pub const SPEED_1TO1: u32 = 10;

/// `SPI_GETMOUSE` の 3 値 (しきい値 1, しきい値 2, 加速) と速度 (1..=20)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerSettings {
    pub mouse: [i32; 3],
    pub speed: u32,
}

impl PointerSettings {
    /// `mode` を適用した後の設定 (速度は 1:1 のときだけ変える)。
    pub fn with_mode(self, mode: PointerMode) -> Self {
        match mode {
            PointerMode::Keep => self,
            PointerMode::AccelOff => Self {
                mouse: [0, 0, 0],
                speed: self.speed,
            },
            PointerMode::Raw => Self {
                mouse: [0, 0, 0],
                speed: SPEED_1TO1,
            },
        }
    }

    pub fn accel_on(&self) -> bool {
        self.mouse[2] != 0
    }
}

/// (切替前の元の設定, いま適用しているモード)。None = 何も上書きしていない。
static STATE: Mutex<Option<(PointerSettings, PointerMode)>> = Mutex::new(None);

/// いま上書き中のモード (None = 元のまま)。
pub fn mode() -> Option<PointerMode> {
    STATE.lock().unwrap_or_else(|p| p.into_inner()).map(|(_, m)| m)
}

/// `mode` に切り替える。元の値は最初の 1 回だけ保存する。成功で true。
pub fn engage(mode: PointerMode) -> bool {
    if mode == PointerMode::Keep {
        return release();
    }
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let original = match *st {
        Some((orig, _)) => orig,
        None => match get() {
            Some(s) => s,
            None => return false,
        },
    };
    if !set(original.with_mode(mode)) {
        return false;
    }
    *st = Some((original, mode));
    true
}

/// 元の設定に戻す。上書きしていなければ何もしない。成功 (または不要) で true。
pub fn release() -> bool {
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let Some((original, _)) = *st else {
        return true;
    };
    if set(original) {
        *st = None;
        true
    } else {
        false
    }
}

#[cfg(windows)]
pub fn get() -> Option<PointerSettings> {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETMOUSE, SPI_GETMOUSESPEED, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut mouse = [0i32; 3];
    let mut speed = 0u32;
    unsafe {
        SystemParametersInfoW(
            SPI_GETMOUSE,
            0,
            Some(mouse.as_mut_ptr() as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
        SystemParametersInfoW(
            SPI_GETMOUSESPEED,
            0,
            Some(&mut speed as *mut u32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    Some(PointerSettings { mouse, speed })
}

/// 設定を反映する (レジストリへは書かない)。
#[cfg(windows)]
fn set(s: PointerSettings) -> bool {
    use std::ffi::c_void;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_SETMOUSE, SPI_SETMOUSESPEED, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut mouse = s.mouse;
    unsafe {
        let a = SystemParametersInfoW(
            SPI_SETMOUSE,
            0,
            Some(mouse.as_mut_ptr() as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        // SPI_SETMOUSESPEED は値そのものを pvParam に入れる (ポインタではない)。
        let b = SystemParametersInfoW(
            SPI_SETMOUSESPEED,
            0,
            Some(s.speed.clamp(1, 20) as usize as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        a.is_ok() && b.is_ok()
    }
}

#[cfg(not(windows))]
pub fn get() -> Option<PointerSettings> {
    None
}

#[cfg(not(windows))]
fn set(_s: PointerSettings) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_only_touch_what_they_promise() {
        let user = PointerSettings {
            mouse: [6, 10, 1],
            speed: 12,
        };
        assert_eq!(user.with_mode(PointerMode::Keep), user);
        let off = user.with_mode(PointerMode::AccelOff);
        assert!(!off.accel_on());
        assert_eq!(off.speed, 12);
        let raw = user.with_mode(PointerMode::Raw);
        assert!(!raw.accel_on());
        assert_eq!(raw.speed, SPEED_1TO1);
    }
}
