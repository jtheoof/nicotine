//! Hinted text rendering for the Linux preview chrome (swash).
//!
//! Replaces plain ab_glyph rasterization for the Wayland preview titles,
//! which looked soft next to Windows' ClearType for four reasons this
//! module addresses:
//!
//! - **Size:** sizes are pixels per em (what GDI's `lfHeight = -14`
//!   means), not ab_glyph's ascender-to-descender height, which drew
//!   JetBrains Mono at ~10.6 px for a requested 14.
//! - **Hinting:** outlines are hinted (swash "smooth" mode: vertical-only,
//!   like FreeType's light hinting; TrueType and CFF), so horizontal stems
//!   and the baseline land on whole pixels.
//! - **Subpixel AA:** optional per-channel coverage for RGB/BGR panels,
//!   tripling horizontal resolution like ClearType.
//! - **Blending:** gamma-correct (linear-light) compositing, so light text
//!   on dark chrome doesn't look thin and washed out.
//!
//! Fonts are configurable (`preview_font` in config.toml): a file path, a
//! fontconfig family name, or `eve:<font>` to load a font straight from
//! the local EVE install's resource cache (e.g. `eve:evesansneue-regular`),
//! which can't be bundled. The bundled JetBrains Mono is the default and
//! the fallback.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use swash::scale::{Render, ScaleContext, Source};
use swash::shape::ShapeContext;
use swash::zeno::{Format, Vector};
use swash::FontRef;

const BUNDLED_FONT: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const BUNDLED_FONT_NAME: &str = "JetBrains Mono (bundled)";
const MARLBORO_FONT: &[u8] = include_bytes!("../assets/fonts/Marlboro.ttf");

/// How glyph coverage is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Antialias {
    Grayscale,
    /// Horizontal subpixels, red on the left.
    SubpixelRgb,
    /// Horizontal subpixels, blue on the left.
    SubpixelBgr,
}

impl Antialias {
    /// Parse the `preview_font_antialias` setting. `None` means "auto"
    /// (let the caller pick from the output's reported subpixel layout).
    pub fn from_setting(value: &str) -> Option<Option<Self>> {
        match value.to_ascii_lowercase().as_str() {
            "auto" | "" => Some(None),
            "grayscale" | "greyscale" | "gray" | "grey" | "none" => Some(Some(Self::Grayscale)),
            "rgb" | "subpixel" => Some(Some(Self::SubpixelRgb)),
            "bgr" => Some(Some(Self::SubpixelBgr)),
            _ => None,
        }
    }
}

/// A loaded font file.
pub struct TextFont {
    data: Vec<u8>,
    /// Human-readable origin, for logs.
    pub source: String,
}

impl TextFont {
    pub fn bundled() -> Self {
        Self {
            data: BUNDLED_FONT.to_vec(),
            source: BUNDLED_FONT_NAME.to_string(),
        }
    }

    /// The bundled Marlboro logo font ("Nicotine" wordmark).
    pub fn marlboro() -> Self {
        Self {
            data: MARLBORO_FONT.to_vec(),
            source: "Marlboro (bundled)".to_string(),
        }
    }

    /// Load `spec` (see module docs); `None` or a failure falls back to the
    /// bundled font, logging why.
    pub fn load_or_bundled(spec: Option<&str>) -> Self {
        let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
            return Self::bundled();
        };
        match Self::load(spec) {
            Ok(font) => font,
            Err(e) => {
                eprintln!("Preview font '{spec}' unavailable ({e:#}); using {BUNDLED_FONT_NAME}");
                Self::bundled()
            }
        }
    }

    pub fn load(spec: &str) -> Result<Self> {
        let path = resolve_font_spec(spec)?;
        let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if FontRef::from_index(&data, 0).is_none() {
            bail!("{} is not a usable font file", path.display());
        }
        Ok(Self {
            data,
            source: format!("{spec} ({})", path.display()),
        })
    }

    fn font_ref(&self) -> FontRef<'_> {
        FontRef::from_index(&self.data, 0).expect("validated at load")
    }
}

