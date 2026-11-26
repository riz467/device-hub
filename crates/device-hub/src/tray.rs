//! 通知領域 (タスクバー右下) への常駐と多重起動の防止 (Windows のみ)。
//!
//! - × (WM_CLOSE) では終了せず窓を隠し、トレイアイコンから戻す。
//!   本当の終了はトレイメニューの「終了」→ `quitting()` を立てて WM_CLOSE を送り直す。
//! - 2 つ目の起動は既存のトレイ窓に「表示」を送って即終了する (HID の取り合いを防ぐ)。
//!
//! GPUI の窓 (メインスレッド) を WM_CLOSE 処理の最中に ShowWindow で触ると
//! WM_SIZE 等が再入してアプリ状態の二重借用になり得るので、隠す/出すは必ず
//! トレイ用スレッドの窓へ PostMessage して、そちらから行う。

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    const CLASS: &str = "DeviceHubTrayWindow";
    const WM_TRAY: u32 = WM_APP + 1;
    const WM_SHOW_MAIN: u32 = WM_APP + 2;
    const WM_HIDE_MAIN: u32 = WM_APP + 3;
    const ID_OPEN: usize = 1;
    const ID_QUIT: usize = 2;

    static MAIN: AtomicIsize = AtomicIsize::new(0);
    static TRAY: AtomicIsize = AtomicIsize::new(0);
    static QUIT: AtomicBool = AtomicBool::new(false);
    static TASKBAR_CREATED: AtomicIsize = AtomicIsize::new(0);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 多重起動なら既存インスタンスの窓を前に出して false。
    /// `DEVHUB_MULTI=1` で無効化 (開発用)。
    pub fn claim_single_instance() -> bool {
        if std::env::var_os("DEVHUB_MULTI").is_some() {
            return true;
        }
        let name = wide("Local\\DeviceHub.SingleInstance");
        unsafe {
            // ハンドルはプロセス終了まで保持 (閉じない)。
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if h.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
                return true;
            }
            let class = wide(CLASS);
            let other = FindWindowW(class.as_ptr(), std::ptr::null());
            if !other.is_null() {
                PostMessageW(other, WM_SHOW_MAIN, 0, 0);
            }
        }
        false
    }

    pub fn set_main_window(hwnd: isize) {
        MAIN.store(hwnd, Ordering::Relaxed);
    }

    /// トレイの「終了」が押された (× でも本当に閉じる)。
    pub fn quitting() -> bool {
        QUIT.load(Ordering::Relaxed)
    }

    /// 窓を隠す (トレイスレッドで実行)。
    pub fn hide_main() {
        let t = TRAY.load(Ordering::Relaxed);
        if t != 0 {
            unsafe { PostMessageW(t as HWND, WM_HIDE_MAIN, 0, 0) };
        }
    }

    /// トレイアイコンを消す (終了時)。
    pub fn remove_icon() {
        let t = TRAY.swap(0, Ordering::Relaxed);
        if t != 0 {
            unsafe {
                let nid = nid(t as HWND);
                Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }
    }

    unsafe fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
        let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = hwnd;
        n.uID = 1;
        n
    }

    unsafe fn add_icon(hwnd: HWND) -> bool {
        unsafe {
            let mut n = nid(hwnd);
            n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            n.uCallbackMessage = WM_TRAY;
            // build.rs が埋め込んだリソース ID 1 (窓アイコンと同じ)。
            let inst = GetModuleHandleW(std::ptr::null());
            let cx = GetSystemMetrics(SM_CXSMICON);
            let cy = GetSystemMetrics(SM_CYSMICON);
            let icon = LoadImageW(inst, 1 as _, IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR);
            n.hIcon = if icon.is_null() {
                LoadIconW(std::ptr::null_mut(), IDI_APPLICATION)
            } else {
                icon as _
            };
            for (d, s) in n.szTip.iter_mut().zip("Device HUB".encode_utf16()) {
                *d = s;
            }
            Shell_NotifyIconW(NIM_ADD, &n) != 0
        }
    }

    unsafe fn show_main() {
        let h = MAIN.load(Ordering::Relaxed) as HWND;
        if h.is_null() {
            return;
        }
        unsafe {
            ShowWindow(h, SW_SHOW);
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            }
            SetForegroundWindow(h);
        }
    }

    unsafe fn hide_main_now() {
        let h = MAIN.load(Ordering::Relaxed) as HWND;
        if h.is_null() {
            return;
        }
        unsafe {
            // 最小化してから隠す — 最小化中は描画が止まり、mem_tick もワーキングセットを返す。
            ShowWindow(h, SW_MINIMIZE);
            ShowWindow(h, SW_HIDE);
        }
    }

    unsafe fn menu(hwnd: HWND) {
        unsafe {
            let m = CreatePopupMenu();
            if m.is_null() {
                return;
            }
            let open = wide("開く");
            let quit = wide("終了");
            AppendMenuW(m, MF_STRING, ID_OPEN, open.as_ptr());
            AppendMenuW(m, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(m, MF_STRING, ID_QUIT, quit.as_ptr());
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // 前面にしないとメニュー外クリックで閉じない (既知の Win32 の癖)。
            SetForegroundWindow(hwnd);
            let cmd = TrackPopupMenu(
                m,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(m);
            match cmd as usize {
                ID_OPEN => show_main(),
                ID_QUIT => {
                    QUIT.store(true, Ordering::Relaxed);
                    let h = MAIN.load(Ordering::Relaxed) as HWND;
                    if !h.is_null() {
                        PostMessageW(h, WM_CLOSE, 0, 0);
                    }
                }
                _ => {}
            }
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_TRAY => {
                    match lp as u32 {
                        WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => menu(hwnd),
                        _ => {}
                    }
                    0
                }
                WM_SHOW_MAIN => {
                    show_main();
                    0
                }
                WM_HIDE_MAIN => {
                    hide_main_now();
                    0
                }
                _ => {
                    // エクスプローラー再起動でアイコンが消えるので出し直す。
                    let tc = TASKBAR_CREATED.load(Ordering::Relaxed) as u32;
                    if tc != 0 && msg == tc {
                        add_icon(hwnd);
                        return 0;
                    }
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
        }
    }

    /// トレイ用スレッドを立ててアイコンを出す。出せなければ false (× は通常どおり終了させる)。
    pub fn start() -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || unsafe {
                let inst = GetModuleHandleW(std::ptr::null());
                let class = wide(CLASS);
                let mut wc: WNDCLASSEXW = std::mem::zeroed();
                wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
                wc.lpfnWndProc = Some(wndproc);
                wc.hInstance = inst;
                wc.lpszClassName = class.as_ptr();
                RegisterClassExW(&wc);
                let tc = wide("TaskbarCreated");
                TASKBAR_CREATED.store(RegisterWindowMessageW(tc.as_ptr()) as isize, Ordering::Relaxed);
                // メッセージ専用窓 (HWND_MESSAGE) は TaskbarCreated のブロードキャストを
                // 受けないので、表示しない通常のトップレベル窓にする。
                let hwnd = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    class.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    inst,
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    eprintln!("tray: CreateWindowExW failed ({})", GetLastError());
                    let _ = tx.send(false);
                    return;
                }
                if !add_icon(hwnd) {
                    eprintln!("tray: Shell_NotifyIconW(NIM_ADD) failed ({})", GetLastError());
                    let _ = tx.send(false);
                    return;
                }
                TRAY.store(hwnd as isize, Ordering::Relaxed);
                let _ = tx.send(true);
                let mut m: MSG = std::mem::zeroed();
                while GetMessageW(&mut m, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&m);
                    DispatchMessageW(&m);
                }
            })
            .ok();
        rx.recv().unwrap_or(false)
    }

    /// GPUI の窓から HWND を取る。
    pub fn hwnd_of(window: &gpui_kit::Window) -> isize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
            _ => 0,
        }
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
mod imp {
    pub fn claim_single_instance() -> bool {
        true
    }
    pub fn set_main_window(_: isize) {}
    pub fn quitting() -> bool {
        true
    }
    pub fn hide_main() {}
    pub fn remove_icon() {}
    pub fn start() -> bool {
        false
    }
    pub fn hwnd_of(_: &gpui_kit::Window) -> isize {
        0
    }
}

#[cfg(not(windows))]
pub use imp::*;
