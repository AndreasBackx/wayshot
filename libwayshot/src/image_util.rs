use image::RgbaImage;
use wayland_client::protocol::wl_output::Transform;

#[tracing::instrument(skip(image))]
pub(crate) fn rotate_image_buffer(
    image: RgbaImage,
    transform: Transform,
    width: u32,
    height: u32,
) -> RgbaImage {
    tracing::debug!("Rotating image buffer");
    match transform {
        Transform::_90 => image::imageops::rotate90(&image),
        Transform::_180 => image::imageops::rotate180(&image),
        Transform::_270 => image::imageops::rotate270(&image),
        Transform::Flipped => image::imageops::flip_horizontal(&image),
        Transform::Flipped90 => {
            let flipped_buffer = image::imageops::flip_horizontal(&image);
            image::imageops::rotate90(&flipped_buffer)
        }
        Transform::Flipped180 => {
            let flipped_buffer = image::imageops::flip_horizontal(&image);
            image::imageops::rotate180(&flipped_buffer)
        }
        Transform::Flipped270 => {
            let flipped_buffer = image::imageops::flip_horizontal(&image);
            image::imageops::rotate270(&flipped_buffer)
        }
        _ => image,
    }
}
