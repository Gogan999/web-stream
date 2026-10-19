//! Windows pointer reading: `GetCursorInfo` for the handle and position,
//! the system cursors' shared handles for the standard shapes, and
//! `GetIconInfo` + `GetDIBits` for the image of anything else.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, HCURSOR, HICON, ICONINFO,
    IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, LoadCursorW,
};
use windows::core::PCWSTR;
use ws_proto::{CursorImage, CursorKind, MAX_CURSOR_SIDE};

/// Largest pointer bitmap read; it is scaled to [`MAX_CURSOR_SIDE`].
const MAX_READ_SIDE: u32 = 512;
/// An unchanged handle is read again this often: a handle can be destroyed
/// and reused for another image, and an animated cursor keeps its handle.
const REREAD: Duration = Duration::from_secs(1);
/// How often the reader checks it is on the desktop that shows (the lock
/// screen and UAC prompts have their own).
const DESKTOP_CHECK: Duration = Duration::from_millis(250);

use crate::cursor::{
    CursorLook, CursorState, ImageIds, fit, rgba_from_color, rgba_from_monochrome,
};

/// Reads the pointer; one per streaming session, polled from one thread.
pub struct CursorReader {
    /// The system cursors' handles. Windows hands out the same shared
    /// handle to every window that uses one.
    standard: Vec<(usize, CursorKind)>,
    ids: ImageIds,
    /// The handle last read and what it looked like, so an unchanged pointer
    /// costs one call.
    last: Option<(usize, Instant, CursorLook)>,
    desktop_checked: Option<Instant>,
}

impl Default for CursorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorReader {
    pub fn new() -> Self {
        // Positions in the same physical pixels as DXGI's display rects.
        crate::win::ensure_dpi_aware();
        let table: [(PCWSTR, CursorKind); 13] = [
            (IDC_ARROW, CursorKind::Default),
            (IDC_HAND, CursorKind::Pointer),
            (IDC_IBEAM, CursorKind::Text),
            (IDC_WAIT, CursorKind::Wait),
            (IDC_APPSTARTING, CursorKind::Progress),
            (IDC_CROSS, CursorKind::Crosshair),
            (IDC_SIZEALL, CursorKind::Move),
            (IDC_SIZEWE, CursorKind::EwResize),
            (IDC_SIZENS, CursorKind::NsResize),
            (IDC_SIZENWSE, CursorKind::NwseResize),
            (IDC_SIZENESW, CursorKind::NeswResize),
            (IDC_NO, CursorKind::NotAllowed),
            (IDC_HELP, CursorKind::Help),
        ];
        let standard = table
            .into_iter()
            .filter_map(|(name, kind)| {
                // SAFETY: a predefined cursor id with no module.
                let h = unsafe { LoadCursorW(None, name) }.ok()?;
                Some((h.0 as usize, kind))
            })
            .collect();
        Self {
            standard,
            ids: ImageIds::default(),
            last: None,
            desktop_checked: None,
        }
    }

    /// The pointer now, or `None` when Windows won't say (the secure
    /// desktop, say).
    pub fn read(&mut self) -> Option<CursorState> {
        if self
            .desktop_checked
            .is_none_or(|t| t.elapsed() >= DESKTOP_CHECK)
        {
            self.desktop_checked = Some(Instant::now());
            ws_desktop::follow_input_desktop();
        }
        let mut info = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid CURSORINFO with cbSize set.
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            if !ws_desktop::follow_input_desktop() {
                return None;
            }
            // SAFETY: as above.
            unsafe { GetCursorInfo(&mut info) }.ok()?;
        }
        let POINT { x, y } = info.ptScreenPos;
        let handle = info.hCursor.0 as usize;
        if info.flags.0 & CURSOR_SHOWING.0 == 0 || handle == 0 {
            return Some(CursorState {
                look: CursorLook::HIDDEN,
                x,
                y,
            });
        }
        let look = match &self.last {
            Some((h, at, look)) if *h == handle && at.elapsed() < REREAD => look.clone(),
            _ => {
                let look = self.look(info.hCursor);
                self.last = Some((handle, Instant::now(), look.clone()));
                look
            }
        };
        Some(CursorState { look, x, y })
    }

    /// Standard shapes keep their name (the browser shows its own in
    /// absolute mode) and carry the host's image too, for the overlay drawn
    /// in relative mode.
    fn look(&mut self, cursor: HCURSOR) -> CursorLook {
        let handle = cursor.0 as usize;
        let standard = self
            .standard
            .iter()
            .find(|(h, _)| *h == handle)
            .map(|(_, kind)| *kind);
        let image = image(cursor).map(|img| self.ids.get(img));
        let kind = match (standard, &image) {
            (Some(kind), _) => kind,
            (None, Some(_)) => CursorKind::Custom,
            (None, None) => CursorKind::Default,
        };
        CursorLook { kind, image }
    }
}

