//! A frame as Alpaca's `imagebytes` carries it: the body of a camera's
//! `imagearray` answer to `Accept: application/imagebytes`. See
//! `docs/crates/guide-replay.md` (Decoding a frame).

use ndarray::Array2;

/// The metadata's length: eleven little-endian 32-bit integers.
const HEADER_BYTES: usize = 44;

/// The one metadata version Alpaca defines.
const METADATA_VERSION: i32 = 1;

/// Why a body is no frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The body ends before its metadata or its pixels do.
    #[error("the body ends at {len} bytes, before the {needed} its frame takes")]
    Short {
        /// The body's length.
        len: usize,
        /// The length its metadata calls for.
        needed: usize,
    },
    /// The metadata is of a version Alpaca does not define.
    #[error("metadata version {0} is not 1")]
    Version(i32),
    /// The camera answered with an error instead of a frame.
    #[error("the camera answered error {0}")]
    Camera(i32),
    /// The frame has more than one plane, or a dimension that is not
    /// positive.
    #[error("a frame of rank {rank}, {width} × {height}, is no guide frame")]
    Shape {
        /// Its rank.
        rank: i32,
        /// Its first dimension.
        width: i32,
        /// Its second dimension.
        height: i32,
    },
    /// The pixels are sent as a type Alpaca does not define for them.
    #[error("transmission element type {0} is not one of 1, 2, 6 or 8")]
    ElementType(i32),
    /// The pixels start inside the metadata.
    #[error("the pixels start at byte {0}, inside the metadata")]
    DataStart(i32),
}

/// How the pixels are sent.
#[derive(Debug, Clone, Copy)]
enum Element {
    I16,
    I32,
    U8,
    U16,
}

impl Element {
    const fn from_code(code: i32) -> Option<Self> {
        match code {
            1 => Some(Self::I16),
            2 => Some(Self::I32),
            6 => Some(Self::U8),
            8 => Some(Self::U16),
            _ => None,
        }
    }

    const fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 => 4,
        }
    }

    fn values(self, pixels: &[u8]) -> Vec<i32> {
        match self {
            Self::U8 => pixels.iter().map(|byte| i32::from(*byte)).collect(),
            Self::I16 => pixels
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i32::from(i16::from_le_bytes(*bytes)))
                .collect(),
            Self::U16 => pixels
                .as_chunks::<2>()
                .0
                .iter()
                .map(|bytes| i32::from(u16::from_le_bytes(*bytes)))
                .collect(),
            Self::I32 => pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| i32::from_le_bytes(*bytes))
                .collect(),
        }
    }
}

/// Decodes a body into its pixels, indexed `[[y, x]]`.
///
/// The body sends the frame width-major, as Alpaca's arrays are: the
/// first dimension is the width, and `y` runs fastest. Bytes after the
/// pixels are ignored.
///
/// # Errors
///
/// A [`DecodeError`] when the body is no single-plane frame.
pub fn decode(body: &[u8]) -> Result<Array2<i32>, DecodeError> {
    let header = header(body)?;
    let [version, error, _, _, data_start, _, element, rank, width, height, _] = header;
    if version != METADATA_VERSION {
        return Err(DecodeError::Version(version));
    }
    if error != 0 {
        return Err(DecodeError::Camera(error));
    }
    let shape = DecodeError::Shape {
        rank,
        width,
        height,
    };
    let (Ok(columns), Ok(rows)) = (usize::try_from(width), usize::try_from(height)) else {
        return Err(shape);
    };
    if rank != 2 || columns == 0 || rows == 0 {
        return Err(shape);
    }
    let element = Element::from_code(element).ok_or(DecodeError::ElementType(element))?;
    let start = usize::try_from(data_start)
        .ok()
        .filter(|start| *start >= HEADER_BYTES)
        .ok_or(DecodeError::DataStart(data_start))?;
    let needed = columns
        .checked_mul(rows)
        .and_then(|pixels| pixels.checked_mul(element.bytes()))
        .and_then(|bytes| bytes.checked_add(start))
        .ok_or(DecodeError::Short {
            len: body.len(),
            needed: usize::MAX,
        })?;
    let pixels = body.get(start..needed).ok_or(DecodeError::Short {
        len: body.len(),
        needed,
    })?;
    let width_major =
        Array2::from_shape_vec((columns, rows), element.values(pixels)).map_err(|_| shape)?;
    Ok(width_major
        .reversed_axes()
        .as_standard_layout()
        .into_owned())
}

