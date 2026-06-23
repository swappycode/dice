// Hide the console window on Windows release builds (no effect in debug).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]

slint::include_modules!();

mod seed;

use i_slint_backend_winit::WinitWindowAccessor;
use slint::ComponentHandle;

/// Register the bundled OFL fonts (Chakra Petch display, Space Mono mono) so the
/// `font-family` names in the .slint files resolve. Embedded via include_bytes!.
/// Must run AFTER the backend is initialized (i.e. after `AppWindow::new`), since
/// the shared font collection lives on the platform context.
fn register_fonts() {
    use slint::fontique_08::fontique;
    let mut col = slint::fontique_08::shared_collection();
    for bytes in [
        &include_bytes!("../fonts/ChakraPetch-Regular.ttf")[..],
        &include_bytes!("../fonts/ChakraPetch-SemiBold.ttf")[..],
        &include_bytes!("../fonts/ChakraPetch-Bold.ttf")[..],
        &include_bytes!("../fonts/SpaceMono-Regular.ttf")[..],
        &include_bytes!("../fonts/SpaceMono-Bold.ttf")[..],
    ] {
        let blob = fontique::Blob::new(std::sync::Arc::new(bytes.to_vec()));
        let _ = col.register_fonts(blob, None);
    }
    // monochrome emoji glyphs (the software renderer can't do COLOR emoji, but
    // this gives clean outline glyphs for reactions + the composer ☺ instead of
    // missing "tofu"). Registered as an explicit fallback for the emoji script.
    let blob = fontique::Blob::new(std::sync::Arc::new(
        include_bytes!("../fonts/NotoEmoji-VF.ttf").to_vec(),
    ));
    let emoji = col.register_fonts(blob, None);
    col.append_fallbacks(
        fontique::FallbackKey::new(fontique::Script::from_str_unchecked("Zsye"), None),
        emoji.iter().map(|x| x.0),
    );
}

fn main() -> Result<(), slint::PlatformError> {
    let args: Vec<String> = std::env::args().collect();

    // Headless screenshot mode: `dice-native --shots <out-dir> [screen]`
    // renders the chosen screen in all 8 themes to PNGs via the software
    // renderer — no display needed. `screen` ∈ login|chat|voice|home (default chat).
    if let Some(pos) = args.iter().position(|a| a == "--shots") {
        let dir = args.get(pos + 1).map(String::as_str).unwrap_or(".");
        let screen = args.get(pos + 2).map(String::as_str).unwrap_or("chat");
        shots::render_all(dir, screen);
        return Ok(());
    }

    let ui = AppWindow::new()?;
    register_fonts();
    ui.global::<Theme>().set_id(0); // midnight (hero)
    seed::apply(&ui);

    // optional `--start <login|chat|voice|home>` (used by the RAM measure script
    // to idle on the login screen, matching how the old client was measured)
    if let Some(pos) = args.iter().position(|a| a == "--start") {
        if let Some(s) = args.get(pos + 1) {
            select_screen(&ui, s);
        }
    }

    // dialog open intent → set the dialog state property
    let weak = ui.as_weak();
    ui.global::<State>().on_open_dialog(move |n| {
        if let Some(ui) = weak.upgrade() {
            ui.global::<State>().set_dialog(n);
        }
    });

    // custom window controls (the window is frameless)
    let w = ui.as_weak();
    ui.global::<State>().on_win_close(move || {
        if let Some(ui) = w.upgrade() {
            let _ = ui.hide();
        }
    });
    let w = ui.as_weak();
    ui.global::<State>().on_win_minimize(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let w = ui.as_weak();
    ui.global::<State>().on_win_maximize(move || {
        if let Some(ui) = w.upgrade() {
            let win = ui.window();
            win.set_maximized(!win.is_maximized());
        }
    });
    // Title-bar drag: frameless windows have no OS move handle, so reach the winit
    // window under Slint's backend and start the OS move-loop. Ignored if the
    // platform can't drag (returns Err) — no-op rather than a panic.
    let w = ui.as_weak();
    ui.global::<State>().on_win_drag(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().with_winit_window(|win| {
                let _ = win.drag_window();
            });
        }
    });
    // edge/corner resize for the frameless window → OS resize-loop
    let w = ui.as_weak();
    ui.global::<State>().on_win_resize(move |dir| {
        if let Some(ui) = w.upgrade() {
            use winit::window::ResizeDirection::*;
            let d = match dir {
                0 => West,
                1 => East,
                2 => North,
                3 => South,
                4 => NorthWest,
                5 => NorthEast,
                6 => SouthWest,
                _ => SouthEast,
            };
            ui.window().with_winit_window(|win| {
                let _ = win.drag_resize_window(d);
            });
        }
    });

    // Apply native window chrome (taskbar icon + Windows 11 rounded corners) once
    // the loop is running and the winit window is realized. Doing it before run()
    // (or right after show()) is too early — `with_winit_window` returns None and
    // the icon/corners silently never apply. A single-shot timer fires on the
    // first tick, when the window exists.
    let w = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(80), move || {
        if let Some(ui) = w.upgrade() {
            apply_native_window_chrome(&ui);
        }
    });
    ui.run()
}

