//! Central theme: quiet dark colors, component theme, and treemap palette.
//! Views use these constants and functions instead of defining their own colors.
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use acre_model::render::Palette;

pub const PRODUCT_NAME: &str = "Acre";

/// Page background.
pub const BG: u32 = 0x101113;
/// Sidebar, title bar, and status bar.
pub const NAV: u32 = 0x0c0d0f;
/// Control surfaces and skeletons.
pub const SURFACE: u32 = 0x191b1e;
pub const HOVER: u32 = 0x24262a;
pub const PRESS: u32 = 0x303238;
pub const POPOVER: u32 = 0x1c1e22;
pub const TEXT: u32 = 0xededee;
pub const MUTED: u32 = 0x90939b;
pub const DIM: u32 = 0x71757e;
pub const FAINT: u32 = 0x4d5057;
pub const ACCENT: u32 = 0xe1e2e5;
pub const ACCENT_HOVER: u32 = 0xf4f4f5;
pub const ACCENT_PRESS: u32 = 0xc9cbd0;
pub const ON_ACCENT: u32 = 0x191a1d;
pub const ERR: u32 = 0xe0736c;

/// Treemap categories: eight fixed dark visualization colors, validated against #101113 for
/// luminance, minimum chroma, adjacent color-blind separation (delta E >= 8.4), normal-vision
/// separation (delta E >= 19.3), and contrast >= 3:1. Other is neutral gray. Order matches Category.
pub const CATEGORY_RGB: [u32; 9] = [
    0x3987e5, // Programs and libraries
    0xd95926, // Video
    0x199e70, // Images
    0xc98500, // Archives and disk images
    0xd55181, // Audio
    0x008300, // Documents
    0x9085e9, // Code
    0xe66767, // System and data
    0x6b6e75, // Other
];
/// Directory leaves and aggregated small items.
pub const DIR_LEAF_RGB: u32 = 0x4a4d54;
pub const REST_RGB: u32 = 0x35373c;

fn rgb3(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

pub fn palette() -> Palette {
    let mut categories = [[0u8; 3]; 9];
    for (i, c) in CATEGORY_RGB.iter().enumerate() {
        categories[i] = rgb3(*c);
    }
    Palette { categories, dir_leaf: rgb3(DIR_LEAF_RGB), rest: rgb3(REST_RGB) }
}

pub fn c(rgb_: u32) -> Hsla {
    rgb(rgb_).into()
}
fn white_a(alpha: f32) -> Hsla {
    c(0xffffff).opacity(alpha)
}
fn accent_a(alpha: f32) -> Hsla {
    c(ACCENT).opacity(alpha)
}
pub fn border() -> Hsla {
    white_a(0.08)
}
pub fn row_alt() -> Hsla {
    white_a(0.017)
}
pub fn row_hover() -> Hsla {
    white_a(0.04)
}
pub fn row_selected() -> Hsla {
    accent_a(0.115)
}
pub fn bar_track() -> Hsla {
    white_a(0.06)
}
pub fn bar_fill() -> Hsla {
    accent_a(0.38)
}
pub fn focus_ring() -> Hsla {
    white_a(0.8)
}
/// Treemap hover outline and the thin outline of its containing folder.
pub fn tm_hover() -> Hsla {
    c(ACCENT)
}
pub fn tm_hover_parent() -> Hsla {
    white_a(0.4)
}
/// Treemap selection: white outline, light fill, and dark inner line.
pub fn tm_selected() -> Hsla {
    c(ACCENT)
}
pub fn tm_selected_fill() -> Hsla {
    white_a(0.16)
}
pub fn tm_ring() -> Hsla {
    c(0x000000).opacity(0.6)
}
/// Treemap labels: dark background and secondary size text; folder header backgrounds are rendered into the bitmap.
pub fn label_bg() -> Hsla {
    c(0x000000).opacity(0.58)
}
pub fn label_dim() -> Hsla {
    white_a(0.68)
}
pub const TM_HEADER_RGB: u32 = HOVER;
pub fn header_rgb() -> [u8; 3] {
    rgb3(TM_HEADER_RGB)
}

pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |t| {
        t.font_family = "Noto Sans SC".into();
        t.font_size = px(13.);
        t.radius = px(6.);
        t.radius_lg = px(8.);
        let p = &mut t.colors;
        p.background = c(BG);
        p.foreground = c(TEXT);
        p.muted = c(SURFACE);
        p.muted_foreground = c(MUTED);
        p.secondary = c(SURFACE);
        p.secondary_hover = c(HOVER);
        p.secondary_active = c(PRESS);
        p.secondary_foreground = c(TEXT);
        p.primary = c(ACCENT);
        p.primary_hover = c(ACCENT_HOVER);
        p.primary_active = c(ACCENT_PRESS);
        p.primary_foreground = c(ON_ACCENT);
        p.ring = focus_ring();
        p.border = border();
        p.input = border();
        p.popover = c(POPOVER);
        p.popover_foreground = c(TEXT);
        p.accent = c(HOVER);
        p.accent_foreground = c(TEXT);
        p.list_hover = c(HOVER);
        p.caret = c(TEXT);
        p.selection = accent_a(0.25);
        p.scrollbar = transparent_black();
        p.scrollbar_thumb = white_a(0.18);
        p.scrollbar_thumb_hover = white_a(0.30);
        p.title_bar = c(NAV);
        p.title_bar_border = border();
        p.tab_active_foreground = c(TEXT);
        p.skeleton = c(SURFACE);
        p.danger = c(ERR);
    });
}
