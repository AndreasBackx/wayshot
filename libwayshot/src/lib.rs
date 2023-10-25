//! `libwayshot` is a convenient wrapper over the wlroots screenshot protocol
//! that provides a simple API to take screenshots with.
//!
//! To get started, look at [`WayshotConnection`].

mod convert;
mod dispatch;
mod error;
mod image_util;
pub mod output;
mod region;
mod screencopy;

use std::{
    collections::HashSet,
    fs::File,
    os::fd::AsFd,
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};

use dispatch::LayerShellState;
use image::{imageops::replace, Rgba, RgbaImage};
use memmap2::MmapMut;
use region::RegionCapturer;
use screencopy::FrameGuard;
use tracing::{debug, span, Level};
use wayland_client::{
    globals::{registry_queue_init, GlobalList},
    protocol::{
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_shm::{self, WlShm},
    },
    Connection, EventQueue,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::Anchor,
    },
    screencopy::v1::client::{
        zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

use crate::{
    convert::create_converter,
    dispatch::{CaptureFrameState, FrameState, OutputCaptureState, WayshotState},
    output::OutputInfo,
    screencopy::{create_shm_fd, FrameCopy, FrameFormat},
};

pub use crate::{
    error::{Error, Result},
    region::CaptureRegion,
};

pub mod reexport {
    use wayland_client::protocol::wl_output;
    pub use wl_output::{Transform, WlOutput};
}

/// Struct to store wayland connection and globals list.
/// # Example usage
///
/// ```
/// let wayshot_connection = WayshotConnection::new().unwrap();
/// let image_buffer = wayshot_connection.screenshot_all().unwrap();
/// ```
#[derive(Debug)]
pub struct WayshotConnection {
    pub conn: Connection,
    pub globals: GlobalList,
    output_infos: Vec<OutputInfo>,
}

impl WayshotConnection {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env()?;

        Self::from_connection(conn)
    }

    /// Recommended if you already have a [`wayland_client::Connection`].
    pub fn from_connection(conn: Connection) -> Result<Self> {
        let (globals, _) = registry_queue_init::<WayshotState>(&conn)?;

        let mut initial_state = Self {
            conn,
            globals,
            output_infos: Vec::new(),
        };

        initial_state.refresh_outputs()?;

        Ok(initial_state)
    }

    /// Fetch all accessible wayland outputs.
    pub fn get_all_outputs(&self) -> &Vec<OutputInfo> {
        &self.output_infos
    }

    /// refresh the outputs, to get new outputs
    pub fn refresh_outputs(&mut self) -> Result<()> {
        // Connecting to wayland environment.
        let mut state = OutputCaptureState {
            outputs: Vec::new(),
        };
        let mut event_queue = self.conn.new_event_queue::<OutputCaptureState>();
        let qh = event_queue.handle();

        // Bind to xdg_output global.
        let zxdg_output_manager = match self.globals.bind::<ZxdgOutputManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Failed to create ZxdgOutputManagerV1 version 3. Does your compositor implement ZxdgOutputManagerV1?");
                panic!("{:#?}", e);
            }
        };

        // Fetch all outputs; when their names arrive, add them to the list
        let _ = self.conn.display().get_registry(&qh, ());
        event_queue.roundtrip(&mut state)?;
        event_queue.roundtrip(&mut state)?;

        // We loop over each output and request its position data.
        let xdg_outputs: Vec<ZxdgOutputV1> = state
            .outputs
            .iter()
            .enumerate()
            .map(|(index, output)| {
                zxdg_output_manager.get_xdg_output(&output.wl_output, &qh, index)
            })
            .collect();

        event_queue.roundtrip(&mut state)?;

        for xdg_output in xdg_outputs {
            xdg_output.destroy();
        }

        if state.outputs.is_empty() {
            tracing::error!("Compositor did not advertise any wl_output devices!");
            exit(1);
        }
        tracing::trace!("Outputs detected: {:#?}", state.outputs);
        self.output_infos = state.outputs;

        Ok(())
    }

    /// Get a FrameCopy instance with screenshot pixel data for any wl_output object.
    ///  Data will be written to fd.
    pub fn capture_output_frame_shm_fd<T: AsFd>(
        &self,
        cursor_overlay: i32,
        output: &WlOutput,
        fd: T,
    ) -> Result<(FrameFormat, FrameGuard)> {
        let (state, event_queue, frame, frame_format) =
            self.capture_output_frame_get_state(cursor_overlay, output)?;
        let frame_guard =
            self.capture_output_frame_inner(state, event_queue, frame, frame_format, fd)?;

        Ok((frame_format, frame_guard))
    }

    fn capture_output_frame_get_state(
        &self,
        cursor_overlay: i32,
        output: &WlOutput,
    ) -> Result<(
        CaptureFrameState,
        EventQueue<CaptureFrameState>,
        ZwlrScreencopyFrameV1,
        FrameFormat,
    )> {
        let mut state = CaptureFrameState {
            formats: Vec::new(),
            state: None,
            buffer_done: AtomicBool::new(false),
        };
        let mut event_queue = self.conn.new_event_queue::<CaptureFrameState>();
        let qh = event_queue.handle();

        // Instantiating screencopy manager.
        let screencopy_manager = match self.globals.bind::<ZwlrScreencopyManagerV1, _, _>(
            &qh,
            3..=3,
            (),
        ) {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Failed to create screencopy manager. Does your compositor implement ZwlrScreencopy?");
                tracing::error!("err: {e}");
                return Err(Error::ProtocolNotFound(
                    "ZwlrScreencopy Manager not found".to_string(),
                ));
            }
        };

        debug!("Capturing output...");
        let frame = screencopy_manager.capture_output(cursor_overlay, output, &qh, ());

        // Empty internal event buffer until buffer_done is set to true which is when the Buffer done
        // event is fired, aka the capture from the compositor is succesful.
        while !state.buffer_done.load(Ordering::SeqCst) {
            event_queue.blocking_dispatch(&mut state)?;
        }

        tracing::trace!(
            "Received compositor frame buffer formats: {:#?}",
            state.formats
        );
        // Filter advertised wl_shm formats and select the first one that matches.
        let frame_format = state
            .formats
            .iter()
            .find(|frame| {
                matches!(
                    frame.format,
                    wl_shm::Format::Xbgr2101010
                        | wl_shm::Format::Abgr2101010
                        | wl_shm::Format::Argb8888
                        | wl_shm::Format::Xrgb8888
                        | wl_shm::Format::Xbgr8888
                )
            })
            .copied();
        tracing::trace!("Selected frame buffer format: {:#?}", frame_format);

        // Check if frame format exists.
        let frame_format = match frame_format {
            Some(format) => format,
            None => {
                tracing::error!("No suitable frame format found");
                return Err(Error::NoSupportedBufferFormat);
            }
        };
        Ok((state, event_queue, frame, frame_format))
    }

    fn capture_output_frame_inner<T: AsFd>(
        &self,
        mut state: CaptureFrameState,
        mut event_queue: EventQueue<CaptureFrameState>,
        frame: ZwlrScreencopyFrameV1,
        frame_format: FrameFormat,
        fd: T,
    ) -> Result<FrameGuard> {
        // Connecting to wayland environment.
        let qh = event_queue.handle();

        // Bytes of data in the frame = stride * height.
        let frame_bytes = frame_format.stride * frame_format.height;

        // Instantiate shm global.
        let shm = self.globals.bind::<WlShm, _, _>(&qh, 1..=1, ()).unwrap();
        let shm_pool = shm.create_pool(fd.as_fd(), frame_bytes as i32, &qh, ());
        let buffer = shm_pool.create_buffer(
            0,
            frame_format.width as i32,
            frame_format.height as i32,
            frame_format.stride as i32,
            frame_format.format,
            &qh,
            (),
        );

        // Copy the pixel data advertised by the compositor into the buffer we just created.
        frame.copy(&buffer);
        // On copy the Ready / Failed events are fired by the frame object, so here we check for them.
        loop {
            // Basically reads, if frame state is not None then...
            if let Some(state) = state.state {
                match state {
                    FrameState::Failed => {
                        tracing::error!("Frame copy failed");
                        return Err(Error::FramecopyFailed);
                    }
                    FrameState::Finished => {
                        return Ok(FrameGuard { buffer, shm_pool });
                    }
                }
            }

            event_queue.blocking_dispatch(&mut state)?;
        }
    }

    fn capture_output_frame_shm_from_file(
        &self,
        cursor_overlay: bool,
        output: &WlOutput,
        file: &File,
    ) -> Result<(FrameFormat, FrameGuard)> {
        let (state, event_queue, frame, frame_format) =
            self.capture_output_frame_get_state(cursor_overlay as i32, output)?;

        // Bytes of data in the frame = stride * height.
        let frame_bytes = frame_format.stride * frame_format.height;
        file.set_len(frame_bytes as u64)?;

        let frame_guard =
            self.capture_output_frame_inner(state, event_queue, frame, frame_format, file)?;

        Ok((frame_format, frame_guard))
    }

    /// Get a FrameCopy instance with screenshot pixel data for any wl_output object.
    #[tracing::instrument(skip_all, fields(output = output_info.name))]
    fn capture_frame_copy(
        &self,
        cursor_overlay: bool,
        output_info: &OutputInfo,
    ) -> Result<(FrameCopy, FrameGuard)> {
        // Create an in memory file and return it's file descriptor.
        let fd = create_shm_fd()?;
        // Create a writeable memory map backed by a mem_file.
        let mem_file = File::from(fd);

        let (frame_format, frame_guard) = self.capture_output_frame_shm_from_file(
            cursor_overlay,
            &output_info.wl_output,
            &mem_file,
        )?;

        let mut frame_mmap = unsafe { MmapMut::map_mut(&mem_file)? };
        let data = &mut *frame_mmap;
        let frame_color_type = if let Some(converter) = create_converter(frame_format.format) {
            converter.convert_inplace(data)
        } else {
            tracing::error!("Unsupported buffer format: {:?}", frame_format.format);
            tracing::error!("You can send a feature request for the above format to the mailing list for wayshot over at https://sr.ht/~shinyzenith/wayshot.");
            return Err(Error::NoSupportedBufferFormat);
        };
        Ok((
            FrameCopy {
                frame_format,
                frame_color_type,
                frame_mmap,
                transform: output_info.transform,
                position: (
                    output_info.dimensions.x as i64,
                    output_info.dimensions.y as i64,
                ),
            },
            frame_guard,
        ))
    }

    pub fn capture_frame_copies(
        &self,
        outputs: &Vec<OutputInfo>,
        cursor_overlay: bool,
    ) -> Result<Vec<(FrameCopy, FrameGuard, OutputInfo)>> {
        let frame_copies = thread::scope(|scope| -> Result<_> {
            let join_handles = outputs
                .into_iter()
                .map(|output_info| {
                    scope.spawn(move || {
                        self.capture_frame_copy(cursor_overlay, &output_info).map(
                            |(frame_copy, frame_guard)| {
                                (frame_copy, frame_guard, output_info.clone())
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();

            join_handles
                .into_iter()
                .map(|join_handle| join_handle.join())
                .flatten()
                .collect::<Result<_>>()
        })?;

        Ok(frame_copies)
    }


            }

                Ok(())
            })?;
        }
        Ok(())
    }

    /// Take a screenshot from the specified region.
    fn screenshot_region_capturer(
        &self,
        region_capturer: RegionCapturer,
        cursor_overlay: bool,
    ) -> Result<RgbaImage> {
        let outputs = if let RegionCapturer::Outputs(ref outputs) = region_capturer {
            outputs
        } else {
            &self.get_all_outputs()
        };
        let frames = self.capture_frame_copies(outputs, cursor_overlay)?;

        let capture_region: CaptureRegion = match region_capturer {
            RegionCapturer::Outputs(ref outputs) => outputs.try_into()?,
            RegionCapturer::Region(region) => region,
        };

        thread::scope(|scope| {
            let rotate_join_handles = frames
                .into_iter()
                // Filter out the frames that do not contain the capture region.
                .filter(|(frame_copy, _, _)| capture_region.overlaps(&frame_copy.into()))
                .map(|(frame_copy, _, _)| {
                    scope.spawn(move || {
                        let image = (&frame_copy).try_into()?;
                        Ok((
                            image_util::rotate_image_buffer(
                                image,
                                frame_copy.transform,
                                frame_copy.frame_format.width,
                                frame_copy.frame_format.height,
                            ),
                            frame_copy,
                        ))
                    })
                })
                .collect::<Vec<_>>();

            rotate_join_handles
                .into_iter()
                .map(|join_handle| join_handle.join())
                .flatten()
                .fold(
                    None,
                    |composite_image: Option<Result<_>>, image: Result<_>| {
                        // Default to a transparent image.
                        let composite_image = composite_image.unwrap_or_else(|| {
                            Ok(RgbaImage::from_pixel(
                                capture_region.width as u32,
                                capture_region.height as u32,
                                Rgba([0 as u8, 0 as u8, 0 as u8, 255 as u8]),
                            ))
                        });

                        Some(|| -> Result<_> {
                            let mut composite_image = composite_image?;
                            let (image, frame_copy) = image?;
                            replace(
                                &mut composite_image,
                                &image,
                                frame_copy.position.0 - capture_region.x_coordinate as i64,
                                frame_copy.position.1 - capture_region.y_coordinate as i64,
                            );
                            Ok(composite_image)
                        }())
                    },
                )
                .ok_or_else(|| {
                    tracing::error!("Provided capture region doesn't intersect with any outputs!");
                    Error::NoOutputs
                })?
        })
    }

    /// Take a screenshot from the specified region.
    pub fn screenshot(
        &self,
        capture_region: CaptureRegion,
        cursor_overlay: bool,
    ) -> Result<RgbaImage> {
        self.screenshot_region_capturer(RegionCapturer::Region(capture_region), cursor_overlay)
    }

    /// Take a screenshot, overlay the screenshot, run the callback, and then
    /// unfreeze the screenshot and return the selected region.
    pub fn screenshot_freeze(
        &self,
        callback: Box<dyn Fn() -> Result<CaptureRegion>>,
        cursor_overlay: bool,
    ) -> Result<RgbaImage> {
        self.screenshot_region_capturer(RegionCapturer::Freeze(callback), cursor_overlay)
    }
    /// shot one ouput
    pub fn screenshot_single_output(
        &self,
        output_info: &OutputInfo,
        cursor_overlay: bool,
    ) -> Result<RgbaImage> {
        let (frame_copy, _) = self.capture_frame_copy(cursor_overlay, output_info)?;
        (&frame_copy).try_into()
    }

    /// Take a screenshot from all of the specified outputs.
    pub fn screenshot_outputs(
        &self,
        outputs: &Vec<OutputInfo>,
        cursor_overlay: bool,
    ) -> Result<RgbaImage> {
        if outputs.is_empty() {
            return Err(Error::NoOutputs);
        }

        self.screenshot_region_capturer(RegionCapturer::Outputs(outputs.clone()), cursor_overlay)
    }

    /// Take a screenshot from all accessible outputs.
    pub fn screenshot_all(&self, cursor_overlay: bool) -> Result<RgbaImage> {
        self.screenshot_outputs(self.get_all_outputs(), cursor_overlay)
    }
}
