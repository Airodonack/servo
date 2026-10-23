/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use mime::Mime;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_weak_referenceable_dom_object_with_proto;
use servo_base::generic_channel::GenericCallback;
use servo_media::ServoMedia;
use servo_media::player::Player;
use servo_media::player::media_source::{
    EndOfStreamError as BackendEndOfStreamError, MediaSourceError, MediaSourceEvent,
    MediaSourceHandle,
};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, ReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot, MutNullableDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::weakref::{WeakRef, WeakReferenceable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::htmlmediaelement::HTMLMediaElement;
use crate::dom::sourcebuffer::SourceBuffer;
use crate::dom::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;
use crate::tasks::task_source::SendableTaskSource;

/// The longest a coded frame is taken to last, in seconds, where the duration
/// change algorithm needs the presentation timestamp of the last buffered frame
/// and only the end of the buffered ranges is known. web-platform-tests' media
/// source tests assume the same bound for their media.
const LONGEST_FRAME: f64 = 0.1;

/// How many SourceBuffers a MediaSource may have at once. Each one runs a
/// parsing pipeline and several threads in the backend. Pages use one per
/// stream type (audio, video, text).
const MAX_SOURCE_BUFFERS: usize = 4;

/// What a backend callback holds to reach a DOM object: a weak reference
/// behind an `Arc` that only the DOM object owns, through
/// [`BackendCallbackTarget::new`]. The callback keeps a `std::sync::Weak`, so
/// it never keeps the object alive (the object owns the backend, which owns
/// the callback, and a strong reference back would keep both for good), and
/// the `WeakRef`, which is not thread-safe, is only used and dropped on the
/// script thread.
pub(crate) struct BackendCallbackTarget<T: WeakReferenceable>(WeakRef<T>);

#[expect(unsafe_code)]
unsafe impl<T: WeakReferenceable> Send for BackendCallbackTarget<T> {}

impl<T: WeakReferenceable> BackendCallbackTarget<T> {
    /// The strong reference, for `object` to keep, and the weak one, for the
    /// callback.
    pub(crate) fn new(object: &T) -> (Arc<Mutex<Self>>, Weak<Mutex<Self>>) {
        let target = Arc::new(Mutex::new(Self(WeakRef::new(object))));
        let weak = Arc::downgrade(&target);
        (target, weak)
    }

    /// On the script thread: the object, if it is still alive.
    pub(crate) fn root(target: &Weak<Mutex<Self>>) -> Option<DomRoot<T>> {
        target.upgrade()?.lock().unwrap().0.root()
    }
}

/// Maps a backend refusal to the exception the Media Source spec names.
pub(crate) fn media_source_error(error: MediaSourceError) -> Error {
    match error {
        MediaSourceError::InvalidState => Error::InvalidState(None),
        MediaSourceError::Type => Error::Type(c"Invalid content type".to_owned()),
        MediaSourceError::NotSupported => Error::NotSupported(None),
        MediaSourceError::NotFound => Error::NotFound(None),
        MediaSourceError::QuotaExceeded => Error::QuotaExceeded {
            quota: None,
            requested: None,
        },
        MediaSourceError::Backend(message) => Error::InvalidState(Some(message)),
    }
}

/// Whether `type_` names a byte stream format and codecs that could go through
/// a SourceBuffer, before asking the backend whether they decode: the MIME
/// type must be one of the registered byte stream formats, MP4 and WebM must
/// name their codecs, the MPEG audio formats must not, and every codec must
/// belong to the container and have the shape the registry gives it.
///
/// <https://w3c.github.io/mse-byte-stream-format-registry/>
pub(crate) fn is_valid_media_source_type(type_: &str) -> bool {
    let Ok(mime) = type_.trim().parse::<Mime>() else {
        return false;
    };
    let essence = format!("{}/{}", mime.type_(), mime.subtype()).to_ascii_lowercase();
    let codecs: Option<Vec<String>> = mime.get_param("codecs").map(|codecs| {
        codecs
            .as_str()
            .split(',')
            .map(|codec| codec.trim().to_ascii_lowercase())
            .filter(|codec| !codec.is_empty())
            .collect()
    });
    let valid_codec = |codec: &str, video_allowed: bool, container: &str| -> bool {
        let id = codec.split('.').next().unwrap_or("");
        let video = matches!(
            id,
            "avc1" | "avc3" | "hev1" | "hvc1" | "av01" | "vp8" | "vp09" | "vp9"
        );
        let audio = matches!(
            id,
            "mp4a" | "opus" | "flac" | "vorbis" | "mp3" | "ac-3" | "ec-3"
        );
        if !(video || audio) || (video && !video_allowed) {
            return false;
        }
        match container {
            "mp4" => {
                if !matches!(
                    id,
                    "avc1" |
                        "avc3" |
                        "hev1" |
                        "hvc1" |
                        "av01" |
                        "vp09" |
                        "mp4a" |
                        "opus" |
                        "flac" |
                        "ac-3" |
                        "ec-3"
                ) {
                    return false;
                }
                // mp4a.XX[.N] and avc1.XXXXXX need their object type / profile.
                match id {
                    // mp4a.<object type>, with the audio object type as a
                    // third part exactly when the object type is 40 (AAC).
                    "mp4a" => {
                        let parts: Vec<&str> = codec.split('.').collect();
                        match parts.as_slice() {
                            ["mp4a", oti] => {
                                oti.len() == 2 &&
                                    *oti != "40" &&
                                    oti.chars().all(|c| c.is_ascii_hexdigit())
                            },
                            ["mp4a", "40", aot] => {
                                !aot.is_empty() && aot.chars().all(|c| c.is_ascii_digit())
                            },
                            _ => false,
                        }
                    },
                    "avc1" | "avc3" => codec.len() > 5,
                    _ => true,
                }
            },
            "webm" => matches!(id, "vp8" | "vp9" | "vorbis" | "opus" | "av01"),
            _ => false,
        }
    };
    match essence.as_str() {
        "video/mp4" | "audio/mp4" | "video/webm" | "audio/webm" => {
            let container = &essence[6..];
            let video_allowed = essence.starts_with("video/");
            match codecs {
                Some(codecs) if !codecs.is_empty() => codecs
                    .iter()
                    .all(|codec| valid_codec(codec, video_allowed, container)),
                _ => false,
            }
        },
        "audio/mpeg" | "audio/aac" => codecs.is_none(),
        _ => false,
    }
}

/// <https://w3c.github.io/media-source/#mediasource>
#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    source_buffers: Dom<SourceBufferList>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    active_source_buffers: Dom<SourceBufferList>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    ready_state: Cell<ReadyState>,
    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    duration: Cell<f64>,
    /// The element this media source is attached to, while it is.
    ///
    /// <https://w3c.github.io/media-source/#mediasource-attach>
    media_element: MutNullableDom<HTMLMediaElement>,
    /// The backend side of this media source, while attached. The backend
    /// owns the appended data, the buffered ranges and the ended transition.
    #[ignore_malloc_size_of = "servo_media"]
    #[no_trace]
    handle: DomRefCell<Option<Box<dyn MediaSourceHandle>>>,
    /// What the backend's callback reaches this object through.
    #[ignore_malloc_size_of = "Arc"]
    #[no_trace]
    callback_target: RefCell<Option<Arc<Mutex<BackendCallbackTarget<MediaSource>>>>>,
    /// The ids of every track announced to the media element, so that a track
    /// of a removed SourceBuffer, which the backend still lists, is not
    /// announced again for the next one.
    announced_track_ids: DomRefCell<HashSet<String>>,
}

