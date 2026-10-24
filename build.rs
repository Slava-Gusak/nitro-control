fn main() {
    // Embeds the application icon (resource id 1, loaded by the window class).
    embed_resource::compile("app.rc", embed_resource::NONE).manifest_optional().unwrap();
}
