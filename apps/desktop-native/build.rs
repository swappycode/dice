fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");

    // Embed the Dice die icon into the Windows .exe as a resource. Windows reads
    // the executable's icon resource for the taskbar / Explorer / Alt-Tab / pinned
    // shortcuts; the runtime winit `set_window_icon` only touches the live window
    // and doesn't reliably reach the taskbar. Build-time only.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/dice.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/dice.ico");
        // Per-Monitor-V2 DPI awareness so Windows renders the window at PHYSICAL
        // pixels instead of bitmap-upscaling a logical-size window (the latter is
        // the usual cause of blurry text on displays scaled above 100%).
        res.set_manifest(DPI_MANIFEST);
        if let Err(e) = res.compile() {
            // Don't fail the build if the SDK resource compiler isn't found — the
            // runtime icon still applies; just warn so it's visible.
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}

#[cfg(windows)]
const DPI_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>"#;