impl MediaSource {
    fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            ready_state: Cell::new(ReadyState::Closed),
            duration: Cell::new(f64::NAN),
            media_element: Default::default(),
            handle: DomRefCell::new(None),
            callback_target: RefCell::new(None),
            announced_track_ids: Default::default(),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        let source_buffers = SourceBufferList::new(cx, global);
        let active_source_buffers = SourceBufferList::new(cx, global);
        reflect_weak_referenceable_dom_object_with_proto(
            cx,
            Rc::new(MediaSource::new_inherited(
                &source_buffers,
                &active_source_buffers,
            )),
            global,
            proto,
        )
    }

    pub(crate) fn ready_state(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// The duration as the `duration` attribute reports it: NaN while closed.
    pub(crate) fn duration(&self) -> f64 {
        if self.ready_state.get() == ReadyState::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// Whether the media element this is attached to has an error: the
    /// prepare append algorithm's recent element error.
    pub(crate) fn element_has_error(&self) -> bool {
        self.media_element
            .get()
            .is_some_and(|element| element.in_error_state())
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.handle.borrow().is_some()
    }

    /// Whether any source buffer has an append or remove in progress.
    fn any_updating(&self) -> bool {
        self.source_buffers
            .buffers()
            .iter()
            .any(|buffer| buffer.updating())
    }

    /// <https://w3c.github.io/media-source/#mediasource-attach>
    ///
    /// Called by the media element's resource fetch algorithm once it has
    /// created a player of the media source kind. The backend reports the
    /// open transition through [`MediaSourceEvent::SourceOpen`].
    pub(crate) fn attach(
        &self,
        element: &HTMLMediaElement,
        player: &Arc<Mutex<dyn Player>>,
    ) -> Result<(), Error> {
        if self.is_attached() || self.ready_state.get() != ReadyState::Closed {
            return Err(Error::InvalidState(Some(
                "The media source is already attached".into(),
            )));
        }
        let (target, weak_target) = BackendCallbackTarget::new(self);
        *self.callback_target.borrow_mut() = Some(target);
        let task_source: SendableTaskSource = self
            .global()
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message| {
            let Ok(event) = message else {
                return;
            };
            let weak_target = weak_target.clone();
            task_source.queue(task!(media_source_event: move |cx| {
                if let Some(this) = BackendCallbackTarget::root(&weak_target) {
                    this.handle_backend_event(cx, event);
                }
            }));
        })
        .map_err(|_| Error::InvalidState(Some("Could not create a callback".into())))?;
        let handle = player
            .lock()
            .unwrap()
            .attach_media_source(callback)
            .map_err(|error| Error::NotSupported(Some(format!("{error:?}"))))?;
        *self.handle.borrow_mut() = Some(handle);
        self.media_element.set(Some(element));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#mediasource-detach>
    pub(crate) fn detach(&self, cx: &mut JSContext) {
        // Step 1. Set the readyState attribute to "closed".
        self.ready_state.set(ReadyState::Closed);
        // Step 2. Update duration to NaN.
        self.duration.set(f64::NAN);
        // Steps 3-6. Remove all the SourceBuffer objects from activeSourceBuffers and
        // sourceBuffers, with their events.
        for buffer in self.source_buffers.buffers() {
            buffer.mark_removed();
        }
        self.active_source_buffers.clear(cx);
        self.source_buffers.clear(cx);
        // Dropping the handle detaches the backend side.
        *self.handle.borrow_mut() = None;
        *self.callback_target.borrow_mut() = None;
        self.announced_track_ids.borrow_mut().clear();
        self.media_element.set(None);
        // Step 7. Queue a task to fire an event named sourceclose at the MediaSource.
        self.queue_event("sourceclose");
    }

    fn handle_backend_event(&self, _cx: &mut JSContext, event: MediaSourceEvent) {
        if !self.is_attached() {
            return;
        }
        match event {
            // The attachment's open transition. Reopening an ended media
            // source is done by the DOM, which the backend reports as well.
            MediaSourceEvent::SourceOpen => {
                if self.ready_state.get() == ReadyState::Closed {
                    self.open();
                }
            },
            // The DOM ends the stream itself: in endOfStream() and in the
            // append error algorithm, when a SourceBuffer reports an error.
            // The backend's report of either can arrive after the page has
            // reopened the media source.
            MediaSourceEvent::SourceEnded => {},
            MediaSourceEvent::SourceClose => {
                if self.ready_state.get() != ReadyState::Closed {
                    self.ready_state.set(ReadyState::Closed);
                    self.queue_event("sourceclose");
                }
            },
        }
    }

    /// Sets the readyState to "open" and queues sourceopen, if not open yet.
    ///
    /// <https://w3c.github.io/media-source/#mediasource-attach> (step 5) and
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    /// (an append on an ended media source reopens it).
    pub(crate) fn open(&self) {
        if self.ready_state.get() == ReadyState::Open {
            return;
        }
        self.ready_state.set(ReadyState::Open);
        self.queue_event("sourceopen");
    }

    /// <https://w3c.github.io/media-source/#duration-change-algorithm>
    fn change_duration(&self, new_duration: f64) -> ErrorResult {
        // Step 1. If the current value of duration is equal to new duration,
        // then return.
        if self.duration.get() == new_duration {
            return Ok(());
        }
        // Step 2. If new duration is less than the highest presentation
        // timestamp of any buffered coded frames for all SourceBuffer objects
        // in sourceBuffers, then throw an InvalidStateError exception.
        // Steps 3-4. Let highest end time be the largest track buffer ranges
        // end time across all SourceBuffer objects; if new duration is less,
        // update it to highest end time.
        // The backend reports buffered ranges, not the timestamps of the frames
        // in them. Between the highest presentation timestamp and the highest
        // end time there is at most the last frame, so a new duration within
        // LONGEST_FRAME of the highest end time is taken to be in it and
        // raised; one further below is refused. Buffered media is never
        // truncated.
        let highest_end_time = self
            .source_buffers
            .buffers()
            .iter()
            .flat_map(|buffer| buffer.buffered_ranges())
            .map(|(_, end)| end)
            .fold(f64::NEG_INFINITY, f64::max);
        if new_duration < highest_end_time - LONGEST_FRAME {
            return Err(Error::InvalidState(Some(
                "The duration is below the buffered media".into(),
            )));
        }
        let new_duration = new_duration.max(highest_end_time);
        // Step 5. Update duration to new duration.
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle.set_duration(new_duration).map_err(media_source_error)?;
        }
        // Step 6. Update the media element's duration and run its duration
        // change algorithm.
        self.set_duration_internal(new_duration);
        Ok(())
    }

    /// Duration change algorithm, steps 4-5: the new duration, and the media
    /// element's duration with it.
    ///
    /// <https://w3c.github.io/media-source/#duration-change-algorithm>
    fn set_duration_internal(&self, duration: f64) {
        if self.duration.get() == duration {
            return;
        }
        self.duration.set(duration);
        if let Some(element) = self.media_element.get() {
            element.media_source_duration_changed(duration);
        }
    }

    /// Initialization segment received, for a buffer whose first one the
    /// backend has processed.
    ///
    /// <https://w3c.github.io/media-source/#sourcebuffer-init-segment-received>
    pub(crate) fn first_initialization_segment_received(
        &self,
        cx: &mut JSContext,
        buffer: &SourceBuffer,
    ) {
        // Step 1. Update the duration attribute if it currently equals NaN.
        self.refresh_duration();
        let element = self.media_element.get();
        // Step 5. For each audio, video and text track in the initialization
        // segment, create the track on the media element. The backend lists
        // the tracks of all buffers; those the element already has came from
        // another buffer's initialization segment.
        let tracks = self
            .handle
            .borrow()
            .as_ref()
            .map(|handle| handle.tracks())
            .unwrap_or_default();
        let new_tracks: Vec<_> = {
            let announced = self.announced_track_ids.borrow();
            tracks
                .iter()
                .filter(|track| !announced.contains(&track.id))
                .cloned()
                .collect()
        };
        if let Some(element) = element.as_ref() {
            let added = element.add_media_source_tracks(cx, &new_tracks);
            self.announced_track_ids
                .borrow_mut()
                .extend(added.iter().cloned());
            buffer.set_track_ids(added);
        }
        // Step 5.4. If active track flag equals true, add this SourceBuffer
        // to activeSourceBuffers and queue addsourcebuffer at it.
        buffer.set_active(true);
        self.active_source_buffers.add(buffer);
        // Step 8. If each object in sourceBuffers of the parent media source
        // has [[first initialization segment received flag]] equal to true,
        // and the HTMLMediaElement's readyState attribute is HAVE_NOTHING,
        // then set it to HAVE_METADATA.
        // Step 7, which drops a readyState above HAVE_CURRENT_DATA back to
        // HAVE_METADATA when a buffer added during playback gets its first
        // initialization segment, is not implemented: nothing would raise
        // the readyState again afterwards.
        let all_received = self
            .source_buffers
            .buffers()
            .iter()
            .all(|buffer| buffer.first_initialization_segment_received());
        if all_received && let Some(element) = element {
            element.media_source_metadata_received(cx, &tracks);
        }
    }

    /// Takes the duration mselib derived from the appended data when script
    /// never set one.
    ///
    /// <https://w3c.github.io/media-source/#dfn-initialization-segment-received> (step 5)
    fn refresh_duration(&self) {
        if !self.duration.get().is_nan() {
            return;
        }
        let duration = self
            .handle
            .borrow()
            .as_ref()
            .and_then(|handle| handle.duration())
            .filter(|duration| *duration > 0.0);
        // Step 5.1-5.2. If the initialization segment contains a duration,
        // use it; otherwise the duration is positive Infinity.
        self.set_duration_internal(duration.unwrap_or(f64::INFINITY));
    }

    /// End of stream algorithm, run on the DOM side. The backend has been
    /// told already, or ended its side itself (an append error).
    ///
    /// <https://w3c.github.io/media-source/#end-of-stream-algorithm>
    fn end_of_stream_steps(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) {
        // Steps 1-2. Change the readyState attribute value to "ended" and
        // queue a task to fire an event named sourceended.
        self.ready_state.set(ReadyState::Ended);
        self.queue_event("sourceended");
        match error {
            // Step 3, error not set: run the duration change algorithm with
            // the largest track buffer ranges end time across all
            // SourceBuffer objects in sourceBuffers.
            None => {
                let largest_end_time = self
                    .source_buffers
                    .buffers()
                    .iter()
                    .flat_map(|buffer| buffer.buffered_ranges())
                    .map(|(_, end)| end)
                    .fold(0.0, f64::max);
                if let Err(error) = self.change_duration(largest_end_time) {
                    warn!("Could not set the duration at the end of stream: {error:?}");
                }
            },
            // Error set to "network" or "decode": the media element's network
            // or decode error steps.
            Some(error) => {
                if let Some(element) = self.media_element.get() {
                    element.media_source_ended_with_error(cx, error == EndOfStreamError::Network);
                }
            },
        }
    }

    /// Append error algorithm, step 5: run the end of stream algorithm with
    /// the error parameter set to "decode".
    ///
    /// <https://w3c.github.io/media-source/#sourcebuffer-append-error>
    pub(crate) fn append_error(&self, cx: &mut JSContext) {
        if self.ready_state.get() == ReadyState::Open {
            self.end_of_stream_steps(cx, Some(EndOfStreamError::Decode));
        }
    }

    fn queue_event(&self, name: &str) {
        let this = Trusted::new(self);
        let name = Atom::from(name);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(media_source_queued_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, name);
            }));
    }

    pub(crate) fn handle(&self) -> std::cell::Ref<'_, Option<Box<dyn MediaSourceHandle>>> {
        self.handle.borrow()
    }

    /// The media element's `buffered` while a MediaSource is attached:
    /// the intersection of the source buffers' buffered ranges, the highest
    /// end extended to the end of the last range of any buffer when ended.
    ///
    /// <https://w3c.github.io/media-source/#htmlmediaelement-extensions-buffered>
    pub(crate) fn buffered(&self) -> Vec<(f64, f64)> {
        let buffers: Vec<Vec<(f64, f64)>> = self
            .active_source_buffers
            .buffers()
            .iter()
            .map(|buffer| buffer.buffered_ranges())
            .collect();
        // Step 1-2. If activeSourceBuffers.length equals 0 or no buffer has
        // ranges, return an empty TimeRanges.
        if buffers.is_empty() || buffers.iter().any(|ranges| ranges.is_empty()) {
            return vec![];
        }
        // Step 3. Let highest end time be the largest range end time in the
        // active ranges.
        let highest_end = buffers
            .iter()
            .filter_map(|ranges| ranges.last().map(|range| range.1))
            .fold(0.0, f64::max);
        // Step 4. Let intersection ranges equal a TimeRanges object containing
        // a single range from 0 to highest end time.
        let mut intersection = vec![(0.0, highest_end)];
        // Step 5. For each SourceBuffer object in activeSourceBuffers run the
        // following steps:
        for ranges in &buffers {
            let mut ranges = ranges.clone();
            // Step 5.2. If readyState is "ended", then set the end time on the
            // last range in source ranges to highest end time.
            if self.ready_state.get() == ReadyState::Ended &&
                let Some(last) = ranges.last_mut()
            {
                last.1 = highest_end;
            }
            // Step 5.3. Let new intersection ranges equal the intersection
            // between the intersection ranges and the source ranges.
            intersection = intersect_ranges(&intersection, &ranges);
        }
        intersection
    }

    /// The media element's `seekable` while a MediaSource is attached.
    ///
    /// <https://w3c.github.io/media-source/#htmlmediaelement-extensions-seekable>
    pub(crate) fn seekable(&self) -> Vec<(f64, f64)> {
        let duration = self.duration.get();
        // If duration equals NaN: return an empty TimeRanges object.
        if duration.is_nan() {
            return vec![];
        }
        if duration.is_infinite() {
            let live_range = self
                .handle
                .borrow()
                .as_ref()
                .and_then(|handle| handle.live_seekable_range());
            let buffered = self.buffered();
            // If live seekable range is not empty: let union ranges be the
            // union of live seekable range and the HTMLMediaElement's buffered
            // attribute, and return a single range from the earliest start
            // time to the highest end time in union ranges.
            if let Some(live_range) = live_range {
                let start = buffered
                    .iter()
                    .map(|range| range.0)
                    .fold(live_range.start, f64::min);
                let end = buffered
                    .iter()
                    .map(|range| range.1)
                    .fold(live_range.end, f64::max);
                return vec![(start, end)];
            }
            // If the HTMLMediaElement.buffered attribute returns an empty
            // TimeRanges object, then return an empty TimeRanges object.
            // Return a single range with a start time of 0 and an end time
            // equal to the highest end time reported by buffered.
            return match buffered.last() {
                Some(last) => vec![(0.0, last.1)],
                None => vec![],
            };
        }
        // Otherwise: return a single range with a start time of 0 and an end
        // time equal to duration.
        vec![(0.0, duration)]
    }
}

