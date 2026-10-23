/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions over GStreamer's mselib (gst-plugins-bad).
//!
//! A `GstMediaSource` is attached to the `msesrc` element that playbin3
//! created from the `mse://` URI. mselib keeps the appended data, the
//! buffered ranges, the `updating` flag and the ready state; these handles
//! only forward calls and relay signals as events.

use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use glib::translate::{FromGlibPtrNone, ToGlibPtr};
use gstreamer::prelude::*;
use gstreamer_mse as gst_mse;
use servo_base::generic_channel::GenericCallback;
use servo_media_player::media_source::{
    AppendMode, BufferedRanges, EndOfStreamError, MediaSourceError, MediaSourceEvent,
    MediaSourceHandle, MediaSourceReadyState, MediaSourceTrack, MediaSourceTrackKind,
    SourceBufferEvent, SourceBufferHandle,
};

static NEXT_SOURCE_BUFFER_ID: AtomicUsize = AtomicUsize::new(0);

fn map_error(error: glib::Error) -> MediaSourceError {
    match error.kind::<gst_mse::MediaSourceError>() {
        Some(gst_mse::MediaSourceError::InvalidState) => MediaSourceError::InvalidState,
        Some(gst_mse::MediaSourceError::Type) => MediaSourceError::Type,
        Some(gst_mse::MediaSourceError::NotSupported) => MediaSourceError::NotSupported,
        Some(gst_mse::MediaSourceError::NotFound) => MediaSourceError::NotFound,
        Some(gst_mse::MediaSourceError::QuotaExceeded) => MediaSourceError::QuotaExceeded,
        _ => MediaSourceError::Backend(error.to_string()),
    }
}

fn clock_time(seconds: f64) -> gstreamer::ClockTime {
    gstreamer::ClockTime::from_nseconds((seconds.max(0.0) * 1_000_000_000.0) as u64)
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
    /// The `msesrc`, which counts the tracks of the initialization segments
    /// received so far.
    mse_src: gst_mse::MseSrc,
}

impl GStreamerMediaSource {
    /// Creates a media source and attaches it to `element`, the `msesrc` of
    /// the pipeline.
    pub fn attach(
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
        Ok(Self { source, mse_src })
    }
}

impl Drop for GStreamerMediaSource {
    fn drop(&mut self) {
        self.source.detach();
    }
}

impl MediaSourceHandle for GStreamerMediaSource {
    fn ready_state(&self) -> MediaSourceReadyState {
        match self.source.ready_state() {
            gst_mse::MediaSourceReadyState::Open => MediaSourceReadyState::Open,
            gst_mse::MediaSourceReadyState::Ended => MediaSourceReadyState::Ended,
            _ => MediaSourceReadyState::Closed,
        }
    }

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
        callback: GenericCallback<SourceBufferEvent>,
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

