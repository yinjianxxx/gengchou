use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::{self, Color, WM_APP_TRAY};
use crate::theme;

const CLAUDE_TRAY_ICON_ID: u32 = 1;
const CODEX_TRAY_ICON_ID: u32 = 2;
const ANTIGRAVITY_TRAY_ICON_ID: u32 = 3;

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 70;

/// Actions the tray message handler can request from the main window.
pub enum TrayAction {
    None,
    ShowDetails,
    ShowContextMenu,
}

#[derive(Clone, Copy)]
pub enum TrayIconKind {
    Claude,
    Codex,
    Antigravity,
}

pub struct TrayIconData {
    pub kind: TrayIconKind,
    /// 5h window usage; None while no data is available.
    pub percent: Option<f64>,
    /// 7d window usage; None while no data is available.
    pub weekly_percent: Option<f64>,
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
}

/// Provider base colour for the icon number/bars: Claude orange, Codex
/// monochrome against the taskbar theme, Antigravity Google blue. Fixed
/// brand colours (no usage gradient - its mid-range read as yellow), same
/// language as the widget accents and the detail popup.
fn provider_color(kind: TrayIconKind, is_dark: bool) -> Color {
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
fn number_color(kind: TrayIconKind, percent: f64, is_dark: bool) -> Color {
    if percent >= 90.0 {
        Color::from_hex("#E5484D")
    } else {
        provider_color(kind, is_dark)
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

fn bar_track_color(is_dark: bool) -> Color {
    // Keep the track well away from the mid-tone provider fills (Claude's
    // darker oranges especially) so the filled portion reads at 16px.
    if is_dark {
        Color::from_hex("#3C3C3C")
    } else {
        Color::from_hex("#D8D8D8")
    }
}

/// Create the tray icon: the 5h usage number on top and two proportional
/// bars below (upper = 5h, lower = 7d), all in the provider's colour -
/// frameless, so the number and bars use the full canvas and stay legible
/// at 16px. While no data is available the number gives way to the
/// provider's company initial (A/O/G).
pub fn create_icon(
    kind: TrayIconKind,
    percent: Option<f64>,
    weekly_percent: Option<f64>,
    is_dark: bool,
) -> HICON {
    let size = 64_i32;

    // Layout mirrors jens-duttke/usage-monitor-for-claude for the bottom bars,
    // while the text cell stops a little earlier so digits do not touch them.
    const BAR_LEFT: i32 = 0;
    const BAR_RIGHT: i32 = 64;
    const BAR_5H_TOP: i32 = 43;
    const BAR_7D_TOP: i32 = 55;
    const BAR_HEIGHT: i32 = 9;
    const NUMBER_TOP: i32 = 0;
    const NUMBER_BOTTOM: i32 = 38;

    let base_col = provider_color(kind, is_dark);
    let number_col = match percent {
        Some(p) => number_color(kind, p, is_dark),
        None => base_col,
    };
    let track_col = bar_track_color(is_dark);

    let display_text = match percent {
        Some(p) => format!("{}", p.round().clamp(0.0, 100.0) as u32),
        None => placeholder_letter(kind).to_string(),
    };

    let font_h = if percent.is_some() { -40 } else { -42 };

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
            top: NUMBER_TOP,
            right: size,
            bottom: NUMBER_BOTTOM,
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

        // 2. Dual usage bars: upper = 5h, lower = 7d.
        let draw_bar = |dc: HDC, top: i32, value: Option<f64>, fill_col: Color| {
            let track = CreateSolidBrush(COLORREF(track_col.to_colorref()));
            let rect = RECT {
                left: BAR_LEFT,
                top,
                right: BAR_RIGHT,
                bottom: top + BAR_HEIGHT,
            };
            let _ = FillRect(dc, &rect, track);
            let _ = DeleteObject(track);
            if let Some(pct) = value {
                let width =
                    ((BAR_RIGHT - BAR_LEFT) as f64 * pct.clamp(0.0, 100.0) / 100.0).round() as i32;
                if width > 0 {
                    let fill = CreateSolidBrush(COLORREF(fill_col.to_colorref()));
                    let rect = RECT {
                        left: BAR_LEFT,
                        top,
                        right: BAR_LEFT + width.min(BAR_RIGHT - BAR_LEFT),
                        bottom: top + BAR_HEIGHT,
                    };
                    let _ = FillRect(dc, &rect, fill);
                    let _ = DeleteObject(fill);
                }
            }
        };
        draw_bar(
            mem_dc,
            BAR_5H_TOP,
            percent,
            number_color(kind, percent.unwrap_or(0.0), is_dark),
        );
        draw_bar(
            mem_dc,
            BAR_7D_TOP,
            weekly_percent,
            number_color(kind, weekly_percent.unwrap_or(0.0), is_dark),
        );

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
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = kind.id();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(title, &mut nid.szInfoTitle);
        copy_wide_256(message, &mut nid.szInfo);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
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
pub fn ensure(
    hwnd: HWND,
    kind: TrayIconKind,
    percent: Option<f64>,
    weekly_percent: Option<f64>,
    tooltip: &str,
) {
    let hicon = create_icon(kind, percent, weekly_percent, theme::is_dark_mode());
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = kind.id();
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = WM_APP_TRAY;
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        }
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Remove the tray icon from the shell.
pub fn remove(hwnd: HWND, kind: TrayIconKind) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = kind.id();
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

pub fn sync(hwnd: HWND, icons: &[TrayIconData]) {
    for kind in [
        TrayIconKind::Claude,
        TrayIconKind::Codex,
        TrayIconKind::Antigravity,
    ] {
        match icons.iter().find(|icon| icon.kind.id() == kind.id()) {
            Some(icon) => ensure(
                hwnd,
                icon.kind,
                icon.percent,
                icon.weekly_percent,
                &icon.tooltip,
            ),
            None => remove(hwnd, kind),
        }
    }
}

pub fn remove_all(hwnd: HWND) {
    remove(hwnd, TrayIconKind::Claude);
    remove(hwnd, TrayIconKind::Codex);
    remove(hwnd, TrayIconKind::Antigravity);
}

/// Render every icon state to 32bpp BMP files for offline visual review
/// (`--dump-tray-icons <dir>`). Returns a process exit code.
pub fn dump_icons(dir: &str) -> i32 {
    let cases: &[(TrayIconKind, &str, Option<f64>, Option<f64>)] = &[
        (TrayIconKind::Claude, "claude-nodata", None, None),
        (TrayIconKind::Claude, "claude-35-62", Some(35.0), Some(62.0)),
        (TrayIconKind::Claude, "claude-72-48", Some(72.0), Some(48.0)),
        (TrayIconKind::Claude, "claude-95-88", Some(95.0), Some(88.0)),
        (TrayIconKind::Codex, "codex-nodata", None, None),
        (TrayIconKind::Codex, "codex-42-12", Some(42.0), Some(12.0)),
        (TrayIconKind::Codex, "codex-93-97", Some(93.0), Some(97.0)),
        (TrayIconKind::Antigravity, "ag-nodata", None, None),
        (
            TrayIconKind::Antigravity,
            "ag-60-30",
            Some(60.0),
            Some(30.0),
        ),
        (
            TrayIconKind::Antigravity,
            "ag-100-95",
            Some(100.0),
            Some(95.0),
        ),
    ];
    if std::fs::create_dir_all(dir).is_err() {
        return 1;
    }
    let mut failures = 0;
    for (kind, name, session, weekly) in cases {
        for (theme_name, is_dark) in [("dark", true), ("light", false)] {
            let hicon = create_icon(*kind, *session, *weekly, is_dark);
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

fn icon_to_bmp(hicon: HICON, path: &str) -> bool {
    const SIZE: i32 = 64;
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
    let mouse_msg = lparam.0 as u32;
    match mouse_msg {
        WM_LBUTTONUP => TrayAction::ShowDetails,
        WM_RBUTTONUP => TrayAction::ShowContextMenu,
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
