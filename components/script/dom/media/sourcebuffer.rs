/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_cx;
use servo_base::generic_channel::GenericCallback;
use servo_media::player::media_source::{
    AppendMode as BackendAppendMode, SourceBufferEvent, SourceBufferHandle,
};
use stylo_atoms::Atom;

use crate::dom::bindings::buffer_source::get_buffer_source_copy;
use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::ReadyState;
use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::mediasource::{MediaSource, media_source_error};
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};

/// <https://w3c.github.io/media-source/#sourcebuffer>
#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    /// The MediaSource that created this buffer.
    media_source: Dom<MediaSource>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    updating: Cell<bool>,
    /// Set once removed from the parent's sourceBuffers, after which every
    /// method throws InvalidStateError.
    removed: Cell<bool>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    append_window_start: Cell<f64>,
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    append_window_end: Cell<f64>,
    /// Whether this buffer is in the parent's activeSourceBuffers.
    active: Cell<bool>,
    /// Whether the update in progress is a remove, which abort() must refuse.
    removing: Cell<bool>,
    /// Whether the backend reported success (update) for the update in
    /// progress; updateend implies it on success.
    update_reported: Cell<bool>,
    /// The backend side of this buffer. It holds the appended data and the
    /// buffered ranges, and reports the outcome of appends and removes.
    #[ignore_malloc_size_of = "servo_media"]
    #[no_trace]
    handle: DomRefCell<Option<Box<dyn SourceBufferHandle>>>,
}

