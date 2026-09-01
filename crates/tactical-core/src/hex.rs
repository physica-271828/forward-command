//! Hex grid math — axial coordinates, pointy-top hexes (DESIGN §4).
//!
//! World mapping targets the 3D renderer: hexes lie on the XZ plane,
//! x grows "east", z grows "south".

use std::fmt;

/// Axial coordinate (q = column, r = row), pointy-top layout.
/// Default = (0,0), same as [`HexCoord::ZERO`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HexCoord {
    pub q: i32,
    pub r: i32,
}

/// Cube coordinate for rotation/lerp algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CubeCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// The 6 neighbor directions of a pointy-top hex.
/// Names follow compass bearings as seen from above (north = -z).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexDirection {
    NE,
    E,
    SE,
    SW,
    W,
    NW,
}

impl HexDirection {
    pub const ALL: [HexDirection; 6] = [
        HexDirection::NE,
        HexDirection::E,
        HexDirection::SE,
        HexDirection::SW,
        HexDirection::W,
        HexDirection::NW,
    ];

    /// Axial offset for pointy-top axial coordinates.
    pub fn offset(self) -> (i32, i32) {
        match self {
            HexDirection::NE => (1, -1),
            HexDirection::E => (1, 0),
            HexDirection::SE => (0, 1),
            HexDirection::SW => (-1, 1),
            HexDirection::W => (-1, 0),
            HexDirection::NW => (0, -1),
        }
    }

    pub fn opposite(self) -> HexDirection {
        match self {
            HexDirection::NE => HexDirection::SW,
            HexDirection::E => HexDirection::W,
            HexDirection::SE => HexDirection::NW,
            HexDirection::SW => HexDirection::NE,
            HexDirection::W => HexDirection::E,
            HexDirection::NW => HexDirection::SE,
        }
    }

    /// Bit convention for `GridCell::river_edges` (§4.2 step 8): bit `i`
    /// corresponds to `HexDirection::ALL[i]` — NE 0x01 … NW 0x20. Canonical
    /// home is tactical-core so pathfinding can test crossed edges;
    /// tactical-map's `river_bit` delegates here.
    pub fn bit(self) -> u8 {
        match self {
            HexDirection::NE => 0x01,
            HexDirection::E => 0x02,
            HexDirection::SE => 0x04,
            HexDirection::SW => 0x08,
            HexDirection::W => 0x10,
            HexDirection::NW => 0x20,
        }
    }

    /// Parse HOI4-style attack-direction tokens ("N", "NE", ... "NW").
    pub fn from_token(s: &str) -> Option<HexDirection> {
        match s.to_ascii_uppercase().as_str() {
            "NE" => Some(HexDirection::NE),
            "E" => Some(HexDirection::E),
            "SE" => Some(HexDirection::SE),
            "SW" => Some(HexDirection::SW),
            "W" => Some(HexDirection::W),
            "NW" => Some(HexDirection::NW),
            // Map N/S onto the two closest bearings (pointy-top has no N/S edge).
            "N" => Some(HexDirection::NE),
            "S" => Some(HexDirection::SW),
            _ => None,
        }
    }
}

impl fmt::Display for HexDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HexDirection::NE => "NE",
            HexDirection::E => "E",
            HexDirection::SE => "SE",
            HexDirection::SW => "SW",
            HexDirection::W => "W",
            HexDirection::NW => "NW",
        };
        f.write_str(s)
    }
}

impl HexCoord {
    pub const ZERO: HexCoord = HexCoord { q: 0, r: 0 };

    pub const fn new(q: i32, r: i32) -> Self {
        HexCoord { q, r }
    }

    pub fn to_cube(self) -> CubeCoord {
        let x = self.q;
        let z = self.r;
        CubeCoord { x, y: -x - z, z }
    }

    pub fn from_cube(c: CubeCoord) -> HexCoord {
        debug_assert_eq!(c.x + c.y + c.z, 0);
        HexCoord { q: c.x, r: c.z }
    }

