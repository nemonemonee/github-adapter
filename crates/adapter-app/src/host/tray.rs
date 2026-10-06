use std::cell::{Cell, RefCell};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use adapter_protocol::Result;
use windows_sys::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem,
    GetDpiForWindow, GetSystemMetricsForDpi, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{HostHandle, State, TrayAction, failure};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> HMODULE;
}

const CALLBACK: u32 = WM_APP + 17;
const UPDATE: u32 = WM_APP + 18;
const FINISHED: u32 = WM_APP + 19;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const STATUS: usize = 1;
const OPEN: usize = 2;
const QUIT: usize = 3;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn text<const N: usize>(target: &mut [u16; N], value: &str) {
    for (to, from) in target.iter_mut().take(N - 1).zip(value.encode_utf16()) {
        *to = from;
    }
}

#[derive(Clone)]
pub(super) struct Notifier {
    hwnd: Arc<AtomicIsize>,
    pending: Arc<AtomicBool>,
    thread: u32,
}

impl Notifier {
    pub(super) fn wake(&self) -> Arc<dyn Fn() + Send + Sync> {
        let this = self.clone();
        Arc::new(move || {
            let hwnd = this.hwnd.load(Ordering::Acquire) as HWND;
            if !hwnd.is_null()
                && !this.pending.swap(true, Ordering::AcqRel)
                && unsafe { PostMessageW(hwnd, UPDATE, 0, 0) } == 0
            {
                this.pending.store(false, Ordering::Release);
            }
        })
    }

    pub(super) fn finish(&self) {
        let hwnd = self.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() && unsafe { PostMessageW(hwnd, FINISHED, 0, 0) } == 0 {
            unsafe { PostThreadMessageW(self.thread, WM_QUIT, 0, 0) };
        }
    }
}

struct ThreadDpi(DPI_AWARENESS_CONTEXT);

impl ThreadDpi {
    fn enter() -> Result<Self> {
        // Only the tray thread changes; existing CLI/worker behavior stays unchanged.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(unavailable());
        }
        Ok(Self(previous))
    }
}

impl Drop for ThreadDpi {
    fn drop(&mut self) {
        if unsafe { SetThreadDpiAwarenessContext(self.0) }.is_null() {
            eprintln!(
                "tray_dpi_restore_failed: Windows could not restore the previous thread DPI context."
            );
        }
    }
}

fn small_icon_size(dpi: u32) -> Result<(i32, i32)> {
    if dpi == 0 {
        return Err(unavailable());
    }
    let width = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
    let height = unsafe { GetSystemMetricsForDpi(SM_CYSMICON, dpi) };
    if width <= 0 || height <= 0 {
        return Err(unavailable());
    }
    Ok((width, height))
}

// Resource 2 removes transparent padding only; the window keeps resource 1.
// Load at the actual DPI, not the virtualized 16px metric of an unaware caller.
struct SmallIcon {
    handle: HICON,
    size: (i32, i32),
}

impl SmallIcon {
    fn load(instance: HINSTANCE, dpi: u32) -> Result<Self> {
        let (width, height) = small_icon_size(dpi)?;
        let handle = unsafe {
            LoadImageW(
                instance,
                ptr::without_provenance::<u16>(2),
                IMAGE_ICON,
                width,
                height,
                0,
            )
        };
        if handle.is_null() {
            return Err(unavailable());
        }
        Ok(Self {
            handle,
            size: (width, height),
        })
    }
}

impl Drop for SmallIcon {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { DestroyIcon(self.handle) };
        }
    }
}

struct Window {
    host: HostHandle,
    hwnd: Cell<HWND>,
    instance: HINSTANCE,
    icon: RefCell<SmallIcon>,
    installed: Cell<bool>,
    failed: Cell<bool>,
    taskbar_created: u32,
    notifier: Notifier,
}

