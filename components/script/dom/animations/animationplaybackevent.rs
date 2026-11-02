/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The event an [`Animation`](crate::dom::animation::Animation) fires when it finishes, is
//! canceled or is removed.

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::AnimationPlaybackEventBinding::{
    AnimationPlaybackEventInit, AnimationPlaybackEventMethods,
};
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/web-animations-1/#the-animationplaybackevent-interface>
#[dom_struct]
pub(crate) struct AnimationPlaybackEvent {
    event: Event,
    current_time: Option<Finite<f64>>,
    timeline_time: Option<Finite<f64>>,
}

impl AnimationPlaybackEvent {
    /// Create an event of type `type_` for an animation: it does not bubble and cannot be
    /// canceled.
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        type_: Atom,
        current_time: Option<f64>,
        timeline_time: Option<f64>,
    ) -> DomRoot<AnimationPlaybackEvent> {
        let event = reflect_dom_object_with_proto(
            cx,
            Box::new(AnimationPlaybackEvent {
                event: Event::new_inherited(),
                current_time: current_time.and_then(Finite::new),
                timeline_time: timeline_time.and_then(Finite::new),
            }),
            window,
            None,
        );
        event.upcast::<Event>().init_event(type_, false, false);
        event
    }
}

impl AnimationPlaybackEventMethods<crate::DomTypeHolder> for AnimationPlaybackEvent {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationplaybackevent-animationplaybackevent>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &AnimationPlaybackEventInit,
    ) -> DomRoot<AnimationPlaybackEvent> {
        let event = reflect_dom_object_with_proto(
            cx,
            Box::new(AnimationPlaybackEvent {
                event: Event::new_inherited(),
                current_time: init.currentTime,
                timeline_time: init.timelineTime,
            }),
            window,
            proto,
        );
        event.upcast::<Event>().init_event(
            Atom::from(type_),
            init.parent.bubbles,
            init.parent.cancelable,
        );
        event
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationplaybackevent-currenttime>
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationplaybackevent-timelinetime>
    fn GetTimelineTime(&self) -> Option<Finite<f64>> {
        self.timeline_time
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}
