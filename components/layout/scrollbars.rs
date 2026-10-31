/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Classic scrollbars: the space they take from a scroll container (its scrollbar
//! gutter) and the thickness given by `scrollbar-width`.
//!
//! From <https://drafts.csswg.org/css-overflow-3/#scrollbar-gutter-property>:
//! > The space between the inner border edge and the outer padding edge which user agents
//! > may reserve to display the scrollbar is called the scrollbar gutter.
//!
//! and from <https://www.w3.org/TR/CSS21/visufx.html#overflow>:
//! > Any space taken up by the scrollbars should be taken out of (subtracted from the
//! > dimensions of) the containing block formed by the element with the scrollbars.

use std::hash::{DefaultHasher, Hash, Hasher};

use app_units::Au;
use bitflags::bitflags;
use style::Zero;
use style::computed_values::direction::T as Direction;
use style::computed_values::scrollbar_width::T as ScrollbarWidth;
use style::properties::ComputedValues;
use style::values::computed::ScrollbarGutter;
use style::values::specified::Overflow;
use webrender_api as wr;

use crate::fragment_tree::{BoxFragmentWithStyle, FragmentFlags};
use crate::geom::{PhysicalPoint, PhysicalRect, PhysicalSides, PhysicalSize};
use crate::style_ext::ComputedValuesExt;

/// The thickness of a classic scrollbar for `scrollbar-width: auto`, in CSS pixels. This
/// is the thickness of Chromium's classic scrollbars on Linux and Windows, so that pages
/// sized for them lay out the same way.
const AUTO_SCROLLBAR_THICKNESS_PX: i32 = 15;

/// The thickness of a classic scrollbar for `scrollbar-width: thin`, in CSS pixels, again
/// matching Chromium.
const THIN_SCROLLBAR_THICKNESS_PX: i32 = 10;

/// The space between a scrollbar thumb and the long edges of its track, in CSS pixels.
const THUMB_INSET_PX: i32 = 2;

bitflags! {
    /// The physical scrollbars of a scroll container.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub(crate) struct ScrollbarAxes: u8 {
        /// The scrollbar along the right (or left) edge, scrolling vertically.
        const VERTICAL = 1 << 0;
        /// The scrollbar along the bottom edge, scrolling horizontally.
        const HORIZONTAL = 1 << 1;
    }
}

/// The thickness of this box's classic scrollbars, from `scrollbar-width`.
/// <https://drafts.csswg.org/css-scrollbars-1/#scrollbar-width>
pub(crate) fn scrollbar_thickness(style: &ComputedValues) -> Au {
    match style.get_ui().scrollbar_width {
        ScrollbarWidth::Auto => Au::from_px(AUTO_SCROLLBAR_THICKNESS_PX),
        ScrollbarWidth::Thin => Au::from_px(THIN_SCROLLBAR_THICKNESS_PX),
        ScrollbarWidth::None => Au::zero(),
    }
}

/// The scrollbars a scroll container shows: `overflow: scroll` always shows one,
/// `overflow: auto` only when the content overflows in that axis, which layout found
/// out the last time it laid the box out (`overflowing_auto_axes`).
pub(crate) fn scrollbars_shown(
    style: &ComputedValues,
    fragment_flags: FragmentFlags,
    overflowing_auto_axes: ScrollbarAxes,
) -> ScrollbarAxes {
    if !style.establishes_scroll_container(fragment_flags) {
        return ScrollbarAxes::empty();
    }
    let overflow = style.effective_overflow(fragment_flags);
    let shown = |overflow: Overflow, axis: ScrollbarAxes| match overflow {
        Overflow::Scroll => true,
        Overflow::Auto => overflowing_auto_axes.contains(axis),
        _ => false,
    };
    let mut axes = ScrollbarAxes::empty();
    axes.set(
        ScrollbarAxes::VERTICAL,
        shown(overflow.y, ScrollbarAxes::VERTICAL),
    );
    axes.set(
        ScrollbarAxes::HORIZONTAL,
        shown(overflow.x, ScrollbarAxes::HORIZONTAL),
    );
    axes
}

