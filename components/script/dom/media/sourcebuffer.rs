/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_weak_referenceable_dom_object;
use servo_base::generic_channel::GenericCallback;
use servo_media::player::media_source::{
    AppendMode as BackendAppendMode, OperationId, SourceBufferEvent, SourceBufferHandle,
    SourceBufferReport,
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
use crate::dom::mediasource::{BackendCallbackTarget, MediaSource, media_source_error};
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
    /// <https://w3c.github.io/media-source/#dfn-first-initialization-segment-received-flag>
    first_initialization_segment_received: Cell<bool>,
    /// The ids of the media element's AudioTrack and VideoTrack objects that this buffer's
    /// initialization segment created, which removeSourceBuffer removes again.
    track_ids: DomRefCell<Vec<String>>,
    /// Whether the update in progress is a remove, which abort() must refuse.
    removing: Cell<bool>,
    /// The backend's number for the append or remove in progress. Reports of any
    /// other operation are not this update's outcome.
    operation: Cell<Option<OperationId>>,
    /// The backend side of this buffer. It holds the appended data and the
    /// buffered ranges, and reports the outcome of appends and removes.
    #[ignore_malloc_size_of = "servo_media"]
    #[no_trace]
    handle: DomRefCell<Option<Box<dyn SourceBufferHandle>>>,
    /// What the backend's callback reaches this object through.
    #[ignore_malloc_size_of = "Arc"]
    #[no_trace]
    callback_target: RefCell<Option<Arc<Mutex<BackendCallbackTarget<SourceBuffer>>>>>,
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
            first_initialization_segment_received: Cell::new(false),
            track_ids: DomRefCell::new(vec![]),
            removing: Cell::new(false),
            operation: Cell::new(None),
            handle: DomRefCell::new(None),
            callback_target: RefCell::new(None),
        }
    }

    /// Creates the DOM object and its backend side for `content_type`.
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        media_source: &MediaSource,
        content_type: &str,
    ) -> Fallible<DomRoot<SourceBuffer>> {
        let buffer: DomRoot<SourceBuffer> = reflect_weak_referenceable_dom_object(
            cx,
            Rc::new(SourceBuffer::new_inherited(media_source)),
            global,
        );
        let (target, weak_target) = BackendCallbackTarget::new(&*buffer);
        *buffer.callback_target.borrow_mut() = Some(target);
        let task_source = global
            .task_manager()
            .media_element_task_source()
            .to_sendable();
        let callback = GenericCallback::new(move |message| {
            let Ok(event) = message else {
                return;
            };
            let weak_target = weak_target.clone();
            task_source.queue(task!(source_buffer_event: move |cx| {
                if let Some(this) = BackendCallbackTarget::root(&weak_target) {
                    this.handle_backend_event(cx, event);
                }
            }));
        })
        .map_err(|_| Error::InvalidState(Some("Could not create a callback".into())))?;
        let handle = match media_source.handle().as_ref() {
            Some(handle) => handle
                .add_source_buffer(content_type, callback)
                .map_err(media_source_error)?,
            None => {
                return Err(Error::InvalidState(Some(
                    "The media source is not ready".into(),
                )));
            },
        };
        *buffer.handle.borrow_mut() = Some(handle);
        Ok(buffer)
    }

    pub(crate) fn updating(&self) -> bool {
        self.updating.get()
    }

    /// Takes the backend handle, for removing the buffer from its media
    /// source. The buffer takes no further operation afterwards.
    pub(crate) fn take_handle(&self) -> Option<Box<dyn SourceBufferHandle>> {
        self.handle.borrow_mut().take()
    }

    pub(crate) fn mark_removed(&self) {
        self.removed.set(true);
        self.updating.set(false);
        self.active.set(false);
        self.first_initialization_segment_received.set(false);
        *self.handle.borrow_mut() = None;
        *self.callback_target.borrow_mut() = None;
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

    pub(crate) fn first_initialization_segment_received(&self) -> bool {
        self.first_initialization_segment_received.get()
    }

    pub(crate) fn set_track_ids(&self, ids: Vec<String>) {
        *self.track_ids.borrow_mut() = ids;
    }

    pub(crate) fn take_track_ids(&self) -> Vec<String> {
        std::mem::take(&mut *self.track_ids.borrow_mut())
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
    fn start_update(&self, removing: bool, operation: OperationId) {
        self.updating.set(true);
        self.removing.set(removing);
        self.operation.set(Some(operation));
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

    /// Once the backend has processed the first initialization segment the
    /// buffer joins activeSourceBuffers, its tracks are added to the media
    /// element, and the duration gets its initial value. The backend reports
    /// a buffer active once it has processed its first initialization
    /// segment, so the two are not told apart here.
    ///
    /// <https://w3c.github.io/media-source/#sourcebuffer-init-segment-received>
    fn initialization_segment_received(&self, cx: &mut JSContext) {
        if self.first_initialization_segment_received.get() {
            return;
        }
        let active = self
            .handle
            .borrow()
            .as_ref()
            .is_some_and(|handle| handle.is_active());
        if active {
            // Step 6. Set [[first initialization segment received flag]] to true.
            self.first_initialization_segment_received.set(true);
            self.media_source
                .first_initialization_segment_received(cx, self);
        }
    }

    fn handle_backend_event(&self, cx: &mut JSContext, report: SourceBufferReport) {
        let SourceBufferReport { operation, event } = report;
        // Reports of an operation the DOM already finished (an abort from
        // script, an error already reported), or did not start as an update,
        // are not this update's outcome.
        if self.removed.get() || !self.updating.get() || self.operation.get() != Some(operation) {
            return;
        }
        match event {
            // updatestart is queued when the operation starts.
            SourceBufferEvent::UpdateStart => {},
            // The append or remove succeeded. Buffer append, steps 5-7, and
            // range removal, steps 7-9: set updating to false, queue update,
            // queue updateend. The three go together: an update listener
            // may start the next append at once, so updating is already
            // false when update fires. The backend reports updateend after
            // update as a separate event; by then the operation is over and
            // the guard above drops it. An updateend with no update before
            // it is taken as the success it implies.
            SourceBufferEvent::Update | SourceBufferEvent::UpdateEnd => {
                self.updating.set(false);
                self.initialization_segment_received(cx);
                self.queue_event("update");
                self.queue_event("updateend");
            },
            // Append error algorithm: updating goes false, error and
            // updateend fire, and the media source has ended with a decode
            // error.
            SourceBufferEvent::Error => {
                self.updating.set(false);
                self.queue_event("error");
                self.queue_event("updateend");
                self.media_source.append_error(cx);
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
            return Err(Error::InvalidState(Some(
                "The source buffer has been removed or is being updated".into(),
            )));
        }
        if reopen && self.media_source.ready_state() == ReadyState::Ended {
            self.media_source.open();
        }
        Ok(())
    }

    fn with_handle<T>(&self, f: impl FnOnce(&dyn SourceBufferHandle) -> T) -> Fallible<T> {
        match self.handle.borrow().as_ref() {
            Some(handle) => Ok(f(handle.as_ref())),
            None => Err(Error::InvalidState(Some(
                "The source buffer is not ready".into(),
            ))),
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
            return Err(Error::InvalidState(Some(
                "The source buffer has been removed".into(),
            )));
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
        // Prepare append algorithm, steps 1-2: removed or updating means
        // InvalidStateError.
        self.check_state(false)?;
        // Steps 3-4. If the HTMLMediaElement's error attribute is not null,
        // then throw an InvalidStateError exception.
        if self.media_source.element_has_error() {
            return Err(Error::InvalidState(Some(
                "The media element has an error".into(),
            )));
        }
        // Step 5. If the parent media source is "ended", reopen it.
        self.check_state(true)?;
        // Prepare append, steps 6-7. The backend evicts what it must and
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
        let operation = self
            .with_handle(|handle| handle.append(bytes))?
            .map_err(media_source_error)?;
        self.start_update(false, operation);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self, _cx: &mut JSContext) -> ErrorResult {
        // Step 1. If this object has been removed from the sourceBuffers
        // attribute of the parent media source then throw an InvalidStateError
        // exception.
        // Step 2. If the readyState attribute of the parent media source is
        // not in the "open" state then throw an InvalidStateError exception.
        if self.removed.get() || self.media_source.ready_state() != ReadyState::Open {
            return Err(Error::InvalidState(Some(
                "The source buffer has been removed or the media source is not open".into(),
            )));
        }
        // Step 3. If the range removal algorithm is running, then throw an
        // InvalidStateError exception. (An abort during a remove is refused by
        // the backend.)
        // Step 3. If the range removal algorithm is running, then throw an
        // InvalidStateError exception.
        if self.updating.get() && self.removing.get() {
            return Err(Error::InvalidState(Some(
                "A range removal is in progress".into(),
            )));
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
        // Step 1. If type is an empty string then throw a TypeError exception.
        if type_.is_empty() {
            return Err(Error::Type(c"The content type is empty".to_owned()));
        }
        // Steps 2-3. Removed or updating: InvalidStateError.
        self.check_state(false)?;
        // Step 4. If type contains a MIME type that is not supported, then
        // throw a NotSupportedError exception: the same test as addSourceBuffer,
        // registry and backend, before step 5 reopens an ended media source.
        if !crate::dom::mediasource::is_media_source_type_supported(&type_.str()) {
            return Err(Error::NotSupported(Some(
                "The content type is not supported".into(),
            )));
        }
        // Step 5. If the parent media source is "ended", reopen it.
        self.check_state(true)?;
        // Steps 6-9. The backend switches its parser to the new type, or
        // refuses with NotSupportedError when it cannot change types at all.
        self.with_handle(|handle| handle.change_type(&type_.str()))?
            .map_err(media_source_error)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, _cx: &mut JSContext, start: Finite<f64>, end: f64) -> ErrorResult {
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
        let operation = self
            .with_handle(|handle| handle.remove(*start, end))?
            .map_err(media_source_error)?;
        self.start_update(true, operation);
        Ok(())
    }
}
