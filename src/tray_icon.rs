use serde::{Deserialize, Serialize};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_INFO,
    NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETFOCUS,
    NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::native_interop::{self, Color, WM_APP_TRAY};
use crate::theme;

const CLAUDE_TRAY_ICON_ID: u32 = 1;
const CODEX_TRAY_ICON_ID: u32 = 2;
const ANTIGRAVITY_TRAY_ICON_ID: u32 = 3;
const APP_TRAY_ICON_ID: u32 = 4;
const APP_ICON_RESOURCE_ID: usize = 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

const ICON_SIZE: i32 = 64;
const BAR_LEFT: i32 = 0;
const BAR_RIGHT: i32 = ICON_SIZE;
const BAR_5H_TOP: i32 = 42;
const BAR_7D_TOP: i32 = 55;
const BAR_HEIGHT: i32 = 9;
const SINGLE_BAR_TOP: i32 = 48;
const SINGLE_BAR_HEIGHT: i32 = 13;
const NUMBER_TOP: i32 = 0;
const NUMBER_BOTTOM: i32 = 38;

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 70;

/// Actions the tray message handler can request from the main window.
pub enum TrayAction {
    None,
    ShowDetails,
    ShowContextMenu(Option<TrayIconKind>),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayIconKind {
    Claude,
    Codex,
    Antigravity,
}

pub struct TrayIconData {
    pub kind: TrayIconKind,
    /// Available provider windows, ordered shortest to longest (at most two).
    pub percents: Vec<f64>,
    pub tooltip: String,
}

impl TrayIconKind {
    fn id(self) -> u32 {
        match self {
            Self::Claude => CLAUDE_TRAY_ICON_ID,
            Self::Codex => CODEX_TRAY_ICON_ID,
            Self::Antigravity => ANTIGRAVITY_TRAY_ICON_ID,
        }
    }

    fn from_id(id: u32) -> Option<Self> {
        match id {
            CLAUDE_TRAY_ICON_ID => Some(Self::Claude),
            CODEX_TRAY_ICON_ID => Some(Self::Codex),
            ANTIGRAVITY_TRAY_ICON_ID => Some(Self::Antigravity),
            _ => None,
        }
    }

    fn guid(self) -> GUID {
        match self {
            Self::Claude => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f01),
            Self::Codex => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f02),
            Self::Antigravity => GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f03),
        }
    }
}

fn app_icon_guid() -> GUID {
    GUID::from_u128(0x2b924f36_5cf3_4fcc_8ee7_03eb58e91f00)
}

/// Query the screen rectangle of one of this process's notification icons.
/// The public Shell API identifies our icons by their shared owner window and
/// distinct uID values, matching how `ensure` registers them.
pub fn rect(hwnd: HWND, kind: TrayIconKind) -> Option<RECT> {
    let identifier = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: kind.id(),
        guidItem: kind.guid(),
    };
    unsafe { Shell_NotifyIconGetRect(&identifier).ok() }
}

/// Read and validate the left-to-right (or top-to-bottom for a vertical
/// taskbar) order of this app's visible notification icons. All requested
/// icons must resolve to distinct rectangles on the selected taskbar; hidden
/// overflow icons and partial Shell results deliberately produce no order.
pub fn visible_order(
    hwnd: HWND,
    kinds: &[TrayIconKind],
    taskbar_rect: &RECT,
) -> Option<Vec<TrayIconKind>> {
    if kinds.len() <= 1 {
        return Some(kinds.to_vec());
    }

    let positions = kinds
        .iter()
        .copied()
        .map(|kind| rect(hwnd, kind).map(|rect| (kind, rect)))
        .collect::<Option<Vec<_>>>()?;
    order_from_rects(&positions, taskbar_rect)
}

