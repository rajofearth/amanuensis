use std::{sync::mpsc, time::Duration};

use gpui::{
    App, Context, TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;
use raycast_dictation_clone::log;
use raycast_dictation_clone::logging;
use raycast_dictation_clone::pill_window as pw;
use windows_sys::Win32::{
    Foundation::COLORREF,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetPixel, ReleaseDC, SRCCOPY,
        SelectObject,
    },
};

const TITLE_PREFIX: &str = "win-probe-window";

struct ProbeView {
    variant: String,
}

impl Render for ProbeView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0xcc2222))
            .text_color(rgb(0xffffff))
            .text_size(px(28.))
            .child(format!("PROBE {}", self.variant))
    }
}

struct VariantConfig {
    popup: bool,
    transparent: bool,
    morph: bool,
    hide_cycle: bool,
    long_hide: bool,
}

fn variant_config(name: &str) -> VariantConfig {
    match name {
        "baseline" => VariantConfig {
            popup: true,
            transparent: true,
            morph: false,
            hide_cycle: false,
            long_hide: false,
        },
        "opaque" => VariantConfig {
            popup: true,
            transparent: false,
            morph: false,
            hide_cycle: false,
            long_hide: false,
        },
        "normal-trans" => VariantConfig {
            popup: false,
            transparent: true,
            morph: false,
            hide_cycle: false,
            long_hide: false,
        },
        "normal-opaque" => VariantConfig {
            popup: false,
            transparent: false,
            morph: false,
            hide_cycle: false,
            long_hide: false,
        },
        "rawhide" => VariantConfig {
            popup: true,
            transparent: true,
            morph: false,
            hide_cycle: true,
            long_hide: false,
        },
        "morph" => VariantConfig {
            popup: true,
            transparent: true,
            morph: true,
            hide_cycle: false,
            long_hide: false,
        },
        "morph-hide" => VariantConfig {
            popup: true,
            transparent: true,
            morph: true,
            hide_cycle: true,
            long_hide: false,
        },
        "rawhide-long" => VariantConfig {
            popup: true,
            transparent: true,
            morph: false,
            hide_cycle: true,
            long_hide: true,
        },
        "morph-hidden" => VariantConfig {
            popup: true,
            transparent: true,
            morph: true,
            hide_cycle: true,
            long_hide: true,
        },
        "morph-after-show" => VariantConfig {
            popup: true,
            transparent: true,
            morph: true,
            hide_cycle: false,
            long_hide: false,
        },
        "hide-before-paint" => VariantConfig {
            popup: true,
            transparent: true,
            morph: true,
            hide_cycle: true,
            long_hide: true,
        },
        _ => VariantConfig {
            popup: true,
            transparent: true,
            morph: false,
            hide_cycle: false,
            long_hide: false,
        },
    }
}

fn title_utf16(variant: &str) -> Vec<u16> {
    format!("{TITLE_PREFIX}-{variant}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn capture_to_bmp(hwnd: pw::HWND, path: &std::path::Path) -> Option<Vec<(u8, u8, u8)>> {
    let (left, top, right, bottom) = pw::window_rect_full(hwnd)?;
    let width = right - left;
    let height = bottom - top;
    if width <= 0 || height <= 0 {
        return None;
    }
    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        if screen_dc.is_null() {
            return None;
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        let old = SelectObject(mem_dc, bitmap);
        BitBlt(mem_dc, 0, 0, width, height, screen_dc, left, top, SRCCOPY);
        let samples: Vec<(i32, i32)> = vec![
            (6, 6),
            (width - 7, 6),
            (6, height - 7),
            (width - 7, height - 7),
            (width / 2, height / 2),
        ];
        let colors: Vec<(u8, u8, u8)> = samples
            .iter()
            .map(|(sx, sy)| {
                let px: COLORREF = GetPixel(mem_dc, *sx, *sy);
                (
                    (px & 0xFF) as u8,
                    ((px >> 8) & 0xFF) as u8,
                    ((px >> 16) & 0xFF) as u8,
                )
            })
            .collect();

        let row_bytes = ((width * 4 + 3) / 4) * 4;
        let mut pixels = vec![0_u8; (row_bytes * height) as usize];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: (row_bytes * height) as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default(); 1],
        };
        let got = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        );
        if got > 0 {
            let file_header_size = 14_u32;
            let data_offset = file_header_size + info.bmiHeader.biSize;
            let file_size = data_offset + pixels.len() as u32;
            let mut bytes = Vec::with_capacity(file_size as usize);
            bytes.extend_from_slice(b"BM");
            bytes.extend_from_slice(&file_size.to_le_bytes());
            bytes.extend_from_slice(&0_u32.to_le_bytes());
            bytes.extend_from_slice(&data_offset.to_le_bytes());
            let header = info.bmiHeader;
            bytes.extend_from_slice(&header.biSize.to_le_bytes());
            bytes.extend_from_slice(&header.biWidth.to_le_bytes());
            bytes.extend_from_slice(&header.biHeight.to_le_bytes());
            bytes.extend_from_slice(&header.biPlanes.to_le_bytes());
            bytes.extend_from_slice(&header.biBitCount.to_le_bytes());
            bytes.extend_from_slice(&header.biCompression.to_le_bytes());
            bytes.extend_from_slice(&header.biSizeImage.to_le_bytes());
            bytes.extend_from_slice(&header.biXPelsPerMeter.to_le_bytes());
            bytes.extend_from_slice(&header.biYPelsPerMeter.to_le_bytes());
            bytes.extend_from_slice(&header.biClrUsed.to_le_bytes());
            bytes.extend_from_slice(&header.biClrImportant.to_le_bytes());
            bytes.extend_from_slice(&pixels);
            let _ = std::fs::write(path, &bytes);
        }

        let _ = SelectObject(mem_dc, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);

        Some(colors)
    }
}