fn intersect_ranges(a: &[(f64, f64)], b: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut result = vec![];
    for &(start_a, end_a) in a {
        for &(start_b, end_b) in b {
            let start = start_a.max(start_b);
            let end = end_a.min(end_b);
            if start < end {
                result.push((start, end));
            }
        }
    }
    result
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        global: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        MediaSource::new_with_proto(cx, &global.global(), proto)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&self.active_source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> ReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn Duration(&self) -> f64 {
        // Step 1. If the readyState attribute is "closed" then return NaN.
        if self.ready_state.get() == ReadyState::Closed {
            return f64::NAN;
        }
        self.duration.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, value: f64) -> ErrorResult {
        debug!("MediaSource: duration = {value}");
        // Step 1. If the value being set is negative or NaN then throw a TypeError.
        if value.is_nan() || value < 0.0 {
            return Err(Error::Type(c"Duration is negative or NaN".to_owned()));
        }
        // Step 2. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 3. If the updating attribute equals true on any SourceBuffer in
        // sourceBuffers, then throw an InvalidStateError exception.
        if self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        // Step 4. Run the duration change algorithm with new duration set to
        // the value being assigned to this attribute.
        self.change_duration(value)
    }

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceopen
    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceended
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);

    // https://w3c.github.io/media-source/#dom-mediasource-onsourceclose
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);

    /// <https://w3c.github.io/media-source/#dom-mediasource-canconstructindedicatedworker>
    fn CanConstructInDedicatedWorker(_global: &Window) -> bool {
        false
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(
        &self,
        cx: &mut JSContext,
        type_: DOMString,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(c"The content type is empty".to_owned()));
        }
        // Step 2. If type contains a MIME type that is not supported or
        // contains a MIME type that is not supported with the types specified
        // for the other SourceBuffer objects in sourceBuffers, then throw a
        // NotSupportedError exception.
        if !is_valid_media_source_type(&type_.str()) ||
            !ServoMedia::get().is_media_source_type_supported(&type_.str())
        {
            debug!("MediaSource: addSourceBuffer({type_}) refused: unsupported type");
            return Err(Error::NotSupported(None));
        }
        debug!("MediaSource: addSourceBuffer({type_})");
        // Step 3. If the user agent can't handle any more SourceBuffer objects,
        // then throw a QuotaExceededError exception.
        if self.source_buffers.buffers().len() >= MAX_SOURCE_BUFFERS {
            return Err(Error::QuotaExceeded {
                quota: None,
                requested: None,
            });
        }
        // Step 4. If the readyState attribute is not in the "open" state then
        // throw an InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Steps 5-7. Let buffer be a new SourceBuffer with its associated
        // resources; the backend sets its mode from the byte stream format.
        let buffer = SourceBuffer::new(cx, &self.global(), self, &type_.str())?;
        // Steps 8-9. Append buffer to sourceBuffers and queue a task to fire an
        // event named addsourcebuffer at sourceBuffers. It joins
        // activeSourceBuffers once its first initialization segment is
        // processed.
        self.source_buffers.add(&buffer);
        // Step 10. Return buffer.
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, _cx: &mut JSContext, source_buffer: &SourceBuffer) -> ErrorResult {
        // Step 1. If sourceBuffer specifies an object that is not in
        // sourceBuffers then throw a NotFoundError exception.
        if !self.source_buffers.contains(source_buffer) {
            return Err(Error::NotFound(None));
        }
        // Step 2. If the sourceBuffer.updating attribute equals true, then
        // abort the buffer append algorithm, set updating to false and queue
        // update-abort and updateend events.
        if source_buffer.updating() {
            source_buffer.abort_update();
        }
        // Steps 3-5. Remove the audio, video and text tracks the buffer created from the media
        // element's lists, queueing removetrack (and change for an enabled or selected one).
        if let Some(element) = self.media_element.get() {
            element.remove_media_source_tracks(&source_buffer.take_track_ids());
        }
        // Steps 3-9. Remove the tracks, remove from activeSourceBuffers and
        // sourceBuffers, with their events, and destroy the resources.
        if let Some(handle) = self.handle.borrow().as_ref() &&
            let Some(buffer_handle) = source_buffer.handle().as_ref()
        {
            handle
                .remove_source_buffer(buffer_handle.as_ref())
                .map_err(media_source_error)?;
        }
        source_buffer.mark_removed();
        self.active_source_buffers.remove(source_buffer);
        self.source_buffers.remove(source_buffer);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, cx: &mut JSContext, error: Option<EndOfStreamError>) -> ErrorResult {
        debug!("MediaSource: endOfStream({error:?})");
        // Step 1. If the readyState attribute is not in the "open" state then
        // throw an InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 2. If the updating attribute equals true on any SourceBuffer in
        // sourceBuffers, then throw an InvalidStateError exception.
        if self.any_updating() {
            return Err(Error::InvalidState(None));
        }
        // Step 3. Run the end of stream algorithm with the error parameter set
        // to error.
        let backend_error = match error {
            None => BackendEndOfStreamError::None,
            Some(EndOfStreamError::Network) => BackendEndOfStreamError::Network,
            Some(EndOfStreamError::Decode) => BackendEndOfStreamError::Decode,
        };
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle
                .end_of_stream(backend_error)
                .map_err(media_source_error)?;
        }
        self.end_of_stream_steps(cx, error);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, start: Finite<f64>, end: Finite<f64>) -> ErrorResult {
        // Step 1. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 2. If start is negative or greater than end, then throw a
        // TypeError exception.
        if *start < 0.0 || *start > *end {
            return Err(Error::Type(c"Invalid live seekable range".to_owned()));
        }
        // Step 3. Set live seekable range to be a new normalized TimeRanges
        // object containing a single range whose start position is start and
        // end position is end.
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle
                .set_live_seekable_range(*start, *end)
                .map_err(media_source_error)?;
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> ErrorResult {
        // Step 1. If the readyState attribute is not "open" then throw an
        // InvalidStateError exception.
        if self.ready_state.get() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 2. If live seekable range contains a range, then set live
        // seekable range to be a new empty TimeRanges object.
        if let Some(handle) = self.handle.borrow().as_ref() {
            handle
                .clear_live_seekable_range()
                .map_err(media_source_error)?;
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_global: &Window, type_: DOMString) -> bool {
        // Step 1. If type is an empty string, then return false.
        if type_.is_empty() {
            return false;
        }
        // Steps 2-6. The type must be a registered byte stream format with
        // codecs the format allows, and the media backend must decode them.
        let valid = is_valid_media_source_type(&type_.str());
        let supported = valid && ServoMedia::get().is_media_source_type_supported(&type_.str());
        debug!("MediaSource: isTypeSupported({type_}) = {supported} (valid syntax: {valid})");
        supported
    }
}
