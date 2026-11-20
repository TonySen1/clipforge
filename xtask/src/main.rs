use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

mod package;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("icons") | None => icons(),
        Some("ui-icons") => ui_icons(),
        Some("package") => package::package(),
        Some("check") => check(),
        Some(other) => {
            anyhow::bail!("unknown xtask command: {other} (expected `icons`, `package` or `check`)")
        }
    }
}

/// Runs the checks CI would run, in the order that fails fastest.
fn check() -> Result<()> {
    let root = repo_root();
    run_cargo(&root, &["fmt", "--all", "--check"])?;
    run_cargo(
        &root,
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_cargo(&root, &["test", "--workspace"])?;
    println!("all checks passed");
    Ok(())
}

pub(crate) fn run_cargo(root: &Path, args: &[&str]) -> Result<()> {
    println!("cargo {}", args.join(" "));
    let status = Command::new("cargo")
        .args(args)
        .current_dir(root)
        .status()
        .context("running cargo")?;
    if !status.success() {
        anyhow::bail!("cargo {} failed", args.join(" "));
    }
    Ok(())
}

pub(crate) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live inside the workspace")
        .to_path_buf()
}

fn icons() -> Result<()> {
    let root = repo_root();
    let logo_dir = root.join("assets").join("logo");
    let out_dir = root.join("assets").join("icons");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    // The 3D mark is the app icon: thickness, bevel and the hot cut face read
    // better at 32 px than the flat tile did. `app-icon.svg` stays in the repo
    // as the previous design.
    let app_icon = logo_dir.join("logo-3d.svg");

    for size in [16u32, 24, 32, 48, 64, 128, 256, 512, 1024] {
        render(&app_icon, &out_dir.join(format!("icon-{size}.png")), size)?;
    }

    for (source, prefix) in [
        (logo_dir.join("logo-light.svg"), "logo-light"),
        (logo_dir.join("logo-dark.svg"), "logo-dark"),
    ] {
        for size in [128u32, 256, 512] {
            render(&source, &out_dir.join(format!("{prefix}-{size}.png")), size)?;
        }
    }

    // Toolbar mark is rendered with transparency so it can sit on either theme.
    let neutral = logo_dir.join("mark-neutral.svg");
    for size in [24u32, 32, 48, 64] {
        render(
            &neutral,
            &out_dir.join(format!("mark-neutral-{size}.png")),
            size,
        )?;
    }

    // Alternative marks, rendered alongside the app icon so the designs can be
    // compared at real sizes.
    for (source, prefix) in [
        (logo_dir.join("logo-duo.svg"), "logo-duo"),
        (logo_dir.join("app-icon.svg"), "logo-flat"),
    ] {
        for size in [16u32, 24, 32, 48, 64, 128, 256, 512, 1024] {
            render(&source, &out_dir.join(format!("{prefix}-{size}.png")), size)?;
        }
    }

    write_ico(&out_dir)?;

    println!("icons written to {}", out_dir.display());
    Ok(())
}

/// Rasterises `assets/icons/ui/*.svg` into white-on-transparent PNGs.
///
/// The SVGs use `stroke="currentColor"` so they can be tinted at runtime. egui
/// has no SVG support, so each icon becomes a 32 and 64 px PNG whose RGB is
/// white and whose alpha carries the shape; the UI then multiplies by the
/// theme's text color.
fn ui_icons() -> Result<()> {
    let root = repo_root();
    let src_dir = root.join("assets").join("icons").join("ui");
    let out_dir = root.join("assets").join("icons").join("ui-png");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let mut rendered = 0usize;
    let entries =
        std::fs::read_dir(&src_dir).with_context(|| format!("reading {}", src_dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|e| e.to_str());
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Brand marks arrive as ready-made PNGs (the GitHub cat is the official
        // avatar). SVG icons go through the white-on-transparent pass so the
        // draw-time tint works; a PNG already carries its own colours.
        match extension {
            Some("svg") => {
                for size in [32u32, 64, 128] {
                    let out = out_dir.join(format!("{stem}-{size}.png"));
                    render_white(&path, &out, size)?;
                    rendered += 1;
                }
            }
            Some("png") => {
                // `foo-mask.png` is a silhouette: alpha is kept, RGB is forced
                // white so the draw-time tint produces the right colour in
                // both themes. The output drops the `-mask` suffix.
                let (name, as_mask) = match stem.strip_suffix("-mask") {
                    Some(base) => (base.to_owned(), true),
                    None => (stem.clone(), false),
                };
                for size in [32u32, 64, 128] {
                    let out = out_dir.join(format!("{name}-{size}.png"));
                    resize_png(&path, &out, size, as_mask)?;
                    rendered += 1;
                }
            }
            _ => {}
        }
    }

    println!("rendered {rendered} ui icons into {}", out_dir.display());
    Ok(())
}

/// Rewrites every explicit `fill="..."` to white and adds a white root fill so
/// paths without one inherit it. Leaves `fill="none"` alone: those shapes are
/// stroke-only and must stay hollow.
fn normalize_fills(svg: &str) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut rest = svg;

    while let Some(index) = rest.find("fill=\"") {
        let (before, after_start) = rest.split_at(index);
        out.push_str(before);
        let value_start = &after_start["fill=\"".len()..];
        match value_start.find('"') {
            Some(end) => {
                let value = &value_start[..end];
                if value.eq_ignore_ascii_case("none") {
                    out.push_str("fill=\"none\"");
                } else {
                    out.push_str("fill=\"#FFFFFF\"");
                }
                rest = &value_start[end + 1..];
            }
            None => {
                out.push_str(after_start);
                rest = "";
            }
        }
    }
    out.push_str(rest);

    // Give the root a white fill so `<path>` elements without one are not
    // rendered black.
    if let Some(pos) = out.find("<svg") {
        if let Some(close) = out[pos..].find('>') {
            let open_end = pos + close;
            if !out[pos..open_end].contains("fill=") {
                out.insert_str(open_end, " fill=\"#FFFFFF\"");
            }
        }
    }

    out
}

