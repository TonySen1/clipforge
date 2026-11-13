//! Embeds the Windows icon (and version metadata) into the executable.
//!
//! `ViewportBuilder::with_icon` only sets the runtime window icon; Explorer and
//! the taskbar read the PE resources instead. Without this step a packaged
//! build shows the generic Windows application icon.

fn main() {
    #[cfg(windows)]
    {
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/icons/clipforge.ico");
        println!("cargo:rerun-if-changed={}", icon.display());
        // A missing toolchain should not fail the build: without a resource
        // compiler the exe simply keeps the default icon.
        if let Some(err) = embed_resource::compile(icon, embed_resource::NONE)
            .manifest_optional()
            .err()
        {
            println!("cargo:warning=could not embed the application icon: {err:?}");
        }
    }
}