fn header(body: &[u8]) -> Result<[i32; 11], DecodeError> {
    let bytes = body.get(..HEADER_BYTES).ok_or(DecodeError::Short {
        len: body.len(),
        needed: HEADER_BYTES,
    })?;
    let mut header = [0; 11];
    for (value, word) in header.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *value = i32::from_le_bytes(*word);
    }
    Ok(header)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::test_support::{body, encode};

    #[test]
    fn a_frame_decodes_to_rows_of_its_width() {
        let pixels = Array2::from_shape_fn((3, 4), |(y, x)| (10 * y + x) as u16);
        let decoded = decode(&encode(&pixels)).unwrap();
        assert_eq!(decoded.dim(), (3, 4));
        assert_eq!(decoded, pixels.mapv(i32::from));
    }

    #[test]
    fn each_element_type_decodes() {
        let values: [(i32, Vec<u8>); 4] = [
            (1, (-2_i16).to_le_bytes().repeat(2)),
            (2, (-70_000_i32).to_le_bytes().repeat(2)),
            (6, vec![200, 200]),
            (8, 65_000_u16.to_le_bytes().repeat(2)),
        ];
        let expected = [-2, -70_000, 200, 65_000];
        for ((element, pixels), value) in values.into_iter().zip(expected) {
            let decoded = decode(&body([1, 0, 0, 0, 44, 2, element, 2, 1, 2, 0], &pixels)).unwrap();
            assert_eq!(decoded, Array2::from_elem((2, 1), value), "type {element}");
        }
    }

    #[test]
    fn the_pixels_start_where_the_metadata_says() {
        let mut pixels = vec![0xFF; 4];
        pixels.extend(7_u16.to_le_bytes());
        let decoded = decode(&body([1, 0, 0, 0, 48, 2, 8, 2, 1, 1, 0], &pixels)).unwrap();
        assert_eq!(decoded, Array2::from_elem((1, 1), 7));
    }

    #[test]
    fn a_body_that_is_no_frame_is_refused() {
        let pixel = 7_u16.to_le_bytes();
        let cases = [
            ([2, 0, 0, 0, 44, 2, 8, 2, 1, 1, 0], DecodeError::Version(2)),
            (
                [1, 1025, 0, 0, 44, 2, 8, 2, 1, 1, 0],
                DecodeError::Camera(1025),
            ),
            (
                [1, 0, 0, 0, 44, 2, 8, 3, 1, 1, 3],
                DecodeError::Shape {
                    rank: 3,
                    width: 1,
                    height: 1,
                },
            ),
            (
                [1, 0, 0, 0, 44, 2, 8, 2, 0, 1, 0],
                DecodeError::Shape {
                    rank: 2,
                    width: 0,
                    height: 1,
                },
            ),
            (
                [1, 0, 0, 0, 44, 2, 8, 2, -1, 1, 0],
                DecodeError::Shape {
                    rank: 2,
                    width: -1,
                    height: 1,
                },
            ),
            (
                [1, 0, 0, 0, 44, 2, 3, 2, 1, 1, 0],
                DecodeError::ElementType(3),
            ),
            (
                [1, 0, 0, 0, 40, 2, 8, 2, 1, 1, 0],
                DecodeError::DataStart(40),
            ),
            (
                [1, 0, 0, 0, 44, 2, 8, 2, 2, 1, 0],
                DecodeError::Short {
                    len: 46,
                    needed: 48,
                },
            ),
        ];
        for (header, error) in cases {
            assert_eq!(decode(&body(header, &pixel)), Err(error), "{header:?}");
        }
        assert_eq!(
            decode(&[0; 20]),
            Err(DecodeError::Short {
                len: 20,
                needed: 44
            })
        );
    }
}
