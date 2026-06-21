// Hide the console window on Windows release builds (no effect in debug).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]

slint::include_modules!();

mod seed;

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

    ui.run()
}

/// Apply screen/view selection used by both the live app and screenshots.
fn select_screen(ui: &AppWindow, screen: &str) {
    let st = ui.global::<State>();
    match screen {
        "login" => st.set_screen(0),
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
        "d-theme" => st.set_dialog(0),
        "d-guild" => st.set_dialog(1),
        "d-security" => st.set_dialog(2),
        "d-voice" => st.set_dialog(3),
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
