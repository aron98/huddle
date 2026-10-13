//! WASM entrypoint: mount the app onto the page.

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(huddle_web::App);
}
