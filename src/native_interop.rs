use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::*;

// Window style constants
pub const WS_POPUP_STYLE: u32 = 0x80000000;
pub const WS_CHILD_STYLE: u32 = 0x40000000;
pub const WS_CLIPSIBLINGS_STYLE: u32 = 0x04000000;

// Win event constants
pub const EVENT_OBJECT_LOCATIONCHANGE: u32 = 0x800B;
pub const WINEVENT_OUTOFCONTEXT: u32 = 0x0000;

// Timer IDs
pub const TIMER_POLL: usize = 1;
pub const TIMER_COUNTDOWN: usize = 2;
pub const TIMER_RESET_POLL: usize = 3;
pub const TIMER_UPDATE_CHECK: usize = 4;
/// Re-checks the credentials on disk while polling is paused after an auth
/// failure, so re-authenticating is noticed in seconds rather than at the
/// next poll interval (up to an hour).
pub const TIMER_AUTH_WATCH: usize = 5;

// Custom messages
pub const WM_APP: u32 = 0x8000;
pub const WM_APP_USAGE_UPDATED: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug)]
pub struct TaskbarWindow {
    pub hwnd: HWND,
    pub rect: RECT,
}

pub fn find_taskbars() -> Vec<TaskbarWindow> {
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let taskbars = &mut *(lparam.0 as *mut Vec<TaskbarWindow>);
        let mut class_name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_TrayWnd" || class_name == "Shell_SecondaryTrayWnd" {
                if let Some(rect) = get_taskbar_rect(hwnd).or_else(|| get_window_rect_safe(hwnd)) {
                    taskbars.push(TaskbarWindow { hwnd, rect });
                }
            }
        }
        BOOL(1)
    }

    let mut taskbars: Vec<TaskbarWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut taskbars as *mut _ as isize));
    }
    taskbars.sort_by_key(|taskbar| {
        (
            taskbar.rect.top,
            taskbar.rect.left,
            taskbar.rect.bottom,
            taskbar.rect.right,
        )
    });
    taskbars
}

/// Find a child window by class name
pub fn find_child_window(parent: HWND, class_name: &str) -> Option<HWND> {
    unsafe {
        let class = wide_str(class_name);
        match FindWindowExW(
            parent,
            HWND::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::null(),
        ) {
            Ok(h) if h != HWND::default() => Some(h),
            _ => None,
        }
    }
}

/// Get taskbar position via SHAppBarMessage
pub fn get_taskbar_rect(taskbar_hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut class_name = [0u16; 64];
        let len = GetClassNameW(taskbar_hwnd, &mut class_name);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name[..len as usize]);
            if class_name == "Shell_SecondaryTrayWnd" {
                return get_window_rect_safe(taskbar_hwnd);
            }
        }

        let mut abd = APPBARDATA {
            cbSize: std::mem::size_of::<APPBARDATA>() as u32,
            hWnd: taskbar_hwnd,
            ..Default::default()
        };
        let result = SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd);
        if result == 0 {
            return None;
        }
        Some(abd.rc)
    }
}

/// Get the bounding rectangle of a window
pub fn get_window_rect_safe(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            Some(rect)
        } else {
            None
        }
    }
}

fn embedding_state_is_valid(parent: HWND, taskbar_hwnd: HWND, style: u32) -> bool {
    parent == taskbar_hwnd && style & WS_CHILD_STYLE != 0 && style & WS_POPUP_STYLE == 0
}

/// Verify the live relationship instead of inferring it from a transient
/// taskbar enumeration. During display/RDP transitions Explorer can briefly
/// omit a still-valid taskbar HWND from `EnumWindows`.
pub fn is_embedded_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(hwnd).as_bool() || !IsWindow(taskbar_hwnd).as_bool() {
            return false;
        }
        let parent = GetAncestor(hwnd, GA_PARENT);
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        embedding_state_is_valid(parent, taskbar_hwnd, style)
    }
}

fn popup_state_is_valid(owner_or_parent: HWND, style: u32) -> bool {
    owner_or_parent == HWND::default() && style & WS_CHILD_STYLE == 0 && style & WS_POPUP_STYLE != 0
}