/// Apply native window decoration the Slint markup can't: the taskbar/window
/// icon and, on Windows 11, rounded corners (the window is frameless, so DWM
/// won't round it by default).
fn apply_native_window_chrome(ui: &AppWindow) {
    let applied = ui.window().with_winit_window(|win| {
        win.set_window_icon(die_icon());
        #[cfg(windows)]
        round_window_corners(win);
        true
    });
    if applied.is_none() {
        eprintln!("dice-native: winit window not ready — chrome not applied");
    }
}

/// The Dice die mark rasterized to a 64×64 RGBA icon (rounded blue face + the
/// 5-pip quincunx), so the taskbar/Alt-Tab show the app logo instead of a
/// generic window icon. Generated — no raster asset on disk.
fn die_icon() -> Option<winit::window::Icon> {
    const S: i32 = 64;
    let mut px = vec![0u8; (S * S * 4) as usize];
    let mut put = |x: i32, y: i32, c: [u8; 4]| {
        if x < 0 || y < 0 || x >= S || y >= S {
            return;
        }
        let i = ((y * S + x) * 4) as usize;
        let a = c[3] as u32;
        for k in 0..3 {
            let dst = px[i + k] as u32;
            px[i + k] = ((c[k] as u32 * a + dst * (255 - a)) / 255) as u8;
        }
        px[i + 3] = px[i + 3].max(c[3]);
    };
    // rounded-square face (accent blue, matching the midnight palette)
    let (x0, y0, x1, y1, r) = (5, 5, 59, 59, 14);
    for y in y0..y1 {
        for x in x0..x1 {
            let nx = x.clamp(x0 + r, x1 - 1 - r);
            let ny = y.clamp(y0 + r, y1 - 1 - r);
            let (dx, dy) = ((x - nx) as f32, (y - ny) as f32);
            if dx * dx + dy * dy <= (r * r) as f32 {
                put(x, y, [0x5b, 0x8c, 0xff, 0xff]);
            }
        }
    }
    // 5 white pips (quincunx), centers scaled from the logo's 100-unit viewbox
    for (cx, cy) in [(19, 19), (45, 19), (32, 32), (19, 45), (45, 45)] {
        let pr = 5;
        for y in cy - pr..=cy + pr {
            for x in cx - pr..=cx + pr {
                let (dx, dy) = ((x - cx) as f32, (y - cy) as f32);
                if dx * dx + dy * dy <= (pr * pr) as f32 {
                    put(x, y, [0xf4, 0xf6, 0xff, 0xff]);
                }
            }
        }
    }
    winit::window::Icon::from_rgba(px, S as u32, S as u32).ok()
}

/// Opt the borderless window into Windows 11's rounded-corner DWM policy (a
/// frameless window otherwise gets square corners).
#[cfg(windows)]
#[allow(unsafe_code)]
fn round_window_corners(win: &winit::window::Window) {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    let Ok(handle) = win.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(h) = handle.as_raw() {
        let hwnd = h.hwnd.get() as *mut core::ffi::c_void;
        let pref: i32 = DWMWCP_ROUND;
        // SAFETY: hwnd is a live top-level window owned by this process; the
        // attribute id and value size match the DWM corner-preference contract.
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                &pref as *const i32 as *const core::ffi::c_void,
                core::mem::size_of::<i32>() as u32,
            );
        }
    }
}

