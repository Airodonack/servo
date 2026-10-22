/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions seen from the media backend.
//!
//! The `MediaSource`, `SourceBuffer` and `SourceBufferList` DOM objects in
//! script forward every call to these handles and relay the events they get
//! back. The backend owns everything underneath: the appended data, the
//! buffered ranges, the `updating` flag of each source buffer, the ready state
//! of the media source and its `ended` transition. Nothing about the buffer
//! layout is decided in script.
//!
//! <https://w3c.github.io/media-source/>

use std::ops::Range;

use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericCallback;

/// <https://w3c.github.io/media-source/#dom-readystate>
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MediaSourceReadyState {
    Closed,
    Open,
    Ended,
}

/// <https://w3c.github.io/media-source/#dom-endofstreamerror>
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EndOfStreamError {
    /// Not an error: `endOfStream()` with no argument.
    None,
    Network,
    Decode,
}

/// <https://w3c.github.io/media-source/#dom-appendmode>
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AppendMode {
    Segments,
    Sequence,
}

/// Why a media source or source buffer operation was refused. The DOM maps
/// each one to the exception the spec names for the method that was called.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MediaSourceError {
    /// The object is not in a state where the operation is allowed, such as an
    /// append while `updating` is true or on an ended media source.
    InvalidState,
    /// The content type given is malformed.
    Type,
    /// The content type given is not supported.
    NotSupported,
    /// The source buffer is not one of this media source's.
    NotFound,
    /// The buffer is full and nothing could be evicted.
    QuotaExceeded,
    /// Backend specific error.
    Backend(String),
}

/// What a media source reports back, from any thread. The DOM queues a task
/// for each.
///
/// <https://w3c.github.io/media-source/#mediasource-events>
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MediaSourceEvent {
    SourceOpen,
    SourceEnded,
    SourceClose,
}

/// What a source buffer reports back, from any thread. The DOM queues a task
/// for each.
///
/// <https://w3c.github.io/media-source/#sourcebuffer-events>
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SourceBufferEvent {
    /// `updating` went true: an append or remove has started.
    UpdateStart,
    /// The append or remove succeeded.
    Update,
    /// `updating` went false, after `Update`, `Error` or `Abort`.
    UpdateEnd,
    /// The append failed. The media source has gone to the ended state with
    /// a decode error.
    Error,
    /// The append or remove was aborted.
    Abort,
}

/// Buffered time ranges, in seconds.
pub type BufferedRanges = Vec<Range<f64>>;

/// A media source attached to a player. Dropping the handle detaches it.
///
/// <https://w3c.github.io/media-source/#mediasource>
pub trait MediaSourceHandle: Send {
    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ready_state(&self) -> MediaSourceReadyState;

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    /// `None` is the spec's NaN: no duration yet.
    fn duration(&self) -> Option<f64>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn set_duration(&self, duration: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    /// Events of the new buffer go to `callback`.
    fn add_source_buffer(
        &self,
        content_type: &str,
        callback: GenericCallback<SourceBufferEvent>,
    ) -> Result<Box<dyn SourceBufferHandle>, MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn remove_source_buffer(&self, buffer: &dyn SourceBufferHandle)
    -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn end_of_stream(&self, error: EndOfStreamError) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn set_live_seekable_range(&self, start: f64, end: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn clear_live_seekable_range(&self) -> Result<(), MediaSourceError>;

    /// The range set with `set_live_seekable_range`, if any.
    fn live_seekable_range(&self) -> Option<Range<f64>>;
}

/// One source buffer of an attached media source.
///
/// <https://w3c.github.io/media-source/#sourcebuffer>
pub trait SourceBufferHandle: Send {
    /// Identifies the buffer within its media source, for `remove_source_buffer`.
    fn id(&self) -> usize;

    /// The content type it was created or last changed with.
    fn content_type(&self) -> String;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn updating(&self) -> bool;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn buffered(&self) -> BufferedRanges;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn append_mode(&self) -> AppendMode;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn set_append_mode(&self, mode: AppendMode) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn timestamp_offset(&self) -> f64;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn set_timestamp_offset(&self, offset: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn append_window_start(&self) -> f64;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn set_append_window_start(&self, start: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    /// `None` is the spec's positive infinity.
    fn append_window_end(&self) -> Option<f64>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn set_append_window_end(&self, end: Option<f64>) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    /// Returns once the append has been accepted; its outcome arrives as
    /// `SourceBufferEvent`s.
    fn append(&self, data: Vec<u8>) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn abort(&self) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    /// `end` of `None` is the spec's positive infinity.
    fn remove(&self, start: f64, end: Option<f64>) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn change_type(&self, content_type: &str) -> Result<(), MediaSourceError>;
}