/// Deletes the bitmaps `GetIconInfo` hands over.
struct Bitmaps(HBITMAP, HBITMAP);

impl Drop for Bitmaps {
    fn drop(&mut self) {
        for b in [self.0, self.1] {
            if !b.is_invalid() {
                // SAFETY: the caller owns these bitmaps (GetIconInfo).
                let _ = unsafe { DeleteObject(HGDIOBJ(b.0)) };
            }
        }
    }
}

struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Option<Self> {
        // SAFETY: no preconditions; released on drop.
        let dc = unsafe { GetDC(None) };
        (!dc.is_invalid()).then_some(Self(dc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        // SAFETY: obtained from GetDC(None).
        unsafe { ReleaseDC(None, self.0) };
    }
}

fn image(cursor: HCURSOR) -> Option<CursorImage> {
    let mut info = ICONINFO::default();
    // SAFETY: `info` is a valid out pointer; the bitmaps are freed below.
    unsafe { GetIconInfo(HICON(cursor.0), &mut info) }.ok()?;
    let _owned = Bitmaps(info.hbmMask, info.hbmColor);
    let dc = ScreenDc::get()?;
    let (width, mask_height) = size(info.hbmMask)?;
    let (width, height, rgba) = if info.hbmColor.is_invalid() {
        // Monochrome: the mask holds the AND half on top of the XOR half.
        let height = mask_height / 2;
        let bits = pixels(&dc, info.hbmMask, width, mask_height)?;
        let (and, xor) = bits.split_at(width as usize * height as usize * 4);
        (
            width,
            height,
            rgba_from_monochrome(and, xor, width as usize),
        )
    } else {
        let (width, height) = size(info.hbmColor)?;
        let color = pixels(&dc, info.hbmColor, width, height)?;
        let mask = pixels(&dc, info.hbmMask, width, height);
        (width, height, rgba_from_color(&color, mask.as_deref()))
    };
    if width == 0 || height == 0 {
        return None;
    }
    Some(fit(
        CursorImage {
            width,
            height,
            hot_x: info.xHotspot.min(width - 1),
            hot_y: info.yHotspot.min(height - 1),
            rgba,
        },
        MAX_CURSOR_SIDE,
    ))
}

fn size(bitmap: HBITMAP) -> Option<(u32, u32)> {
    if bitmap.is_invalid() {
        return None;
    }
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a BITMAP of the size passed.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut c_void),
        )
    };
    (got != 0 && bm.bmWidth > 0 && bm.bmHeight > 0)
        .then_some((bm.bmWidth as u32, bm.bmHeight as u32))
}

/// The bitmap as 32-bit top-down BGRA, whatever its own format.
fn pixels(dc: &ScreenDc, bitmap: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
    if width > MAX_READ_SIDE || height > MAX_READ_SIDE * 2 {
        return None;
    }
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: `buf` holds `height` rows of `width` 32-bit pixels, as `info`
    // describes.
    let lines = unsafe {
        GetDIBits(
            dc.0,
            bitmap,
            0,
            height,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    (lines == height as i32).then_some(buf)
}