fn main() {
    logging::init();
    let variant = std::env::var("PROBE_VARIANT").unwrap_or_else(|_| "baseline".to_owned());
    let config = variant_config(&variant);
    let title = title_utf16(&variant);
    let out_dir = std::path::PathBuf::from("probe-out");
    let _ = std::fs::create_dir_all(&out_dir);
    application().run(move |cx: &mut App| {
        let bounds = gpui::Bounds::centered(None, size(px(340.), px(64.)), cx);
        let handle = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(String::from_utf16_lossy(&title[..title.len() - 1]).into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    focus: false,
                    show: true,
                    is_resizable: false,
                    kind: if config.popup {
                        WindowKind::PopUp
                    } else {
                        WindowKind::Normal
                    },
                    window_background: if config.transparent {
                        WindowBackgroundAppearance::Transparent
                    } else {
                        WindowBackgroundAppearance::Opaque
                    },
                    ..Default::default()
                },
                |_, cx| {
                    let (sender, _receiver) = mpsc::channel::<()>();
                    let _ = sender;
                    let view = cx.new(|_| ProbeView {
                        variant: variant.clone(),
                    });
                    if variant == "hide-before-paint" {
                        if let Some(hwnd) = pw::find_by_title(&title_utf16(&variant)) {
                            pw::hide(hwnd);
                            log!("probe", "hide-before-paint: hid inside creation closure");
                        }
                    }
                    view
                },
            )
            .unwrap();

        handle
            .update(cx, {
                let variant = variant.clone();
                let out_path = out_dir.join(format!("probe-{variant}.bmp"));
                move |_, window, cx| {
                    window
                        .spawn(cx, {
                            let variant = variant.clone();
                            async move |cx| {
                                cx.background_executor()
                                    .timer(Duration::from_millis(1200))
                                    .await;
                                cx.update(|_window, _| {
                                    let Some(hwnd) = pw::find_by_title(&title_utf16(&variant))
                                    else {
                                        println!(
                                            "VARIANT={variant} CENTER=#ERROR VISIBLE=false REASON=no-hwnd"
                                        );
                                        return;
                                    };
                                    match variant.as_str() {
                                        "morph-hidden" => {
                                            pw::hide(hwnd);
                                            std::thread::sleep(Duration::from_millis(3000));
                                            apply_pill_morph(hwnd);
                                            pw::show_no_activate(hwnd);
                                        }
                                        "morph-after-show" => {
                                            std::thread::sleep(Duration::from_millis(200));
                                            apply_pill_morph(hwnd);
                                            pw::show_no_activate(hwnd);
                                        }
                                        _ => {
                                            let cfg = variant_config(&variant);
                                            if cfg.hide_cycle {
                                                pw::hide(hwnd);
                                                let hidden_ms = if cfg.long_hide {
                                                    3000
                                                } else {
                                                    400
                                                };
                                                std::thread::sleep(Duration::from_millis(
                                                    hidden_ms,
                                                ));
                                            }
                                            if cfg.morph {
                                                apply_pill_morph(hwnd);
                                            }
                                            pw::show_no_activate(hwnd);
                                        }
                                    }
                                })
                                .ok();
                                cx.background_executor()
                                    .timer(Duration::from_millis(1500))
                                    .await;
                                let result = cx
                                    .update(|_window, _| {
                                        let Some(hwnd) =
                                            pw::find_by_title(&title_utf16(&variant))
                                        else {
                                            return None;
                                        };
                                        let rect = pw::window_rect_full(hwnd);
                                        log!(
                                            "probe",
                                            "variant {variant} rect={rect:?} dpi={}",
                                            pw::dpi(hwnd)
                                        );
                                        capture_to_bmp(hwnd, &out_path)
                                    })
                                    .ok()
                                    .flatten();
                                match result {
                                    Some(colors) => {
                                        let hex: Vec<String> = colors
                                            .iter()
                                            .map(|(r, g, b)| format!("#{r:02X}{g:02X}{b:02X}"))
                                            .collect();
                                        let red_matches = colors
                                            .iter()
                                            .take(4)
                                            .filter(|&&(r, g, b)| {
                                                r > 150 && g < 90 && b < 90
                                            })
                                            .count();
                                        let visible = red_matches >= 3;
                                        println!(
                                            "VARIANT={variant} SAMPLES={} RED_CORNERS={red_matches}/4 VISIBLE={visible}",
                                            hex.join(",")
                                        );
                                    }
                                    None => {
                                        println!(
                                            "VARIANT={variant} SAMPLES=none VISIBLE=false REASON=capture-failed"
                                        );
                                    }
                                }
                                let _ = cx.update(|_, cx| cx.quit());
                            }
                        })
                        .detach();
                }
            })
            .ok();
    });
}

fn apply_pill_morph(hwnd: pw::HWND) {
    let (style, ex) = pw::styles(hwnd);
    let style = pw::pill_style(style);
    let ex = (ex & !pw::EX_CLEAR_MASK) | pw::EX_PILL;
    pw::set_styles(hwnd, style, ex);
    let (frame_w, frame_h) = pw::frame_size_for_client(pw::PILL_WIDTH, pw::PILL_HEIGHT, style, ex);
    let (wax, way, waw, wah) = pw::primary_work_area();
    pw::place(
        hwnd,
        wax + (waw - frame_w) / 2,
        way + wah - frame_h - 12,
        frame_w,
        frame_h,
        true,
    );
}
