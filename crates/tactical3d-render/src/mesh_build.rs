//! Vertex-colored-style mesh building blocks for the pixel/blocky art style.
//!
//! Bevy 0.15's StandardMaterial has no vertex-color support, so instead each
//! mesh carries a small 2D palette texture (8192×N rows); per-vertex UVs
//! point at palette texels. Dynamic recoloring (fog/highlights) = rewriting
//! palette pixels, no mesh/UV rebuild needed.
//!
//! Palette capacity note: the board allocs 2 slots
//! PER HEX, so a 1-D palette texture blew the GPU's 32768-width limit on
//! big provinces (wgpu panic "Dimension X value 50604 exceeds the limit") —
//! hence the 2D layout and u32 slots (u16 wrapped at 65535).

use std::collections::HashMap;

use bevy::asset::RenderAssetUsages;
use bevy::image::{Image, ImageSampler};
use bevy::prelude::*;
use bevy::render::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

/// Palette texture row width (pixels). 8192 is the max texture dimension
/// wgpu guarantees on any backend; rows grow downward instead. The flat
/// slot→pixel mapping is preserved (slot s == pixel s, row-major), so
/// recoloring code can keep indexing `data[slot * 4]`.
const PALETTE_TEX_WIDTH: u32 = 8192;

/// Accumulates quads/triangles into a single palette-textured mesh.
#[derive(Default)]
pub struct MeshBuilder {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    /// Slot → color. Slot count = palette pixel count.
    pub palette: Vec<[f32; 4]>,
    color_to_slot: HashMap<u32, u32>,
}

