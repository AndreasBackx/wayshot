use wayland_client::protocol::wl_output::Transform;

use crate::error::{Error, Result};
use crate::output::OutputInfo;
use crate::screencopy::FrameCopy;

pub enum RegionCapturer {
    Outputs(Vec<OutputInfo>),
    Region(CaptureRegion),
    Freeze(Box<dyn Fn() -> Result<CaptureRegion>>),
}

/// Struct to store region capture details.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CaptureRegion {
    /// X coordinate of the area to capture.
    pub x_coordinate: i32,
    /// y coordinate of the area to capture.
    pub y_coordinate: i32,
    /// Width of the capture area.
    pub width: i32,
    /// Height of the capture area.
    pub height: i32,
}

impl CaptureRegion {
    #[tracing::instrument(ret, level = "debug")]
    pub fn overlaps(&self, other: &CaptureRegion) -> bool {
        let left = self.x_coordinate;
        let bottom = self.y_coordinate;
        let right = self.x_coordinate + self.width;
        let top = self.y_coordinate + self.height;

        let other_left = other.x_coordinate;
        let other_bottom: i32 = other.y_coordinate;
        let other_right = other.x_coordinate + other.width;
        let other_top: i32 = other.y_coordinate + other.height;

        left < other_right && other_left < right && bottom < other_top && other_bottom < top
    }
}

impl TryFrom<&Vec<OutputInfo>> for CaptureRegion {
    type Error = Error;

    fn try_from(value: &Vec<OutputInfo>) -> std::result::Result<Self, Self::Error> {
        let x1 = value
            .iter()
            .map(|output| output.dimensions.x)
            .min()
            .unwrap();
        let y1 = value
            .iter()
            .map(|output| output.dimensions.y)
            .min()
            .unwrap();
        let x2 = value
            .iter()
            .map(|output| output.dimensions.x + output.dimensions.width)
            .max()
            .unwrap();
        let y2 = value
            .iter()
            .map(|output| output.dimensions.y + output.dimensions.height)
            .max()
            .unwrap();
        Ok(CaptureRegion {
            x_coordinate: x1,
            y_coordinate: y1,
            width: x2 - x1,
            height: y2 - y1,
        })
    }
}

impl From<&FrameCopy> for CaptureRegion {
    fn from(value: &FrameCopy) -> Self {
        let (width, height) = (
            value.frame_format.width as i32,
            value.frame_format.height as i32,
        );
        let is_portait = match value.transform {
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => true,
            _ => false,
        };
        CaptureRegion {
            x_coordinate: value.position.0 as i32,
            y_coordinate: value.position.1 as i32,
            width: if is_portait { height } else { width },
            height: if is_portait { width } else { height },
        }
    }
}