    pub fn neighbor(self, dir: HexDirection) -> HexCoord {
        let (dq, dr) = dir.offset();
        HexCoord {
            q: self.q + dq,
            r: self.r + dr,
        }
    }

    pub fn neighbors(self) -> [HexCoord; 6] {
        HexDirection::ALL.map(|d| self.neighbor(d))
    }

    /// Direction to an adjacent hex, if `other` is one of the 6 neighbours.
    pub fn direction_to(self, other: HexCoord) -> Option<HexDirection> {
        HexDirection::ALL
            .into_iter()
            .find(|d| self.neighbor(*d) == other)
    }

    pub fn distance(self, other: HexCoord) -> i32 {
        let a = self.to_cube();
        let b = other.to_cube();
        ((a.x - b.x).abs() + (a.y - b.y).abs() + (a.z - b.z).abs()) / 2
    }

    /// World position on the XZ plane for pointy-top hexes with the given
    /// outer radius `size` (center to corner, in world units = 1 km hexes).
    pub fn to_world(self, size: f32) -> (f32, f32) {
        let x = size * 3f32.sqrt() * (self.q as f32 + self.r as f32 * 0.5);
        let z = size * 1.5 * self.r as f32;
        (x, z)
    }

    /// Inverse of `to_world`: pick the hex containing world point (x, z).
    pub fn from_world(x: f32, z: f32, size: f32) -> HexCoord {
        let qf = (3f32.sqrt() / 3.0 * x - 1.0 / 3.0 * z) / size;
        let rf = (2.0 / 3.0 * z) / size;
        cube_round(qf, rf)
    }

    /// Straight hex line from self to target (cube lerp + round).
    pub fn line_to(self, target: HexCoord) -> Vec<HexCoord> {
        let n = self.distance(target);
        if n == 0 {
            return vec![self];
        }
        let a = self.to_cube();
        let b = target.to_cube();
        let nf = n as f32;
        (0..=n)
            .map(|i| {
                let t = i as f32 / nf;
                let fx = lerp(a.x as f32, b.x as f32, t);
                let fz = lerp(a.z as f32, b.z as f32, t);
                cube_round(fx, fz)
            })
            .collect()
    }

    /// The axial bounding rectangle between two corners (used for sector
    /// deployment): every hex with q ∈ [q₁,q₂] ∧ r ∈ [r₁,r₂]. Corner order
    /// is irrelevant; empty only when the corners coincide... no — a single
    /// hex is still one hex, so it is never empty.
    pub fn rect_between(self, other: HexCoord) -> Vec<HexCoord> {
        let (q0, q1) = (self.q.min(other.q), self.q.max(other.q));
        let (r0, r1) = (self.r.min(other.r), self.r.max(other.r));
        let mut out = Vec::with_capacity(((q1 - q0 + 1) * (r1 - r0 + 1)) as usize);
        for q in q0..=q1 {
            for r in r0..=r1 {
                out.push(HexCoord::new(q, r));
            }
        }
        out
    }

    /// All hexes within `radius` (inclusive), unclipped.
    pub fn hexes_in_range(self, radius: i32) -> Vec<HexCoord> {
        let mut out = Vec::new();
        for dq in -radius..=radius {
            let lo = (-radius).max(-dq - radius);
            let hi = radius.min(-dq + radius);
            for dr in lo..=hi {
                out.push(HexCoord {
                    q: self.q + dq,
                    r: self.r + dr,
                });
            }
        }
        out
    }

