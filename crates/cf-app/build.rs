//! Embeds the Windows icon and version metadata into the executable.
//!
//! `ViewportBuilder::with_icon` only sets the runtime window icon; Explorer and
//! the taskbar read the PE resources instead. Without this step a packaged
//! build shows the generic Windows application icon and an empty "Details" tab.
//!
//! `embed-resource` hands its input straight to `rc.exe`, which only accepts
//! resource scripts — passing the `.ico` itself compiles to an empty stub, so
//! the icon must be referenced from a generated `.rc`.

use std::path::PathBuf;

fn main() {
    #[cfg(windows)]
    {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let icon = manifest_dir.join("../../assets/icons/clipforge.ico");

        println!("cargo:rerun-if-changed={}", icon.display());
        println!("cargo:rerun-if-changed=build.rs");

        // A missing toolchain should not fail the build: without a resource
        // compiler the exe simply keeps the default icon.
        let Ok(out_dir) = std::env::var("OUT_DIR") else {
            return;
        };

        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_owned());
        let (major, minor, patch) = parse_version(&version);
        let icon_path = icon.display().to_string().replace('\\', "\\\\");

        let script = format!(
            r#"1 ICON "{icon_path}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "ClipForge contributors"
      VALUE "FileDescription", "ClipForge"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "clipforge"
      VALUE "LegalCopyright", "GPL-3.0-or-later"
      VALUE "OriginalFilename", "clipforge.exe"
      VALUE "ProductName", "ClipForge"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
        );

        let rc_path = PathBuf::from(&out_dir).join("clipforge.rc");
        if let Err(err) = std::fs::write(&rc_path, script) {
            println!("cargo:warning=could not write the resource script: {err}");
            return;
        }

        if let Some(err) = embed_resource::compile(&rc_path, embed_resource::NONE)
            .manifest_optional()
            .err()
        {
            println!("cargo:warning=could not embed the application icon: {err:?}");
        }
    }
}

/// Splits `1.2.3` into the four-part Windows version number.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_version(version: &str) -> (u16, u16, u16) {
    let mut parts = version.split('.').map(|part| {
        part.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u16>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}
