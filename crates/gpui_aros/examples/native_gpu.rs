// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper
//! Bounded native scene check. Built by a consumer selecting AROS WGPU.
#[cfg(target_os = "aros")]
fn main() {
    use gpui::{
        App, AppContext, Application, Bounds, Render, Window, WindowBounds, WindowOptions, div,
        prelude::*, px, rgb, size,
    };
    use std::{rc::Rc, time::Duration};

    struct ProbeLogger;
    impl log::Log for ProbeLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            eprintln!("{} {}: {}", record.level(), record.target(), record.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: ProbeLogger = ProbeLogger;
    log::set_logger(&LOGGER).expect("probe logger");
    log::set_max_level(log::LevelFilter::Info);
    let failed = Rc::new(std::cell::Cell::new(false));
    let failed_in_app = failed.clone();

    struct Scene;
    impl Render for Scene {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(rgb(0x123456))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .size_32()
                        .rounded_lg()
                        .bg(rgb(0xee4422))
                        .border_4()
                        .border_color(rgb(0x44ee88))
                        .shadow_lg(),
                )
                .child(div().text_color(rgb(0xffffff)).child("GPUI native GPU"))
        }
    }

    Application::with_platform(Rc::new(gpui_aros::ArosPlatform::new(false))).run(
        move |cx: &mut App| {
            let bounds = Bounds::centered(None, size(px(560.), px(360.)), cx);
            let window = match cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Scene),
            ) {
                Ok(window) => window,
                Err(error) => {
                    eprintln!("GPUI native GPU: initialization failed: {error:#}");
                    failed_in_app.set(true);
                    cx.quit();
                    return;
                }
            };
            let specs = window
                .update(cx, |_, window, _| window.gpu_specs())
                .unwrap()
                .expect("native GPU identity");
            assert!(
                !specs.is_software_emulated,
                "software renderer is not GPU acceptance"
            );
            println!(
                "GPUI native GPU: adapter={} driver={}",
                specs.device_name, specs.driver_name
            );
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(8)).await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        },
    );
    println!("GPUI native GPU: event loop exited");
    if failed.get() {
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "aros"))]
fn main() {
    panic!("this example requires native AROS and the native-gpu feature");
}