    /// Ring of hexes at exactly `radius`. Radius 0 yields self.
    pub fn ring(self, radius: i32) -> Vec<HexCoord> {
        if radius == 0 {
            return vec![self];
        }
        let mut out = Vec::with_capacity(6 * radius as usize);
        // Start at the W corner of the ring, walk the 6 edges clockwise.
        let mut cur = HexCoord {
            q: self.q - radius,
            r: self.r,
        };
        for dir in HexDirection::ALL {
            for _ in 0..radius {
                out.push(cur);
                cur = cur.neighbor(dir);
            }
        }
        out
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Round fractional axial coords to the nearest hex.
#[allow(unused_assignments)]
fn cube_round(qf: f32, rf: f32) -> HexCoord {
    let xf = qf;
    let zf = rf;
    let yf = -xf - zf;
    let (mut rx, mut ry, mut rz) = (xf.round(), yf.round(), zf.round());
    let dx = (rx - xf).abs();
    let dy = (ry - yf).abs();
    let dz = (rz - zf).abs();
    if dx > dy && dx > dz {
        rx = -ry - rz;
    } else if dy > dz {
        ry = -rx - rz;
    } else {
        rz = -rx - ry;
    }
    HexCoord {
        q: rx as i32,
        r: rz as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_symmetric() {
        let a = HexCoord::new(2, 3);
        let b = HexCoord::new(-1, 5);
        assert_eq!(a.distance(b), b.distance(a));
        assert_eq!(a.distance(a), 0);
    }

    #[test]
    fn neighbor_distance_one() {
        let a = HexCoord::new(4, 4);
        for n in a.neighbors() {
            assert_eq!(a.distance(n), 1);
        }
    }

    #[test]
    fn opposite_direction_returns_start() {
        let a = HexCoord::new(1, 2);
        for d in HexDirection::ALL {
            assert_eq!(a.neighbor(d).neighbor(d.opposite()), a);
        }
    }

    #[test]
    fn world_roundtrip() {
        for (q, r) in [(0, 0), (3, 5), (-2, 7), (10, -4)] {
            let h = HexCoord::new(q, r);
            let (x, z) = h.to_world(1.0);
            assert_eq!(HexCoord::from_world(x, z, 1.0), h);
        }
    }

    #[test]
    fn line_is_contiguous() {
        let a = HexCoord::new(0, 0);
        let b = HexCoord::new(5, -3);
        let line = a.line_to(b);
        assert_eq!(line.len() as i32, a.distance(b) + 1);
        assert_eq!(*line.first().unwrap(), a);
        assert_eq!(*line.last().unwrap(), b);
        for w in line.windows(2) {
            assert_eq!(w[0].distance(w[1]), 1);
        }
    }

    #[test]
    fn range_counts() {
        let c = HexCoord::new(0, 0);
        assert_eq!(c.hexes_in_range(0).len(), 1);
        assert_eq!(c.hexes_in_range(1).len(), 7);
        assert_eq!(c.hexes_in_range(2).len(), 19);
        assert_eq!(c.ring(1).len(), 6);
        assert_eq!(c.ring(3).len(), 18);
    }

    #[test]
    fn direction_tokens() {
        assert_eq!(HexDirection::from_token("NW"), Some(HexDirection::NW));
        assert_eq!(HexDirection::from_token("w"), Some(HexDirection::W));
        assert_eq!(HexDirection::from_token("XX"), None);
    }

    /// Sector deployment: the axial bounding rectangle — corner
    /// order irrelevant, every q/r combination inside the bounds included.
    #[test]
    fn rect_between_spans_the_bounding_box() {
        let a = HexCoord::new(3, 1);
        let b = HexCoord::new(7, 4);
        let r1 = a.rect_between(b);
        let r2 = b.rect_between(a);
        assert_eq!(r1.len(), 5 * 4);
        assert_eq!(r1, r2, "corner order must not matter");
        assert!(r1.contains(&HexCoord::new(3, 1)));
        assert!(r1.contains(&HexCoord::new(7, 4)));
        assert!(r1.contains(&HexCoord::new(5, 2)));
        assert!(!r1.contains(&HexCoord::new(2, 1)));
        assert!(!r1.contains(&HexCoord::new(7, 5)));
        // Degenerate: a single hex is still a one-hex rectangle.
        let s = HexCoord::new(0, 0);
        assert_eq!(s.rect_between(s), vec![s]);
    }
}