/// Embed our window as a child of the taskbar and verify the resulting Shell
/// relationship. Win32 setters can report an ambiguous zero return value, so
/// the final parent/style state is the source of truth.
pub fn embed_in_taskbar(hwnd: HWND, taskbar_hwnd: HWND) -> Result<(), String> {
    unsafe {
        if !IsWindow(hwnd).as_bool() || !IsWindow(taskbar_hwnd).as_bool() {
            return Err("widget or taskbar window handle is no longer valid".to_string());
        }

        // Preserve existing extended style, add tool window + no activate
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32,
        );

        // Change from popup to child
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_POPUP_STYLE) | WS_CHILD_STYLE | WS_CLIPSIBLINGS_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

        let _ = SetParent(hwnd, taskbar_hwnd);

        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let parent = GetAncestor(hwnd, GA_PARENT);
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if embedding_state_is_valid(parent, taskbar_hwnd, style) {
            Ok(())
        } else {
            Err(format!(
                "taskbar embedding verification failed: parent={parent:?} expected={taskbar_hwnd:?} style={style:#010x}"
            ))
        }
    }
}

/// Undo `embed_in_taskbar`: turn the window back into a top-level popup
/// style. Callers keep that transitional window hidden until taskbar
/// re-embedding succeeds.
pub fn detach_to_popup(hwnd: HWND) -> Result<(), String> {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err("widget window handle is no longer valid".to_string());
        }
        // Clear WS_CHILD before re-parenting to the desktop, per SetParent docs.
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        let new_style = (style & !WS_CHILD_STYLE) | WS_POPUP_STYLE;
        let _ = SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);
        let _ = SetParent(hwnd, HWND::default());
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // GetAncestor(GA_PARENT) reports the desktop window for an unowned
        // top-level popup. GetParent reports its owner instead, which is null
        // for the detached window we create.
        let owner_or_parent = GetParent(hwnd).unwrap_or_default();
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if popup_state_is_valid(owner_or_parent, style) {
            Ok(())
        } else {
            Err(format!(
                "detached popup verification failed: owner_or_parent={owner_or_parent:?} style={style:#010x}"
            ))
        }
    }
}

/// Move the window
pub fn move_window(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
    unsafe {
        let _ = MoveWindow(hwnd, x, y, w, h, true);
    }
}

/// Set up a WinEvent hook for tray location changes
pub fn set_tray_event_hook(
    thread_id: u32,
    callback: unsafe extern "system" fn(HWINEVENTHOOK, u32, HWND, i32, i32, u32, u32),
) -> Option<HWINEVENTHOOK> {
    unsafe {
        let hook = SetWinEventHook(
            EVENT_OBJECT_LOCATIONCHANGE,
            EVENT_OBJECT_LOCATIONCHANGE,
            None,
            Some(callback),
            0,
            thread_id,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.is_invalid() {
            None
        } else {
            Some(hook)
        }
    }
}

/// Get the thread ID that owns a window
pub fn get_window_thread_id(hwnd: HWND) -> u32 {
    unsafe { GetWindowThreadProcessId(hwnd, None) }
}

/// Unhook a WinEvent hook
pub fn unhook_win_event(hook: HWINEVENTHOOK) {
    unsafe {
        let _ = UnhookWinEvent(hook);
    }
}

/// Convert a Rust string to a null-terminated wide string
pub fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// COLORREF wrapper (RGB packed into u32)
pub fn colorref(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Color helper
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    #[allow(dead_code)]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(hex: &str) -> Self {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Self { r, g, b }
    }

    pub const fn from_colorref(value: u32) -> Self {
        Self {
            r: (value & 0xFF) as u8,
            g: ((value >> 8) & 0xFF) as u8,
            b: ((value >> 16) & 0xFF) as u8,
        }
    }

    pub fn to_colorref(self) -> u32 {
        colorref(self.r, self.g, self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_validation_requires_expected_parent_and_child_style() {
        let taskbar = HWND(1usize as *mut _);
        let other = HWND(2usize as *mut _);
        assert!(embedding_state_is_valid(taskbar, taskbar, WS_CHILD_STYLE));
        assert!(!embedding_state_is_valid(other, taskbar, WS_CHILD_STYLE));
        assert!(!embedding_state_is_valid(taskbar, taskbar, WS_POPUP_STYLE));
    }

    #[test]
    fn popup_validation_rejects_child_or_parented_windows() {
        assert!(popup_state_is_valid(HWND::default(), WS_POPUP_STYLE));
        assert!(!popup_state_is_valid(HWND::default(), WS_CHILD_STYLE));
        assert!(!popup_state_is_valid(
            HWND(1usize as *mut _),
            WS_POPUP_STYLE
        ));
    }

    #[test]
    fn colorref_round_trip_preserves_rgb_channels() {
        let color = Color::new(12, 34, 56);
        assert_eq!(Color::from_colorref(color.to_colorref()).r, 12);
        assert_eq!(Color::from_colorref(color.to_colorref()).g, 34);
        assert_eq!(Color::from_colorref(color.to_colorref()).b, 56);
    }
}