/// The axes in which this box has `overflow: auto`, the ones where whether a scrollbar
/// is shown depends on the content.
pub(crate) fn auto_axes(style: &ComputedValues, fragment_flags: FragmentFlags) -> ScrollbarAxes {
    if !style.establishes_scroll_container(fragment_flags) {
        return ScrollbarAxes::empty();
    }
    let overflow = style.effective_overflow(fragment_flags);
    let mut axes = ScrollbarAxes::empty();
    axes.set(ScrollbarAxes::VERTICAL, overflow.y == Overflow::Auto);
    axes.set(ScrollbarAxes::HORIZONTAL, overflow.x == Overflow::Auto);
    axes
}

/// Whether the vertical scrollbar of this box sits on its left edge. Like other
/// engines, a right-to-left box in a horizontal writing mode puts it there.
pub(crate) fn vertical_scrollbar_on_left(style: &ComputedValues) -> bool {
    style.writing_mode.is_horizontal() && style.get_inherited_box().direction == Direction::Rtl
}

/// The scrollbar gutters of this box on each physical side: the space between the inner
/// border edge and the outer padding edge taken by its classic scrollbars, or reserved for
/// them by `scrollbar-gutter`. Zero for boxes that are not scroll containers.
/// <https://drafts.csswg.org/css-overflow-3/#scrollbar-gutter-property>
pub(crate) fn scrollbar_gutter(
    style: &ComputedValues,
    fragment_flags: FragmentFlags,
    overflowing_auto_axes: ScrollbarAxes,
) -> PhysicalSides<Au> {
    let mut gutter = PhysicalSides::zero();
    let thickness = scrollbar_thickness(style);
    if thickness.is_zero() || !style.establishes_scroll_container(fragment_flags) {
        return gutter;
    }

    let mut shown = scrollbars_shown(style, fragment_flags, overflowing_auto_axes);

    // > The scrollbar-gutter property gives control to authors over the presence of
    // > scrollbar gutters separately from the ability to control the presence of
    // > scrollbars. [...] This property applies to the scrollbar in the inline axis
    // > [...] when the box is a scroll container: `stable` reserves its gutter even
    // > when the scrollbar is not shown, `both-edges` also on the opposite edge.
    let scrollbar_gutter = style.get_box().scrollbar_gutter;
    let inline_axis_bar = if style.writing_mode.is_horizontal() {
        ScrollbarAxes::VERTICAL
    } else {
        ScrollbarAxes::HORIZONTAL
    };
    let stable = scrollbar_gutter.contains(ScrollbarGutter::STABLE);
    if stable {
        shown.insert(inline_axis_bar);
    }
    let both_edges = stable && scrollbar_gutter.contains(ScrollbarGutter::BOTH_EDGES);

    if shown.contains(ScrollbarAxes::VERTICAL) {
        if vertical_scrollbar_on_left(style) {
            gutter.left = thickness;
        } else {
            gutter.right = thickness;
        }
        if both_edges && inline_axis_bar == ScrollbarAxes::VERTICAL {
            gutter.left = thickness;
            gutter.right = thickness;
        }
    }
    if shown.contains(ScrollbarAxes::HORIZONTAL) {
        gutter.bottom = thickness;
        if both_edges && inline_axis_bar == ScrollbarAxes::HORIZONTAL {
            gutter.top = thickness;
        }
    }
    gutter
}

/// One classic scrollbar of a scroll container, as painted: its track and its thumb at the
/// start of the track. The painter moves the thumb along the track by up to `travel` as
/// the container scrolls.
pub(crate) struct ScrollbarParts {
    pub vertical: bool,
    pub track: PhysicalRect<Au>,
    /// The thumb, absent when there is nothing to scroll (`overflow: scroll` with
    /// content that fits).
    pub thumb: Option<PhysicalRect<Au>>,
    pub travel: Au,
}

