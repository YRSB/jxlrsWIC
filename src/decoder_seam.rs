//! Pure-Rust decoding seam for JPEG XL, free of COM, Win32, `IStream`, `HRESULT` and GUID types.
//!
//! The COM layer in [`crate::decoder`] owns COM lifecycle, parameter marshalling and `HRESULT`
//! translation only. Every bit of bitstream parsing, pixel format selection, colour profile
//! extraction and raster storage lives behind the [`DecoderSeam`] and [`FrameSeam`] traits
//! defined here, so the whole decode pipeline can be exercised with plain in-memory buffers.

use std::sync::Arc;

/// Largest accepted image edge. Larger dimensions are rejected before any pixel allocation.
pub const MAX_DIMENSION: u32 = 16_384;

/// Largest accepted input stream, in bytes. Guards host processes against bitstream bombs.
pub const MAX_STREAM_BYTES: usize = 256 * 1024 * 1024;

/// A decoded still image container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Number of decodable frames. Animated JPEG XL is out of scope, so this is always 1.
    pub frame_count: usize,
}

/// Everything the COM layer needs to answer `IWICBitmapSource` queries for a frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameInfo {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Horizontal resolution in dots per inch.
    pub dpi_x: f64,
    /// Vertical resolution in dots per inch.
    pub dpi_y: f64,
    /// Pixel format the frame reports through `GetPixelFormat`.
    pub pixel_format: WicPixelFormat,
}

/// Pixel formats exposed by the decoder, named after their WIC counterparts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WicPixelFormat {
    /// `GUID_WICPixelFormat32bppBGRA`, straight alpha.
    Bgra32,
    /// `GUID_WICPixelFormat64bppRGBA`, 16-bit unsigned integer channels.
    Rgba64,
    /// `GUID_WICPixelFormat64bppRGBAHalf`, IEEE 754 binary16 channels.
    RgbaHalf64,
}

impl WicPixelFormat {
    /// Bytes occupied by a single pixel in this format.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Bgra32 => 4,
            Self::Rgba64 | Self::RgbaHalf64 => 8,
        }
    }

    /// Bytes required for `width` pixels of this format.
    pub fn row_bytes(self, width: u32) -> usize {
        self.bytes_per_pixel() * width as usize
    }
}

/// Requested sub-rectangle of a frame, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Rectangle width; zero means "up to the right edge".
    pub width: u32,
    /// Rectangle height; zero means "up to the bottom edge".
    pub height: u32,
}

impl Rect {
    /// Builds a rectangle. Width and height of zero are interpreted as "to the edge".
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Resolves this (possibly zero-sized) rectangle against a frame of `frame_width` by
    /// `frame_height` pixels. Returns `None` when the rectangle is empty or escapes the frame.
    pub fn resolve(self, frame_width: u32, frame_height: u32) -> Option<Rect> {
        if self.x >= frame_width
            || self.y >= frame_height
            || self.width > frame_width - self.x
            || self.height > frame_height - self.y
        {
            return None;
        }
        let width = if self.width == 0 {
            frame_width - self.x
        } else {
            self.width
        };
        let height = if self.height == 0 {
            frame_height - self.y
        } else {
            self.height
        };
        Some(Rect {
            x: self.x,
            y: self.y,
            width,
            height,
        })
    }
}

/// Decoding failures, translated to WIC `HRESULT`s by the COM layer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input stream could not be read.
    #[error("stream read or ingestion error: {0}")]
    StreamError(String),
    /// The image is larger than [`MAX_DIMENSION`] on at least one edge.
    #[error("image dimensions exceed the safe limit of {MAX_DIMENSION}x{MAX_DIMENSION}")]
    DimensionTooLarge,
    /// The bitstream is not JPEG XL, or is corrupt.
    #[error("bitstream unsupported or corrupt: {0}")]
    InvalidBitstream(String),
    /// The requested rectangle is empty or escapes the frame bounds.
    #[error("copy rectangle is outside the frame bounds")]
    InvalidRect,
    /// The destination row stride is smaller than one packed row.
    #[error("row stride {stride} is smaller than the required minimum {min_stride}")]
    InvalidStride {
        /// Stride supplied by the caller.
        stride: usize,
        /// Minimum stride that would have sufficed.
        min_stride: usize,
    },
    /// The destination buffer cannot hold `height` rows of `stride` bytes.
    #[error("destination buffer holds {provided} bytes but {required} were required")]
    InsufficientBuffer {
        /// Bytes supplied by the caller.
        provided: usize,
        /// Bytes required for the requested rectangle and stride.
        required: usize,
    },
    /// The requested frame index does not exist.
    #[error("frame index {index} is out of range (frame count {frame_count})")]
    FrameIndexOutOfRange {
        /// Requested index.
        index: usize,
        /// Number of frames in the container.
        frame_count: usize,
    },
}

/// Container-level seam: owns the buffered bitstream and hands out frames.
pub trait DecoderSeam: Send + Sync {
    /// Container metadata, available immediately after the seam is constructed.
    fn info(&self) -> &ContainerInfo;

    /// Number of frames in the container.
    fn frame_count(&self) -> usize {
        self.info().frame_count
    }

    /// Borrows the frame at `index`.
    fn frame(&self, index: usize) -> Result<Arc<dyn FrameSeam>, DecodeError>;
}

/// Frame-level seam: metadata, colour profile and stride-aware pixel blits.
pub trait FrameSeam: Send + Sync {
    /// Frame metadata, including the selected WIC pixel format.
    fn info(&self) -> &FrameInfo;