impl MeshBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Deduplicating slot allocation (models/props: fixed colors).
    pub fn slot_for(&mut self, color: [f32; 4]) -> u32 {
        let key = color_key(color);
        if let Some(&s) = self.color_to_slot.get(&key) {
            return s;
        }
        let s = self.palette.len() as u32;
        self.palette.push(color);
        self.color_to_slot.insert(key, s);
        s
    }

    /// Explicit slot allocation (board: predictable layout for recoloring).
    /// Returns the allocated slot index.
    pub fn alloc_slot(&mut self, color: [f32; 4]) -> u32 {
        let s = self.palette.len() as u32;
        self.palette.push(color);
        s
    }

    fn uv(&self, slot: u32) -> [f32; 2] {
        // Raw slot id; normalized to 2D texture UVs in `build()` once the
        // final palette dimensions are known.
        [slot as f32, 0.5]
    }

    pub fn add_quad(&mut self, a: Vec3, b: Vec3, c: Vec3, d: Vec3, normal: Vec3, color: [f32; 4]) {
        let slot = self.slot_for(color);
        self.add_quad_slot(a, b, c, d, normal, slot);
    }

    pub fn add_quad_slot(&mut self, a: Vec3, b: Vec3, c: Vec3, d: Vec3, normal: Vec3, slot: u32) {
        let base = self.positions.len() as u32;
        let uv = self.uv(slot);
        for p in [a, b, c, d] {
            self.positions.push(p.to_array());
            self.normals.push(normal.to_array());
            self.uvs.push(uv);
        }
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    pub fn add_tri(&mut self, a: Vec3, b: Vec3, c: Vec3, normal: Vec3, color: [f32; 4]) {
        let slot = self.slot_for(color);
        self.add_tri_slot(a, b, c, normal, slot);
    }

    pub fn add_tri_slot(&mut self, a: Vec3, b: Vec3, c: Vec3, normal: Vec3, slot: u32) {
        let base = self.positions.len() as u32;
        let uv = self.uv(slot);
        for p in [a, b, c] {
            self.positions.push(p.to_array());
            self.normals.push(normal.to_array());
            self.uvs.push(uv);
        }
        self.indices.extend_from_slice(&[base, base + 1, base + 2]);
    }

    /// Axis-aligned cuboid with directional shading baked into the palette.
    pub fn add_box(&mut self, min: Vec3, max: Vec3, color: [f32; 4]) {
        let top = self.slot_for(scale_color(color, 1.0));
        let side_ns = self.slot_for(scale_color(color, 0.82));
        let side_ew = self.slot_for(scale_color(color, 0.70));
        let bottom = self.slot_for(scale_color(color, 0.45));
        let (x0, y0, z0) = (min.x, min.y, min.z);
        let (x1, y1, z1) = (max.x, max.y, max.z);
        let v = |x, y, z| Vec3::new(x, y, z);
        self.add_quad_slot(
            v(x0, y1, z0),
            v(x0, y1, z1),
            v(x1, y1, z1),
            v(x1, y1, z0),
            Vec3::Y,
            top,
        );
        self.add_quad_slot(
            v(x0, y0, z1),
            v(x0, y0, z0),
            v(x1, y0, z0),
            v(x1, y0, z1),
            Vec3::NEG_Y,
            bottom,
        );
        self.add_quad_slot(
            v(x0, y0, z1),
            v(x1, y0, z1),
            v(x1, y1, z1),
            v(x0, y1, z1),
            Vec3::Z,
            side_ns,
        );
        self.add_quad_slot(
            v(x1, y0, z0),
            v(x0, y0, z0),
            v(x0, y1, z0),
            v(x1, y1, z0),
            Vec3::NEG_Z,
            side_ns,
        );
        self.add_quad_slot(
            v(x1, y0, z1),
            v(x1, y0, z0),
            v(x1, y1, z0),
            v(x1, y1, z1),
            Vec3::X,
            side_ew,
        );
        self.add_quad_slot(
            v(x0, y0, z0),
            v(x0, y0, z1),
            v(x0, y1, z1),
            v(x0, y1, z0),
            Vec3::NEG_X,
            side_ew,
        );
    }

    pub fn add_box_c(&mut self, center: Vec3, size: Vec3, color: [f32; 4]) {
        let h = size * 0.5;
        self.add_box(center - h, center + h, color);
    }

    /// Rotated cuboid (for angled gun barrels etc.). `rot` is applied to the
    /// box's local axes around its center.
    pub fn add_box_rot(&mut self, center: Vec3, size: Vec3, rot: Quat, color: [f32; 4]) {
        let h = size * 0.5;
        let c = [
            Vec3::new(-h.x, -h.y, -h.z),
            Vec3::new(h.x, -h.y, -h.z),
            Vec3::new(h.x, -h.y, h.z),
            Vec3::new(-h.x, -h.y, h.z),
            Vec3::new(-h.x, h.y, -h.z),
            Vec3::new(h.x, h.y, -h.z),
            Vec3::new(h.x, h.y, h.z),
            Vec3::new(-h.x, h.y, h.z),
        ];
        let p = |i: usize| center + rot * c[i];
        let n = |v: Vec3| rot * v;
        // face corners (CCW from outside, same winding as add_box) + normal + tint
        let faces: [([usize; 4], Vec3, f32); 6] = [
            ([4, 7, 6, 5], Vec3::Y, 1.0),      // top
            ([3, 0, 1, 2], Vec3::NEG_Y, 0.45), // bottom
            ([3, 2, 6, 7], Vec3::Z, 0.82),     // south
            ([1, 0, 4, 5], Vec3::NEG_Z, 0.82), // north
            ([2, 1, 5, 6], Vec3::X, 0.70),     // east
            ([0, 3, 7, 4], Vec3::NEG_X, 0.70), // west
        ];
        for (idx, normal, tint) in faces {
            let slot = self.slot_for(scale_color(color, tint));
            self.add_quad_slot(p(idx[0]), p(idx[1]), p(idx[2]), p(idx[3]), n(normal), slot);
        }
    }

    /// Pointy-top hexagonal prism. Returns (top_slot, side_slot) for recoloring.
    pub fn add_hex_prism(
        &mut self,
        center: Vec3,
        radius: f32,
        y_bottom: f32,
        y_top: f32,
        color: [f32; 4],
    ) -> (u32, u32) {
        let top_slot = self.alloc_slot(color);
        let side_slot = self.alloc_slot(scale_color(color, 0.55));
        self.add_hex_prism_slot(center, radius, y_bottom, y_top, top_slot, side_slot);
        (top_slot, side_slot)
    }

    pub fn add_hex_prism_slot(
        &mut self,
        center: Vec3,
        radius: f32,
        y_bottom: f32,
        y_top: f32,
        top_slot: u32,
        side_slot: u32,
    ) {
        let corners = hex_corners(center, radius, y_top);
        for k in 0..6 {
            let a = corners[k];
            let b = corners[(k + 1) % 6];
            self.add_tri_slot(
                Vec3::new(center.x, y_top, center.z),
                b,
                a,
                Vec3::Y,
                top_slot,
            );
        }
        for k in 0..6 {
            let a_top = corners[k];
            let b_top = corners[(k + 1) % 6];
            let a_bot = Vec3::new(a_top.x, y_bottom, a_top.z);
            let b_bot = Vec3::new(b_top.x, y_bottom, b_top.z);
            let mid = (a_top + b_top) * 0.5 - center;
            let normal = Vec3::new(mid.x, 0.0, mid.z).normalize();
            self.add_quad_slot(a_bot, b_bot, b_top, a_top, normal, side_slot);
        }
    }

    /// Flat hexagon plate (base plates / overlays). Returns the top slot.
    pub fn add_hex_plate(
        &mut self,
        center: Vec3,
        radius: f32,
        y_bottom: f32,
        y_top: f32,
        color: [f32; 4],
    ) -> u32 {
        let (top, _) = self.add_hex_prism(center, radius, y_bottom, y_top, color);
        top
    }

    /// Vertical cylinder (n-gon prism) — unit base plates.
    /// Directional shading is baked in like `add_box` (top bright, sides
    /// shaded, bottom dark).
    pub fn add_cylinder(
        &mut self,
        center: Vec3,
        radius: f32,
        y_bottom: f32,
        y_top: f32,
        color: [f32; 4],
        segments: usize,
    ) {
        let n = segments.max(6);
        let top_slot = self.slot_for(scale_color(color, 1.0));
        let bottom_slot = self.slot_for(scale_color(color, 0.45));
        let ring: Vec<Vec3> = (0..n)
            .map(|k| {
                let a = std::f32::consts::TAU * k as f32 / n as f32;
                Vec3::new(
                    center.x + radius * a.cos(),
                    0.0,
                    center.z + radius * a.sin(),
                )
            })
            .collect();
        // Top + bottom fans.
        for k in 0..n {
            let a = ring[k];
            let b = ring[(k + 1) % n];
            self.add_tri_slot(
                Vec3::new(center.x, y_top, center.z),
                Vec3::new(b.x, y_top, b.z),
                Vec3::new(a.x, y_top, a.z),
                Vec3::Y,
                top_slot,
            );
            self.add_tri_slot(
                Vec3::new(center.x, y_bottom, center.z),
                Vec3::new(a.x, y_bottom, a.z),
                Vec3::new(b.x, y_bottom, b.z),
                Vec3::NEG_Y,
                bottom_slot,
            );
        }
        // Side quads, shaded by facing (sun from +X/+Z reads brightest).
        // The winding MUST be (a_bot → a_top → b_top → b_bot) — the ring
        // runs (cos,sin) with k, so the old (a_bot, b_bot, b_top, a_top)
        // order faced INWARD and was back-face culled from outside: you
        // could see into the base plate from the side.
        // (The hex prism shares the inward convention and stays — the board
        // is double-sided on purpose.)
        for k in 0..n {
            let a = ring[k];
            let b = ring[(k + 1) % n];
            let mid = (a + b) * 0.5 - center;
            let normal = Vec3::new(mid.x, 0.0, mid.z).normalize();
            let shade = 0.62 + 0.20 * (normal.x * 0.7 + normal.z * 0.7).clamp(-1.0, 1.0);
            let slot = self.slot_for(scale_color(color, shade));
            self.add_quad_slot(
                Vec3::new(a.x, y_bottom, a.z),
                Vec3::new(a.x, y_top, a.z),
                Vec3::new(b.x, y_top, b.z),
                Vec3::new(b.x, y_bottom, b.z),
                normal,
                slot,
            );
        }
    }

    /// Build mesh + palette image (nearest sampling, sRGB RGBA8). The
    /// palette is a 2D texture (PALETTE_TEX_WIDTH × ceil(n/width)) instead
    /// of a 1×N strip: a 1-D strip over 32768 px panics wgpu validation
    /// (big provinces alloc 2 slots/hex).
    pub fn build(mut self) -> (Mesh, Image) {
        let n = self.palette.len().max(1) as u32;
        let width = n.min(PALETTE_TEX_WIDTH);
        let height = n.div_ceil(width);
        let mut data = vec![0u8; (width * height * 4) as usize];
        // Slot s stays at flat pixel index s (row-major) — recolors index
        // data[s * 4] regardless of the 2D dimensions.
        for (i, c) in self.palette.iter().enumerate() {
            data[i * 4] = (c[0].clamp(0.0, 1.0) * 255.0).round() as u8;
            data[i * 4 + 1] = (c[1].clamp(0.0, 1.0) * 255.0).round() as u8;
            data[i * 4 + 2] = (c[2].clamp(0.0, 1.0) * 255.0).round() as u8;
            data[i * 4 + 3] = (c[3].clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let mut image = Image::new(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        image.sampler = ImageSampler::nearest();

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        // Normalize raw slot ids to 2D palette texel-centre UVs now the
        // final dimensions are known.
        let (wf, hf) = (width as f32, height as f32);
        for uv in self.uvs.iter_mut() {
            let slot = uv[0] as u32;
            uv[0] = ((slot % width) as f32 + 0.5) / wf;
            uv[1] = ((slot / width) as f32 + 0.5) / hf;
        }
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs);
        mesh.insert_indices(Indices::U32(self.indices));
        (mesh, image)
    }
}

fn color_key(c: [f32; 4]) -> u32 {
    let q = |f: f32| ((f.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    q(c[0]) | (q(c[1]) << 8) | (q(c[2]) << 16) | (q(c[3]) << 24)
}

/// Corner positions of a pointy-top hexagon at height `y`.
pub fn hex_corners(center: Vec3, radius: f32, y: f32) -> [Vec3; 6] {
    let mut out = [Vec3::ZERO; 6];
    for k in 0..6 {
        let angle = (30.0 + 60.0 * k as f32).to_radians();
        out[k] = Vec3::new(
            center.x + radius * angle.cos(),
            y,
            center.z + radius * angle.sin(),
        );
    }
    out
}

/// The two corners of the hex edge that FACES `HexDirection::ALL[d]`
/// (order NE, E, SE, SW, W, NW). Corner k sits at angle 30°+60°k, so the
/// edge between corners k and k+1 faces 60°+60°k; the six neighbor
/// directions sit at 300°, 0°, 60°, …, 240°. Hence the edge facing
/// direction d spans corners (d+4)%6 → (d+5)%6. Using corners d → d+1
/// instead draws the band on the edge rotated 240° away.
pub fn hex_edge(corners: &[Vec3; 6], d: usize) -> (Vec3, Vec3) {
    (corners[(d + 4) % 6], corners[(d + 5) % 6])
}

pub fn scale_color(c: [f32; 4], f: f32) -> [f32; 4] {
    [c[0] * f, c[1] * f, c[2] * f, c[3]]
}

pub fn color3(rgb: [f32; 3]) -> [f32; 4] {
    [rgb[0], rgb[1], rgb[2], 1.0]
}

/// Deterministic 0..1 hash for per-hex color jitter (SplitMix-style).
pub fn hash01(seed: u64) -> f32 {
    let mut z = seed.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    (z >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::render::mesh::VertexAttributeValues;

    /// Read the built UV attribute back as [f32; 2] pairs.
    fn uvs_of(mesh: &Mesh) -> Vec<[f32; 2]> {
        match mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap() {
            VertexAttributeValues::Float32x2(v) => v.clone(),
            other => panic!("unexpected UV format: {other:?}"),
        }
    }

    #[test]
    fn small_palette_stays_one_row_and_centres_texels() {
        let mut mb = MeshBuilder::new();
        mb.add_tri(Vec3::ZERO, Vec3::X, Vec3::Z, Vec3::Y, [1.0, 0.0, 0.0, 1.0]);
        mb.add_tri(Vec3::ZERO, Vec3::X, Vec3::Z, Vec3::Y, [0.0, 1.0, 0.0, 1.0]);
        let (mesh, image) = mb.build();
        assert_eq!(image.texture_descriptor.size.width, 2);
        assert_eq!(image.texture_descriptor.size.height, 1);
        // slot 0 → u 0.25, slot 1 → u 0.75; v centred on the single row.
        let uvs = uvs_of(&mesh);
        assert!((uvs[0][0] - 0.25).abs() < 1e-6, "slot0 u: {}", uvs[0][0]);
        assert!((uvs[0][1] - 0.5).abs() < 1e-6, "slot0 v: {}", uvs[0][1]);
        assert!((uvs[3][0] - 0.75).abs() < 1e-6, "slot1 u: {}", uvs[3][0]);
    }

    /// Regression test for the oversized-palette crash: a board over
    /// 16384 hexes allocs >32768 palette slots — the old 1×N palette
    /// texture blew the GPU width limit and panicked wgpu validation.
    /// Slots must stay addressable (u32, no 65535 wrap) and the palette
    /// must grow DOWNWARD in rows of at most 8192 px.
    #[test]
    fn oversized_palette_wraps_into_rows() {
        const SLOTS: usize = 9000; // > PALETTE_TEX_WIDTH, < u16::MAX
        let mut mb = MeshBuilder::new();
        let mut slots = Vec::new();
        for i in 0..SLOTS {
            slots.push(mb.alloc_slot([i as f32 / SLOTS as f32, 0.0, 0.0, 1.0]));
        }
        // One quad pinned to the LAST slot (the one that overflowed u16/1-D).
        let last = *slots.last().unwrap();
        assert_eq!(last, (SLOTS - 1) as u32, "u32 slot ids, no u16 wrap");
        mb.add_quad_slot(
            Vec3::ZERO,
            Vec3::X,
            Vec3::X + Vec3::Z,
            Vec3::Z,
            Vec3::Y,
            last,
        );
        let (mesh, image) = mb.build();
        let size = image.texture_descriptor.size;
        assert_eq!(size.width, PALETTE_TEX_WIDTH);
        assert_eq!(size.height, 2, "9000 slots → two rows");
        // Flat slot→pixel layout preserved: slot s sits at pixel s.
        assert_eq!(image.data.len(), (PALETTE_TEX_WIDTH * 2 * 4) as usize);
        let px = SLOTS - 1;
        assert_eq!(image.data[px * 4], 255, "last slot color at pixel {px}");
        // UV of the quad points at row 1, column (SLOTS-1) % 8192 = 807.
        let uvs = uvs_of(&mesh);
        let (w, h) = (PALETTE_TEX_WIDTH as f32, 2.0f32);
        let expect_u = (807.0 + 0.5) / w;
        let expect_v = (1.0 + 0.5) / h;
        assert!(
            (uvs[0][0] - expect_u).abs() < 1e-6,
            "u: {} vs {expect_u}",
            uvs[0][0]
        );
        assert!(
            (uvs[0][1] - expect_v).abs() < 1e-6,
            "v: {} vs {expect_v}",
            uvs[0][1]
        );
    }

    /// Beyond-64k slots: the scale big provinces actually reach (2 slots
    /// per hex → a 33k-hex board needs 66k slots; u16 truncated these).
    #[test]
    fn slots_past_u16_max_stay_exact() {
        let mut mb = MeshBuilder::new();
        let mut last = 0;
        for i in 0..70_000u32 {
            last = mb.alloc_slot([i as f32 / 70_000.0, 0.0, 0.0, 1.0]);
        }
        assert_eq!(last, 69_999);
        let (_, image) = mb.build();
        let size = image.texture_descriptor.size;
        assert_eq!(size.width, PALETTE_TEX_WIDTH);
        assert_eq!(size.height, 9, "70000/8192 → 9 rows");
        assert_eq!(image.data[69_999 * 4], 255);
    }
}
