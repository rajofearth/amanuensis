use std::sync::{Arc, OnceLock};

use gpui::{Image, ImageFormat, Img, Styled, img, px};

fn image(bytes: &'static [u8]) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Svg, bytes.to_vec()))
}

pub(crate) fn horizontal(size: f32) -> Img {
    static IMAGE: OnceLock<Arc<Image>> = OnceLock::new();
    img(IMAGE
        .get_or_init(|| {
            image(include_bytes!(
                "../assets/logos/transparent-logo-workmark-horizontal.svg"
            ))
        })
        .clone())
    .size(px(size))
}
