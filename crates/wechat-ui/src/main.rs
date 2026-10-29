#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use gpui_kit::{
    AppContext as _, Bounds, WindowBounds, WindowOptions, application, init, open_window, point,
    px, size,
};
use wechat_ui::WeChatApp;

fn main() {
    let account_dir = std::env::args().nth(1).map(PathBuf::from);

    application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(80.), px(60.)),
                    size: size(px(1280.), px(820.)),
                })),
                focus: true,
                ..gpui_kit::component::TitleBar::window_options()
            };
            open_window(options, cx, {
                let account_dir = account_dir.clone();
                move |window, cx| {
                    #[cfg(target_os = "windows")]
                    wechat_ui::install_tray(window, cx);
                    #[cfg(not(target_os = "windows"))]
                    let _ = window;
                    cx.new(|_| WeChatApp::new(account_dir.clone()))
                }
            })
            .expect("failed to open window");
        });
}