/// The classic scrollbars of a scroll container fragment, in the coordinates of its
/// containing block offset by `origin`, and the corner where they meet. A scrollbar is
/// painted in its gutter when it is shown (`overflow: scroll`, or `overflow: auto` with
/// content that overflows in that axis).
pub(crate) fn scrollbar_parts(
    fragment: &BoxFragmentWithStyle<'_>,
    origin: PhysicalPoint<Au>,
) -> (Vec<ScrollbarParts>, Option<PhysicalRect<Au>>) {
    let style = fragment.style();
    let gutter = fragment.scrollbar_gutter;
    let thickness = scrollbar_thickness(style);
    if thickness.is_zero() || gutter == PhysicalSides::zero() {
        return (Vec::new(), None);
    }

    let scrollport = fragment.padding_rect().translate(origin.to_vector());
    let scrollable_overflow = fragment.scrollable_overflow();
    let overflowing_axes = {
        let mut axes = ScrollbarAxes::empty();
        axes.set(
            ScrollbarAxes::VERTICAL,
            scrollable_overflow.size.height > scrollport.size.height,
        );
        axes.set(
            ScrollbarAxes::HORIZONTAL,
            scrollable_overflow.size.width > scrollport.size.width,
        );
        axes
    };
    let shown = scrollbars_shown(style, fragment.base.flags, overflowing_axes);

    let vertical_gutter = if vertical_scrollbar_on_left(style) {
        gutter.left
    } else {
        gutter.right
    };
    let inset = Au::from_px(THUMB_INSET_PX);
    let mut parts = Vec::new();

    let vertical = shown.contains(ScrollbarAxes::VERTICAL) && vertical_gutter >= thickness;
    if vertical {
        let x = if vertical_scrollbar_on_left(style) {
            scrollport.min_x() - thickness
        } else {
            scrollport.max_x()
        };
        let track = PhysicalRect::new(
            PhysicalPoint::new(x, scrollport.min_y()),
            PhysicalSize::new(thickness, scrollport.size.height),
        );
        let (thumb_length, travel) = thumb_length_and_travel(
            track.size.height,
            scrollport.size.height,
            scrollable_overflow.size.height,
            thickness,
        );
        let thumb = overflowing_axes.contains(ScrollbarAxes::VERTICAL).then(|| {
            PhysicalRect::new(track.origin, PhysicalSize::new(thickness, thumb_length))
                .inflate(-inset, -inset)
        });
        parts.push(ScrollbarParts {
            vertical: true,
            track,
            thumb,
            travel,
        });
    }

    let horizontal = shown.contains(ScrollbarAxes::HORIZONTAL) && gutter.bottom >= thickness;
    if horizontal {
        let track = PhysicalRect::new(
            PhysicalPoint::new(scrollport.min_x(), scrollport.max_y()),
            PhysicalSize::new(scrollport.size.width, thickness),
        );
        let (thumb_length, travel) = thumb_length_and_travel(
            track.size.width,
            scrollport.size.width,
            scrollable_overflow.size.width,
            thickness,
        );
        let thumb = overflowing_axes
            .contains(ScrollbarAxes::HORIZONTAL)
            .then(|| {
                PhysicalRect::new(track.origin, PhysicalSize::new(thumb_length, thickness))
                    .inflate(-inset, -inset)
            });
        parts.push(ScrollbarParts {
            vertical: false,
            track,
            thumb,
            travel,
        });
    }

    let corner = (vertical && horizontal).then(|| {
        let vertical_track = parts[0].track;
        PhysicalRect::new(
            PhysicalPoint::new(vertical_track.min_x(), scrollport.max_y()),
            PhysicalSize::new(thickness, thickness),
        )
    });
    (parts, corner)
}

/// The length of a thumb on a track of `track_length` for a scrollport showing
/// `visible_length` of `content_length`, and how far it travels along the track. The
/// thumb is never shorter than the track is thick, so that it can still be seen.
fn thumb_length_and_travel(
    track_length: Au,
    visible_length: Au,
    content_length: Au,
    thickness: Au,
) -> (Au, Au) {
    if content_length <= Au::zero() || track_length <= Au::zero() {
        return (track_length, Au::zero());
    }
    let proportion = (visible_length.to_f32_px() / content_length.to_f32_px()).min(1.0);
    let thumb_length = track_length
        .scale_by(proportion)
        .max(thickness.min(track_length));
    (thumb_length, track_length - thumb_length)
}

/// The id of the WebRender transform binding that moves the thumb of one scrollbar of a
/// scroll frame. It only has to be stable for that scroll frame across display lists
/// (so that the transform the painter last set keeps applying) and distinct between
/// scroll frames, so it is a hash of the frame's external scroll id and the axis.
pub(crate) fn thumb_transform_key(external_scroll_id: wr::ExternalScrollId, vertical: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    external_scroll_id.hash(&mut hasher);
    vertical.hash(&mut hasher);
    hasher.finish()
}
