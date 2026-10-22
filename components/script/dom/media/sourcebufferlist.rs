/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_cx;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::sourcebuffer::SourceBuffer;

/// <https://w3c.github.io/media-source/#sourcebufferlist>
#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    fn new_inherited() -> SourceBufferList {
        SourceBufferList {
            eventtarget: EventTarget::new_inherited(),
            buffers: DomRefCell::new(vec![]),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<SourceBufferList> {
        reflect_dom_object_with_cx(Box::new(SourceBufferList::new_inherited()), global, cx)
    }

    pub(crate) fn buffers(&self) -> Vec<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .iter()
            .map(|buffer| DomRoot::from_ref(&**buffer))
            .collect()
    }

    pub(crate) fn contains(&self, buffer: &SourceBuffer) -> bool {
        self.buffers
            .borrow()
            .iter()
            .any(|candidate| &**candidate == buffer)
    }

    /// Adds a buffer and queues a task to fire addsourcebuffer.
    pub(crate) fn add(&self, buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
        self.queue_event("addsourcebuffer");
    }

    /// Removes a buffer and queues a task to fire removesourcebuffer.
    pub(crate) fn remove(&self, buffer: &SourceBuffer) {
        let removed = {
            let mut buffers = self.buffers.borrow_mut();
            let before = buffers.len();
            buffers.retain(|candidate| &**candidate != buffer);
            buffers.len() != before
        };
        if removed {
            self.queue_event("removesourcebuffer");
        }
    }

    /// Removes every buffer, with one removesourcebuffer event per buffer.
    pub(crate) fn clear(&self, _cx: &mut JSContext) {
        let count = self.buffers.borrow().len();
        self.buffers.borrow_mut().clear();
        for _ in 0..count {
            self.queue_event("removesourcebuffer");
        }
    }

    fn queue_event(&self, name: &str) {
        let this = Trusted::new(self);
        let name = Atom::from(name);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(source_buffer_list_event: move |cx| {
                this.root().upcast::<EventTarget>().fire_event(cx, name);
            }));
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onaddsourcebuffer
    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);

    // https://w3c.github.io/media-source/#dom-sourcebufferlist-onremovesourcebuffer
    event_handler!(
        removesourcebuffer,
        GetOnremovesourcebuffer,
        SetOnremovesourcebuffer
    );

    /// <https://w3c.github.io/media-source/#dfn-sourcebufferlist-getter>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|buffer| DomRoot::from_ref(&**buffer))
    }
}
