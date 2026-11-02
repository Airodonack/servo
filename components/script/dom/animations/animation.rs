/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The `Animation` interface and the timing model's playback procedures: playing, pausing,
//! finishing, canceling, seeking and speed control, the current ready and finished promises,
//! and the finish and cancel events.
//!
//! Times are in milliseconds, as the bindings expose them, and `None` is an unresolved time
//! value. The pending play and pause tasks run from the document's animation update (see
//! [`AnimationManager::update_web_animations`]), which is when an animation becomes ready,
//! because Servo does no setup work before the first frame of an effect.
//!
//! [`AnimationManager::update_web_animations`]: crate::dom::document::animation_manager::AnimationManager::update_web_animations

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, ErrorResult, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::{Dom, DomRoot};
use script_bindings::str::DOMString;
use stylo_atoms::Atom;

use crate::dom::animationeffect::AnimationEffect;
use crate::dom::animationplaybackevent::AnimationPlaybackEvent;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::{
    AnimationMethods, AnimationPlayState,
};
use crate::dom::bindings::codegen::Bindings::AnimationTimelineBinding::AnimationTimelineMethods;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::document::Document;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::{AnimationTimeline, DocumentTimeline};
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;
use crate::runtime::job_queue::MicrotaskRunnable;

/// The task an animation is waiting to run once it is ready.
///
/// <https://drafts.csswg.org/web-animations-1/#pending-play-task>
/// <https://drafts.csswg.org/web-animations-1/#pending-pause-task>
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
enum PendingTask {
    None,
    Play,
    Pause,
}

/// The kinds of [`AnimationPlaybackEvent`] an animation queues.
///
/// <https://drafts.csswg.org/web-animations-1/#animation-playback-event-types>
#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum PlaybackEventType {
    Finish,
    Cancel,
}

impl PlaybackEventType {
    fn atom(self) -> Atom {
        match self {
            PlaybackEventType::Finish => Atom::from("finish"),
            PlaybackEventType::Cancel => atom!("cancel"),
        }
    }
}

/// <https://drafts.csswg.org/web-animations-1/#animation>
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,

    /// <https://drafts.csswg.org/web-animations-1/#timeline>
    timeline: MutNullableDom<AnimationTimeline>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-associated-effect>
    associated_effect: MutNullableDom<AnimationEffect>,

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    id: DomRefCell<DOMString>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-start-time>
    start_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-hold-time>
    hold_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#previous-current-time>
    previous_current_time: Cell<Option<f64>>,

    /// <https://drafts.csswg.org/web-animations-1/#playback-rate>
    playback_rate: Cell<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#pending-playback-rate>
    pending_playback_rate: Cell<Option<f64>>,

    /// Whether the animation has a pending play task or a pending pause task.
    pending_task: Cell<PendingTask>,

    /// <https://drafts.csswg.org/web-animations-1/#current-ready-promise>
    ready_promise: DomRefCell<Option<TracedPromise>>,

    /// <https://drafts.csswg.org/web-animations-1/#current-finished-promise>
    finished_promise: DomRefCell<Option<TracedPromise>>,

    /// Whether a microtask to run the finish notification steps is queued. Each queued
    /// microtask carries the generation it was queued in, so that canceling it is a matter of
    /// moving to the next generation.
    finish_notification_queued: Cell<bool>,
    finish_notification_generation: Cell<u32>,

    /// Whether the document for timing is keeping this animation in its list of animations
    /// to update on each animation frame.
    tracked_by_document: Cell<bool>,
}

impl Animation {
    pub(crate) fn new_inherited() -> Self {
        Self {
            event_target: EventTarget::new_inherited(),
            timeline: Default::default(),
            associated_effect: Default::default(),
            id: Default::default(),
            start_time: Cell::new(None),
            hold_time: Cell::new(None),
            previous_current_time: Cell::new(None),
            playback_rate: Cell::new(1.0),
            pending_playback_rate: Cell::new(None),
            pending_task: Cell::new(PendingTask::None),
            ready_promise: Default::default(),
            finished_promise: Default::default(),
            finish_notification_queued: Cell::new(false),
            finish_notification_generation: Cell::new(0),
            tracked_by_document: Cell::new(false),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        let animation =
            reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited()), global, proto);

        // > The current ready promise is initially a resolved Promise created using the procedure
        // > to create a new resolved Promise with the animation itself as its value and created
        // > in the relevant Realm of the animation.
        let ready = Promise::new(cx, global);
        ready.resolve_native(cx, &animation);
        *animation.ready_promise.borrow_mut() = Some(ready.to_traced());

        // > The current finished promise is initially a pending Promise object.
        let finished = Promise::new(cx, global);
        *animation.finished_promise.borrow_mut() = Some(finished.to_traced());