fn order_from_rects(
    positions: &[(TrayIconKind, RECT)],
    taskbar_rect: &RECT,
) -> Option<Vec<TrayIconKind>> {
    let taskbar_width = taskbar_rect.right - taskbar_rect.left;
    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    if taskbar_width <= 0 || taskbar_height <= 0 {
        return None;
    }
    let horizontal = taskbar_width >= taskbar_height;

    let mut located = Vec::with_capacity(positions.len());
    let mut cross_min = i32::MAX;
    let mut cross_max = i32::MIN;
    let mut max_cross_extent = 0;
    for (kind, rect) in positions {
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;
        if width <= 0 || height <= 0 {
            return None;
        }
        let center_x = rect.left + width / 2;
        let center_y = rect.top + height / 2;
        if center_x < taskbar_rect.left
            || center_x >= taskbar_rect.right
            || center_y < taskbar_rect.top
            || center_y >= taskbar_rect.bottom
        {
            return None;
        }

        let axis = if horizontal { center_x } else { center_y };
        let cross = if horizontal { center_y } else { center_x };
        let cross_extent = if horizontal { height } else { width };
        cross_min = cross_min.min(cross);
        cross_max = cross_max.max(cross);
        max_cross_extent = max_cross_extent.max(cross_extent);
        located.push((*kind, axis));
    }

    // A thick legacy taskbar can arrange icons in multiple rows. There is no
    // unambiguous single left/right order in that layout, so retain the last
    // valid widget order instead of guessing.
    if cross_max - cross_min > max_cross_extent / 2 {
        return None;
    }

    located.sort_by_key(|(_, axis)| *axis);
    if located.windows(2).any(|pair| pair[0].1 == pair[1].1) {
        return None;
    }
    Some(located.into_iter().map(|(kind, _)| kind).collect())
}

/// Provider base colour for the icon number/bars: Claude orange, Codex
/// monochrome against the taskbar theme, Antigravity Google blue. Fixed
/// brand colours (no usage gradient - its mid-range read as yellow), same
/// language as the widget accents and the detail popup.
fn provider_color(kind: TrayIconKind, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        return theme::system_color(COLOR_HIGHLIGHT);
    }
    match kind {
        TrayIconKind::Claude => Color::from_hex("#D97757"),
        TrayIconKind::Codex => {
            if is_dark {
                Color::from_hex("#F5F5F5")
            } else {
                Color::from_hex("#111111")
            }
        }
        TrayIconKind::Antigravity => Color::from_hex("#4285F4"),
    }
}

/// Number/bar colour: the provider colour, switching to warning red near the
/// limit so a nearly-exhausted window is visible at a glance.
fn number_color(kind: TrayIconKind, percent: f64, is_dark: bool, high_contrast: bool) -> Color {
    if high_contrast {
        theme::system_color(COLOR_HIGHLIGHT)
    } else if percent >= 90.0 {
        Color::from_hex("#E5484D")
    } else {
        provider_color(kind, is_dark, false)
    }
}

/// Placeholder letter while no usage data is available: the provider
/// companies' initials (A = Anthropic, O = OpenAI, G = Google), which stay
/// unambiguous where Claude/Codex would both be "C".
fn placeholder_letter(kind: TrayIconKind) -> &'static str {
    match kind {
        TrayIconKind::Claude => "A",
        TrayIconKind::Codex => "O",
        TrayIconKind::Antigravity => "G",
    }
}

fn bar_track_color(is_dark: bool, high_contrast: bool) -> Color {
    // Keep the track well away from the mid-tone provider fills (Claude's
    // darker oranges especially) so the filled portion reads at 16px.
    if high_contrast {
        theme::system_color(COLOR_GRAYTEXT)
    } else if is_dark {
        Color::from_hex("#3C3C3C")
    } else {
        Color::from_hex("#D8D8D8")
    }
}

