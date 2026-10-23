/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions over GStreamer's mselib (gst-plugins-bad).
//!
//! A `GstMediaSource` is attached to the `msesrc` element that playbin3
//! created from the `mse://` URI. mselib keeps the appended data, the
//! buffered ranges, the `updating` flag and the ready state; these handles
//! only forward calls and relay signals as events.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use glib::translate::{FromGlibPtrFull, ToGlibPtr};
use gstreamer::prelude::*;
use gstreamer_mse as gst_mse;
use servo_base::generic_channel::GenericCallback;
use servo_media_player::PlayerError;
use servo_media_player::media_source::{
    AppendMode, BufferedRanges, EndOfStreamError, MediaSourceError, MediaSourceEvent,
    MediaSourceHandle, OperationId, SourceBufferEvent, SourceBufferHandle, SourceBufferReport,
};

use crate::player::{GStreamerPlayer, clock_time};

/// Whether mselib can parse a `SourceBuffer` of this content type.
pub fn is_type_supported(media_type: &str) -> bool {
    gst_mse::MediaSource::is_type_supported(media_type)
}

/// Creates a media source and attaches it to the player's `msesrc`. Its
/// events go to `callback`.
pub fn attach(
    player: &GStreamerPlayer,
    callback: GenericCallback<MediaSourceEvent>,
) -> Result<Box<dyn MediaSourceHandle>, PlayerError> {
    let element = player.media_source_element()?;
    let source = GStreamerMediaSource::attach(&element, callback)
        .map_err(|error| PlayerError::Backend(format!("{error:?}")))?;
    Ok(Box::new(source))
}

fn map_error(error: glib::Error) -> MediaSourceError {
    match error.kind::<gst_mse::MediaSourceError>() {
        Some(gst_mse::MediaSourceError::InvalidState) => MediaSourceError::InvalidState,
        Some(gst_mse::MediaSourceError::Type) => MediaSourceError::Type(error.to_string()),
        Some(gst_mse::MediaSourceError::NotSupported) => MediaSourceError::NotSupported,
        Some(gst_mse::MediaSourceError::NotFound) => MediaSourceError::NotFound,
        Some(gst_mse::MediaSourceError::QuotaExceeded) => MediaSourceError::QuotaExceeded,
        _ => MediaSourceError::Backend(error.to_string()),
    }
}

fn seconds(time: Option<gstreamer::ClockTime>) -> Option<f64> {
    time.map(|time| time.seconds_f64())
}

/// Sends an event to the DOM from a mselib signal handler. The callbacks are
/// behind a mutex because the signal closures must be `Sync`.
macro_rules! notify {
    ($callback:expr, $event:expr) => {
        if let Err(error) = $callback.lock().unwrap().send($event) {
            log::warn!("Could not deliver media source event: {error:?}");
        }
    };
}

pub struct GStreamerMediaSource {
    source: gst_mse::MediaSource,
}

impl GStreamerMediaSource {
    /// Creates a media source and attaches it to `element`, the `msesrc` of
    /// the pipeline.
    fn attach(
        element: &gstreamer::Element,
        callback: GenericCallback<MediaSourceEvent>,
    ) -> Result<Self, MediaSourceError> {
        let mse_src = element
            .clone()
            .downcast::<gst_mse::MseSrc>()
            .map_err(|_| MediaSourceError::Backend("source element is not a msesrc".into()))?;
        let source = gst_mse::MediaSource::new();
        let callback = Arc::new(Mutex::new(callback));
        let cb = callback.clone();
        source.connect_on_source_open(move |_| notify!(cb, MediaSourceEvent::SourceOpen));
        let cb = callback.clone();
        source.connect_on_source_ended(move |_| notify!(cb, MediaSourceEvent::SourceEnded));
        let cb = callback;
        source.connect_on_source_close(move |_| notify!(cb, MediaSourceEvent::SourceClose));
        source.attach(&mse_src);
        Ok(Self { source })
    }
}

impl Drop for GStreamerMediaSource {
    fn drop(&mut self) {
        self.source.detach();
    }
}

impl MediaSourceHandle for GStreamerMediaSource {
    fn duration(&self) -> Option<f64> {
        seconds(self.source.duration())
    }

    fn set_duration(&self, duration: f64) -> Result<(), MediaSourceError> {
        // A live stream's duration is positive Infinity, which mselib holds as
        // GST_CLOCK_TIME_NONE.
        let duration = duration.is_finite().then(|| clock_time(duration));
        self.source.set_duration(duration).map_err(map_error)
    }