        animation
    }

    pub(crate) fn set_id(&self, id: DOMString) {
        *self.id.borrow_mut() = id;
    }

    fn window(&self) -> DomRoot<Window> {
        DomRoot::from_ref(self.global().as_window())
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-document-for-timing>
    ///
    /// Every timeline Servo has is a document timeline of this animation's own document.
    fn document_for_timing(&self) -> Option<DomRoot<Document>> {
        self.timeline.get()?;
        Some(self.window().Document())
    }

    /// The current time of the timeline, or `None` when there is no timeline. Document
    /// timelines are always active.
    fn timeline_time(&self) -> Option<f64> {
        self.timeline
            .get()
            .and_then(|timeline| timeline.GetCurrentTime())
            .map(|time| *time)
    }

    /// <https://drafts.csswg.org/web-animations-1/#associated-effect-end>
    pub(crate) fn associated_effect_end(&self) -> f64 {
        // > If the animation has no associated effect, the associated effect end is zero.
        self.associated_effect
            .get()
            .map_or(0.0, |effect| effect.end_time())
    }

    /// <https://drafts.csswg.org/web-animations-1/#effective-playback-rate>
    fn effective_playback_rate(&self) -> f64 {
        self.pending_playback_rate
            .get()
            .unwrap_or(self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#apply-any-pending-playback-rate>
    fn apply_any_pending_playback_rate(&self) {
        // Step 1. If animation does not have a pending playback rate, abort these steps.
        // Step 2. Set animation’s playback rate to its pending playback rate.
        // Step 3. Clear animation’s pending playback rate.
        if let Some(rate) = self.pending_playback_rate.take() {
            self.playback_rate.set(rate);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-current-time>
    pub(crate) fn current_time(&self) -> Option<f64> {
        self.current_time_given_hold_time(self.hold_time.get())
    }

    fn current_time_given_hold_time(&self, hold_time: Option<f64>) -> Option<f64> {
        // > If the animation’s hold time is resolved, the current time is the animation’s hold time.
        if hold_time.is_some() {
            return hold_time;
        }

        // > If any of the following are true: the animation has no associated timeline, or the
        // > associated timeline is inactive, or the animation’s start time is unresolved, the
        // > current time is an unresolved time value.
        let timeline_time = self.timeline_time()?;
        let start_time = self.start_time.get()?;

        // > Otherwise, current time = (timeline time - start time) × playback rate
        //
        // Adding zero turns a negative zero, from a negative playback rate, into zero.
        Some((timeline_time - start_time) * self.playback_rate.get() + 0.0)
    }

    pub(crate) fn has_pending_task(&self) -> bool {
        self.pending_task.get() != PendingTask::None
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-state>
    pub(crate) fn play_state(&self) -> AnimationPlayState {
        let current_time = self.current_time();
        let start_time = self.start_time.get();
        let pending_task = self.pending_task.get();

        // > All of the following conditions are true: The current time of animation is
        // > unresolved, and the start time of animation is unresolved, and animation does not
        // > have either a pending play task or a pending pause task, → idle
        if current_time.is_none() && start_time.is_none() && pending_task == PendingTask::None {
            return AnimationPlayState::Idle;
        }

        // > Either of the following conditions are true: animation has a pending pause task, or
        // > both the start time of animation is unresolved and it does not have a pending play
        // > task, → paused
        if pending_task == PendingTask::Pause ||
            (start_time.is_none() && pending_task != PendingTask::Play)
        {
            return AnimationPlayState::Paused;
        }

        // > For animation, current time is resolved and either of the following conditions are
        // > true: animation’s effective playback rate > 0 and current time ≥ associated effect
        // > end; or animation’s effective playback rate < 0 and current time ≤ 0, → finished
        if let Some(current_time) = current_time {
            let rate = self.effective_playback_rate();
            if (rate > 0.0 && current_time >= self.associated_effect_end()) ||
                (rate < 0.0 && current_time <= 0.0)
            {
                return AnimationPlayState::Finished;
            }
        }

        // > Otherwise, → running
        AnimationPlayState::Running
    }

    /// Whether the animation's state can change on the next animation frame without any call
    /// from script, so that the document has to keep producing animation frames for it.
    pub(crate) fn needs_animation_frames(&self) -> bool {
        self.has_pending_task() ||
            self.finish_notification_queued.get() ||
            self.play_state() == AnimationPlayState::Running
    }

    /// Whether the document for timing has to keep this animation in its list of animations.
    pub(crate) fn needs_tracking(&self) -> bool {
        self.needs_animation_frames()
    }

    pub(crate) fn set_tracked_by_document(&self, tracked: bool) {
        self.tracked_by_document.set(tracked);
    }

    /// Make sure the document for timing updates this animation on each animation frame
    /// while it needs it, and that animation frames are produced.
    fn ensure_tracked(&self) {
        if !self.needs_tracking() {
            return;
        }
        let Some(document) = self.document_for_timing() else {
            return;
        };
        if !self.tracked_by_document.get() {
            self.tracked_by_document.set(true);
            document.animation_manager().track_web_animation(self);
        }
        document
            .animation_manager()
            .request_web_animation_frames(document.window());
    }

    fn ready_promise(&self, cx: &JSContext) -> RootedPromise {
        self.ready_promise
            .borrow()
            .as_ref()
            .expect("The ready promise is created with the animation")
            .root(cx)
    }

    fn finished_promise(&self, cx: &JSContext) -> RootedPromise {
        self.finished_promise
            .borrow()
            .as_ref()
            .expect("The finished promise is created with the animation")
            .root(cx)
    }

    fn set_ready_promise(&self, promise: &RootedPromise) {
        *self.ready_promise.borrow_mut() = Some(promise.to_traced());
    }

    fn set_finished_promise(&self, promise: &RootedPromise) {
        *self.finished_promise.borrow_mut() = Some(promise.to_traced());
    }

    /// Let animation’s current ready promise be a new promise in the relevant Realm of
    /// animation.
    fn replace_ready_promise_with_new_promise(&self, cx: &mut JSContext) {
        let promise = Promise::new(cx, &self.global());
        self.set_ready_promise(&promise);
    }

    /// Resolve animation’s current ready promise with animation.
    fn resolve_ready_promise(&self, cx: &mut JSContext) {
        let promise = self.ready_promise(cx);
        if promise.is_pending() {
            promise.resolve_native(cx, &DomRoot::from_ref(self));
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-timeline-of-an-animation>
    fn set_the_timeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // Step 1. Let old timeline be the current timeline of animation, if any.
        // Step 2. If new timeline is the same object as old timeline, abort this procedure.
        if self.timeline.get().as_deref() == timeline {
            return;
        }

        // Step 3. Let the timeline of animation be new timeline.
        self.timeline.set(timeline);

        // Step 4. If the start time of animation is resolved, make animation’s hold time
        // unresolved.
        if self.start_time.get().is_some() {
            self.hold_time.set(None);
        }

        // Step 5. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-set-the-associated-effect-of-an-animation>
    fn set_the_associated_effect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // Step 1. Let old effect be the current associated effect of animation, if any.
        let old_effect = self.associated_effect.get();

        // Step 2. If new effect is the same object as old effect, abort this procedure.
        if old_effect.as_deref() == effect {
            return;
        }

        // Step 3. If animation has a pending pause task, reschedule that task to run as soon as
        // animation is ready.
        // Step 4. If animation has a pending play task, reschedule that task to run as soon as
        // animation is ready to play new effect.
        //
        // Nothing to do: pending tasks run on the next animation frame whatever the effect.

        // Step 5. If new effect is not null and if new effect is the associated effect of
        // another animation, previous animation, run the procedure to set the associated effect
        // of an animation (this procedure) on previous animation, passing null as new effect.
        if let Some(previous_animation) = effect.and_then(|effect| effect.associated_animation()) {
            previous_animation.set_the_associated_effect(cx, None);
        }

        // Step 6. Let the associated effect of animation be new effect.
        if let Some(old_effect) = old_effect {
            old_effect.set_associated_animation(None);
        }
        if let Some(effect) = effect {
            effect.set_associated_animation(Some(self));
        }
        self.associated_effect.set(effect);

        // Step 7. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#silently-set-the-current-time>
    fn silently_set_the_current_time(&self, seek_time: Option<f64>) -> ErrorResult {
        // Step 1. If seek time is an unresolved time value, then perform the following steps.
        let Some(seek_time) = seek_time else {
            // Step 1.1. If the current time is resolved, then throw a TypeError.
            if self.current_time().is_some() {
                return Err(Error::Type(
                    c"The current time of a playing animation cannot be unset".to_owned(),
                ));
            }
            // Step 1.2. Abort these steps.
            return Ok(());
        };

        // Step 2. Update either animation’s hold time or start time as follows:
        // > If any of the following conditions are true: animation’s hold time is resolved, or
        // > animation’s start time is unresolved, or animation has no associated timeline or the
        // > associated timeline is inactive, or animation’s playback rate is 0,
        let timeline_time = self.timeline_time();
        match timeline_time {
            Some(timeline_time)
                if self.hold_time.get().is_none() &&
                    self.start_time.get().is_some() &&
                    self.playback_rate.get() != 0.0 =>
            {
                // > Otherwise, Set animation’s start time to the result of evaluating
                // > timeline time - (seek time / playback rate)
                self.start_time
                    .set(Some(timeline_time - seek_time / self.playback_rate.get()));
            },
            // > Set animation’s hold time to seek time.
            _ => self.hold_time.set(Some(seek_time)),
        }

        // Step 3. If animation has no associated timeline or the associated timeline is
        // inactive, make animation’s start time unresolved.
        if timeline_time.is_none() {
            self.start_time.set(None);
        }

        // Step 4. Make animation’s previous current time unresolved.
        self.previous_current_time.set(None);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-current-time>
    fn set_the_current_time(&self, cx: &mut JSContext, seek_time: Option<f64>) -> ErrorResult {
        // Step 1. Run the steps to silently set the current time of animation to seek time.
        self.silently_set_the_current_time(seek_time)?;

        // Step 2. If animation has a pending pause task, synchronously complete the pause
        // operation by performing the following steps:
        if self.pending_task.get() == PendingTask::Pause {
            // Step 2.1. Set animation’s hold time to seek time.
            self.hold_time.set(seek_time);
            // Step 2.2. Apply any pending playback rate to animation.
            self.apply_any_pending_playback_rate();
            // Step 2.3. Make animation’s start time unresolved.
            self.start_time.set(None);
            // Step 2.4. Cancel the pending pause task.
            self.pending_task.set(PendingTask::None);
            // Step 2.5. Resolve animation’s current ready promise with animation.
            self.resolve_ready_promise(cx);
        }

        // Step 3. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-start-time>
    fn set_the_start_time(&self, cx: &mut JSContext, new_start_time: Option<f64>) {
        // Step 1. Let timeline time be the current time value of the timeline that animation is
        // associated with. If there is no timeline associated with animation or the associated
        // timeline is inactive, let the timeline time be unresolved.
        let timeline_time = self.timeline_time();

        // Step 2. If timeline time is unresolved and new start time is resolved, make
        // animation’s hold time unresolved.
        if timeline_time.is_none() && new_start_time.is_some() {
            self.hold_time.set(None);
        }

        // Step 3. Let previous current time be animation’s current time.
        let previous_current_time = self.current_time();

        // Step 4. Apply any pending playback rate on animation.
        self.apply_any_pending_playback_rate();

        // Step 5. Set animation’s start time to new start time.
        self.start_time.set(new_start_time);

        // Step 6. Update animation’s hold time based on the first matching condition from the
        // following,
        if new_start_time.is_some() {
            // > If new start time is resolved, If animation’s playback rate is not zero, make
            // > animation’s hold time unresolved.
            if self.playback_rate.get() != 0.0 {
                self.hold_time.set(None);
            }
        } else {
            // > Otherwise (new start time is unresolved), Set animation’s hold time to previous
            // > current time even if previous current time is unresolved.
            self.hold_time.set(previous_current_time);
        }

        // Step 7. If animation has a pending play task or a pending pause task, cancel that task
        // and resolve animation’s current ready promise with animation.
        if self.has_pending_task() {
            self.pending_task.set(PendingTask::None);
            self.resolve_ready_promise(cx);
        }

        // Step 8. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to false.
        self.update_finished_state(cx, true, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#play-an-animation>
    pub(crate) fn play_an_animation(&self, cx: &mut JSContext, auto_rewind: bool) -> ErrorResult {
        // Step 1. Let aborted pause be a boolean flag that is true if animation has a pending
        // pause task, and false otherwise.
        let aborted_pause = self.pending_task.get() == PendingTask::Pause;

        // Step 2. Let has pending ready promise be a boolean flag that is initially false.
        let mut has_pending_ready_promise = false;

        // Step 3. Let seek time be a time value that is initially unresolved.
        let mut seek_time = None;

        // Step 4. If the auto-rewind flag is true, perform the steps corresponding to the first
        // matching condition from the following, if any:
        if auto_rewind {
            let rate = self.effective_playback_rate();
            let current_time = self.current_time();
            let effect_end = self.associated_effect_end();
            if rate >= 0.0 && current_time.is_none_or(|time| time < 0.0 || time >= effect_end) {
                // > If animation’s effective playback rate ≥ 0, and animation’s current time is
                // > either: unresolved, or less than zero, or greater than or equal to associated
                // > effect end, Set seek time to zero.
                seek_time = Some(0.0);
            } else if rate < 0.0 && current_time.is_none_or(|time| time <= 0.0 || time > effect_end)
            {
                // > If animation’s effective playback rate < 0, and animation’s current time is
                // > either: unresolved, or less than or equal to zero, or greater than associated
                // > effect end,
                if effect_end == f64::INFINITY {
                    // > If associated effect end is positive infinity, throw an
                    // > "InvalidStateError" DOMException and abort these steps.
                    return Err(Error::InvalidState(Some(
                        "An animation that plays backwards cannot rewind to an infinite end".into(),
                    )));
                }
                // > Otherwise, Set seek time to animation’s associated effect end.
                seek_time = Some(effect_end);
            }
        }

        // Step 5. If the following three conditions are all satisfied: seek time is unresolved,
        // and animation’s start time is unresolved, and animation’s current time is unresolved,
        // set seek time to zero.
        if seek_time.is_none() && self.start_time.get().is_none() && self.current_time().is_none() {
            seek_time = Some(0.0);
        }

        // Step 6. Let has finite timeline be true if animation has an associated timeline that
        // is not monotonically increasing.
        //
        // Document timelines are monotonically increasing, so has finite timeline is false.

        // Step 7. If seek time is resolved, ... Otherwise, Set animation’s hold time to seek
        // time.
        if seek_time.is_some() {
            self.hold_time.set(seek_time);
        }

        // Step 8. If animation’s hold time is resolved, let its start time be unresolved.
        if self.hold_time.get().is_some() {
            self.start_time.set(None);
        }

        // Step 9. If animation has a pending play task or a pending pause task, Cancel that
        // task. Set has pending ready promise to true.
        if self.has_pending_task() {
            self.pending_task.set(PendingTask::None);
            has_pending_ready_promise = true;
        }

        // Step 10. If the following four conditions are all satisfied: animation’s hold time is
        // unresolved, and seek time is unresolved, and aborted pause is false, and animation
        // does not have a pending playback rate, abort this procedure.
        if self.hold_time.get().is_none() &&
            seek_time.is_none() &&
            !aborted_pause &&
            self.pending_playback_rate.get().is_none()
        {
            return Ok(());
        }

        // Step 11. If has pending ready promise is false, let animation’s current ready promise
        // be a new promise in the relevant Realm of animation.
        if !has_pending_ready_promise {
            self.replace_ready_promise_with_new_promise(cx);
        }

        // Step 12. Schedule a task to run as soon as animation is ready.
        // (See `run_pending_play_task`.)
        self.pending_task.set(PendingTask::Play);

        // Step 13. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The task scheduled by step 12 of
    /// <https://drafts.csswg.org/web-animations-1/#play-an-animation>.
    fn run_pending_play_task(&self, cx: &mut JSContext, ready_time: f64) {
        self.pending_task.set(PendingTask::None);

        // Step 1. Assert that at least one of animation’s start time or hold time is resolved.
        debug_assert!(self.start_time.get().is_some() || self.hold_time.get().is_some());

        // Step 2. Let ready time be the time value of the timeline associated with animation
        // at the moment when animation became ready.
        // Step 3. Perform the steps corresponding to the first matching condition below, if any:
        if let Some(hold_time) = self.hold_time.get() {
            // > If animation’s hold time is resolved,
            // > 1. Apply any pending playback rate on animation.
            self.apply_any_pending_playback_rate();

            // > 2. Let new start time be the result of evaluating ready time - hold time /
            // > playback rate for animation. If the playback rate is zero, let new start time be
            // > simply ready time.
            let rate = self.playback_rate.get();
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - hold_time / rate
            };

            // > 3. Set the start time of animation to new start time.
            self.start_time.set(Some(new_start_time));

            // > 4. If animation’s playback rate is not 0, make animation’s hold time unresolved.
            if rate != 0.0 {
                self.hold_time.set(None);
            }
        } else if let (Some(start_time), Some(_)) =
            (self.start_time.get(), self.pending_playback_rate.get())
        {
            // > If animation’s start time is resolved and animation has a pending playback rate,
            // > 1. Let current time to match be the result of evaluating (ready time - start
            // > time) × playback rate for animation.
            let current_time_to_match = (ready_time - start_time) * self.playback_rate.get();

            // > 2. Apply any pending playback rate on animation.
            self.apply_any_pending_playback_rate();

            // > 3. If animation’s playback rate is zero, let animation’s hold time be current
            // > time to match.
            let rate = self.playback_rate.get();
            if rate == 0.0 {
                self.hold_time.set(Some(current_time_to_match));
            }

            // > 4. Let new start time be the result of evaluating ready time - current time to
            // > match / playback rate for animation. If the playback rate is zero, let new start
            // > time be simply ready time.
            let new_start_time = if rate == 0.0 {
                ready_time
            } else {
                ready_time - current_time_to_match / rate
            };

            // > 5. Set the start time of animation to new start time.
            self.start_time.set(Some(new_start_time));
        }

        // Step 4. Resolve animation’s current ready promise with animation.
        self.resolve_ready_promise(cx);

        // Step 5. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// <https://drafts.csswg.org/web-animations-1/#pause-an-animation>
    fn pause_an_animation(&self, cx: &mut JSContext) -> ErrorResult {
        // Step 1. If animation has a pending pause task, abort these steps.
        if self.pending_task.get() == PendingTask::Pause {
            return Ok(());
        }

        // Step 2. If the play state of animation is paused, abort these steps.
        if self.play_state() == AnimationPlayState::Paused {
            return Ok(());
        }

        // Step 3. Let seek time be a time value that is initially unresolved.
        let mut seek_time = None;

        // Step 4. Let has finite timeline be true if animation has an associated timeline that
        // is not monotonically increasing.
        //
        // Document timelines are monotonically increasing, so has finite timeline is false.

        // Step 5. If the animation’s current time is unresolved, perform the steps according to
        // the first matching condition below:
        if self.current_time().is_none() {
            if self.playback_rate.get() >= 0.0 {
                // > If animation’s playback rate is ≥ 0, Set seek time to zero.
                seek_time = Some(0.0);
            } else {
                // > Otherwise, If associated effect end for animation is positive infinity,
                // > throw an "InvalidStateError" DOMException and abort these steps.
                let effect_end = self.associated_effect_end();
                if effect_end == f64::INFINITY {
                    return Err(Error::InvalidState(Some(
                        "An animation that plays backwards cannot pause at an infinite end".into(),
                    )));
                }
                // > Otherwise, Set seek time to animation’s associated effect end.
                seek_time = Some(effect_end);
            }
        }

        // Step 6. If seek time is resolved, ... Otherwise, Set animation’s hold time to seek
        // time.
        if seek_time.is_some() {
            self.hold_time.set(seek_time);
        }

        // Step 7. Let has pending ready promise be a boolean flag that is initially false.
        let mut has_pending_ready_promise = false;

        // Step 8. If animation has a pending play task, cancel that task and let has pending
        // ready promise be true.
        if self.pending_task.get() == PendingTask::Play {
            self.pending_task.set(PendingTask::None);
            has_pending_ready_promise = true;
        }

        // Step 9. If has pending ready promise is false, set animation’s current ready promise
        // to a new promise in the relevant Realm of animation.
        if !has_pending_ready_promise {
            self.replace_ready_promise_with_new_promise(cx);
        }

        // Step 10. Schedule a task to be executed at the first possible moment where both of
        // the following conditions are true: ... (See `run_pending_pause_task`.)
        self.pending_task.set(PendingTask::Pause);

        // Step 11. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
        Ok(())
    }

    /// The task scheduled by step 10 of
    /// <https://drafts.csswg.org/web-animations-1/#pause-an-animation>.
    fn run_pending_pause_task(&self, cx: &mut JSContext, ready_time: f64) {
        self.pending_task.set(PendingTask::None);

        // Step 1. Let ready time be the time value of the timeline associated with animation at
        // the moment when the user agent completed processing necessary to suspend playback of
        // animation’s associated effect.
        // Step 2. If animation’s start time is resolved and its hold time is not resolved, let
        // animation’s hold time be the result of evaluating (ready time - start time) ×
        // playback rate.
        if let (Some(start_time), None) = (self.start_time.get(), self.hold_time.get()) {
            self.hold_time
                .set(Some((ready_time - start_time) * self.playback_rate.get()));
        }

        // Step 3. Apply any pending playback rate on animation.
        self.apply_any_pending_playback_rate();

        // Step 4. Make animation’s start time unresolved.
        self.start_time.set(None);

        // Step 5. Resolve animation’s current ready promise with animation.
        self.resolve_ready_promise(cx);

        // Step 6. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to false, and the synchronously notify flag set to false.
        self.update_finished_state(cx, false, false);
    }

    /// Run the pending play or pause task, if any, now that the animation is ready: on the
    /// animation frame after it was scheduled, with the timeline's time of that frame.
    pub(crate) fn run_pending_task_if_ready(&self, cx: &mut JSContext) {
        // > An animation is ready at the first moment where both of the following conditions
        // > are true: the user agent has completed any setup required to begin the playback of
        // > the animation’s associated effect ... the animation is associated with a timeline
        // > that is not inactive.
        let Some(ready_time) = self.timeline_time() else {
            return;
        };
        match self.pending_task.get() {
            PendingTask::None => {},
            PendingTask::Play => self.run_pending_play_task(cx, ready_time),
            PendingTask::Pause => self.run_pending_pause_task(cx, ready_time),
        }
    }

    /// Bring a timeline other than the document's default one up to date for this animation
    /// frame. The default timeline is updated by the document itself.
    pub(crate) fn update_timeline(&self, window: &Window) {
        if let Some(timeline) = self.timeline.get() &&
            let Some(document_timeline) = timeline.downcast::<DocumentTimeline>() &&
            *document_timeline != *window.Document().Timeline()
        {
            document_timeline.update(window);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>
    pub(crate) fn update_finished_state(
        &self,
        cx: &mut JSContext,
        did_seek: bool,
        synchronously_notify: bool,
    ) {
        // Step 1. Let the unconstrained current time be the result of calculating the current
        // time, substituting an unresolved time value for the hold time if did seek is false.
        // If did seek is true, the unconstrained current time is equal to the current time.
        let unconstrained_current_time = if did_seek {
            self.current_time()
        } else {
            self.current_time_given_hold_time(None)
        };

        // Step 2. If all three of the following conditions are true, the unconstrained current
        // time is resolved, and animation’s start time is resolved, and animation does not have
        // a pending play task or a pending pause task, then update animation’s hold time based
        // on the first matching condition for animation from below, if any:
        if let (Some(unconstrained_current_time), Some(_), false) = (
            unconstrained_current_time,
            self.start_time.get(),
            self.has_pending_task(),
        ) {
            let rate = self.playback_rate.get();
            let effect_end = self.associated_effect_end();
            let previous_current_time = self.previous_current_time.get();
            if rate > 0.0 && unconstrained_current_time >= effect_end {
                // > If playback rate > 0 and unconstrained current time is greater than or
                // > equal to associated effect end, If did seek is true, let the hold time be
                // > the value of unconstrained current time. If did seek is false, let the hold
                // > time be the maximum value of previous current time and associated effect
                // > end. If the previous current time is unresolved, let the hold time be
                // > associated effect end.
                self.hold_time.set(Some(if did_seek {
                    unconstrained_current_time
                } else {
                    previous_current_time.map_or(effect_end, |time| time.max(effect_end))
                }));
            } else if rate < 0.0 && unconstrained_current_time <= 0.0 {
                // > If playback rate < 0 and unconstrained current time is less than or equal to
                // > 0, If did seek is true, let the hold time be the value of unconstrained
                // > current time. If did seek is false, let the hold time be the minimum value of
                // > previous current time and zero. If the previous current time is unresolved,
                // > let the hold time be zero.
                self.hold_time.set(Some(if did_seek {
                    unconstrained_current_time
                } else {
                    previous_current_time.map_or(0.0, |time| time.min(0.0))
                }));
            } else if rate != 0.0 &&
                let Some(timeline_time) = self.timeline_time()
            {
                // > If playback rate ≠ 0, and animation is associated with an active timeline,
                // > 1. If did seek is true and the hold time is resolved, let animation’s start
                // > time be equal to the result of evaluating timeline time - (hold time /
                // > playback rate) where timeline time is the current time value of timeline
                // > associated with animation.
                if did_seek && let Some(hold_time) = self.hold_time.get() {
                    self.start_time.set(Some(timeline_time - hold_time / rate));
                }
                // > 2. Let the hold time be unresolved.
                self.hold_time.set(None);
            }
        }

        // Step 3. Set the previous current time of animation be the result of calculating its
        // current time.
        self.previous_current_time.set(self.current_time());

        // Step 4. Let current finished state be true if the play state of animation is finished.
        // Otherwise, let it be false.
        let current_finished_state = self.play_state() == AnimationPlayState::Finished;

        // Step 5. If current finished state is true and the current finished promise is not yet
        // resolved, perform the following steps:
        let finished_promise_is_resolved = self.finished_promise(cx).is_fulfilled();
        if current_finished_state && !finished_promise_is_resolved {
            if synchronously_notify {
                // Step 5.2. If synchronously notify is true, cancel any queued microtask to run
                // the finish notification steps for this animation, and run the finish
                // notification steps immediately.
                self.cancel_finish_notification_microtask();
                self.run_finish_notification_steps(cx);
            } else if !self.finish_notification_queued.get() {
                // > Otherwise, if synchronously notify is false, queue a microtask to run finish
                // > notification steps for animation unless there is already a microtask queued
                // > to run those steps for animation.
                self.finish_notification_queued.set(true);
                self.global().enqueue_microtask(
                    cx,
                    Box::new(FinishNotificationMicrotask {
                        animation: Dom::from_ref(self),
                        generation: self.finish_notification_generation.get(),
                    }),
                );
            }
        }

        // Step 6. If current finished state is false and animation’s current finished promise is
        // already resolved, set animation’s current finished promise to a new promise in the
        // relevant Realm of animation.
        if !current_finished_state && finished_promise_is_resolved {
            let promise = Promise::new(cx, &self.global());
            self.set_finished_promise(&promise);
        }

        self.ensure_tracked();
    }

    fn cancel_finish_notification_microtask(&self) {
        self.finish_notification_queued.set(false);
        self.finish_notification_generation
            .set(self.finish_notification_generation.get().wrapping_add(1));
    }

    /// The finish notification steps of
    /// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>.
    fn run_finish_notification_steps(&self, cx: &mut JSContext) {
        // Step 1. If animation’s play state is not equal to finished, abort these steps.
        //
        // An animation with a pending play task can be finished, when its associated effect
        // has no length. Like Chrome, wait for the task: it runs the finished state update
        // again, so that the current ready promise is resolved before the current finished
        // promise, as the note under the procedure to play an animation intends.
        if self.play_state() != AnimationPlayState::Finished || self.has_pending_task() {
            return;
        }

        // Step 2. Resolve animation’s current finished promise object with animation.
        self.finished_promise(cx)
            .resolve_native(cx, &DomRoot::from_ref(self));

        // Step 3. Create an AnimationPlaybackEvent, finishEvent.
        // Step 4. Set finishEvent’s type attribute to finish.
        // Step 5. Set finishEvent’s currentTime attribute to the current time of animation.
        // Step 6. Set finishEvent’s timelineTime attribute to the current time of the timeline
        // with which animation is associated. If animation is not associated with a timeline,
        // or the timeline is inactive, let timelineTime be null.
        // Step 7. If animation has a document for timing, then append finishEvent to its
        // document for timing’s pending animation event queue along with its target, animation.
        // For the scheduled event time, use the result of converting animation’s associated
        // effect end to an origin-relative time. Otherwise, queue a task to dispatch
        // finishEvent at animation. The task source for this task is the DOM manipulation task
        // source.
        let scheduled_event_time =
            self.animation_time_to_origin_relative_time(Some(self.associated_effect_end()));
        self.queue_playback_event(
            PlaybackEventType::Finish,
            self.current_time(),
            self.timeline_time(),
            scheduled_event_time,
        );
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-time-to-origin-relative-time>
    fn animation_time_to_origin_relative_time(&self, time: Option<f64>) -> Option<f64> {
        // Converting an animation time to timeline time:
        // > If time is unresolved, return time. If time is infinity, return an unresolved time
        // > value. If animation’s playback rate is zero, return an unresolved time value. If
        // > animation’s start time is unresolved, return an unresolved time value. Return the
        // > result of calculating: time × (1 / playback rate) + start time
        let time = time?;
        let rate = self.playback_rate.get();
        if time.is_infinite() || rate == 0.0 {
            return None;
        }
        let timeline_time = time / rate + self.start_time.get()?;

        // Converting a timeline time to an origin-relative time: the default document timeline
        // has an origin time of zero, and other document timelines are offset from it.
        let timeline = self.timeline.get()?;
        let origin_time = timeline
            .downcast::<DocumentTimeline>()
            .map_or(0.0, |timeline| timeline.origin_time_in_milliseconds());
        Some(timeline_time + origin_time)
    }

    /// Queue an [`AnimationPlaybackEvent`] for this animation: in the pending animation event
    /// queue of its document for timing if it has one, or else as a task on the DOM
    /// manipulation task source.
    fn queue_playback_event(
        &self,
        event_type: PlaybackEventType,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
        scheduled_event_time: Option<f64>,
    ) {
        if let Some(document) = self.document_for_timing() {
            document.animation_manager().queue_playback_event(
                document.window(),
                self,
                event_type,
                current_time,
                timeline_time,
                scheduled_event_time,
            );
            return;
        }

        let animation = Trusted::new(self);
        self.global()
            .task_manager()
            .dom_manipulation_task_source()
            .queue(task!(dispatch_animation_playback_event: move |cx| {
                let animation = animation.root();
                animation.dispatch_playback_event(cx, event_type, current_time, timeline_time);
            }));
    }

    /// Dispatch an [`AnimationPlaybackEvent`] at this animation.
    pub(crate) fn dispatch_playback_event(
        &self,
        cx: &mut JSContext,
        event_type: PlaybackEventType,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
    ) {
        let window = self.window();
        let event = AnimationPlaybackEvent::new(
            cx,
            &window,
            event_type.atom(),
            current_time,
            timeline_time,
        );
        event
            .upcast::<crate::dom::event::Event>()
            .fire(cx, self.upcast());
    }

    /// <https://drafts.csswg.org/web-animations-1/#cancel-an-animation>
    pub(crate) fn cancel_an_animation(&self, cx: &mut JSContext) {
        // Step 1. If animation’s play state is not idle, perform the following steps:
        if self.play_state() != AnimationPlayState::Idle {
            // Step 1.1. Run the procedure to reset an animation’s pending tasks on animation.
            self.reset_pending_tasks(cx);

            // Step 1.2. Reject the current finished promise with a DOMException named
            // "AbortError".
            // Step 1.3. Set the [[PromiseIsHandled]] internal slot of the current finished
            // promise to true.
            let finished_promise = self.finished_promise(cx);
            if finished_promise.is_pending() {
                finished_promise
                    .reject_error(cx, Error::Abort(Some("The animation was canceled".into())));
            }
            finished_promise.set_promise_is_handled(cx);

            // Step 1.4. Let current finished promise be a new promise in the relevant Realm of
            // animation.
            let promise = Promise::new(cx, &self.global());
            self.set_finished_promise(&promise);

            // Step 1.5. Create an AnimationPlaybackEvent, cancelEvent.
            // Step 1.6. Set cancelEvent’s type attribute to cancel.
            // Step 1.7. Set cancelEvent’s currentTime to null.
            // Step 1.8. Let timeline time be the current time of the timeline with which
            // animation is associated. If animation is not associated with an active timeline,
            // let timeline time be an unresolved time value.
            // Step 1.9. Set cancelEvent’s timelineTime to timeline time. If timeline time is
            // unresolved, set it to null.
            // Step 1.10. If animation has a document for timing, then append cancelEvent to its
            // document for timing’s pending animation event queue along with its target,
            // animation. If animation is associated with an active timeline that defines a
            // procedure to convert timeline times to origin-relative time, let the scheduled
            // event time be the result of applying that procedure to timeline time. Otherwise,
            // the scheduled event time is an unresolved time value. Otherwise, queue a task to
            // dispatch cancelEvent at animation. The task source for this task is the DOM
            // manipulation task source.
            let timeline_time = self.timeline_time();
            let origin_time = self
                .timeline
                .get()
                .and_then(|timeline| {
                    timeline
                        .downcast::<DocumentTimeline>()
                        .map(|timeline| timeline.origin_time_in_milliseconds())
                })
                .unwrap_or(0.0);
            self.queue_playback_event(
                PlaybackEventType::Cancel,
                None,
                timeline_time,
                timeline_time.map(|time| time + origin_time),
            );
        }

        // Step 2. Make animation’s hold time unresolved.
        self.hold_time.set(None);

        // Step 3. Make animation’s start time unresolved.
        self.start_time.set(None);
    }

    /// <https://drafts.csswg.org/web-animations-1/#reset-an-animations-pending-tasks>
    fn reset_pending_tasks(&self, cx: &mut JSContext) {
        // Step 1. If animation does not have a pending play task or a pending pause task, abort
        // this procedure.
        if !self.has_pending_task() {
            return;
        }

        // Step 2. If animation has a pending play task, cancel that task.
        // Step 3. If animation has a pending pause task, cancel that task.
        self.pending_task.set(PendingTask::None);

        // Step 4. Apply any pending playback rate on animation.
        self.apply_any_pending_playback_rate();

        // Step 5. Reject animation’s current ready promise with a DOMException named
        // "AbortError".
        // Step 6. Set the [[PromiseIsHandled]] internal slot of animation’s current ready
        // promise to true.
        let ready_promise = self.ready_promise(cx);
        ready_promise.reject_error(
            cx,
            Error::Abort(Some(
                "The animation's pending play or pause was canceled".into(),
            )),
        );
        ready_promise.set_promise_is_handled(cx);

        // Step 7. Let animation’s current ready promise be the result of creating a new resolved
        // Promise object with value animation in the relevant Realm of animation.
        let promise = Promise::new(cx, &self.global());
        promise.resolve_native(cx, &DomRoot::from_ref(self));
        self.set_ready_promise(&promise);
    }

    /// <https://drafts.csswg.org/web-animations-1/#finish-an-animation>
    fn finish_an_animation(&self, cx: &mut JSContext) -> ErrorResult {
        // Step 1. If animation’s effective playback rate is zero, or if animation’s effective
        // playback rate > 0 and associated effect end is infinity, throw an "InvalidStateError"
        // DOMException and abort these steps.
        let rate = self.effective_playback_rate();
        if rate == 0.0 || (rate > 0.0 && self.associated_effect_end() == f64::INFINITY) {
            return Err(Error::InvalidState(Some(
                "An animation with a playback rate of zero or an infinite end cannot finish".into(),
            )));
        }

        // Step 2. Apply any pending playback rate to animation.
        self.apply_any_pending_playback_rate();

        // Step 3. Set limit as follows: If playback rate > 0, Let limit be associated effect
        // end. Otherwise, Let limit be zero.
        let rate = self.playback_rate.get();
        let limit = if rate > 0.0 {
            self.associated_effect_end()
        } else {
            0.0
        };

        // Step 4. Silently set the current time to limit.
        self.silently_set_the_current_time(Some(limit))?;

        // Step 5. If animation’s start time is unresolved and animation has an associated active
        // timeline, let the start time be the result of evaluating timeline time - (limit /
        // playback rate) where timeline time is the current time value of the associated
        // timeline.
        if self.start_time.get().is_none() &&
            let Some(timeline_time) = self.timeline_time()
        {
            self.start_time.set(Some(timeline_time - limit / rate));
        }

        // Step 6. If there is a pending pause task and start time is resolved,
        if self.pending_task.get() == PendingTask::Pause && self.start_time.get().is_some() {
            // Step 6.1. Let the hold time be unresolved.
            self.hold_time.set(None);
            // Step 6.2. Cancel the pending pause task.
            self.pending_task.set(PendingTask::None);
            // Step 6.3. Resolve the current ready promise of animation with animation.
            self.resolve_ready_promise(cx);
        }

        // Step 7. If there is a pending play task and start time is resolved, cancel that task
        // and resolve the current ready promise of animation with animation.
        if self.pending_task.get() == PendingTask::Play && self.start_time.get().is_some() {
            self.pending_task.set(PendingTask::None);
            self.resolve_ready_promise(cx);
        }

        // Step 8. Run the procedure to update an animation’s finished state for animation with
        // the did seek flag set to true, and the synchronously notify flag set to true.
        self.update_finished_state(cx, true, true);
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#set-the-playback-rate>
    fn set_the_playback_rate(&self, cx: &mut JSContext, new_playback_rate: f64) {
        // Step 1. Clear any pending playback rate on animation.
        self.pending_playback_rate.set(None);

        // Step 2. Let previous time be the value of the current time of animation before
        // changing the playback rate.
        let previous_time = self.current_time();

        // Step 3. Let previous playback rate be the current effective playback rate of
        // animation.
        // Step 4. Set the playback rate to new playback rate.
        self.playback_rate.set(new_playback_rate);

        // Step 5. Perform the steps corresponding to the first matching condition from the
        // following, if any: If animation is associated with a monotonically increasing
        // timeline and the previous time is resolved, Set the current time of animation to
        // previous time.
        //
        // Document timelines are monotonically increasing, so the second condition, for
        // timelines that are not, never applies.
        if self.timeline.get().is_some() && previous_time.is_some() {
            // Setting a resolved current time cannot throw.
            let _ = self.set_the_current_time(cx, previous_time);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#seamlessly-update-the-playback-rate>
    fn seamlessly_update_the_playback_rate(
        &self,
        cx: &mut JSContext,
        new_playback_rate: f64,
    ) -> ErrorResult {
        // Step 1. Let previous play state be animation’s play state.
        let previous_play_state = self.play_state();

        // Step 2. Let animation’s pending playback rate be new playback rate.
        self.pending_playback_rate.set(Some(new_playback_rate));

        // Step 3. Perform the steps corresponding to the first matching condition below:
        if self.has_pending_task() {
            // > If animation has a pending play task or a pending pause task, Abort these steps.
            return Ok(());
        }
        match previous_play_state {
            AnimationPlayState::Idle | AnimationPlayState::Paused => {
                // > If previous play state is idle or paused, or animation’s current time is
                // > unresolved, Apply any pending playback rate on animation.
                self.apply_any_pending_playback_rate();
            },
            _ if self.current_time().is_none() => self.apply_any_pending_playback_rate(),
            AnimationPlayState::Finished => {
                // > If previous play state is finished,
                // > 1. Let the unconstrained current time be the result of calculating the
                // > current time of animation substituting an unresolved time value for the
                // > hold time.
                let unconstrained_current_time = self.current_time_given_hold_time(None);

                // > 2. Let animation’s start time be the result of evaluating the following
                // > expression: timeline time - (unconstrained current time / pending playback
                // > rate) Where timeline time is the current time value of the timeline
                // > associated with animation. If pending playback rate is zero, let
                // > animation’s start time be timeline time.
                let timeline_time = self.timeline_time();
                self.start_time.set(if new_playback_rate == 0.0 {
                    timeline_time
                } else {
                    timeline_time.zip(unconstrained_current_time).map(
                        |(timeline_time, current_time)| {
                            timeline_time - current_time / new_playback_rate
                        },
                    )
                });

                // > 3. Apply any pending playback rate on animation.
                self.apply_any_pending_playback_rate();

                // > 4. Run the procedure to update an animation’s finished state for animation
                // > with the did seek flag set to false, and the synchronously notify flag set
                // > to false.
                self.update_finished_state(cx, false, false);
            },
            AnimationPlayState::Running => {
                // > Otherwise, Run the procedure to play an animation for animation with the
                // > auto-rewind flag set to false.
                self.play_an_animation(cx, false)?;
            },
        }
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#reverse-an-animation>
    fn reverse_an_animation(&self, cx: &mut JSContext) -> ErrorResult {
        // Step 1. If there is no timeline associated with animation, or the associated timeline
        // is inactive throw an "InvalidStateError" DOMException and abort these steps.
        if self.timeline_time().is_none() {
            return Err(Error::InvalidState(Some(
                "An animation without an active timeline cannot reverse".into(),
            )));
        }

        // Step 2. Let original pending playback rate be animation’s pending playback rate.
        let original_pending_playback_rate = self.pending_playback_rate.get();

        // Step 3. Let animation’s pending playback rate be the additive inverse of its effective
        // playback rate (i.e. -effective playback rate).
        //
        // The additive inverse of zero is zero, not negative zero.
        self.pending_playback_rate
            .set(Some(0.0 - self.effective_playback_rate()));

        // Step 4. Run the steps to play an animation for animation with the auto-rewind flag set
        // to true. If the steps to play an animation throw an exception, set animation’s pending
        // playback rate to original pending playback rate and propagate the exception.
        self.play_an_animation(cx, true).inspect_err(|_| {
            self.pending_playback_rate
                .set(original_pending_playback_rate);
        })
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-animation>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        // Step 1. Let animation be a new Animation object.
        let animation = Animation::new_with_proto(cx, window.upcast(), proto);

        // Step 2. Run the procedure to set the timeline of an animation on animation passing
        // timeline as the new timeline; or, if the timeline argument is missing, passing the
        // default document timeline of the Document associated with the Window that is the
        // current global object.
        match timeline {
            Some(timeline) => animation.set_the_timeline(cx, timeline),
            None => {
                let document = window.Document();
                let timeline = document.Timeline();
                animation.set_the_timeline(cx, Some(timeline.upcast()));
            },
        }

        // Step 3. Run the procedure to set the associated effect of an animation on animation
        // passing source as the new effect.
        animation.set_the_associated_effect(cx, effect);

        animation
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn Id(&self) -> DOMString {
        self.id.borrow().clone()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-id>
    fn SetId(&self, id: DOMString) {
        self.set_id(id);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-effect>
    fn SetEffect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        // > Setting this attribute updates the object’s associated effect using
        // > the procedure to set the associated effect of an animation.
        self.set_the_associated_effect(cx, effect);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-timeline>
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-timeline>
    fn SetTimeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        // > Setting this attribute updates the object’s timeline using the procedure
        // > to set the timeline of an animation.
        self.set_the_timeline(cx, timeline);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn GetStartTime(&self) -> Option<Finite<f64>> {
        // > The start time of this animation. Setting this attribute updates the start time
        // > using the procedure to set the start time of this object to the new value.
        self.start_time.get().and_then(Finite::new)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-starttime>
    fn SetStartTime(&self, cx: &mut JSContext, start_time: Option<Finite<f64>>) {
        self.set_the_start_time(cx, start_time.map(|time| *time));
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        // > The current time of this animation. Setting this attribute follows the procedure
        // > to set the current time of this object to the new value.
        self.current_time().and_then(Finite::new)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-currenttime>
    fn SetCurrentTime(
        &self,
        cx: &mut JSContext,
        current_time: Option<Finite<f64>>,
    ) -> Fallible<()> {
        self.set_the_current_time(cx, current_time.map(|time| *time))
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn PlaybackRate(&self) -> Finite<f64> {
        // > The playback rate of this animation. Setting this attribute follows the procedure
        // > to set the playback rate of this object to the new value.
        Finite::wrap(self.playback_rate.get())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playbackrate>
    fn SetPlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) {
        self.set_the_playback_rate(cx, *playback_rate);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-playstate>
    fn PlayState(&self) -> AnimationPlayState {
        self.play_state()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pending>
    fn Pending(&self) -> bool {
        // > Returns true if this animation has a pending play task or a pending pause task.
        self.has_pending_task()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-ready>
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        // > Returns the current ready promise for this object.
        self.ready_promise(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finished>
    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        // > Returns the current finished promise for this object.
        self.finished_promise(cx)
    }

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-onfinish>
    event_handler!(finish, GetOnfinish, SetOnfinish);

    // <https://drafts.csswg.org/web-animations-1/#dom-animation-oncancel>
    event_handler!(cancel, GetOncancel, SetOncancel);

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-cancel>
    fn Cancel(&self, cx: &mut JSContext) {
        // > Clears all effects caused by this animation and aborts its playback by running the
        // > cancel an animation procedure for this object.
        self.cancel_an_animation(cx);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-finish>
    fn Finish(&self, cx: &mut JSContext) -> Fallible<()> {
        // > Seeks the animation to the associated effect end in the current direction by
        // > running the finish an animation procedure for this object.
        self.finish_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-play>
    fn Play(&self, cx: &mut JSContext) -> Fallible<()> {
        // > Begins or resumes playback of the animation by running the procedure to play an
        // > animation passing true as the value of the auto-rewind flag.
        self.play_an_animation(cx, true)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-pause>
    fn Pause(&self, cx: &mut JSContext) -> Fallible<()> {
        // > Suspends the playback of this animation by running the procedure to pause an
        // > animation for this object.
        self.pause_an_animation(cx)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-updateplaybackrate>
    fn UpdatePlaybackRate(&self, cx: &mut JSContext, playback_rate: Finite<f64>) -> Fallible<()> {
        // > Performs an asynchronous update of the playback rate of this animation by
        // > performing the seamlessly update the playback rate procedure, passing playbackRate
        // > as the new playback rate.
        self.seamlessly_update_the_playback_rate(cx, *playback_rate)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animation-reverse>
    fn Reverse(&self, cx: &mut JSContext) -> Fallible<()> {
        // > Inverts the playback rate of this animation and plays it using the reverse an
        // > animation procedure for this object.
        self.reverse_an_animation(cx)
    }
}

/// The microtask that runs the finish notification steps of
/// <https://drafts.csswg.org/web-animations-1/#update-an-animations-finished-state>.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct FinishNotificationMicrotask {
    animation: Dom<Animation>,
    generation: u32,
}

impl MicrotaskRunnable for FinishNotificationMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        let animation = &self.animation;
        if !animation.finish_notification_queued.get() ||
            animation.finish_notification_generation.get() != self.generation
        {
            return;
        }
        animation.finish_notification_queued.set(false);
        let mut realm = enter_auto_realm(cx, &**animation);
        animation.run_finish_notification_steps(&mut realm);
    }
}
