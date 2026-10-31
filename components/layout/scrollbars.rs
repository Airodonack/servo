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

use app_units::Au;
use bitflags::bitflags;
use style::Zero;
use style::computed_values::direction::T as Direction;
use style::computed_values::scrollbar_width::T as ScrollbarWidth;
use style::properties::ComputedValues;
use style::values::computed::ScrollbarGutter;
use style::values::specified::Overflow;

use crate::fragment_tree::FragmentFlags;
use crate::geom::PhysicalSides;
use crate::style_ext::ComputedValuesExt;

/// The thickness of a classic scrollbar for `scrollbar-width: auto`, in CSS pixels. This
/// is the thickness of Chromium's classic scrollbars on Linux and Windows, so that pages
/// sized for them lay out the same way.
const AUTO_SCROLLBAR_THICKNESS_PX: i32 = 15;

/// The thickness of a classic scrollbar for `scrollbar-width: thin`, in CSS pixels, again
/// matching Chromium.
const THIN_SCROLLBAR_THICKNESS_PX: i32 = 10;

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
