//! Hand-rolled parser for HOI4 `map/provinces.bmp` (DESIGN.md §4.1): a 24-bit
//! uncompressed Windows bitmap (~5632×2048 px, 13k+ provinces).
//!
//! Pixels are stored as packed RGB keys (`0xRRGGBB`, row-major, y = 0 at the
//! top/north edge of the image). The color → province-id resolution happens
//! later in [`crate::MapGenerator`], because the palette is defined by
//! `definition.csv`, not by the bitmap itself.

use std::path::Path;

use crate::{MapError, Result};

/// Parsed `provinces.bmp`: dimensions plus one packed color per pixel.
#[derive(Debug, Clone)]
pub struct ProvinceMap {
    pub width: u32,
    pub height: u32,
    /// Packed `0xRRGGBB` per pixel, row-major starting at the top (north) row.
    colors: Vec<u32>,
}

fn read_u16_le(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn read_u32_le(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn read_i32_le(data: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

impl ProvinceMap {
    /// Load a 24-bit uncompressed BMP (HOI4 `provinces.bmp`). Both bottom-up
    /// (positive height, the HOI4 format) and top-down (negative height)
    /// row orders are supported. Malformed files yield descriptive errors.
    pub fn load_bmp(path: &Path) -> Result<ProvinceMap> {
        let data = std::fs::read(path).map_err(|e| MapError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;

        if data.len() < 54 {
            return Err(MapError::InvalidBmp(format!(
                "file is {} bytes, smaller than the 54-byte header",
                data.len()
            )));
        }
        if &data[0..2] != b"BM" {
            return Err(MapError::InvalidBmp("missing 'BM' magic bytes".into()));
        }
        let pixel_offset = read_u32_le(&data, 10) as usize;
        let width = read_i32_le(&data, 18);
        let height = read_i32_le(&data, 22);
        let planes = read_u16_le(&data, 26);
        let bpp = read_u16_le(&data, 28);
        let compression = read_u32_le(&data, 30);

        if width <= 0 || height == 0 {
            return Err(MapError::InvalidBmp(format!(
                "bad dimensions {width}x{height}"
            )));
        }
        if planes != 1 {
            return Err(MapError::InvalidBmp(format!(
                "unexpected plane count {planes} (expected 1)"
            )));
        }
        if bpp != 24 {
            return Err(MapError::InvalidBmp(format!(
                "unsupported bit depth {bpp} (expected 24-bit)"
            )));
        }
        if compression != 0 {
            return Err(MapError::InvalidBmp(format!(
                "unsupported compression {compression} (expected 0 = BI_RGB uncompressed)"
            )));
        }

        let w = width as usize;
        let h = height.unsigned_abs() as usize;
        // BMP rows are padded to a 4-byte boundary.
        let row_size = (w * 3 + 3) & !3;
        let fits = pixel_offset
            .checked_add(row_size.saturating_mul(h))
            .is_some_and(|end| end <= data.len());
        if !fits {
            return Err(MapError::InvalidBmp(format!(
                "pixel data ({row_size} B/row × {h} rows from offset {pixel_offset}) exceeds file size {}",
                data.len()
            )));
        }

        let top_down = height < 0;
        let mut colors = vec![0u32; w * h];
        for y in 0..h {
            // y = 0 is the top (north) row; bottom-up files store it last.
            let src_row = if top_down { y } else { h - 1 - y };
            let row_start = pixel_offset + src_row * row_size;
            for x in 0..w {
                let o = row_start + x * 3;
                let (b, g, r) = (data[o], data[o + 1], data[o + 2]);
                colors[y * w + x] = ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
            }
        }

        Ok(ProvinceMap {
            width: w as u32,
            height: h as u32,
            colors,
        })
    }

    /// Build a map from raw packed colors (tests, tooling). Returns an error
    /// if the buffer size does not match `width × height`.
    pub fn from_colors(width: u32, height: u32, colors: Vec<u32>) -> Result<ProvinceMap> {
        let expected = width as usize * height as usize;
        if colors.len() != expected {
            return Err(MapError::InvalidBmp(format!(
                "from_colors: got {} pixels for {width}x{height} (expected {expected})",
                colors.len()
            )));
        }
        Ok(ProvinceMap {
            width,
            height,
            colors,
        })
    }

    /// Packed `0xRRGGBB` color of pixel (x, y), or `None` if out of bounds.
    pub fn color_at(&self, x: u32, y: u32) -> Option<u32> {
        if x < self.width && y < self.height {
            Some(self.colors[(y * self.width + x) as usize])
        } else {
            None
        }
    }

    /// Raw packed-color buffer, row-major from the top row. Public for bulk
    /// consumers (the bin crate's pick-map builder scans the whole bitmap).
    pub fn colors(&self) -> &[u32] {
        &self.colors
    }
}

/// Parsed 8-bit indexed BMP (HOI4 `rivers.bmp`): one palette index per
/// pixel, row-major from the top (north) row — the same orientation
/// convention as [`ProvinceMap`]. Palette RGBs are NOT decoded:
/// HOI4 assigns meaning to the index values themselves (rivers.bmp: indices
/// < 254 are river strokes/markers, 254/255 are sea/land background).
#[derive(Debug, Clone)]
pub struct IndexMap {
    pub width: u32,
    pub height: u32,
    indices: Vec<u8>,
}

impl IndexMap {
    /// Load an 8-bit uncompressed indexed BMP (`rivers.bmp`). Both row
    /// orders supported, same as [`ProvinceMap::load_bmp`].
    pub fn load_indexed_bmp(path: &Path) -> Result<IndexMap> {
        let data = std::fs::read(path).map_err(|e| MapError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        if data.len() < 54 {
            return Err(MapError::InvalidBmp(format!(
                "file is {} bytes, smaller than the 54-byte header",
                data.len()
            )));
        }
        if &data[0..2] != b"BM" {
            return Err(MapError::InvalidBmp("missing 'BM' magic bytes".into()));
        }
        let pixel_offset = read_u32_le(&data, 10) as usize;
        let width = read_i32_le(&data, 18);
        let height = read_i32_le(&data, 22);
        let planes = read_u16_le(&data, 26);
        let bpp = read_u16_le(&data, 28);
        let compression = read_u32_le(&data, 30);
        if width <= 0 || height == 0 {
            return Err(MapError::InvalidBmp(format!(
                "bad dimensions {width}x{height}"
            )));
        }
        if planes != 1 {
            return Err(MapError::InvalidBmp(format!(
                "unexpected plane count {planes} (expected 1)"
            )));
        }
        if bpp != 8 {
            return Err(MapError::InvalidBmp(format!(
                "unsupported bit depth {bpp} (expected 8-bit indexed)"
            )));
        }
        if compression != 0 {
            return Err(MapError::InvalidBmp(format!(
                "unsupported compression {compression} (expected 0 = BI_RGB uncompressed)"
            )));
        }
        let w = width as usize;
        let h = height.unsigned_abs() as usize;
        let row_size = (w + 3) & !3;
        let fits = pixel_offset
            .checked_add(row_size.saturating_mul(h))
            .is_some_and(|end| end <= data.len());
        if !fits {
            return Err(MapError::InvalidBmp(format!(
                "pixel data ({row_size} B/row × {h} rows from offset {pixel_offset}) exceeds file size {}",
                data.len()
            )));
        }
        let top_down = height < 0;
        let mut indices = vec![0u8; w * h];
        for y in 0..h {
            // y = 0 is the top (north) row; bottom-up files store it last.
            let src_row = if top_down { y } else { h - 1 - y };
            let row_start = pixel_offset + src_row * row_size;
            indices[y * w..y * w + w].copy_from_slice(&data[row_start..row_start + w]);
        }
        Ok(IndexMap {
            width: w as u32,
            height: h as u32,
            indices,
        })
    }

    /// Palette index of pixel (x, y), or `None` if out of bounds.
    pub fn index_at(&self, x: u32, y: u32) -> Option<u8> {
        if x < self.width && y < self.height {
            Some(self.indices[(y * self.width + x) as usize])
        } else {
            None
        }
    }

    /// Build from raw indices (tests), same bounds check as `from_colors`.
    #[cfg(test)]
    pub(crate) fn from_indices(width: u32, height: u32, indices: Vec<u8>) -> IndexMap {
        assert_eq!(indices.len(), (width * height) as usize);
        IndexMap {
            width,
            height,
            indices,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_file(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "tactical_map_test_{name}_{}_{n}.bmp",
            std::process::id()
        ))
    }

    /// Serialize a 24-bit BMP; `px` is row-major from the top row. A negative
    /// `h` writes a top-down file, a positive `h` a bottom-up one.
    fn build_bmp_bytes(w: i32, h: i32, px: &[(u8, u8, u8)]) -> Vec<u8> {
        let top_down = h < 0;
        let rows = h.unsigned_abs() as usize;
        let cols = w as usize;
        assert_eq!(px.len(), rows * cols);
        let row_size = (cols * 3 + 3) & !3;
        let mut data = vec![0u8; 54 + row_size * rows];
        let file_size = data.len() as u32;
        data[0..2].copy_from_slice(b"BM");
        data[2..6].copy_from_slice(&file_size.to_le_bytes());
        data[10..14].copy_from_slice(&54u32.to_le_bytes());
        data[14..18].copy_from_slice(&40u32.to_le_bytes());
        data[18..22].copy_from_slice(&w.to_le_bytes());
        data[22..26].copy_from_slice(&h.to_le_bytes());
        data[26..28].copy_from_slice(&1u16.to_le_bytes());
        data[28..30].copy_from_slice(&24u16.to_le_bytes());
        for y in 0..rows {
            let dst = if top_down { y } else { rows - 1 - y };
            for x in 0..cols {
                let (r, g, b) = px[y * cols + x];
                let o = 54 + dst * row_size + x * 3;
                data[o] = b;
                data[o + 1] = g;
                data[o + 2] = r;
            }
        }
        data
    }

    fn write_temp(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    const PIX: [(u8, u8, u8); 6] = [
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (10, 20, 30),
        (40, 50, 60),
        (70, 80, 90),
    ];

    #[test]
    fn bmp_roundtrip_bottom_up() {
        let path = temp_file("bottom_up");
        write_temp(&path, &build_bmp_bytes(3, 2, &PIX));
        let map = ProvinceMap::load_bmp(&path).unwrap();
        assert_eq!((map.width, map.height), (3, 2));
        // Row-major from the top, packed 0xRRGGBB.
        assert_eq!(map.color_at(0, 0), Some(0xFF0000));
        assert_eq!(map.color_at(1, 0), Some(0x00FF00));
        assert_eq!(map.color_at(2, 0), Some(0x0000FF));
        assert_eq!(map.color_at(0, 1), Some(0x0A141E));
        assert_eq!(map.color_at(1, 1), Some(0x28323C));
        assert_eq!(map.color_at(2, 1), Some(0x46505A));
        assert_eq!(map.color_at(3, 0), None);
        assert_eq!(map.color_at(0, 2), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bmp_roundtrip_top_down() {
        let path = temp_file("top_down");
        write_temp(&path, &build_bmp_bytes(3, -2, &PIX));
        let map = ProvinceMap::load_bmp(&path).unwrap();
        assert_eq!((map.width, map.height), (3, 2));
        assert_eq!(map.color_at(0, 0), Some(0xFF0000));
        assert_eq!(map.color_at(2, 1), Some(0x46505A));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bmp_rejects_bad_magic() {
        let path = temp_file("bad_magic");
        let mut bytes = build_bmp_bytes(3, 2, &PIX);
        bytes[0] = b'Z';
        write_temp(&path, &bytes);
        let err = ProvinceMap::load_bmp(&path).unwrap_err();
        assert!(err.to_string().contains("magic"), "unexpected error: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bmp_rejects_non_24bit() {
        let path = temp_file("bpp");
        let mut bytes = build_bmp_bytes(3, 2, &PIX);
        bytes[28] = 8; // patch bit depth to 8 bpp
        write_temp(&path, &bytes);
        let err = ProvinceMap::load_bmp(&path).unwrap_err();
        assert!(
            err.to_string().contains("bit depth"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bmp_rejects_truncated_pixel_data() {
        let path = temp_file("truncated");
        let bytes = build_bmp_bytes(3, 2, &PIX);
        write_temp(&path, &bytes[..bytes.len() - 5]);
        let err = ProvinceMap::load_bmp(&path).unwrap_err();
        assert!(
            err.to_string().contains("exceeds file size"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bmp_missing_file_is_io_error() {
        let path = temp_file("does_not_exist");
        let err = ProvinceMap::load_bmp(&path).unwrap_err();
        assert!(
            matches!(err, MapError::Io { .. }),
            "unexpected error: {err}"
        );
    }
}