    fn remove_source_buffer(
        &self,
        buffer: &dyn SourceBufferHandle,
    ) -> Result<(), MediaSourceError> {
        let buffers = self.source.source_buffers();
        let found = (0..buffers.length())
            .filter_map(|i| buffers.index(i))
            .find(|candidate| source_buffer_id(candidate) == Some(buffer.id()));
        match found {
            Some(found) => self.source.remove_source_buffer(&found).map_err(map_error),
            None => Err(MediaSourceError::NotFound),
        }
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

    fn tracks(&self) -> Vec<MediaSourceTrack> {
        // mselib keeps its tracks private; the msesrc counts them by type,
        // updated before the append that delivered an initialization segment
        // completes. Tracks are never removed, so the n-th track of a type
        // keeps its id. The spec has the user agent generate the ids anyway.
        // The caps, and with them the video size, are not exposed, so the
        // size only becomes known with the first frame.
        let kinds = [
            (MediaSourceTrackKind::Audio, "audio", self.mse_src.n_audio()),
            (MediaSourceTrackKind::Video, "video", self.mse_src.n_video()),
            (MediaSourceTrackKind::Text, "text", self.mse_src.n_text()),
        ];
        kinds
            .into_iter()
            .flat_map(|(kind, name, count)| {
                (0..count).map(move |index| MediaSourceTrack {
                    id: format!("{name}{index}"),
                    kind,
                    video_size: None,
                })
            })
            .collect()
    }
}

/// The handle id is kept on the GObject so that `remove_source_buffer` can
/// find the mselib buffer behind a handle without a second registry.
const SOURCE_BUFFER_ID_KEY: &str = "servo-source-buffer-id";

fn source_buffer_id(buffer: &gst_mse::SourceBuffer) -> Option<usize> {
    // SAFETY: the key is only ever set with a `usize` by `GStreamerSourceBuffer::new`,
    // and the value lives as long as the GObject.
    unsafe {
        buffer
            .data::<usize>(SOURCE_BUFFER_ID_KEY)
            .map(|id| *id.as_ref())
    }
}

pub struct GStreamerSourceBuffer {
    id: usize,
    buffer: gst_mse::SourceBuffer,
    /// The media source the buffer belongs to, for its active list.
    source: gst_mse::MediaSource,
}

impl GStreamerSourceBuffer {
    fn new(
        source: &gst_mse::MediaSource,
        buffer: gst_mse::SourceBuffer,
        callback: GenericCallback<SourceBufferEvent>,
    ) -> Self {
        let id = NEXT_SOURCE_BUFFER_ID.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the key is private to this module and the value is a `usize`.
        unsafe { buffer.set_data(SOURCE_BUFFER_ID_KEY, id) };
        let callback = Arc::new(Mutex::new(callback));
        let cb = callback.clone();
        buffer.connect_on_update_start(move |_| notify!(cb, SourceBufferEvent::UpdateStart));
        let cb = callback.clone();
        buffer.connect_on_update(move |_| notify!(cb, SourceBufferEvent::Update));
        let cb = callback.clone();
        buffer.connect_on_update_end(move |_| notify!(cb, SourceBufferEvent::UpdateEnd));
        let cb = callback.clone();
        buffer.connect_on_error(move |_| notify!(cb, SourceBufferEvent::Error));
        let cb = callback;
        buffer.connect_on_abort(move |_| notify!(cb, SourceBufferEvent::Abort));
        Self {
            id,
            buffer,
            source: source.clone(),
        }
    }
}

impl SourceBufferHandle for GStreamerSourceBuffer {
    fn id(&self) -> usize {
        self.id
    }

    fn content_type(&self) -> String {
        self.buffer.content_type().to_string()
    }

    fn updating(&self) -> bool {
        self.buffer.is_updating()
    }

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
        // little later on a background thread. Right at update-end the ranges
        // can lag the append. A fix belongs in gstappendpipeline.c.
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
                let error = glib::Error::from_glib_none(error);
                glib::ffi::g_error_free(error.to_glib_none().0);
                log::warn!("Could not get buffered ranges: {error}");
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
        self.buffer
            .set_timestamp_offset(clock_time(offset))
            .map_err(map_error)
    }

    fn append_window_start(&self) -> f64 {
        seconds(self.buffer.append_window_start()).unwrap_or(0.0)
    }

    fn set_append_window_start(&self, start: f64) -> Result<(), MediaSourceError> {
        self.buffer
            .set_append_window_start(clock_time(start))
            .map_err(map_error)
    }

    fn append_window_end(&self) -> Option<f64> {
        seconds(self.buffer.append_window_end())
    }

    fn set_append_window_end(&self, end: Option<f64>) -> Result<(), MediaSourceError> {
        self.buffer
            .set_append_window_end(end.map(clock_time))
            .map_err(map_error)
    }

    fn append(&self, data: Vec<u8>) -> Result<(), MediaSourceError> {
        self.buffer
            .append_buffer(gstreamer::Buffer::from_mut_slice(data))
            .map_err(map_error)
    }

    fn abort(&self) -> Result<(), MediaSourceError> {
        self.buffer.abort().map_err(map_error)
    }

    fn remove(&self, start: f64, end: Option<f64>) -> Result<(), MediaSourceError> {
        // `None` is positive infinity: mselib takes it as "to the end".
        self.buffer
            .remove(clock_time(start), end.map(clock_time))
            .map_err(map_error)
    }

    fn change_type(&self, content_type: &str) -> Result<(), MediaSourceError> {
        self.buffer
            .change_content_type(content_type)
            .map_err(map_error)
    }
}