/// Create the tray icon: the first available quota percentage on top and zero,
/// one, or two proportional bars below. A single window gets one thicker,
/// centered bar; two windows retain the compact stacked layout. While no data
/// is available the number gives way to the provider's company initial.
pub fn create_icon(
    kind: TrayIconKind,
    percents: &[f64],
    is_dark: bool,
    high_contrast: bool,
) -> HICON {
    let size = ICON_SIZE;
    let base_col = provider_color(kind, is_dark, high_contrast);
    let percent = percents.first().copied();
    let number_col = match percent {
        Some(p) => number_color(kind, p, is_dark, high_contrast),
        None => base_col,
    };
    let track_col = bar_track_color(is_dark, high_contrast);

    let display_text = match percent {
        Some(p) => format!("{}", p.round().clamp(0.0, 100.0) as u32),
        None => placeholder_letter(kind).to_string(),
    };

    let font_h = -42;
    let text_y_offset = if percent.is_some() { -1 } else { 0 };

    unsafe {
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return HICON::default();
        }

        let old_bmp = SelectObject(mem_dc, dib);

        // Zero-fill (transparent background)
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        for px in pixel_data.iter_mut() {
            *px = 0;
        }

        // 1. Number (or placeholder letter) across the upper area.
        // Arial Bold matches jens-duttke/usage-monitor-for-claude's tray digits.
        let font_name = native_interop::wide_str("Arial");
        let font = CreateFontW(
            font_h,
            0,
            0,
            0,
            FW_BOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            ANTIALIASED_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(mem_dc, font);
        let _ = SetBkMode(mem_dc, TRANSPARENT);
        let _ = SetTextColor(mem_dc, COLORREF(number_col.to_colorref()));

        let mut text_rect = RECT {
            left: 0,
            top: NUMBER_TOP + text_y_offset,
            right: size,
            bottom: NUMBER_BOTTOM + text_y_offset,
        };
        let mut text_wide: Vec<u16> = display_text.encode_utf16().collect();
        let _ = DrawTextW(
            mem_dc,
            &mut text_wide,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        SelectObject(mem_dc, old_font);
        let _ = DeleteObject(font);

        // 2. Adaptive usage bars.
        let draw_bar = |dc: HDC, top: i32, height: i32, value: f64, fill_col: Color| {
            let track = CreateSolidBrush(COLORREF(track_col.to_colorref()));
            let rect = RECT {
                left: BAR_LEFT,
                top,
                right: BAR_RIGHT,
                bottom: top + height,
            };
            let _ = FillRect(dc, &rect, track);
            let _ = DeleteObject(track);
            let width =
                ((BAR_RIGHT - BAR_LEFT) as f64 * value.clamp(0.0, 100.0) / 100.0).round() as i32;
            if width > 0 {
                let fill = CreateSolidBrush(COLORREF(fill_col.to_colorref()));
                let rect = RECT {
                    left: BAR_LEFT,
                    top,
                    right: BAR_LEFT + width.min(BAR_RIGHT - BAR_LEFT),
                    bottom: top + height,
                };
                let _ = FillRect(dc, &rect, fill);
                let _ = DeleteObject(fill);
            }
        };
        match percents {
            [single] => draw_bar(
                mem_dc,
                SINGLE_BAR_TOP,
                SINGLE_BAR_HEIGHT,
                *single,
                number_color(kind, *single, is_dark, high_contrast),
            ),
            [first, second, ..] => {
                draw_bar(
                    mem_dc,
                    BAR_5H_TOP,
                    BAR_HEIGHT,
                    *first,
                    number_color(kind, *first, is_dark, high_contrast),
                );
                draw_bar(
                    mem_dc,
                    BAR_7D_TOP,
                    BAR_HEIGHT,
                    *second,
                    number_color(kind, *second, is_dark, high_contrast),
                );
            }
            [] => {}
        }

        // Set alpha: non-zero BGR pixel -> fully opaque; background stays transparent
        for px in pixel_data.iter_mut() {
            if *px != 0 {
                *px = (*px & 0x00FF_FFFF) | 0xFF00_0000;
            }
        }

        // Monochrome mask (per-pixel alpha from colour bitmap)
        let mask_bytes = vec![0u8; ((size * size + 7) / 8) as usize];
        let mask_bmp = CreateBitmap(
            size,
            size,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const std::ffi::c_void),
        );

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: dib,
        };
        let hicon = CreateIconIndirect(&icon_info).unwrap_or_default();

        let _ = DeleteObject(mask_bmp);
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        hicon
    }
}

/// Show a Windows balloon notification from the tray icon.
/// Used to alert the user when re-authentication is required.
pub fn notify_balloon(hwnd: HWND, kind: TrayIconKind, title: &str, message: &str) {
    unsafe {
        let mut nid = notify_icon_data(hwnd, kind);
        nid.uFlags |= NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(title, &mut nid.szInfoTitle);
        copy_wide_256(message, &mut nid.szInfo);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            let mut app_nid = app_notify_icon_data(hwnd);
            app_nid.uFlags |= NIF_INFO;
            app_nid.dwInfoFlags = NIIF_WARNING;
            copy_wide(title, &mut app_nid.szInfoTitle);
            copy_wide_256(message, &mut app_nid.szInfo);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &app_nid);
        }
    }
}