impl Window {
    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd.get(),
            uID: 1,
            uCallbackMessage: CALLBACK,
            hIcon: self.icon.borrow().handle,
            ..Default::default()
        }
    }

    fn install(&self) -> bool {
        if !self.set_icon_dpi(unsafe { GetDpiForWindow(self.hwnd.get()) }) {
            return false;
        }
        let mut data = self.data();
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        text(
            &mut data.szTip,
            &format!("GitHub Adapter - {:?}", self.host.snapshot().state),
        );
        if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
            return false;
        }
        self.installed.set(true);
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        (unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }) != 0
    }

    fn set_icon_dpi(&self, dpi: u32) -> bool {
        let Ok(size) = small_icon_size(dpi) else {
            return false;
        };
        if self.icon.borrow().size == size {
            return true;
        }
        let Ok(icon) = SmallIcon::load(self.instance, dpi) else {
            return false;
        };
        if self.installed.get() {
            let mut data = self.data();
            data.uFlags = NIF_ICON;
            data.hIcon = icon.handle;
            if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
                return false;
            }
        }
        // Keep the old handle valid through Shell's copy; never borrow across native callbacks.
        self.icon.replace(icon);
        true
    }

    fn remove(&self) {
        if self.installed.replace(false) {
            unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data()) };
        }
    }

    fn failed(&self) {
        if !self.failed.replace(true) {
            self.host.ui_failure();
        }
    }

    fn update(&self) {
        self.notifier.pending.store(false, Ordering::Release);
        if self.failed.get() {
            return;
        }
        let snapshot = self.host.snapshot();
        let mut data = self.data();
        data.uFlags = NIF_TIP | NIF_SHOWTIP;
        text(
            &mut data.szTip,
            &format!(
                "GitHub Adapter - {:?}; Codex {:?}",
                snapshot.state, snapshot.activation
            ),
        );
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
            self.failed();
        }
    }

    fn notice(&self, message: &str) {
        let mut data = self.data();
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        text(&mut data.szInfoTitle, "GitHub Adapter");
        text(&mut data.szInfo, message);
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
            self.failed();
        }
    }

    fn action(&self, action: TrayAction) {
        let response = self.host.route_tray(action);
        if let Some(error) = response.error {
            self.notice(&error.message);
        } else if action == TrayAction::Status {
            let snapshot = response.snapshot;
            let address = snapshot
                .address
                .map(|value| value.to_string())
                .unwrap_or_else(|| "not ready".into());
            self.notice(&format!(
                "{:?}; listener {}. Codex: {:?}. {}",
                snapshot.state, address, snapshot.activation,
                snapshot.diagnostics.last().map(|value| value.message.as_str())
                    .unwrap_or("App acknowledgment does not imply settings were reloaded. No client was restarted.")
            ));
        }
    }

    fn menu(&self) {
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            self.failed();
            return;
        }
        let state = self.host.snapshot().state;
        let ok = unsafe {
            AppendMenuW(
                menu,
                MF_STRING,
                STATUS,
                wide(&format!("Status: {state:?}")).as_ptr(),
            ) != 0
                && AppendMenuW(
                    menu,
                    MF_STRING | if state == State::Ready { 0 } else { MF_GRAYED },
                    OPEN,
                    wide("Open Codex").as_ptr(),
                ) != 0
                && AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null()) != 0
                && AppendMenuW(
                    menu,
                    MF_STRING,
                    QUIT,
                    wide("Quit - restore normal Codex").as_ptr(),
                ) != 0
        };
        if !ok {
            unsafe { DestroyMenu(menu) };
            self.failed();
            return;
        }
        let mut point = POINT::default();
        unsafe {
            GetCursorPos(&mut point);
            SetForegroundWindow(self.hwnd.get());
        }
        let choice = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                self.hwnd.get(),
                ptr::null(),
            )
        } as usize;
        unsafe {
            DestroyMenu(menu);
            PostMessageW(self.hwnd.get(), WM_NULL, 0, 0);
        }
        match choice {
            STATUS => self.action(TrayAction::Status),
            OPEN => self.action(TrayAction::Open),
            QUIT => self.action(TrayAction::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let window = create.lpCreateParams as *const Window;
        if window.is_null() {
            return 0;
        }
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, window as isize) };
        unsafe { &*window }.hwnd.set(hwnd);
        return 1;
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Window;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // Shared references remain valid through TrackPopupMenu's reentrant Windows dispatch.
    let window = unsafe { &*pointer };
    if message == window.taskbar_created {
        window.installed.set(false);
        if !window.install() {
            window.failed();
        }
        return 0;
    }
    match message {
        CALLBACK => {
            match (lparam as u32) & 0xffff {
                WM_CONTEXTMENU | WM_RBUTTONUP => window.menu(),
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => window.action(TrayAction::Open),
                _ => {}
            }
            0
        }
        UPDATE => {
            window.update();
            0
        }
        WM_DPICHANGED => {
            if !window.set_icon_dpi((wparam & 0xffff) as u32) {
                window.failed();
            }
            0
        }
        FINISHED => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_CLOSE => {
            window.action(TrayAction::Quit);
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wparam != 0 {
                window.action(TrayAction::Quit);
            }
            0
        }
        WM_DESTROY => {
            window.remove();
            window.notifier.hwnd.store(0, Ordering::Release);
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_NCDESTROY => {
            window.hwnd.set(ptr::null_mut());
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub(super) struct Tray {
    window: Box<Window>,
    class: Vec<u16>,
    instance: HINSTANCE,
    _dpi: ThreadDpi,
}

impl Tray {
    pub(super) fn new(host: HostHandle) -> Result<Self> {
        let dpi = ThreadDpi::enter()?;
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        // MAKEINTRESOURCEW(1): Windows treats this as an ID, never as a UTF-16 address.
        let icon = unsafe { LoadIconW(instance, ptr::without_provenance::<u16>(1)) };
        let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        if instance.is_null() || icon.is_null() || taskbar_created == 0 {
            return Err(unavailable());
        }
        let small_icon = SmallIcon::load(instance, unsafe { GetDpiForSystem() })?;
        let class = wide(&format!("GitHubAdapter.Tray.{}", std::process::id()));
        let descriptor = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            hIcon: icon,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        if unsafe { RegisterClassW(&descriptor) } == 0 {
            return Err(unavailable());
        }
        let notifier = Notifier {
            hwnd: Arc::new(AtomicIsize::new(0)),
            pending: Arc::new(AtomicBool::new(false)),
            thread: unsafe { GetCurrentThreadId() },
        };
        let tray = Self {
            window: Box::new(Window {
                host,
                hwnd: Cell::new(ptr::null_mut()),
                instance,
                icon: RefCell::new(small_icon),
                installed: Cell::new(false),
                failed: Cell::new(false),
                taskbar_created,
                notifier,
            }),
            class,
            instance,
            _dpi: dpi,
        };
        // A hidden top-level window, rather than HWND_MESSAGE, receives TaskbarCreated/logoff.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                tray.class.as_ptr(),
                wide("GitHub Adapter").as_ptr(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                (&*tray.window as *const Window).cast(),
            )
        };
        if hwnd.is_null() {
            return Err(unavailable());
        }
        tray.window
            .notifier
            .hwnd
            .store(hwnd as isize, Ordering::Release);
        if !tray.window.install() {
            return Err(unavailable());
        }
        Ok(tray)
    }

    pub(super) fn notifier(&self) -> Notifier {
        self.window.notifier.clone()
    }

    pub(super) fn run(self) -> Result<()> {
        let mut message = MSG::default();
        loop {
            match unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } {
                -1 => {
                    self.window.failed();
                    return Err(unavailable());
                }
                0 => break,
                _ => unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                },
            }
        }
        if self.window.failed.get() {
            Err(unavailable())
        } else {
            Ok(())
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.window.notifier.hwnd.store(0, Ordering::Release);
        self.window.remove();
        if !self.window.hwnd.get().is_null() {
            unsafe { DestroyWindow(self.window.hwnd.get()) };
        }
        unsafe { UnregisterClassW(self.class.as_ptr(), self.instance) };
    }
}

fn unavailable() -> adapter_protocol::AdapterError {
    failure(
        "tray_unavailable",
        "The native current-session tray is unavailable. No invisible background adapter is retained.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, GetWindowDpiAwarenessContext,
    };

    #[test]
    fn small_icon_dimensions_follow_explicit_display_scaling() {
        for (dpi, pixels) in [(96, 16), (120, 20), (144, 24), (192, 32)] {
            assert_eq!(small_icon_size(dpi).unwrap(), (pixels, pixels));
        }
        assert!(small_icon_size(0).is_err());
    }

    #[test]
    fn tray_thread_dpi_context_restores_its_previous_value() {
        let before = unsafe { GetThreadDpiAwarenessContext() };
        {
            let _dpi = ThreadDpi::enter().unwrap();
            assert_ne!(
                unsafe {
                    AreDpiAwarenessContextsEqual(
                        GetThreadDpiAwarenessContext(),
                        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                    )
                },
                0
            );
        }
        assert_ne!(
            unsafe { AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), before) },
            0
        );
    }

    #[test]
    fn tray_context_creates_a_dpi_aware_hidden_window() {
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) };
            }
        }

        let _dpi = ThreadDpi::enter().unwrap();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                wide("STATIC").as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        let window = TestWindow(hwnd);
        assert_ne!(
            unsafe {
                AreDpiAwarenessContextsEqual(
                    GetWindowDpiAwarenessContext(window.0),
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                )
            },
            0
        );
        assert!(small_icon_size(unsafe { GetDpiForWindow(window.0) }).is_ok());
    }
}
