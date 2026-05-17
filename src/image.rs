//! Image decoding.
//!
//! Currently, supports PNG (treated as 8-bit sRGB), JPEG, BMP and GIF files.
//!
//! Implemented as a wrapper around the C library stb_image, since it supports
//! "CgBI" PNG files (an Apple proprietary extension used in iPhone OS apps).
//!
//! This module also exposes decompression for Imagination Technologies' PVRTC
//! format, implementing as a wrapper around their decoder from the PowerVR
//! SDK.
//!
//! References:
//! - "Supported Image Formats" in [Loading Images](https://developer.apple.com/library/archive/documentation/2DDrawing/Conceptual/DrawingPrintingiOS/LoadingImages/LoadingImages.html)

use std::ffi::{c_int, c_uchar, CStr};

pub struct Image {
    pixels: PixelStore,
    dimensions: (u32, u32),
}

enum PixelStore {
    StbImage(*mut c_uchar),
    Vec(Vec<u8>),
}

impl Image {
    pub fn pixels(&self) -> &[u8] {
        match &self.pixels {
            PixelStore::StbImage(ptr) => {
                // This is a simplified version - in practice, you'd need to handle the memory management properly
                todo!()
            }
            PixelStore::Vec(vec) => vec,
        }
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }
}

// impl Clone for Image {
//     fn clone(&self) -> Image {
//         // Note: implicitly converts pixel storage from StbImage to Vec
//         // (if needed)
//         Image::from_pixel_vec(self.pixels().to_vec(), self.dimensions)
//     }
// }

impl Drop for Image {
    fn drop(&mut self) {
        // match self.pixels {
        //     PixelStore::StbImage(ptr) => unsafe { stbi_image_free(ptr.cast()) },
        //     PixelStore::Vec(_) => (),
        // }
    }
}

/// Approximate implementation of sRGB gamma encoding.
pub fn gamma_encode(intensity: f32) -> f32 {
    // TODO: This doesn't implement the linear section near zero.
    intensity.powf(1.0 / 2.2)
}
/// Approximate implementation of sRGB gamma decoding.
pub fn gamma_decode(intensity: f32) -> f32 {
    // TODO: This doesn't implement the linear section near zero.
    intensity.powf(2.2)
}