fn notify_icon_data(hwnd: HWND, kind: TrayIconKind) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: kind.id(),
        uFlags: NIF_GUID,
        guidItem: kind.guid(),
        ..Default::default()
    }
}

fn app_notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: APP_TRAY_ICON_ID,
        uFlags: NIF_GUID,
        guidItem: app_icon_guid(),
        ..Default::default()
    }
}

/// Copy a string into a fixed-size wide buffer (truncates to fit).
fn copy_wide<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let len = wide.len().min(N - 1);
    buf[..len].copy_from_slice(&wide[..len]);
    buf[len] = 0;
}

/// Copy a string into a 256-wide buffer.
fn copy_wide_256(s: &str, buf: &mut [u16; 256]) {
    copy_wide(s, buf)
}

/// Register or refresh the tray icon with the shell: try NIM_MODIFY first
/// (the common case on every poll) and fall back to NIM_ADD when the icon is
/// not registered - a fresh start, or explorer restarted and dropped every
/// tray registration. One icon render either way.
pub fn ensure(hwnd: HWND, kind: TrayIconKind, percents: &[f64], tooltip: &str) {
    let hicon = create_icon(
        kind,
        percents,
        theme::is_dark_mode(),
        theme::is_high_contrast(),
    );
    unsafe {
        let mut nid = notify_icon_data(hwnd, kind);
        nid.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            nid.uFlags |= NIF_MESSAGE;
            nid.uCallbackMessage = WM_APP_TRAY;
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                if !Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool() {
                    diagnose::log(format!(
                        "tray icon version negotiation failed kind={kind:?}"
                    ));
                }
            } else {
                diagnose::log(format!("tray icon registration failed kind={kind:?}"));
            }
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

fn load_embedded_app_icon(hwnd: HWND) -> HICON {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let dpi = if dpi == 0 { 96 } else { dpi };
        let width = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        let height = GetSystemMetricsForDpi(SM_CYSMICON, dpi);
        if width > 0 && height > 0 {
            if let Ok(module) = GetModuleHandleW(PCWSTR::null()) {
                let resource = PCWSTR::from_raw(APP_ICON_RESOURCE_ID as *const u16);
                if let Ok(icon) = LoadImageW(
                    HINSTANCE(module.0),
                    resource,
                    IMAGE_ICON,
                    width,
                    height,
                    LR_DEFAULTCOLOR,
                ) {
                    return HICON(icon.0);
                }
            }
        }

        let mut exe_buf = [0u16; 260];
        if GetModuleFileNameW(None, &mut exe_buf) == 0 {
            return HICON::default();
        }

        let mut large = HICON::default();
        let mut small = HICON::default();
        if ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large),
            Some(&mut small),
            1,
        ) == 0
        {
            return HICON::default();
        }
        if !small.is_invalid() {
            if !large.is_invalid() {
                let _ = DestroyIcon(large);
            }
            small
        } else {
            large
        }
    }
}

fn ensure_app(hwnd: HWND, tooltip: &str) {
    let hicon = load_embedded_app_icon(hwnd);
    unsafe {
        let mut nid = app_notify_icon_data(hwnd);
        nid.uFlags |= NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            nid.uFlags |= NIF_MESSAGE;
            nid.uCallbackMessage = WM_APP_TRAY;
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                if !Shell_NotifyIconW(NIM_SETVERSION, &nid).as_bool() {
                    diagnose::log("app tray icon version negotiation failed");
                }
            } else {
                diagnose::log("app tray icon registration failed");
            }
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Remove the tray icon from the shell.
pub fn remove(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let nid = notify_icon_data(hwnd, kind);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Return keyboard focus to the notification area after a tray context menu,
/// as required by the Shell notification icon contract.
pub fn restore_focus(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let nid = notify_icon_data(hwnd, kind);
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &nid);
    }
}

pub fn restore_app_focus(hwnd: HWND) {
    unsafe {
        let nid = app_notify_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_SETFOCUS, &nid);
    }
}

