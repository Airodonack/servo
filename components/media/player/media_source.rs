/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Media Source Extensions seen from the media backend.
//!
//! The `MediaSource` and `SourceBuffer` DOM objects in script forward the
//! operations of the spec to these handles and receive the backend's events.
//! The backend owns the media: the appended data, its parsing, the buffered
//! ranges, eviction and removal. Script keeps the state the spec's algorithms
//! decide: the ready state of the media source, the `updating` flag of each
//! source buffer and the events fired at the page.
//!
//! <https://w3c.github.io/media-source/>

use std::ops::Range;

use serde::{Deserialize, Serialize};
use servo_base::generic_channel::GenericCallback;

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
    /// The argument is malformed or not allowed, with the backend's message
    /// saying which: a content type, or an append mode the byte stream format
    /// forbids.
    Type(String),
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
    /// The backend ended the stream on its own. The DOM makes the "ended"
    /// transition itself, in `endOfStream()` and in the append error
    /// algorithm, and does not need this report for either.
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
    /// The append or remove succeeded: `updating` went false.
    Update,
    /// `updating` went false, after `Update`, `Error` or `Abort`. The DOM
    /// already finished the operation on those, so this only means
    /// something on its own.
    UpdateEnd,
    /// The append failed. The media source has gone to the ended state with
    /// a decode error.
    Error,
    /// The append or remove was aborted.
    Abort,
}

/// Identifies one append or remove on a source buffer, as returned when it is
/// accepted. Numbers are not reused within a source buffer.
pub type OperationId = u64;

/// A `SourceBufferEvent` and the operation it belongs to. A backend may report
/// an operation's outcome after the DOM has finished it (an abort) or started
/// the next one, and report events of operations it starts on its own (an
/// idle tick in mselib 1.28); the DOM acts only on events of the operation
/// in progress.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceBufferReport {
    pub operation: OperationId,
    pub event: SourceBufferEvent,
}

/// Buffered time ranges, in seconds.
pub type BufferedRanges = Vec<Range<f64>>;

/// <https://w3c.github.io/media-source/#init-segment>
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MediaSourceTrackKind {
    Audio,
    Video,
    Text,
}

/// A track described by an initialization segment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaSourceTrack {
    /// Unique within the media source, and stable across calls.
    pub id: String,
    pub kind: MediaSourceTrackKind,
}

/// A media source attached to a player. Dropping the handle detaches it.
/// Source buffers are added here and removed through their own handle.
///
/// <https://w3c.github.io/media-source/#mediasource>
pub trait MediaSourceHandle: Send {
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
        callback: GenericCallback<SourceBufferReport>,
    ) -> Result<Box<dyn SourceBufferHandle>, MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn end_of_stream(&self, error: EndOfStreamError) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn set_live_seekable_range(&self, start: f64, end: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn clear_live_seekable_range(&self) -> Result<(), MediaSourceError>;

    /// The range set with `set_live_seekable_range`, if any.
    fn live_seekable_range(&self) -> Option<Range<f64>>;

    /// The tracks of every initialization segment received so far, over all
    /// source buffers, in the order they were found. The DOM creates the
    /// media element's AudioTrack and VideoTrack objects from these.
    ///
    /// <https://w3c.github.io/media-source/#sourcebuffer-init-segment-received> (step 5)
    fn tracks(&self) -> Vec<MediaSourceTrack>;
}

/// One source buffer of an attached media source.
///
/// <https://w3c.github.io/media-source/#sourcebuffer>
pub trait SourceBufferHandle: Send {
    /// Whether the buffer has received an initialization segment and so
    /// contributes tracks: the spec's membership of `activeSourceBuffers`.
    ///
    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn is_active(&self) -> bool;

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
    fn set_append_window_start(&self, start: f64) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn set_append_window_end(&self, end: Option<f64>) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    /// Returns once the append has been accepted; its outcome arrives as
    /// `SourceBufferReport`s carrying the returned operation.
    fn append(&self, data: Vec<u8>) -> Result<OperationId, MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn abort(&self) -> Result<(), MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    /// `end` of `None` is the spec's positive infinity. Its outcome arrives as
    /// `SourceBufferReport`s carrying the returned operation.
    fn remove(&self, start: f64, end: Option<f64>) -> Result<OperationId, MediaSourceError>;

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn change_type(&self, content_type: &str) -> Result<(), MediaSourceError>;

    /// Removes the buffer from its media source and releases its resources.
    /// The handle is consumed: no operation can follow.
    ///
    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn remove_from_media_source(self: Box<Self>) -> Result<(), MediaSourceError>;
}