/// Rasterized text: coverage per pixel (1 byte, or 3 bytes R,G,B in
/// screen order for subpixel masks), `height` rows with the baseline at
/// row `baseline`.
#[derive(Debug, Clone, Default)]
pub struct TextMask {
    pub width: usize,
    pub height: usize,
    pub baseline: usize,
    pub subpixel: bool,
    pub data: Vec<u8>,
}

impl TextMask {
    fn bytes_per_pixel(&self) -> usize {
        if self.subpixel {
            3
        } else {
            1
        }
    }

    /// Coverage (r, g, b) at (x, y).
    pub fn coverage(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let bpp = self.bytes_per_pixel();
        let i = (y * self.width + x) * bpp;
        if self.subpixel {
            (self.data[i], self.data[i + 1], self.data[i + 2])
        } else {
            let a = self.data[i];
            (a, a, a)
        }
    }
}

/// Shape and rasterize `text` at `em_px` pixels per em.
pub fn rasterize(font: &TextFont, text: &str, em_px: f32, aa: Antialias) -> TextMask {
    let font_ref = font.font_ref();
    let em_px = em_px.max(1.0);

    let mut shape_ctx = ShapeContext::new();
    let mut shaper = shape_ctx.builder(font_ref).size(em_px).build();
    let metrics = shaper.metrics();
    shaper.add_str(text);
    // (glyph id, pen x)
    let mut glyphs = Vec::new();
    let mut pen = 0.0f32;
    shaper.shape_with(|cluster| {
        for g in cluster.glyphs {
            glyphs.push((g.id, pen + g.x));
            pen += g.advance;
        }
    });

    // Whole-pixel baseline with room for ascenders and descenders.
    let ascent = metrics.ascent.ceil().max(1.0) as usize;
    let descent = metrics.descent.abs().ceil() as usize;
    let subpixel = aa != Antialias::Grayscale;
    let mut mask = TextMask {
        width: (pen.ceil() as usize).max(1) + 1,
        height: ascent + descent,
        baseline: ascent,
        subpixel,
        data: Vec::new(),
    };
    mask.data = vec![0; mask.width * mask.height * mask.bytes_per_pixel()];

    let mut scale_ctx = ScaleContext::new();
    let mut scaler = scale_ctx.builder(font_ref).size(em_px).hint(true).build();
    let format = if subpixel {
        Format::Subpixel
    } else {
        Format::Alpha
    };
    for (id, x) in glyphs {
        // Integer pen position plus the fractional remainder rendered into
        // the glyph image, so glyph spacing stays exact.
        let origin = x.floor();
        let Some(image) = Render::new(&[Source::Outline])
            .format(format)
            .offset(Vector::new(x - origin, 0.0))
            .render(&mut scaler, id)
        else {
            continue;
        };
        let p = image.placement;
        let src_bpp = if subpixel { 4 } else { 1 };
        for row in 0..p.height as i32 {
            let y = mask.baseline as i32 - p.top + row;
            if y < 0 || y >= mask.height as i32 {
                continue;
            }
            for col in 0..p.width as i32 {
                let dx = origin as i32 + p.left + col;
                if dx < 0 || dx >= mask.width as i32 {
                    continue;
                }
                let src = ((row * p.width as i32 + col) as usize) * src_bpp;
                let dst = (y as usize * mask.width + dx as usize) * mask.bytes_per_pixel();
                if subpixel {
                    let (mut r, g, mut b) =
                        (image.data[src], image.data[src + 1], image.data[src + 2]);
                    if aa == Antialias::SubpixelBgr {
                        std::mem::swap(&mut r, &mut b);
                    }
                    // Overlapping glyphs: keep the strongest coverage.
                    mask.data[dst] = mask.data[dst].max(r);
                    mask.data[dst + 1] = mask.data[dst + 1].max(g);
                    mask.data[dst + 2] = mask.data[dst + 2].max(b);
                } else {
                    mask.data[dst] = mask.data[dst].max(image.data[src]);
                }
            }
        }
    }
    mask
}