fn remove_app(hwnd: HWND) {
    unsafe {
        let nid = app_notify_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

pub fn sync(hwnd: HWND, icons: &[TrayIconData], detailed_icons: bool, app_tooltip: &str) {
    if !detailed_icons {
        ensure_app(hwnd, app_tooltip);
        for kind in [
            TrayIconKind::Claude,
            TrayIconKind::Codex,
            TrayIconKind::Antigravity,
        ] {
            remove(hwnd, kind);
        }
        return;
    }

    for kind in [
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ] {
        match icons.iter().find(|icon| icon.kind.id() == kind.id()) {
            Some(icon) => ensure(hwnd, icon.kind, &icon.percents, &icon.tooltip),
            None => remove(hwnd, kind),
        }
    }
    remove_app(hwnd);
}

pub fn remove_all(hwnd: HWND) {
    remove(hwnd, TrayIconKind::Claude);
    remove(hwnd, TrayIconKind::Codex);
    remove(hwnd, TrayIconKind::Antigravity);
    remove_app(hwnd);
}

/// Render every icon state to 32bpp BMP files for offline visual review
/// (`--dump-tray-icons <dir>`). Returns a process exit code.
pub fn dump_icons(dir: &str) -> i32 {
    let cases: &[(TrayIconKind, &str, &[f64])] = &[
        (TrayIconKind::Claude, "claude-nodata", &[]),
        (TrayIconKind::Claude, "claude-single-35", &[35.0]),
        (TrayIconKind::Claude, "claude-72-48", &[72.0, 48.0]),
        (TrayIconKind::Claude, "claude-95-88", &[95.0, 88.0]),
        (TrayIconKind::Codex, "codex-nodata", &[]),
        (TrayIconKind::Codex, "codex-single-1", &[1.0]),
        (TrayIconKind::Codex, "codex-42-12", &[42.0, 12.0]),
        (TrayIconKind::Antigravity, "ag-nodata", &[]),
        (TrayIconKind::Antigravity, "ag-single-60", &[60.0]),
        (TrayIconKind::Antigravity, "ag-100-95", &[100.0, 95.0]),
    ];
    if std::fs::create_dir_all(dir).is_err() {
        return 1;
    }
    let mut failures = 0;
    for (kind, name, percents) in cases {
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            let hicon = create_icon(*kind, percents, is_dark, false);
            let path = format!("{dir}\\{name}-{theme_name}.bmp");
            if hicon.is_invalid() || !icon_to_bmp(hicon, &path) {
                failures += 1;
            }
            if !hicon.is_invalid() {
                unsafe {
                    let _ = DestroyIcon(hicon);
                }
            }
        }
    }
    if failures == 0 {
        0
    } else {
        1
    }
}

// Kept beside the Shell-ordering helpers; bitmap export and callback helpers
// below are production code rather than additional test-only items.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn orders_single_row_icons_left_to_right() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1810, 1050, 1826, 1066)),
            (TrayIconKind::Antigravity, rect(1830, 1050, 1846, 1066)),
        ];

        assert_eq!(
            order_from_rects(&positions, &taskbar),
            Some(vec![
                TrayIconKind::Codex,
                TrayIconKind::Antigravity,
                TrayIconKind::Claude,
            ])
        );
    }

    #[test]
    fn orders_vertical_taskbar_icons_top_to_bottom() {
        let taskbar = rect(0, 0, 48, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(12, 1020, 28, 1036)),
            (TrayIconKind::Codex, rect(12, 980, 28, 996)),
        ];

        assert_eq!(
            order_from_rects(&positions, &taskbar),
            Some(vec![TrayIconKind::Codex, TrayIconKind::Claude])
        );
    }

    #[test]
    fn rejects_overflow_or_other_taskbar_rectangles() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1600, 900, 1616, 916)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn rejects_ambiguous_multi_row_layout() {
        let taskbar = rect(0, 1000, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1010, 1866, 1026)),
            (TrayIconKind::Codex, rect(1830, 1050, 1846, 1066)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn rejects_duplicate_shell_locations() {
        let taskbar = rect(0, 1040, 1920, 1080);
        let positions = vec![
            (TrayIconKind::Claude, rect(1850, 1050, 1866, 1066)),
            (TrayIconKind::Codex, rect(1850, 1050, 1866, 1066)),
        ];

        assert_eq!(order_from_rects(&positions, &taskbar), None);
    }

    #[test]
    fn icon_guids_are_stable_and_unique() {
        let guids = [
            app_icon_guid(),
            TrayIconKind::Claude.guid(),
            TrayIconKind::Codex.guid(),
            TrayIconKind::Antigravity.guid(),
        ];
        assert_ne!(guids[0], guids[1]);
        assert_ne!(guids[1], guids[2]);
        assert_ne!(guids[0], guids[2]);
        assert_ne!(guids[0], guids[3]);
        assert_ne!(guids[1], guids[3]);
        assert_ne!(guids[2], guids[3]);
    }

    #[test]
    fn version_four_callback_decodes_icon_and_keyboard_events() {
        let select = LPARAM(((TrayIconKind::Codex.id() << 16) | NIN_KEYSELECT) as usize as isize);
        assert!(matches!(handle_message(select), TrayAction::ShowDetails));

        let context =
            LPARAM(((TrayIconKind::Antigravity.id() << 16) | WM_CONTEXTMENU) as usize as isize);
        assert!(matches!(
            handle_message(context),
            TrayAction::ShowContextMenu(Some(TrayIconKind::Antigravity))
        ));

        let app_context = LPARAM(((APP_TRAY_ICON_ID << 16) | WM_CONTEXTMENU) as usize as isize);
        assert!(matches!(
            handle_message(app_context),
            TrayAction::ShowContextMenu(None)
        ));
    }

    #[test]
    fn tray_geometry_keeps_text_and_bars_in_bounds() {
        let geometry = [
            ICON_SIZE,
            NUMBER_TOP,
            NUMBER_BOTTOM,
            BAR_LEFT,
            BAR_RIGHT,
            BAR_5H_TOP,
            BAR_7D_TOP,
            BAR_HEIGHT,
            SINGLE_BAR_TOP,
            SINGLE_BAR_HEIGHT,
        ];
        let [size, number_top, number_bottom, bar_left, bar_right, first_top, second_top, bar_height, single_top, single_height] =
            geometry;
        assert!(number_top >= 0);
        assert!(number_bottom <= first_top);
        assert!(first_top + bar_height <= second_top);
        assert!(second_top + bar_height <= size);
        assert!(number_bottom <= single_top);
        assert!(single_top + single_height <= size);
        assert!(bar_left >= 0 && bar_right <= size);
    }
}

