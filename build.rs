fn main() {
    slint_build::compile("ui/app.slint").unwrap();
    println!("cargo:rerun-if-changed=ui/app.slint");

    #[cfg(target_os = "windows")]
    {
        embed_resource::compile("RED.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
