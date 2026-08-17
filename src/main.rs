use gpui::{
    App, AppContext, Application, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowOptions, div, rgb,
};
use gpui_component::Root;

struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(0x121212))
            .text_color(rgb(0xe8e8e8))
            .p_4()
            .child("Slate")
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        // Root must be the window's first layer or dialog/notification layers panic.
        cx.open_window(WindowOptions::default(), |window, cx| {
            let workspace = cx.new(|_| Workspace);
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open window");

        cx.activate(true);
    });
}