/// Center-crops a square PNG and scales it to `size`, preserving alpha.
///
/// Used for ready-made brand icons that must not be recoloured: the draw-time
/// tint path (`Mesh::add_rect_with_uv`) would flatten them to a silhouette.
fn resize_png(src: &Path, out: &Path, size: u32, as_mask: bool) -> Result<()> {
    let image = image::open(src)
        .with_context(|| format!("decoding {}", src.display()))?
        .to_rgba8();
    let (w, h) = (image.width(), image.height());
    let side = w.min(h);
    let cropped = image::imageops::crop_imm(&image, (w - side) / 2, (h - side) / 2, side, side);
    let mut resized = image::imageops::resize(
        &cropped.to_image(),
        size,
        size,
        image::imageops::FilterType::Lanczos3,
    );

    if as_mask {
        for pixel in resized.pixels_mut() {
            let [_, _, _, alpha] = pixel.0;
            *pixel = image::Rgba([255, 255, 255, alpha]);
        }
    }

    resized
        .save(out)
        .with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

/// Renders an SVG with every paint forced to white, keeping alpha.
fn render_white(svg_path: &Path, out_path: &Path, size: u32) -> Result<()> {
    let data = std::fs::read_to_string(svg_path)
        .with_context(|| format!("reading {}", svg_path.display()))?;

    // `currentColor` has no inherited value outside a browser; substitute the
    // one the UI wants and let the tint provide the real color later.
    let mut data = data.replace("currentColor", "#FFFFFF");
    // Iconfont exports either omit `fill` (SVG default is black) or bake in a
    // grey. Both leave the texture un-tintable: `Mesh` multiplies the texel by
    // the tint, so black stays black. The UI icons are monochrome by design,
    // so every explicit fill becomes white too.
    data = normalize_fills(&data);

    let tree = usvg::Tree::from_str(&data, &usvg::Options::default())
        .with_context(|| format!("parsing {}", svg_path.display()))?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
        .with_context(|| format!("allocating {size}x{size} pixmap"))?;

    let tree_size = tree.size();
    let scale = size as f32 / tree_size.width().max(tree_size.height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    pixmap
        .save_png(out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

fn render(svg_path: &Path, out_path: &Path, size: u32) -> Result<()> {
    let data =
        std::fs::read(svg_path).with_context(|| format!("reading {}", svg_path.display()))?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .with_context(|| format!("parsing {}", svg_path.display()))?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)
        .with_context(|| format!("allocating {size}x{size} pixmap"))?;

    let tree_size = tree.size();
    let scale = size as f32 / tree_size.width().max(tree_size.height());
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    pixmap
        .save_png(out_path)
        .with_context(|| format!("writing {}", out_path.display()))?;
    Ok(())
}

fn write_ico(out_dir: &Path) -> Result<()> {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let png = out_dir.join(format!("icon-{size}.png"));
        let reader = std::io::BufReader::new(
            std::fs::File::open(&png).with_context(|| format!("opening {}", png.display()))?,
        );
        let image = ico::IconImage::read_png(reader)
            .with_context(|| format!("decoding {}", png.display()))?;
        dir.add_entry(
            ico::IconDirEntry::encode(&image)
                .with_context(|| format!("encoding {} into ico", png.display()))?,
        );
    }

    let ico_path = out_dir.join("clipforge.ico");
    dir.write(std::fs::File::create(&ico_path)?)
        .with_context(|| format!("writing {}", ico_path.display()))?;
    Ok(())
}