/// Relative luminance (Rec. 709 weights on linear channels).
fn luminance(c: (u8, u8, u8)) -> f32 {
    0.2126 * srgb_to_linear(c.0) + 0.7152 * srgb_to_linear(c.1) + 0.0722 * srgb_to_linear(c.2)
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(c: f32) -> u8 {
    let c = c.clamp(0.0, 1.0);
    let s = if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

/// Blend `mask` in colour `fg` onto an opaque ARGB8888 little-endian
/// canvas (B, G, R, A bytes) of `canvas_w` × `canvas_h` pixels, with the
/// mask's top-left at (x0, y0), clipped to `max_w` columns, per channel for
/// subpixel masks.
///
/// Light text on a darker background blends in linear light; plain sRGB
/// blending makes it look thin and washed out. Dark text on a lighter
/// background blends in sRGB space: linear blending has the opposite
/// effect there (thin, faded stems). FreeType and Skia make the same
/// luminance-dependent choice.
#[allow(clippy::too_many_arguments)]
pub fn blend_mask(
    canvas: &mut [u8],
    canvas_w: usize,
    canvas_h: usize,
    x0: usize,
    y0: usize,
    max_w: usize,
    mask: &TextMask,
    fg: (u8, u8, u8),
) {
    let fg_lin = [
        srgb_to_linear(fg.2),
        srgb_to_linear(fg.1),
        srgb_to_linear(fg.0),
    ];
    let fg_srgb = [fg.2, fg.1, fg.0];
    for my in 0..mask.height {
        let y = y0 + my;
        if y >= canvas_h {
            break;
        }
        for mx in 0..mask.width.min(max_w) {
            let x = x0 + mx;
            if x >= canvas_w {
                break;
            }
            let (r, g, b) = mask.coverage(mx, my);
            if (r, g, b) == (0, 0, 0) {
                continue;
            }
            let off = (y * canvas_w + x) * 4;
            let bg_px = (canvas[off + 2], canvas[off + 1], canvas[off]);
            let linear = luminance(fg) >= luminance(bg_px);
            // Canvas byte order is B, G, R.
            for (i, cov) in [b, g, r].into_iter().enumerate() {
                let a = cov as f32 / 255.0;
                canvas[off + i] = if linear {
                    let bg = srgb_to_linear(canvas[off + i]);
                    linear_to_srgb(bg + (fg_lin[i] - bg) * a)
                } else {
                    let (bg, fg) = (canvas[off + i] as f32, fg_srgb[i] as f32);
                    (bg + (fg - bg) * a + 0.5) as u8
                };
            }
        }
    }
}

// ============================================================================
// Font spec resolution
// ============================================================================

/// Resolve a `preview_font` value to a font file:
/// - `eve:<name>`: a font from the local EVE install (`res:/ui/fonts/…`)
/// - a path (contains `/`, starts with `~`, or has a font extension)
/// - otherwise a fontconfig family/pattern via `fc-match`
pub fn resolve_font_spec(spec: &str) -> Result<PathBuf> {
    if let Some(name) = spec.strip_prefix("eve:") {
        return resolve_eve_font(name.trim());
    }
    let lower = spec.to_ascii_lowercase();
    if spec.contains('/')
        || spec.starts_with('~')
        || [".ttf", ".otf", ".ttc", ".otc"]
            .iter()
            .any(|e| lower.ends_with(e))
    {
        let path = expand_home(spec);
        if !path.is_file() {
            bail!("no such file {}", path.display());
        }
        return Ok(path);
    }
    resolve_fontconfig(spec)
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

fn resolve_fontconfig(pattern: &str) -> Result<PathBuf> {
    let out = std::process::Command::new("fc-match")
        .args(["--format", "%{file}", pattern])
        .output()
        .context("run fc-match (is fontconfig installed?)")?;
    let file = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || file.is_empty() {
        bail!("fc-match found nothing for '{pattern}'");
    }
    Ok(PathBuf::from(file))
}

/// Look `name` up in each candidate EVE install's resource index and
/// return the cached file. `name` is the file name under `res:/ui/fonts/`,
/// with or without extension (`evesansneue-regular`).
fn resolve_eve_font(name: &str) -> Result<PathBuf> {
    let name = name.to_ascii_lowercase();
    let dirs = eve_install_dirs();
    if dirs.is_empty() {
        bail!("no EVE install found (set NICOTINE_EVE_DIR to the CCP/EVE directory)");
    }
    for dir in &dirs {
        for index in ["tq/resfileindex.txt", "sisi/resfileindex.txt"] {
            let Ok(contents) = std::fs::read_to_string(dir.join(index)) else {
                continue;
            };
            if let Some(rel) = find_eve_font_in_index(&contents, &name) {
                let path = dir.join("ResFiles").join(rel);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    bail!(
        "'{name}' not in the resource index of {}",
        dirs.iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Find `res:/ui/fonts/<name>[.ext]` in a `resfileindex.txt` and return
/// its `ResFiles`-relative path. Lines are `res:/path,rel/path,md5,…`.
fn find_eve_font_in_index<'a>(index: &'a str, name: &str) -> Option<&'a str> {
    index.lines().find_map(|line| {
        let mut fields = line.split(',');
        let res = fields.next()?.strip_prefix("res:/ui/fonts/")?;
        let rel = fields.next()?;
        let res = res.to_ascii_lowercase();
        let stem = res.rsplit_once('.').map_or(res.as_str(), |(s, _)| s);
        (res == name || stem == name).then_some(rel)
    })
}

/// Candidate `…/CCP/EVE` directories: `NICOTINE_EVE_DIR`, then Steam's
/// Proton prefix for EVE (app 8500) in every Steam library, then a plain
/// Wine prefix.
fn eve_install_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("NICOTINE_EVE_DIR") {
        out.push(PathBuf::from(dir));
    }
    let home = dirs::home_dir().unwrap_or_default();
    let steam_roots = [
        home.join(".local/share/Steam"),
        home.join(".steam/steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
    ];
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in &steam_roots {
        libraries.push(root.clone());
        if let Ok(vdf) = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libraries.extend(steam_library_paths(&vdf));
        }
    }
    for lib in libraries {
        out.push(lib.join("steamapps/compatdata/8500/pfx/drive_c/CCP/EVE"));
    }
    out.push(home.join(".wine/drive_c/CCP/EVE"));

    let mut seen = Vec::new();
    out.retain(|d| {
        let key = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        let keep = d.is_dir() && !seen.contains(&key);
        seen.push(key);
        keep
    });
    out
}

/// `"path"  "/mnt/games/SteamLibrary"` entries from libraryfolders.vdf.
fn steam_library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let mut parts = line.split('"').filter(|p| !p.trim().is_empty());
            if parts.next()? != "path" {
                return None;
            }
            Some(PathBuf::from(parts.next()?.replace("\\\\", "\\")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_is_pixels_per_em() {
        // JetBrains Mono advances are 600/1000 em: 8.4 px at 14 px/em
        // (ab_glyph's PxScale(14) gave 6.36 px).
        let font = TextFont::bundled();
        let mask = rasterize(&font, "MMMMMMMMMM", 14.0, Antialias::Grayscale);
        assert!((84..=86).contains(&mask.width), "width {}", mask.width);
        // Ascent 1.02 em + descent 0.3 em at 14 px.
        assert_eq!(mask.baseline, 15);
        assert_eq!(mask.height, 15 + 5);
    }

    #[test]
    fn grayscale_and_subpixel_masks_have_ink() {
        let font = TextFont::bundled();
        let gray = rasterize(&font, "Ryncke", 14.0, Antialias::Grayscale);
        assert!(!gray.subpixel);
        assert_eq!(gray.data.len(), gray.width * gray.height);
        assert!(gray.data.iter().any(|&a| a > 200), "no solid stems");

        let rgb = rasterize(&font, "Ryncke", 14.0, Antialias::SubpixelRgb);
        assert!(rgb.subpixel);
        assert_eq!(rgb.data.len(), rgb.width * rgb.height * 3);
        // Subpixel coverage differs between channels somewhere.
        assert!(rgb.data.chunks(3).any(|c| c[0] != c[2]));
    }

    #[test]
    fn bgr_swaps_red_and_blue_coverage() {
        let font = TextFont::bundled();
        let rgb = rasterize(&font, "l", 14.0, Antialias::SubpixelRgb);
        let bgr = rasterize(&font, "l", 14.0, Antialias::SubpixelBgr);
        for (a, b) in rgb.data.chunks(3).zip(bgr.data.chunks(3)) {
            assert_eq!((a[0], a[1], a[2]), (b[2], b[1], b[0]));
        }
    }

    #[test]
    fn hinting_puts_horizontal_stems_on_whole_pixels() {
        // The crossbar of "H" should produce at least one fully covered
        // row rather than two half-covered ones.
        let font = TextFont::bundled();
        let mask = rasterize(&font, "H", 14.0, Antialias::Grayscale);
        let row_max: Vec<u8> = (0..mask.height)
            .map(|y| {
                (0..mask.width)
                    .map(|x| mask.coverage(x, y).0)
                    .max()
                    .unwrap()
            })
            .collect();
        assert!(row_max.iter().filter(|&&m| m >= 250).count() >= 8);
    }

    #[test]
    fn blending_is_gamma_correct() {
        // 50% cream-white coverage over black lands well above sRGB 128
        // (linear-light midpoint is ~188).
        let mut canvas = vec![0, 0, 0, 255];
        let mask = TextMask {
            width: 1,
            height: 1,
            baseline: 1,
            subpixel: false,
            data: vec![128],
        };
        blend_mask(&mut canvas, 1, 1, 0, 0, 1, &mask, (255, 255, 255));
        assert!((185..=190).contains(&canvas[0]), "{}", canvas[0]);
        // Full coverage is exactly the foreground; alpha untouched.
        let mut canvas = vec![10, 20, 30, 255];
        let full = TextMask {
            data: vec![255],
            ..mask
        };
        blend_mask(&mut canvas, 1, 1, 0, 0, 1, &full, (252, 250, 242));
        assert_eq!(canvas, vec![242, 250, 252, 255]);
    }

    #[test]
    fn dark_text_on_light_blends_in_srgb() {
        // 50% black over white: sRGB-space midpoint (128), not the much
        // lighter linear-light result (~188) that makes dark text faint.
        let mut canvas = vec![255, 255, 255, 255];
        let mask = TextMask {
            width: 1,
            height: 1,
            baseline: 1,
            subpixel: false,
            data: vec![128],
        };
        blend_mask(&mut canvas, 1, 1, 0, 0, 1, &mask, (0, 0, 0));
        assert!((126..=129).contains(&canvas[0]), "{}", canvas[0]);
    }

    #[test]
    fn finds_eve_fonts_in_resfileindex() {
        let index = "res:/ui/fonts/evesansneue-bold.otf,bc/bcf1_4bff,4bff,1,2\n\
                     res:/ui/fonts/evesansneue-regular.otf,78/7875_fa68,fa68,1,2\n\
                     res:/ui/texture/foo.dds,aa/aa_bb,bb,1,2\n";
        assert_eq!(
            find_eve_font_in_index(index, "evesansneue-regular"),
            Some("78/7875_fa68")
        );
        assert_eq!(
            find_eve_font_in_index(index, "evesansneue-bold.otf"),
            Some("bc/bcf1_4bff")
        );
        assert_eq!(find_eve_font_in_index(index, "foo"), None);
    }

    #[test]
    fn parses_steam_library_folders() {
        let vdf = r#""libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
	}
}"#;
        assert_eq!(
            steam_library_paths(vdf),
            vec![
                PathBuf::from("/home/u/.local/share/Steam"),
                PathBuf::from("/mnt/games/SteamLibrary")
            ]
        );
    }

    #[test]
    fn antialias_setting_parses() {
        assert_eq!(Antialias::from_setting("auto"), Some(None));
        assert_eq!(
            Antialias::from_setting("RGB"),
            Some(Some(Antialias::SubpixelRgb))
        );
        assert_eq!(
            Antialias::from_setting("grayscale"),
            Some(Some(Antialias::Grayscale))
        );
        assert_eq!(Antialias::from_setting("bogus"), None);
    }
}