    fn add_source_buffer(
        &self,
        content_type: &str,
        callback: GenericCallback<SourceBufferReport>,
    ) -> Result<Box<dyn SourceBufferHandle>, MediaSourceError> {
        let buffer = self
            .source
            .add_source_buffer(content_type)
            .map_err(map_error)?;
        Ok(Box::new(GStreamerSourceBuffer::new(
            &self.source,
            buffer,
            callback,
        )))
    }

    fn end_of_stream(&self, error: EndOfStreamError) -> Result<(), MediaSourceError> {
        let error = match error {
            EndOfStreamError::None => gst_mse::MediaSourceEOSError::None,
            EndOfStreamError::Network => gst_mse::MediaSourceEOSError::Network,
            EndOfStreamError::Decode => gst_mse::MediaSourceEOSError::Decode,
        };
        self.source.end_of_stream(error).map_err(map_error)
    }

    fn set_live_seekable_range(&self, start: f64, end: f64) -> Result<(), MediaSourceError> {
        self.source
            .set_live_seekable_range(clock_time(start), clock_time(end))
            .map_err(map_error)
    }

    fn clear_live_seekable_range(&self) -> Result<(), MediaSourceError> {
        self.source.clear_live_seekable_range().map_err(map_error)
    }

    fn live_seekable_range(&self) -> Option<Range<f64>> {
        let range = self.source.live_seekable_range();
        let (start, end) = (range.start(), range.end());
        if start == end {
            return None;
        }
        Some(start.seconds_f64()..end.seconds_f64())
    }
}

pub struct GStreamerSourceBuffer {
    buffer: gst_mse::SourceBuffer,
    /// The media source the buffer belongs to, for its active list and for
    /// removing the buffer from it.
    source: gst_mse::MediaSource,
    operations: Arc<Mutex<Operations>>,
}

/// Which operation mselib's signals belong to. mselib signals update-start once
/// for every append and remove it accepts, in order, and every later signal
/// until the next update-start belongs to that operation. It also signals
/// update and update-end with no operation behind them (1.28.6 and main: an
/// idle buffer signals both every second), which this attributes to the
/// operation that already ended.
#[derive(Default)]
struct Operations {
    /// The last operation number handed out.
    last: OperationId,
    /// Accepted operations whose update-start has not been signalled yet.
    pending: VecDeque<OperationId>,
    /// The operation the signals belong to: the one whose update-start came last.
    signalled: OperationId,
}

impl GStreamerSourceBuffer {
    fn new(
        source: &gst_mse::MediaSource,
        buffer: gst_mse::SourceBuffer,
        callback: GenericCallback<SourceBufferReport>,
    ) -> Self {
        let operations = Arc::new(Mutex::new(Operations::default()));
        let callback = Arc::new(Mutex::new(callback));
        let signalled_operations = operations.clone();
        let report = move |event: SourceBufferEvent| {
            let callback = callback.clone();
            let operations = signalled_operations.clone();
            move |_: &gst_mse::SourceBuffer| {
                let operation = {
                    let mut operations = operations.lock().unwrap();
                    if event == SourceBufferEvent::UpdateStart {
                        operations.signalled = operations.pending.pop_front().unwrap_or_default();
                    }
                    operations.signalled
                };
                notify!(
                    callback,
                    SourceBufferReport {
                        operation,
                        event: event.clone()
                    }
                );
            }
        };
        buffer.connect_on_update_start(report(SourceBufferEvent::UpdateStart));
        buffer.connect_on_update(report(SourceBufferEvent::Update));
        buffer.connect_on_update_end(report(SourceBufferEvent::UpdateEnd));
        buffer.connect_on_error(report(SourceBufferEvent::Error));
        buffer.connect_on_abort(report(SourceBufferEvent::Abort));
        Self {
            buffer,
            source: source.clone(),
            operations,
        }
    }

    /// Runs `start`, an mselib call that signals update-start once if it
    /// succeeds, as a numbered operation. The number is queued first: mselib
    /// can signal update-start on its own thread before the call returns.
    fn start_operation(
        &self,
        start: impl FnOnce() -> Result<(), glib::Error>,
    ) -> Result<OperationId, MediaSourceError> {
        let operation = {
            let mut operations = self.operations.lock().unwrap();
            operations.last += 1;
            let operation = operations.last;
            operations.pending.push_back(operation);
            operation
        };
        start().map_err(|error| {
            // Refused: no update-start will come for it.
            self.operations
                .lock()
                .unwrap()
                .pending
                .retain(|pending| *pending != operation);
            map_error(error)
        })?;
        Ok(operation)
    }
}