impl SourceBuffer {
    fn new_inherited(media_source: &MediaSource) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            media_source: Dom::from_ref(media_source),
            updating: Cell::new(false),
            removed: Cell::new(false),
            append_window_start: Cell::new(0.0),
            append_window_end: Cell::new(f64::INFINITY),
            active: Cell::new(false),
            removing: Cell::new(false),
            update_reported: Cell::new(false),
            handle: DomRefCell::new(None),
        }
    }

    /// Creates the DOM object and its backend side for `content_type`.
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        media_source: &MediaSource,
        content_type: &str,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        let buffer: DomRoot<SourceBuffer> = reflect_dom_object_with_cx(
            Box::new(SourceBuffer::new_inherited(media_source)),
            global,
            cx,
        );
        let this = Trusted::new(&*buffer);
        let task_source = global
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message| {
            let Ok(event) = message else {
                return;
            };
            let this = this.clone();
            task_source.queue(task!(source_buffer_event: move |cx| {
                this.root().handle_backend_event(cx, event);
            }));
        })
        .map_err(|_| Error::InvalidState(Some("Could not create a callback".into())))?;
        let handle = match media_source.handle().as_ref() {
            Some(handle) => handle
                .add_source_buffer(content_type, callback)
                .map_err(media_source_error)?,
            None => return Err(Error::InvalidState(None)),
        };
        *buffer.handle.borrow_mut() = Some(handle);
        Ok(buffer)
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }

    /// The content type the backend holds for this buffer, for logging.
    fn content_type(&self) -> String {
        self.handle
            .borrow()
            .as_ref()
            .map(|handle| handle.content_type())
            .unwrap_or_default()
    }

    pub(crate) fn handle(&self) -> std::cell::Ref<'_, Option<Box<dyn SourceBufferHandle>>> {
        self.handle.borrow()
    }

    /// Removes the data between `start` and `end` because the media source's
    /// duration was lowered. Unlike `remove()`, this is not an update the
    /// page sees: no updating flag, no events.
    ///
    /// <https://w3c.github.io/media-source/#duration-change-algorithm> (step 4)
    pub(crate) fn truncate(&self, start: f64, end: f64) {
        let end = if end.is_infinite() { None } else { Some(end) };
        if let Some(handle) = self.handle.borrow().as_ref() &&
            let Err(error) = handle.remove(start, end)
        {
            warn!("Could not truncate the source buffer: {error:?}");
        }
    }

    pub(crate) fn mark_removed(&self) {
        self.removed.set(true);
        self.updating.set(false);
        self.active.set(false);
        *self.handle.borrow_mut() = None;
    }

    /// The buffered ranges as (start, end) pairs in seconds.
    pub(crate) fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        self.handle
            .borrow()
            .as_ref()
            .map(|handle| {
                handle
                    .buffered()
                    .into_iter()
                    .map(|range| (range.start, range.end))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn set_active(&self, active: bool) {
        self.active.set(active);
    }

    /// Aborts the append or remove in progress: `updating` goes false and the
    /// abort and updateend events are queued, whatever the backend reports
    /// afterwards for that operation.
    ///
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort> (steps 4-5)
    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer> (step 2)
    pub(crate) fn abort_update(&self) {
        if let Some(handle) = self.handle.borrow().as_ref() &&
            let Err(error) = handle.abort()
        {
            warn!("Could not abort the source buffer: {error:?}");
        }
        self.updating.set(false);
        self.queue_event("abort");
        self.queue_event("updateend");
    }

    /// Starts an append or remove: `updating` goes true and updatestart is
    /// queued. The backend reports the outcome.
    fn start_update(&self, removing: bool) {
        self.updating.set(true);
        self.removing.set(removing);
        self.update_reported.set(false);
        self.queue_event("updatestart");
    }

    /// Abort, steps 6-7: the parser state is reset and the append window is
    /// reset, whether an update was aborted or not.
    fn reset_after_abort(&self) {
        self.append_window_start.set(0.0);
        self.append_window_end.set(f64::INFINITY);
        if let Some(handle) = self.handle.borrow().as_ref() {
            let _ = handle.set_append_window_start(0.0);
            let _ = handle.set_append_window_end(None);
        }
    }

    /// Once the backend has processed an initialization segment the buffer
    /// joins activeSourceBuffers and the duration gets its initial value.
    ///
    /// <https://w3c.github.io/media-source/#dfn-initialization-segment-received>
    fn initialization_segment_received(&self) {
        if self.active.get() {
            return;
        }
        let active = self
            .handle
            .borrow()
            .as_ref()
            .is_some_and(|handle| handle.is_active());
        if active {
            self.media_source.activate_source_buffer(self);
        }
    }

    fn handle_backend_event(&self, _cx: &mut JSContext, event: SourceBufferEvent) {
        debug!(
            "SourceBuffer({}): backend {event:?}, buffered {:?}",
            self.content_type(),
            self.buffered_ranges()
        );
        // Outcomes of an operation the DOM already finished (an abort from
        // script, an error already reported) are not reported twice.
        if self.removed.get() || !self.updating.get() {
            return;
        }
        match event {
            // updatestart is queued when the operation starts.
            SourceBufferEvent::UpdateStart => {},
            SourceBufferEvent::Update => {
                self.update_reported.set(true);
                self.initialization_segment_received();
                self.queue_event("update");
            },
            SourceBufferEvent::UpdateEnd => {
                self.updating.set(false);
                self.initialization_segment_received();
                // updateend after a success is preceded by update.
                if !self.update_reported.get() {
                    self.queue_event("update");
                }
                self.queue_event("updateend");
            },
            // Append error algorithm: updating goes false, error and
            // updateend fire, and the media source has ended with a decode
            // error.
            SourceBufferEvent::Error => {
                self.updating.set(false);
                self.queue_event("error");
                self.queue_event("updateend");
            },
            SourceBufferEvent::Abort => {
                self.updating.set(false);
                self.queue_event("abort");
                self.queue_event("updateend");
            },
        }
    }

    fn queue_event(&self, name: &str) {
        let this = Trusted::new(self);
        let name = Atom::from(name);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_queued_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, name);
            }));
    }

    /// Steps shared by the setters and operations: removed or updating means
    /// InvalidStateError, and an ended media source is reopened.
    fn check_state(&self, reopen: bool) -> ErrorResult {
        if self.removed.get() || self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if reopen && self.media_source.ready_state() == ReadyState::Ended {
            self.media_source.open();
        }
        Ok(())
    }

    fn with_handle<T>(&self, f: impl FnOnce(&dyn SourceBufferHandle) -> T) -> Fallible<T> {
        match self.handle.borrow().as_ref() {
            Some(handle) => Ok(f(handle.as_ref())),
            None => Err(Error::InvalidState(None)),
        }
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn Mode(&self) -> AppendMode {
        match self
            .handle
            .borrow()
            .as_ref()
            .map(|handle| handle.append_mode())
        {
            Some(BackendAppendMode::Sequence) => AppendMode::Sequence,
            _ => AppendMode::Segments,
        }
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, _cx: &mut JSContext, value: AppendMode) -> ErrorResult {
        debug!("SourceBuffer({}): mode = {value:?}", self.content_type());
        // Steps 1-2. If this object has been removed from the sourceBuffers
        // attribute of the parent media source, or the updating attribute
        // equals true, then throw an InvalidStateError exception.
        // Step 5. If the readyState attribute of the parent media source is in
        // the "ended" state then set it to "open" and queue sourceopen.
        self.check_state(true)?;
        // Steps 3-4, 6-8. The generate timestamps flag and the group start
        // timestamp are the backend's; it refuses an invalid change.
        let mode = match value {
            AppendMode::Segments => BackendAppendMode::Segments,
            AppendMode::Sequence => BackendAppendMode::Sequence,
        };
        self.with_handle(|handle| handle.set_append_mode(mode))?
            .map_err(media_source_error)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn GetBuffered(&self, cx: &mut JSContext) -> Fallible<DomRoot<TimeRanges>> {
        // Step 1. If this object has been removed from the sourceBuffers
        // attribute of the parent media source then throw an InvalidStateError
        // exception.
        if self.removed.get() {
            return Err(Error::InvalidState(None));
        }
        // Steps 2-5. The backend intersects the track buffer ranges.
        let mut ranges = TimeRangesContainer::default();
        for (start, end) in self.buffered_ranges() {
            let _ = ranges.add(start, end);
        }
        Ok(TimeRanges::new(cx, self.global().as_window(), ranges))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn TimestampOffset(&self) -> Finite<f64> {
        Finite::wrap(
            self.handle
                .borrow()
                .as_ref()
                .map_or(0.0, |handle| handle.timestamp_offset()),
        )
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        debug!("SourceBuffer({}): timestampOffset = {}", self.content_type(), *value);
        // Steps 2-3, 5. Removed or updating: InvalidStateError; ended: reopen.
        self.check_state(true)?;
        // Steps 6-7. In sequence mode the group start timestamp follows.
        self.with_handle(|handle| handle.set_timestamp_offset(*value))?
            .map_err(media_source_error)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(self.append_window_start.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn SetAppendWindowStart(&self, value: Finite<f64>) -> ErrorResult {
        // Steps 1-2. Removed or updating: InvalidStateError.
        self.check_state(false)?;
        // Step 3. If the new value is less than 0 or greater than or equal to
        // appendWindowEnd then throw a TypeError exception.
        let end = self.append_window_end.get();
        if *value < 0.0 || *value >= end {
            return Err(Error::Type(c"Invalid append window start".to_owned()));
        }
        // Step 4. Update the attribute to the new value.
        self.append_window_start.set(*value);
        if let Err(error) = self.with_handle(|handle| handle.set_append_window_start(*value))? {
            warn!("The backend refused the append window start: {error:?}");
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn AppendWindowEnd(&self) -> f64 {
        self.append_window_end.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, value: f64) -> ErrorResult {
        // Steps 1-2. Removed or updating: InvalidStateError.
        self.check_state(false)?;
        // Step 3. If the new value equals NaN, then throw a TypeError.
        // Step 4. If the new value is less than or equal to appendWindowStart
        // then throw a TypeError exception.
        if value.is_nan() || value <= self.append_window_start.get() {
            return Err(Error::Type(c"Invalid append window end".to_owned()));
        }
        // Step 5. Update the attribute to the new value.
        self.append_window_end.set(value);
        let end = if value.is_infinite() {
            None
        } else {
            Some(value)
        };
        if let Err(error) = self.with_handle(|handle| handle.set_append_window_end(end))? {
            warn!("The backend refused the append window end: {error:?}");
        }
        Ok(())
    }

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdatestart
    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdate
    event_handler!(update, GetOnupdate, SetOnupdate);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onupdateend
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onerror
    event_handler!(error, GetOnerror, SetOnerror);

    // https://w3c.github.io/media-source/#dom-sourcebuffer-onabort
    event_handler!(abort, GetOnabort, SetOnabort);

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, _cx: &mut JSContext, data: ArrayBufferViewOrArrayBuffer) -> ErrorResult {
        // Prepare append algorithm, steps 1-3: removed or updating means
        // InvalidStateError, and an ended media source is reopened.
        self.check_state(true)?;
        // Prepare append, step 5-6. The backend evicts what it must and
        // refuses with QuotaExceededError when it cannot.
        let bytes = match data {
            ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => {
                get_buffer_source_copy((&view).into())
            },
            ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => {
                get_buffer_source_copy((&buffer).into())
            },
        };
        // Steps 2-4. Add data to the end of the input buffer, set the updating
        // attribute to true, queue updatestart, and run the buffer append
        // algorithm asynchronously: the backend reports its progress.
        debug!(
            "SourceBuffer({}): appendBuffer({} bytes), timestampOffset {}, window {}..{}",
            self.content_type(),
            bytes.len(),
            *self.TimestampOffset(),
            self.append_window_start.get(),
            self.append_window_end.get()
        );
        self.with_handle(|handle| handle.append(bytes))?
            .map_err(|error| {
                debug!("SourceBuffer: append refused: {error:?}");
                media_source_error(error)
            })?;
        self.start_update(false);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self, _cx: &mut JSContext) -> ErrorResult {
        debug!("SourceBuffer({}): abort()", self.content_type());
        // Step 1. If this object has been removed from the sourceBuffers
        // attribute of the parent media source then throw an InvalidStateError
        // exception.
        // Step 2. If the readyState attribute of the parent media source is
        // not in the "open" state then throw an InvalidStateError exception.
        if self.removed.get() || self.media_source.ready_state() != ReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        // Step 3. If the range removal algorithm is running, then throw an
        // InvalidStateError exception. (An abort during a remove is refused by
        // the backend.)
        // Step 3. If the range removal algorithm is running, then throw an
        // InvalidStateError exception.
        if self.updating.get() && self.removing.get() {
            return Err(Error::InvalidState(None));
        }
        // Steps 4-5. If the buffer append algorithm is running, abort it: set
        // updating to false and queue abort and updateend.
        if self.updating.get() {
            self.abort_update();
        } else if let Err(error) = self.with_handle(|handle| handle.abort())? {
            warn!("The backend refused the abort: {error:?}");
        }
        // Steps 6-7. Run the reset parser state algorithm, set
        // appendWindowStart to 0 and appendWindowEnd to positive Infinity.
        self.reset_after_abort();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-changetype>
    fn ChangeType(&self, type_: DOMString) -> ErrorResult {
        debug!("SourceBuffer({}): changeType({type_})", self.content_type());
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(c"The content type is empty".to_owned()));
        }
        // Steps 2-4. Removed or updating: InvalidStateError; ended: reopen.
        self.check_state(true)?;
        // Step 5. If type contains a MIME type that is not supported, then
        // throw a NotSupportedError exception.
        if !crate::dom::mediasource::is_valid_media_source_type(&type_.str()) {
            return Err(Error::NotSupported(None));
        }
        // Steps 6-9. The backend checks the codecs and switches its parser.
        self.with_handle(|handle| handle.change_type(&type_.str()))?
            .map_err(media_source_error)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, _cx: &mut JSContext, start: Finite<f64>, end: f64) -> ErrorResult {
        debug!("SourceBuffer({}): remove({}, {end})", self.content_type(), *start);
        // Steps 1-2. Removed or updating: InvalidStateError.
        self.check_state(false)?;
        // Step 3. If duration equals NaN, then throw a TypeError exception.
        let duration = self.media_source.duration();
        if duration.is_nan() {
            return Err(Error::Type(c"The media source has no duration".to_owned()));
        }
        // Step 4. If start is negative or greater than duration, then throw a
        // TypeError exception.
        if *start < 0.0 || *start > duration {
            return Err(Error::Type(c"Invalid remove start".to_owned()));
        }
        // Step 5. If end is less than or equal to start or end equals NaN, then
        // throw a TypeError exception.
        if end.is_nan() || end <= *start {
            return Err(Error::Type(c"Invalid remove end".to_owned()));
        }
        // Step 6. If the readyState attribute of the parent media source is in
        // the "ended" state then set it to "open" and queue sourceopen.
        if self.media_source.ready_state() == ReadyState::Ended {
            self.media_source.open();
        }
        // Step 7. Run the range removal algorithm with start and end: it sets
        // updating, queues updatestart, removes, and reports update and
        // updateend through the backend.
        let end = if end.is_infinite() { None } else { Some(end) };
        self.with_handle(|handle| handle.remove(*start, end))?
            .map_err(media_source_error)?;
        self.start_update(true);
        Ok(())
    }
}
