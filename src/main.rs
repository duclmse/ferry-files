mod api;
mod app;
mod epub;
mod os;

fn main() {
    console_error_panic_hook::set_once();
    // A window opened to play a video on its own, or the app.
    match app::popped_out() {
        Some(video) => leptos::mount::mount_to_body(move || leptos::view! { <app::PopOut video/> }),
        None => leptos::mount::mount_to_body(app::App),
    }
}
