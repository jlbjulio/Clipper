fn main() {
    // Icons, the DPI-aware manifest and the version info Windows shows as the app name.
    embed_resource::compile("assets/clipper.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