fn icon_to_bmp(hicon: HICON, path: &str) -> bool {
    const SIZE: i32 = ICON_SIZE;
    unsafe {
        let mut info = ICONINFO::default();
        if GetIconInfo(hicon, &mut info).is_err() {
            return false;
        }
        let color_bmp = info.hbmColor;

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
        let screen_dc = GetDC(HWND::default());
        let rows = GetDIBits(
            screen_dc,
            color_bmp,
            0,
            SIZE as u32,
            Some(pixels.as_mut_ptr() as *mut std::ffi::c_void),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(HWND::default(), screen_dc);
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(info.hbmColor);
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(info.hbmMask);
        }
        if rows == 0 {
            return false;
        }

        // Minimal 32bpp BMP: file header + top-down info header + BGRA pixels.
        let pixel_bytes = pixels.len() as u32;
        let mut file = Vec::with_capacity(54 + pixels.len());
        file.extend_from_slice(b"BM");
        file.extend_from_slice(&(54 + pixel_bytes).to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&54u32.to_le_bytes());
        file.extend_from_slice(&40u32.to_le_bytes());
        file.extend_from_slice(&SIZE.to_le_bytes());
        file.extend_from_slice(&(-SIZE).to_le_bytes());
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixel_bytes.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&2835u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&0u32.to_le_bytes());
        file.extend_from_slice(&pixels);
        std::fs::write(path, file).is_ok()
    }
}

/// Interpret a tray callback message and return the action to take.
pub fn handle_message(lparam: LPARAM) -> TrayAction {
    let raw = lparam.0 as u32;
    let event = raw & 0xFFFF;
    let kind = TrayIconKind::from_id(raw >> 16);
    match event {
        WM_LBUTTONUP | NIN_SELECT | NIN_KEYSELECT => TrayAction::ShowDetails,
        WM_RBUTTONUP | WM_CONTEXTMENU => TrayAction::ShowContextMenu(kind),
        _ => TrayAction::None,
    }
}

/// Copy a string into the fixed-size szTip field (max 127 chars + null).
fn copy_to_tip(s: &str, tip: &mut [u16; 128]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut len = wide.len().min(127);
    // Don't leave a lone high surrogate at the truncation point
    if len > 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
        len -= 1;
    }
    tip[..len].copy_from_slice(&wide[..len]);
    tip[len] = 0;
}
