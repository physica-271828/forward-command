//! Deterministic smooth value noise on the hex lattice.
//!
//! A pure function of `(seed, q, r)` — no state, no external crates — so
//! headless runs, the AI and the renderer all see the same terrain field.
//! The elevation pass in `tactical-map` samples this field once and stores
//! per-hex elevation in the grid; LOS and rendering then only read the
//! stored value (single source of truth, DESIGN §4.2 step 8 / §6.6).

/// SplitMix64-style hash of (seed, lattice coordinate) → [0, 1).
/// Independent hash domain from `hash01` in tactical-map so the elevation
/// field never correlates with the terrain-variation/village rolls.
fn lattice_hash(seed: u64, q: i32, r: i32) -> f32 {
    let mut h = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((q as u32 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .wrapping_add((r as u32 as u64).wrapping_mul(0x1656_67B1_9E37_79F9))
        .wrapping_add(0xA2FD_02E7_BF9B_9E41);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    ((h >> 40) as f32) / ((1u64 << 24) as f32)
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// One octave of value noise in [-1, 1]: hash corners of the `lattice`-sized
/// grid cell around (q, r), smoothstep-interpolate. `lattice ≥ 2`.
pub fn value_noise(seed: u64, q: i32, r: i32, lattice: i32) -> f32 {
    debug_assert!(lattice >= 2);
    let q0 = q.div_euclid(lattice);
    let r0 = r.div_euclid(lattice);
    let fx = smoothstep(q.rem_euclid(lattice) as f32 / lattice as f32);
    let fy = smoothstep(r.rem_euclid(lattice) as f32 / lattice as f32);
    let v00 = lattice_hash(seed, q0, r0);
    let v01 = lattice_hash(seed, q0, r0 + 1);
    let v10 = lattice_hash(seed, q0 + 1, r0);
    let v11 = lattice_hash(seed, q0 + 1, r0 + 1);
    let a = v00 + (v10 - v00) * fx;
    let b = v01 + (v11 - v01) * fx;
    let v = a + (b - a) * fy;
    2.0 * v - 1.0
}

/// Combined elevation field in [-1, 1]: base octave at ~8-hex wavelength
/// plus a half-amplitude detail octave at ~4 hexes — peaks, saddles and
/// valley floors inside one mountain range. The two octaves
/// use independent hash domains (distinct seed xors).
pub fn elevation_noise(seed: u64, q: i32, r: i32) -> f32 {
    0.75 * value_noise(seed ^ 0xE1E2_E1E2, q, r, 8)
        + 0.25 * value_noise(seed ^ 0xDE7A_5F3C, q, r, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        for seed in [0u64, 1, 7, 42, 0xDEAD_BEEF] {
            for q in -20..20 {
                for r in -20..20 {
                    let a = elevation_noise(seed, q, r);
                    let b = elevation_noise(seed, q, r);
                    assert_eq!(a, b, "seed {seed} ({q},{r})");
                }
            }
        }
    }

    #[test]
    fn different_seeds_differ() {
        let mut same = 0;
        for q in 0..100 {
            for r in 0..100 {
                if (elevation_noise(3, q, r) - elevation_noise(4, q, r)).abs() < 1e-6 {
                    same += 1;
                }
            }
        }
        assert!(same < 100, "fields must not coincide ({same}/10000)");
    }

    #[test]
    fn bounded_in_minus_one_one() {
        for seed in 0..8u64 {
            for q in 0..200 {
                for r in 0..200 {
                    let v = elevation_noise(seed, q, r);
                    assert!((-1.0..=1.0).contains(&v), "seed {seed} ({q},{r}) = {v}");
                }
            }
        }
    }

    #[test]
    fn smooth_across_adjacent_hexes() {
        // The ~8-hex lattice with smoothstep caps per-cell slope well below
        // 0.5 even on the combined two-octave field — a ridge never pops.
        for seed in 0..8u64 {
            for q in -30..30 {
                for r in -30..30 {
                    for (nq, nr) in [(q + 1, r), (q, r + 1)] {
                        let d = (elevation_noise(seed, q, r) - elevation_noise(seed, nq, nr)).abs();
                        assert!(d < 0.5, "seed {seed} ({q},{r})→({nq},{nr}) jump {d}");
                    }
                }
            }
        }
    }

    #[test]
    fn corners_anchor_and_boundaries_are_continuous() {
        let seed = 9u64;
        // At a lattice corner the field equals the corner hash exactly.
        assert_eq!(
            value_noise(seed, 0, 0, 8),
            2.0 * lattice_hash(seed, 0, 0) - 1.0
        );
        // Crossing a cell boundary is continuous: the left cell's right edge
        // interpolates toward the next corner; both sides nearly agree
        // (smoothstep 0.957 vs 1.0 leaves <= ~5% of one corner range).
        let left = value_noise(seed, 7, 0, 8);
        let right = value_noise(seed, 8, 0, 8);
        assert!(
            (left - right).abs() < 0.15,
            "boundary jump {left} -> {right}"
        );
    }
}