    /// ICC profile bytes for this frame, extracted verbatim or synthesized from CICP.
    fn color_profile(&self) -> Option<&[u8]>;

    /// Copies the requested region into `out`, honouring an arbitrary destination row stride.
    fn copy_pixels(
        &self,
        rect: Option<Rect>,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), DecodeError>;
}

/// Owned decoded raster, stored lazily inside a frame seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedRaster {
    width: u32,
    height: u32,
    format: WicPixelFormat,
    bytes: Vec<u8>,
}

impl CachedRaster {
    /// Builds a raster from tightly packed pixels of `format`.
    pub fn new(width: u32, height: u32, format: WicPixelFormat, bytes: Vec<u8>) -> Self {
        Self {
            width,
            height,
            format,
            bytes,
        }
    }

    /// Raster width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Raster height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Pixel format of the stored bytes.
    pub fn format(&self) -> WicPixelFormat {
        self.format
    }

    /// Tightly packed pixel bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Validates a `CopyPixels` request and returns the resolved rectangle plus row geometry.
    pub fn plan(
        &self,
        rect: Option<Rect>,
        stride: usize,
        buffer_len: usize,
    ) -> Result<CopyPlan, DecodeError> {
        let resolved = match rect {
            Some(rect) => rect
                .resolve(self.width, self.height)
                .ok_or(DecodeError::InvalidRect)?,
            None => Rect::new(0, 0, self.width, self.height),
        };
        let min_stride = self.format.row_bytes(resolved.width);
        if stride < min_stride {
            return Err(DecodeError::InvalidStride { stride, min_stride });
        }
        let required = stride.checked_mul(resolved.height as usize).ok_or(
            DecodeError::InsufficientBuffer {
                provided: buffer_len,
                required: usize::MAX,
            },
        )?;
        if buffer_len < required {
            return Err(DecodeError::InsufficientBuffer {
                provided: buffer_len,
                required,
            });
        }
        Ok(CopyPlan { resolved, stride })
    }

    /// Copies the requested region into `out` using [`CachedRaster::plan`] for validation.
    pub fn blit(
        &self,
        rect: Option<Rect>,
        stride: usize,
        out: &mut [u8],
    ) -> Result<(), DecodeError> {
        let plan = self.plan(rect, stride, out.len())?;
        let src_stride = self.format.row_bytes(self.width);
        let row_len = self.format.row_bytes(plan.resolved.width);

        let bytes_per_pixel = self.format.bytes_per_pixel();
        for (row, dest_row) in out
            .chunks_mut(stride)
            .take(plan.resolved.height as usize)
            .enumerate()
        {
            let src_start = (plan.resolved.y as usize + row) * src_stride
                + plan.resolved.x as usize * bytes_per_pixel;
            let src_end = src_start + row_len;
            match (
                self.bytes.get(src_start..src_end),
                dest_row.get_mut(..row_len),
            ) {
                (Some(src_row), Some(dest_row)) => dest_row.copy_from_slice(src_row),
                _ => return Err(DecodeError::InvalidRect),
            }
        }
        Ok(())
    }
}

/// Resolved `CopyPixels` geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyPlan {
    /// Rectangle that will actually be read, clipped to the frame.
    pub resolved: Rect,
    /// Destination row stride in bytes.
    pub stride: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster() -> CachedRaster {
        // 2x2 BGRA: red, green / blue, white.
        let bytes = vec![
            0, 0, 255, 255, // red
            0, 255, 0, 255, // green
            255, 0, 0, 255, // blue
            255, 255, 255, 255, // white
        ];
        CachedRaster::new(2, 2, WicPixelFormat::Bgra32, bytes)
    }

    #[test]
    fn blits_whole_frame_tightly_packed() {
        let mut out = vec![0u8; 16];
        raster().blit(None, 8, &mut out).unwrap();
        assert_eq!(out, raster().bytes());
    }

    #[test]
    fn blits_sub_rect_with_custom_stride() {
        let mut out = vec![0xAAu8; 16];
        // Bottom-right pixel only, with a stride of 8 and one row.
        raster()
            .blit(Some(Rect::new(1, 1, 1, 1)), 8, &mut out)
            .unwrap();
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
        assert_eq!(&out[4..8], &[0xAA; 4]);
    }

    #[test]
    fn clips_zero_sized_rect_to_frame_edge() {
        let plan = raster().plan(Some(Rect::new(1, 0, 0, 0)), 4, 8).unwrap();
        assert_eq!(plan.resolved, Rect::new(1, 0, 1, 2));
    }

    #[test]
    fn rejects_out_of_bounds_rect() {
        assert_eq!(
            raster().plan(Some(Rect::new(2, 0, 1, 1)), 4, 4),
            Err(DecodeError::InvalidRect)
        );
        assert_eq!(
            raster().plan(Some(Rect::new(0, 0, 3, 1)), 12, 12),
            Err(DecodeError::InvalidRect)
        );
    }

    #[test]
    fn rejects_short_stride_and_small_buffer() {
        assert_eq!(
            raster().plan(None, 7, 64),
            Err(DecodeError::InvalidStride {
                stride: 7,
                min_stride: 8
            })
        );
        assert_eq!(
            raster().plan(None, 8, 15),
            Err(DecodeError::InsufficientBuffer {
                provided: 15,
                required: 16
            })
        );
    }
}
