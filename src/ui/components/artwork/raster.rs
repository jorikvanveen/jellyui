use std::sync::Arc;

use gpui::{ObjectFit, RenderImage};
use image::{ImageBuffer, Rgba, imageops};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Fit {
    Contain,
    Cover,
    Fill,
}

impl Fit {
    pub(super) fn from_object_fit(fit: &ObjectFit) -> Option<Self> {
        match fit {
            ObjectFit::Contain | ObjectFit::ScaleDown => Some(Self::Contain),
            ObjectFit::Cover => Some(Self::Cover),
            ObjectFit::Fill => Some(Self::Fill),
            ObjectFit::None => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Target {
    pub width: u32,
    pub height: u32,
    pub fit: Fit,
}

/// Resize GPUI's decoded BGRA pixels directly, without another PNG encode/decode.
pub(super) fn downsample(source: Arc<RenderImage>, target: Target) -> Arc<RenderImage> {
    if source.frame_count() != 1 || target.width == 0 || target.height == 0 {
        return source;
    }
    let size = source.size(0);
    let (width, height) = (u32::from(size.width), u32::from(size.height));
    if width == 0 || height == 0 {
        return source;
    }
    let (crop_width, crop_height) = if target.fit == Fit::Cover {
        let ratio = f64::from(target.width) / f64::from(target.height);
        (
            width.min((f64::from(height) * ratio).round().max(1.) as u32),
            height.min((f64::from(width) / ratio).round().max(1.) as u32),
        )
    } else {
        (width, height)
    };
    let (out_width, out_height) = if target.fit == Fit::Fill {
        (width.min(target.width), height.min(target.height))
    } else {
        let scale = (f64::from(target.width) / f64::from(crop_width))
            .min(f64::from(target.height) / f64::from(crop_height))
            .min(1.);
        (
            (f64::from(crop_width) * scale).round().max(1.) as u32,
            (f64::from(crop_height) * scale).round().max(1.) as u32,
        )
    };
    if (width, height) == (out_width, out_height) {
        return source;
    }
    // Borrow the full-size pixels only for this operation. Channel filtering
    // works identically for BGRA, which is the ordering RenderImage expects.
    let pixels = ImageBuffer::<Rgba<u8>, _>::from_raw(
        width,
        height,
        source.as_bytes(0).expect("decoded frame has pixels"),
    )
    .expect("decoded frame dimensions match its pixels");
    let crop = imageops::crop_imm(
        &pixels,
        (width - crop_width) / 2,
        (height - crop_height) / 2,
        crop_width,
        crop_height,
    );
    let resized = imageops::resize(
        &*crop,
        out_width,
        out_height,
        imageops::FilterType::Lanczos3,
    );
    Arc::new(RenderImage::new([image::Frame::new(resized)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnails_reduce_resident_bytes_and_preserve_color_and_aspect_ratio() {
        let source = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::from_pixel(640, 320, Rgba([17, 80, 200, 255])),
        )]));
        let thumbnail = downsample(
            source,
            Target {
                width: 160,
                height: 160,
                fit: Fit::Contain,
            },
        );
        assert_eq!(thumbnail.size(0), gpui::size(160.into(), 80.into()));
        let bytes = thumbnail.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), 160 * 80 * 4);
        assert!(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [17, 80, 200, 255])
        );
    }

    #[test]
    fn portrait_cropping_keeps_the_center_without_stretching_the_edges() {
        let source = Arc::new(RenderImage::new([image::Frame::new(
            image::RgbaImage::from_fn(640, 320, |x, _| {
                Rgba(if x < 160 {
                    [255, 0, 0, 255]
                } else if x >= 480 {
                    [0, 0, 255, 255]
                } else {
                    [0, 200, 0, 255]
                })
            }),
        )]));
        let thumbnail = downsample(
            source,
            Target {
                width: 160,
                height: 160,
                fit: Fit::Cover,
            },
        );
        assert_eq!(thumbnail.size(0), gpui::size(160.into(), 160.into()));
        assert!(
            thumbnail
                .as_bytes(0)
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [0, 200, 0, 255])
        );
    }
}
