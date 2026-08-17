mod theme;

use gpui::{
    App, AppContext, Application, Context, IntoElement, ParentElement, Render, Styled, Window,
    WindowOptions, div, px,
};
use gpui_component::Root;

use theme::{Appearance, Theme, layout, theme};

struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = *theme(cx);

        div()
            .size_full()
            .bg(t.bg)
            .text_color(t.text)
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(layout::TITLEBAR_HEIGHT))
                    .w_full()
                    .bg(t.surface)
                    .border_b_1()
                    .border_color(t.border)
                    .flex()
                    .items_center()
                    .px(px(layout::SPACE_MD))
                    .child("Slate"),
            )
            .child(
                div()
                    .flex_1()
                    .p(px(layout::SPACE_LG))
                    .text_color(t.text_muted)
                    .child("No connection."),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);
        cx.set_global(Theme::new(Appearance::Dark));

        // Root must be the window's first layer or dialog and notification
        // layers panic when they look for it.
        cx.open_window(WindowOptions::default(), |window, cx| {
            let workspace = cx.new(|_| Workspace);
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open window");

        cx.activate(true);
    });
}