impl SourceBufferHandle for GStreamerSourceBuffer {
    fn is_active(&self) -> bool {
        let active = self.source.active_source_buffers();
        (0..active.length())
            .filter_map(|i| active.index(i))
            .any(|candidate| candidate == self.buffer)
    }

    fn buffered(&self) -> BufferedRanges {
        // Note: mselib (checked in 1.26.2, 1.28.6 and main) signals update-end
        // as soon as the appended bytes are pushed into its parsing pipeline;
        // the parsed samples reach the track buffers, and so these ranges, a
        // little later on a background thread. So `SourceBuffer.buffered`
        // read in an `updateend` handler, which players do to decide what to
        // fetch next, may not yet include the segment just appended. This
        // backend does not work around it; a fix in GStreamer's mselib makes
        // the append task wait for the parsed samples before update-end.
        // The binding leaves `gst_source_buffer_get_buffered` unimplemented
        // (it returns a GArray of GstMediaSourceRange), so call it directly.
        let mut error = std::ptr::null_mut();
        // SAFETY: a plain FFI call; the array is owned by us afterwards and
        // read within its length before being released.
        unsafe {
            let array = gstreamer_mse::ffi::gst_source_buffer_get_buffered(
                self.buffer.to_glib_none().0,
                &mut error,
            );
            if !error.is_null() {
                let error = glib::Error::from_glib_full(error);
                log::warn!("Could not get buffered ranges: {error}");
                if !array.is_null() {
                    glib::ffi::g_array_unref(array);
                }
                return vec![];
            }
            if array.is_null() {
                return vec![];
            }
            let len = (*array).len as usize;
            let data = (*array).data as *const gstreamer_mse::ffi::GstMediaSourceRange;
            let ranges = (0..len)
                .map(|i| {
                    let range = &*data.add(i);
                    (range.start as f64 / 1e9)..(range.end as f64 / 1e9)
                })
                .collect();
            glib::ffi::g_array_unref(array);
            ranges
        }
    }

    fn append_mode(&self) -> AppendMode {
        match self.buffer.append_mode() {
            gst_mse::SourceBufferAppendMode::Sequence => AppendMode::Sequence,
            _ => AppendMode::Segments,
        }
    }

    fn set_append_mode(&self, mode: AppendMode) -> Result<(), MediaSourceError> {
        let mode = match mode {
            AppendMode::Segments => gst_mse::SourceBufferAppendMode::Segments,
            AppendMode::Sequence => gst_mse::SourceBufferAppendMode::Sequence,
        };
        self.buffer.set_append_mode(mode).map_err(map_error)
    }

    fn timestamp_offset(&self) -> f64 {
        seconds(self.buffer.timestamp_offset()).unwrap_or(0.0)
    }

    fn set_timestamp_offset(&self, offset: f64) -> Result<(), MediaSourceError> {
        // mselib (1.28.6 and main) holds the offset as an unsigned GstClockTime.
        if offset < 0.0 {
            log::warn!("Negative timestampOffset {offset} is not supported; using 0");
        }
        self.buffer
            .set_timestamp_offset(clock_time(offset))
            .map_err(map_error)
    }

    fn set_append_window_start(&self, start: f64) -> Result<(), MediaSourceError> {
        self.buffer
            .set_append_window_start(clock_time(start))
            .map_err(map_error)
    }

    fn set_append_window_end(&self, end: Option<f64>) -> Result<(), MediaSourceError> {
        self.buffer
            .set_append_window_end(end.map(clock_time))
            .map_err(map_error)
    }

    fn append(&self, data: Vec<u8>) -> Result<OperationId, MediaSourceError> {
        self.start_operation(|| {
            self.buffer
                .append_buffer(gstreamer::Buffer::from_mut_slice(data))
        })
    }

    fn abort(&self) -> Result<(), MediaSourceError> {
        self.buffer.abort().map_err(map_error)
    }

    fn remove(&self, start: f64, end: Option<f64>) -> Result<OperationId, MediaSourceError> {
        // `None` is positive infinity: mselib takes it as "to the end". So is an end beyond
        // what a GstClockTime holds, which would otherwise clamp to the same time as a start
        // that far out, and mselib refuses an empty range.
        let end = end
            .map(clock_time)
            .filter(|end| *end != gstreamer::ClockTime::MAX);
        self.start_operation(|| self.buffer.remove(clock_time(start), end))
    }

    fn change_type(&self, content_type: &str) -> Result<(), MediaSourceError> {
        self.buffer
            .change_content_type(content_type)
            .map_err(map_error)
    }

    fn remove_from_media_source(self: Box<Self>) -> Result<(), MediaSourceError> {
        self.source
            .remove_source_buffer(&self.buffer)
            .map_err(map_error)
    }
}
