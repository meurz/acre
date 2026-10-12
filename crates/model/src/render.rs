//! Render the layout to a BGRA bitmap for direct display by the UI.
//!
//! Cushion shading follows van Wijk and van de Wetering, "Cushion Treemaps" (InfoVis 1999):
//! each level adds a parabolic ridge; accumulated coefficients determine each pixel's normal and diffuse light.
//! Draw only leaves and folder headers, writing each pixel once; parallelize by horizontal bands.

use crate::layout::{KIND_DIR_LEAF, KIND_REST, Layout};

#[derive(Clone, Copy, Debug)]
pub struct RenderParams {
    pub cushion: bool,
    /// Ambient and diffuse light intensity.
    pub ambient: f32,
    pub diffuse: f32,
    /// Light direction (toward the light source).
    pub light: [f32; 3],
    /// Background color (RGB channels used when writing BGRA).
    pub background: [u8; 3],
    /// Folder header background; the UI overlays its name.
    pub header: [u8; 3],
}

impl Default for RenderParams {
    fn default() -> Self {
        let (lx, ly, lz) = (-1.0f32, -2.0f32, 10.0f32);
        let n = (lx * lx + ly * ly + lz * lz).sqrt();
        Self {
            cushion: true,
            ambient: 0.32,
            diffuse: 0.78,
            light: [lx / n, ly / n, lz / n],
            background: [0x10, 0x11, 0x13],
            header: [0x24, 0x26, 0x2a],
        }
    }
}

/// Palette: slots 0-8 match [`crate::ext::Category`], plus directory leaves and aggregated items.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub categories: [[u8; 3]; 9],
    pub dir_leaf: [u8; 3],
    pub rest: [u8; 3],
}

/// Render into `out` (`w*h*4` bytes, BGRA).
pub fn render(layout: &Layout, palette: &Palette, params: &RenderParams, out: &mut [u8], w: usize, h: usize, threads: usize) {
    assert_eq!(out.len(), w * h * 4);
    // Fill the background first.
    let bg = params.background;
    for px in out.chunks_exact_mut(4) {
        px[0] = bg[2];
        px[1] = bg[1];
        px[2] = bg[0];
        px[3] = 255;
    }
    let leaves: Vec<usize> = (0..layout.rects.len()).filter(|&i| layout.rects[i].is_leaf()).collect();
    let headers: Vec<usize> = (0..layout.rects.len()).filter(|&i| layout.rects[i].header > 0.0).collect();
    let threads = threads.clamp(1, 32).min(h.max(1));
    let band = h.div_ceil(threads);
    std::thread::scope(|scope| {
        for (bi, chunk) in out.chunks_mut(band * w * 4).enumerate() {
            let y0 = bi * band;
            let y1 = (y0 + band).min(h);
            let leaves = &leaves;
            let headers = &headers;
            scope.spawn(move || {
                for &i in headers {
                    paint_header(layout, i, params, chunk, w, y0, y1);
                }
                for &i in leaves {
                    paint_rect(layout, i, palette, params, chunk, w, y0, y1);
                }
            });
        }
    });
}

#[inline]
fn span(a: f32, len: f32, limit: usize) -> (usize, usize) {
    // Assign pixels whose centers fall in [a, a+len): adjacent blocks have neither overlaps nor gaps.
    let s = (a - 0.5).ceil().max(0.0) as usize;
    let e = ((a + len - 0.5).ceil().max(0.0) as usize).min(limit);
    (s.min(limit), e)
}

/// Folder header: a solid strip above the non-overlapping child blocks.
fn paint_header(layout: &Layout, i: usize, p: &RenderParams, band: &mut [u8], w: usize, y0: usize, y1: usize) {
    let r = &layout.rects[i];
    let (xs, xe) = span(r.x, r.w, w);
    let (ys, ye) = span(r.y, r.header, y0.max(y1));
    let ys = ys.max(y0);
    let ye = ye.min(y1);
    let [c0, c1, c2] = p.header;
    for y in ys..ye {
        let row = &mut band[(y - y0) * w * 4..(y - y0 + 1) * w * 4];
        for x in xs..xe {
            let o = x * 4;
            row[o] = c2;
            row[o + 1] = c1;
            row[o + 2] = c0;
            row[o + 3] = 255;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_rect(
    layout: &Layout,
    i: usize,
    palette: &Palette,
    p: &RenderParams,
    band: &mut [u8],
    w: usize,
    y0: usize,
    y1: usize,
) {
    let r = &layout.rects[i];
    let (xs, xe) = span(r.x, r.w, w);
    let (ys, ye) = span(r.y, r.h, y0.max(y1));
    let ys = ys.max(y0);
    let ye = ye.min(y1);
    if xs >= xe || ys >= ye {
        return;
    }
    // Darken the dominant category for directory leaves and aggregated items; use neutral gray if unknown.
    let (base, k) = match r.kind {
        KIND_DIR_LEAF if r.cat as usize != 8 => (palette.categories[r.cat as usize % 9], 0.72),
        KIND_REST if r.cat as usize != 8 => (palette.categories[r.cat as usize % 9], 0.55),
        KIND_DIR_LEAF => (palette.dir_leaf, 1.0),
        KIND_REST => (palette.rest, 1.0),
        _ => (palette.categories[r.cat as usize % 9], 1.0),
    };
    let (b0, b1, b2) = (base[0] as f32 * k, base[1] as f32 * k, base[2] as f32 * k);
    if !p.cushion {
        // Flat color: leave a one-pixel dark edge on the right and bottom when the block is large enough.
        let edge = xe - xs >= 4 && ye - ys >= 4;
        for y in ys..ye {
            let row = &mut band[(y - y0) * w * 4..(y - y0 + 1) * w * 4];
            for x in xs..xe {
                let dark = edge && (x == xe - 1 || y == ye - 1);
                let k = if dark { 0.45 } else { 1.0 };
                let o = x * 4;
                row[o] = (b2 * k) as u8;
                row[o + 1] = (b1 * k) as u8;
                row[o + 2] = (b0 * k) as u8;
                row[o + 3] = 255;
            }
        }
        return;
    }
    let s = layout.surf[i];
    let [lx, ly, lz] = p.light;
    for y in ys..ye {
        let fy = y as f32 + 0.5;
        let ny = -(2.0 * s[3] * fy + s[2]);
        let row = &mut band[(y - y0) * w * 4..(y - y0 + 1) * w * 4];
        for x in xs..xe {
            let fx = x as f32 + 0.5;
            let nx = -(2.0 * s[1] * fx + s[0]);
            let cosa = (nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt();
            let v = (p.ambient + p.diffuse * cosa.max(0.0)).min(1.25);
            let o = x * 4;
            row[o] = (b2 * v).min(255.0) as u8;
            row[o + 1] = (b1 * v).min(255.0) as u8;
            row[o + 2] = (b0 * v).min(255.0) as u8;
            row[o + 3] = 255;
        }
    }
}