/// Apply screen/view selection used by both the live app and screenshots.
fn select_screen(ui: &AppWindow, screen: &str) {
    let st = ui.global::<State>();
    match screen {
        "login" => st.set_screen(0),
        "register" => st.set_screen(2),
        "voice" => {
            st.set_screen(1);
            st.set_view(1);
            st.set_in_voice(true);
        }
        "home" => {
            st.set_screen(1);
            st.set_view(2);
        }
        // dialogs (over the chat shell)
        "d-guild" => {
            st.set_screen(1);
            st.set_is_admin(true);
            st.set_dialog(1);
        }
        "d-server-member" => {
            st.set_screen(1);
            st.set_is_admin(false);
            st.set_dialog(1);
        }
        "d-addserver" => {
            st.set_screen(1);
            st.set_dialog(6);
        }
        "d-settings" | "d-security" => {
            st.set_screen(1);
            st.set_settings_tab(0);
            st.set_dialog(4);
        }
        "d-voice" => {
            st.set_screen(1);
            st.set_settings_tab(1);
            st.set_dialog(4);
        }
        "d-theme" => {
            st.set_screen(1);
            st.set_settings_tab(2);
            st.set_dialog(4);
        }
        "d-about" => {
            st.set_screen(1);
            st.set_settings_tab(3);
            st.set_dialog(4);
        }
        "d-friend" => {
            st.set_screen(1);
            st.set_dialog(5);
        }
        _ => {
            st.set_screen(1);
            st.set_view(0);
        }
    }
}

#[cfg(not(debug_assertions))]
mod shots {
    pub fn render_all(_dir: &str, _screen: &str) {
        eprintln!("--shots is only available in debug builds");
    }
}

#[cfg(debug_assertions)]
mod shots {
    use crate::{select_screen, seed, AppWindow, Theme};
    use slint::platform::software_renderer::{
        MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel,
    };
    use slint::platform::{Platform, WindowAdapter};
    use slint::{ComponentHandle, PhysicalSize};
    use std::rc::Rc;

    struct ShotPlatform {
        window: Rc<MinimalSoftwareWindow>,
    }
    impl Platform for ShotPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
    }

    const W: u32 = 1280;
    const H: u32 = 800;
    const THEMES: [&str; 8] = [
        "midnight", "aero", "phosphor", "bubble", "luna", "nocturne", "vantablack", "ember",
    ];

    pub fn render_all(dir: &str, screen: &str) {
        std::fs::create_dir_all(dir).ok();

        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(ShotPlatform {
            window: window.clone(),
        }))
        .expect("set_platform");

        let ui = AppWindow::new().expect("AppWindow::new");
        crate::register_fonts();
        seed::apply(&ui);
        select_screen(&ui, screen);
        window.set_size(PhysicalSize::new(W, H));
        ui.show().expect("show");

        let mut buf = vec![Rgb565Pixel(0); (W * H) as usize];

        for (id, name) in THEMES.iter().enumerate() {
            ui.global::<Theme>().set_id(id as i32);
            window.request_redraw();
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut buf, W as usize);
            });

            let path = format!("{dir}/{screen}-{:02}-{name}.png", id);
            write_png(&path, &buf);
            println!("wrote {path}");
        }
    }

    fn write_png(path: &str, buf: &[Rgb565Pixel]) {
        let mut rgb = Vec::with_capacity(buf.len() * 3);
        for px in buf {
            let v = px.0;
            let r = ((v >> 11) & 0x1f) as u8;
            let g = ((v >> 5) & 0x3f) as u8;
            let b = (v & 0x1f) as u8;
            rgb.push((r << 3) | (r >> 2));
            rgb.push((g << 2) | (g >> 4));
            rgb.push((b << 3) | (b >> 2));
        }
        let file = std::fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .expect("png header")
            .write_image_data(&rgb)
            .expect("png data");
    }
}
