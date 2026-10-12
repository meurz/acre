#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![recursion_limit = "256"]
//! Acre desktop app: relaunch elevated at startup when possible, and fall back to a standard scan.

mod app;
mod perf;
mod theme;

use gpui_kit::*;

fn main() {
    perf::mark_process_start();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launch = app::Launch::from_args(&args);
    #[cfg(windows)]
    if launch.should_elevate() {
        let mut child_args = args.clone();
        child_args.push(app::RELAUNCHED_FLAG.to_string());
        if let Ok(exe) = std::env::current_exe()
            && acre_model::core::volume::relaunch_elevated(&exe, &child_args).is_ok()
        {
            return;
        }
    }
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(move |cx| {
        perf::phase("app-run");
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/Inter.ttf")),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/NotoSansSC.ttf")),
            ])
            .expect("embedded fonts");
        perf::phase("fonts-added");
        gpui_kit::init(cx);
        theme::apply(cx);
        perf::phase("kit-init");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1480.), px(940.)), cx)),
            window_min_size: Some(size(px(960.), px(620.))),
            ..gpui_kit::component::TitleBar::window_options()
        };
        let launch = launch.clone();
        gpui_kit::open_window(options, cx, move |window, cx| {
            perf::phase("window-created");
            window.set_window_title(theme::PRODUCT_NAME);
            cx.new(|cx| app::DiskApp::new(launch, window, cx))
        })
        .expect("native window");
        perf::phase("open-window-returned");
        cx.activate(true);
    });
}
