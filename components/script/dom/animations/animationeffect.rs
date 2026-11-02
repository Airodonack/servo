/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    AnimationEffectMethods, ComputedEffectTiming, EffectTiming, FillMode, OptionalEffectTiming,
    PlaybackDirection,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::num::Finite;
use script_bindings::reflector::Reflector;
use script_bindings::root::{Dom, DomRoot};
use script_bindings::str::DOMString;
use style::parser::Parse;
use style::stylesheets::CssRuleType;
use style::values::generics::easing::{BeforeFlag, TimingKeyword};
use style::values::specified::TimingFunction;
use style_traits::{ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Window;
use crate::dom::animation::Animation;
use crate::dom::bindings::codegen::UnionTypes::UnrestrictedDoubleOrString;
use crate::dom::bindings::root::MutNullableDom;

/// <https://drafts.csswg.org/web-animations-1/#animationeffect>
#[dom_struct]
pub(crate) struct AnimationEffect {
    reflector: Reflector,

    /// The window that this `AnimationEffect` was constructed in.
    window: Dom<Window>,

    specified_timing_properties: DomRefCell<SpecifiedTimingProperties>,

    /// <https://drafts.csswg.org/web-animations-1/#animation-effect-associated-with-an-animation>
    associated_animation: MutNullableDom<Animation>,
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct SpecifiedTimingProperties {
    /// <https://drafts.csswg.org/web-animations-1/#start-delay>
    start_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#end-delay>
    end_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#fill-mode>
    fill_mode: FillMode,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-count>
    iteration_count: f64,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-start>
    iteration_start: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-duration>
    iteration_duration: IterationDurationOrAuto,

    /// <https://drafts.csswg.org/web-animations-1/#playback-direction>
    playback_direction: PlaybackDirection,

    /// <https://drafts.csswg.org/css-easing-2/#easing-function>
    #[no_trace]
    easing_function: TimingFunction,
}

impl AnimationEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            reflector: Reflector::new(),
            window: Dom::from_ref(window),

            // The defaults of the EffectTiming dictionary. The constructors assign the timing
            // they are given over them.
            specified_timing_properties: DomRefCell::new(SpecifiedTimingProperties {
                start_delay: Default::default(),
                end_delay: Default::default(),
                fill_mode: FillMode::Auto,
                iteration_count: 1.0,
                iteration_start: Default::default(),
                iteration_duration: IterationDurationOrAuto::Auto,
                playback_direction: PlaybackDirection::Normal,
                easing_function: TimingFunction::Keyword(TimingKeyword::Linear),
            }),
            associated_animation: Default::default(),
        }
    }

    /// The animation whose associated effect this is, if any.
    pub(crate) fn associated_animation(&self) -> Option<DomRoot<Animation>> {
        self.associated_animation.get()
    }

    /// Only for [`Animation`], which keeps both sides of the association in step.
    pub(crate) fn set_associated_animation(&self, animation: Option<&Animation>) {
        self.associated_animation.set(animation);
    }

    /// <https://drafts.csswg.org/web-animations-1/#active-duration>
    pub(crate) fn active_duration(&self) -> f64 {
        let timing = self.specified_timing_properties.borrow();
        let iteration_duration = timing.iteration_duration.computed_value();

        // > If either the iteration duration or iteration count are zero, the active duration is
        // > zero.
        if iteration_duration == 0.0 || timing.iteration_count == 0.0 {
            return 0.0;
        }

        // > active duration = iteration duration × iteration count
        iteration_duration * timing.iteration_count
    }

    /// <https://drafts.csswg.org/web-animations-1/#local-time>
    pub(crate) fn local_time(&self) -> Option<f64> {
        // > If the animation effect is associated with an animation, the local time is the
        // > current time of the animation. Otherwise, the local time is unresolved.
        self.associated_animation()?.current_time()
    }

    /// <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
    ///
    /// `None` is the idle phase: the local time is unresolved.
    pub(crate) fn phase(&self, local_time: Option<f64>) -> Option<Phase> {
        let local_time = local_time?;
        let start_delay = *self.specified_timing_properties.borrow().start_delay;
        let active_duration = self.active_duration();
        let end_time = self.end_time();

        // > animation direction: “backwards” if the effect is associated with an animation and
        // > the associated animation’s playback rate is less than zero; in all other cases, the
        // > animation direction is “forward”.
        let backwards = self
            .associated_animation()
            .is_some_and(|animation| animation.playback_rate() < 0.0);

        // > before-active boundary time: max(min(start delay, end time), 0)
        let before_active_boundary_time = start_delay.min(end_time).max(0.0);

        // > active-after boundary time: max(min(start delay + active duration, end time), 0)
        let active_after_boundary_time = (start_delay + active_duration).min(end_time).max(0.0);

        // > An animation effect is in the before phase if the animation effect’s local time is
        // > not unresolved and either of the following conditions are met: the local time is
        // > less than the before-active boundary time, or the animation direction is
        // > “backwards”, the endpoint-inclusive active interval flag is false, and the local time
        // > is equal to the before-active boundary time.
        if local_time < before_active_boundary_time ||
            (backwards && local_time == before_active_boundary_time)
        {
            return Some(Phase::Before);
        }

        // > An animation effect is in the after phase if the animation effect’s local time is
        // > not unresolved and either of the following conditions are met: the local time is
        // > greater than the active-after boundary time, or the animation direction is
        // > “forwards”, the endpoint-inclusive active interval flag is false, and the local time
        // > is equal to the active-after boundary time.
        if local_time > active_after_boundary_time ||
            (!backwards && local_time == active_after_boundary_time)
        {
            return Some(Phase::After);
        }

        // > An animation effect is in the active phase if the animation effect’s local time is
        // > not unresolved and it is neither in the before phase nor in the after phase.
        Some(Phase::Active)
    }

    /// <https://drafts.csswg.org/web-animations-1/#calculating-the-active-time>
    fn active_time(&self, local_time: Option<f64>, phase: Option<Phase>) -> Option<f64> {
        let local_time = local_time?;
        let (start_delay, fill_mode) = {
            let timing = self.specified_timing_properties.borrow();
            (*timing.start_delay, timing.fill_mode)
        };
        match phase? {
            // > If the animation effect is in the before phase, ... If the fill mode is
            // > backwards or both, Return the result of evaluating max(local time - start delay,
            // > 0). Otherwise, Return an unresolved time value.
            Phase::Before => matches!(fill_mode, FillMode::Backwards | FillMode::Both)
                .then(|| (local_time - start_delay).max(0.0)),
            // > If the animation effect is in the active phase, Return the result of evaluating
            // > local time - start delay.
            Phase::Active => Some(local_time - start_delay),
            // > If the animation effect is in the after phase, ... If the fill mode is forwards
            // > or both, Return the result of evaluating max(min(local time - start delay,
            // > active duration), 0). Otherwise, Return an unresolved time value.
            Phase::After => matches!(fill_mode, FillMode::Forwards | FillMode::Both).then(|| {
                (local_time - start_delay)
                    .min(self.active_duration())
                    .max(0.0)
            }),
        }
    }

    /// The timing model's values at the current local time, from
    /// <https://drafts.csswg.org/web-animations-1/#calculating-progress>.
    fn calculated_timing(&self) -> CalculatedTiming {
        let local_time = self.local_time();
        let phase = self.phase(local_time);
        let active_time = self.active_time(local_time, phase);
        let timing = self.specified_timing_properties.borrow();
        let iteration_duration = timing.iteration_duration.computed_value();
        let iteration_count = timing.iteration_count;
        let iteration_start = *timing.iteration_start;
        let active_duration = self.active_duration();

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-overall-progress>
        // > If the active time is unresolved, return unresolved.
        let overall_progress = active_time.map(|active_time| {
            // > If the iteration duration is zero, If the animation effect is in the before
            // > phase, let overall progress be zero; otherwise, let it be equal to the iteration
            // > count. Otherwise, Let overall progress be the result of calculating active time
            // > / iteration duration.
            let overall_progress = if iteration_duration == 0.0 {
                if phase == Some(Phase::Before) {
                    0.0
                } else {
                    iteration_count
                }
            } else {
                active_time / iteration_duration
            };
            // > Return the result of calculating overall progress + iteration start.
            overall_progress + iteration_start
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-simple-iteration-progress>
        // > If the overall progress is unresolved, return unresolved.
        let simple_iteration_progress = overall_progress.map(|overall_progress| {
            // > If overall progress is infinity, let the simple iteration progress be iteration
            // > start % 1.0; otherwise, let the simple iteration progress be overall progress %
            // > 1.0.
            let simple_iteration_progress = if overall_progress.is_infinite() {
                iteration_start % 1.0
            } else {
                overall_progress % 1.0
            };
            // > If all of the following conditions are true: the simple iteration progress
            // > calculated above is zero, and the animation effect is in the active phase or the
            // > after phase, and the active time is equal to the active duration, and the
            // > iteration count is not equal to zero. Let the simple iteration progress be 1.0.
            if simple_iteration_progress == 0.0 &&
                matches!(phase, Some(Phase::Active | Phase::After)) &&
                active_time == Some(active_duration) &&
                iteration_count != 0.0
            {
                1.0
            } else {
                simple_iteration_progress
            }
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-current-iteration>
        // > If the active time is unresolved, return unresolved.
        let current_iteration = overall_progress.map(|overall_progress| {
            // > If the animation effect is in the after phase and the iteration count is
            // > infinity, return infinity.
            if phase == Some(Phase::After) && iteration_count == f64::INFINITY {
                return f64::INFINITY;
            }
            // > If the simple iteration progress is 1.0, return floor(overall progress) - 1.
            // > Otherwise, return floor(overall progress).
            if simple_iteration_progress == Some(1.0) {
                overall_progress.floor() - 1.0
            } else {
                overall_progress.floor()
            }
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-directed-progress>
        // > Calculate the current direction using the first matching condition from the
        // > following list:
        let going_forwards = match timing.playback_direction {
            // > If playback direction is normal, Let the current direction be forwards.
            PlaybackDirection::Normal => true,
            // > If playback direction is reverse, Let the current direction be reverse.
            PlaybackDirection::Reverse => false,
            // > Otherwise (the playback direction is alternate or alternate-reverse), Let d be
            // > the current iteration index. If playback direction is alternate-reverse
            // > increment d by 1. If d % 2 == 0, let the current direction be forwards; otherwise
            // > let the current direction be reverse. If d is infinity, let the current direction
            // > be forwards.
            PlaybackDirection::Alternate | PlaybackDirection::Alternate_reverse => {
                let mut d = current_iteration.unwrap_or(0.0);
                if timing.playback_direction == PlaybackDirection::Alternate_reverse {
                    d += 1.0;
                }
                d.is_infinite() || d % 2.0 == 0.0
            },
        };
        // > If the simple iteration progress is unresolved, return unresolved. ... If the current
        // > direction is forwards then return the simple iteration progress. Otherwise, return
        // > 1.0 - simple iteration progress.
        let directed_progress = simple_iteration_progress.map(|progress| {
            if going_forwards {
                progress
            } else {
                1.0 - progress
            }
        });

        // <https://drafts.csswg.org/web-animations-1/#calculating-the-transformed-progress>
        // > If the directed progress is unresolved, return unresolved. ... The before flag is set
        // > if the animation effect is in the before phase and going forwards is true; or if the
        // > animation effect is in the after phase and going forwards is false. Return the result
        // > of evaluating the effect easing function passing directed progress as the input
        // > progress value and before flag as the before flag.
        let before_flag = if (phase == Some(Phase::Before) && going_forwards) ||
            (phase == Some(Phase::After) && !going_forwards)
        {
            BeforeFlag::Set
        } else {
            BeforeFlag::Unset
        };
        // getComputedTiming().progress is compared against the exact curve, so solve the
        // bezier closely, finer than the precision stylo uses to paint CSS animations.
        let epsilon = 1e-7;
        let transformed_progress = directed_progress.map(|progress| {
            timing
                .easing_function
                .to_computed_value_without_context()
                .calculate_output(progress, before_flag, epsilon)
        });

        CalculatedTiming {
            local_time,
            transformed_progress,
            current_iteration,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#end-time>
    pub(crate) fn end_time(&self) -> f64 {
        // > The end time, of an animation effect is the result of evaluating
        // > max(start delay + active duration + end delay, 0).
        let (start_delay, end_delay) = {
            let timing = self.specified_timing_properties.borrow();
            (*timing.start_delay, *timing.end_delay)
        };
        (start_delay + self.active_duration() + end_delay).max(0.0)
    }

    pub(crate) fn window(&self) -> &Window {
        &self.window
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-the-timing-properties-of-an-animation-effect>
    pub(crate) fn update_the_timing_properties(
        &self,
        input: &OptionalEffectTiming,
    ) -> Fallible<()> {
        // Step 1. If the iterationStart member of input exists and is less than zero,
        // throw a TypeError and abort this procedure.
        if input
            .iterationStart
            .is_some_and(|iteration_start| *iteration_start < 0.0)
        {
            return Err(Error::Type(
                c"Negative values for iterationStart are not allowed".to_owned(),
            ));
        }

        // Step 2. If the iterations member of input exists, and is less than zero or is the value NaN,
        // throw a TypeError and abort this procedure.
        if input
            .iterations
            .is_some_and(|iterations| iterations < 0.0 || iterations.is_nan())
        {
            return Err(Error::Type(
                c"\"iterations\" must be a positive number".to_owned(),
            ));
        }

        // Step 3. If the duration member of input exists, and is less than zero, is the value NaN,
        // or is a string other than the lowercase auto, throw a TypeError and abort this procedure.
        let Ok(duration) = input
            .duration
            .as_ref()
            .map(|duration| match duration {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double) => {
                    if *double < 0.0 || double.is_nan() {
                        Err(())
                    } else {
                        Ok(IterationDurationOrAuto::Duration(*double))
                    }
                },
                UnrestrictedDoubleOrString::String(string) => {
                    if string == "auto" {
                        Ok(IterationDurationOrAuto::Auto)
                    } else {
                        Err(())
                    }
                },
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"duration\" must be a positive number".to_owned(),
            ));
        };

        // Step 4. If the easing member of input exists but cannot be parsed using the <easing-function> production [CSS-EASING-1],
        // throw a TypeError and abort this procedure.
        let Ok(easing) = input
            .easing
            .as_ref()
            .map(|easing| {
                let easing = easing.str();
                let mut parser = Parser::new(&easing);

                // None of these values should matter
                let document = self.window.Document();
                let urlextradata = document.url().into_url().into();
                let parser_context = parser_context_for_document(
                    &document,
                    CssRuleType::Style,
                    ParsingMode::DEFAULT,
                    &urlextradata,
                );
                parser
                    .parse_entirely(|parser| TimingFunction::parse(&parser_context, parser))
                    .map_err(|_| ())
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"easing\" is not a valid timing function".to_owned(),
            ));
        };

        // Step 5. Assign each member that exists in input to the corresponding timing property of effect as follows:
        // delay → start delay
        let mut specified_timing_properties = self.specified_timing_properties.borrow_mut();
        if let Some(start_delay) = input.delay {
            specified_timing_properties.start_delay = start_delay;
        }

        // endDelay → end delay
        if let Some(end_delay) = input.endDelay {
            specified_timing_properties.end_delay = end_delay;
        }

        // fill → fill mode
        if let Some(fill) = input.fill {
            specified_timing_properties.fill_mode = fill;
        }

        // iterationStart → iteration start
        if let Some(iteration_start) = input.iterationStart {
            specified_timing_properties.iteration_start = iteration_start;
        }

        // iterations → iteration count
        if let Some(iterations) = input.iterations {
            specified_timing_properties.iteration_count = iterations;
        }

        // duration → iteration duration
        if let Some(duration) = duration {
            specified_timing_properties.iteration_duration = duration;
        }

        // direction → playback direction
        if let Some(direction) = input.direction {
            specified_timing_properties.playback_direction = direction;
        }

        // easing → easing function
        if let Some(easing) = easing {
            specified_timing_properties.easing_function = easing;
        }
        Ok(())
    }

    /// The last step of
    /// <https://drafts.csswg.org/web-animations-1/#update-the-timing-properties-of-an-animation-effect>,
    /// which needs a JS context: the earlier steps also run while constructing an effect, which
    /// has no associated animation yet.
    fn update_the_associated_animation(&self, cx: &mut JSContext) {
        // > If effect is associated with an animation, run the procedure to update an animation’s
        // > finished state for its associated animation with the did seek flag set to false, and
        // > the synchronously notify flag set to false.
        if let Some(animation) = self.associated_animation() {
            animation.update_finished_state(cx, false, false);
        }
    }
}

impl AnimationEffectMethods<crate::DomTypeHolder> for AnimationEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-gettiming>
    fn GetTiming(&self) -> EffectTiming {
        // > Returns the specified timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();
        EffectTiming {
            delay: specified_timing_properties.start_delay,
            direction: specified_timing_properties.playback_direction,
            duration: specified_timing_properties.iteration_duration.into(),
            easing: specified_timing_properties
                .easing_function
                .to_css_string()
                .into(),
            endDelay: specified_timing_properties.end_delay,
            fill: specified_timing_properties.fill_mode,
            iterationStart: specified_timing_properties.iteration_start,
            iterations: specified_timing_properties.iteration_count,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-getcomputedtiming>
    fn GetComputedTiming(&self) -> ComputedEffectTiming {
        // > Returns the calculated timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();

        // > while getTiming() can return the string auto, getComputedTiming() must return a number
        // > corresponding to the calculated value of the iteration duration as defined in the description
        // > of the duration member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by zero.
        let computed_duration = specified_timing_properties
            .iteration_duration
            .computed_value();

        // > likewise, while getTiming() can return the string auto, getComputedTiming() must return the
        // > specific FillMode used for timing calculations as defined in the description of the fill
        // > member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by the none FillMode.
        let computed_fill_mode = if specified_timing_properties.fill_mode == FillMode::Auto {
            FillMode::None
        } else {
            specified_timing_properties.fill_mode
        };

        let calculated_timing = self.calculated_timing();
        ComputedEffectTiming {
            parent: EffectTiming {
                delay: specified_timing_properties.start_delay,
                direction: specified_timing_properties.playback_direction,
                duration: UnrestrictedDoubleOrString::UnrestrictedDouble(computed_duration),
                easing: specified_timing_properties
                    .easing_function
                    .to_css_string()
                    .into(),
                endDelay: specified_timing_properties.end_delay,
                fill: computed_fill_mode,
                iterationStart: specified_timing_properties.iteration_start,
                iterations: specified_timing_properties.iteration_count,
            },
            // > endTime: The end time of the animation effect expressed in milliseconds since
            // > zero local time
            endTime: Some(self.end_time()),
            // > activeDuration: The active duration of this animation effect.
            activeDuration: Some(self.active_duration()),
            // > localTime: The local time of this animation effect. This will be null if this
            // > animation effect is not associated with an animation.
            localTime: Some(calculated_timing.local_time.and_then(Finite::new)),
            // > progress: The current iteration progress of this animation effect.
            progress: Some(calculated_timing.transformed_progress.and_then(Finite::new)),
            // > currentIteration: The current iteration index beginning with zero for the first
            // > iteration. In most cases this will be a (positive) integer. However, for a zero
            // > duration animation that repeats infinite times, the value will be positive
            // > Infinity. As with unresolved times, an unresolved current iteration is
            // > represented by a null value.
            currentIteration: Some(calculated_timing.current_iteration),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-updatetiming>
    fn UpdateTiming(&self, cx: &mut JSContext, timing: &OptionalEffectTiming) -> Fallible<()> {
        // > Updates the specified timing properties of this animation effect by performing the procedure
        // > to update the timing properties of an animation effect passing the timing parameter as input.
        self.update_the_timing_properties(timing)?;
        self.update_the_associated_animation(cx);
        Ok(())
    }
}

/// <https://drafts.csswg.org/web-animations-1/#animation-effect-phases-and-states>
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Before,
    Active,
    After,
}

/// The values of <https://drafts.csswg.org/web-animations-1/#calculating-progress> at one
/// moment.
struct CalculatedTiming {
    local_time: Option<f64>,
    transformed_progress: Option<f64>,
    current_iteration: Option<f64>,
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
enum IterationDurationOrAuto {
    Duration(f64),
    Auto,
}

impl IterationDurationOrAuto {
    fn computed_value(&self) -> f64 {
        match self {
            IterationDurationOrAuto::Auto => 0.0,
            IterationDurationOrAuto::Duration(double) => *double,
        }
    }
}

impl From<IterationDurationOrAuto> for UnrestrictedDoubleOrString {
    fn from(value: IterationDurationOrAuto) -> Self {
        match value {
            IterationDurationOrAuto::Auto => {
                UnrestrictedDoubleOrString::String(DOMString::from_static("auto"))
            },
            IterationDurationOrAuto::Duration(double) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double)
            },
        }
    }
}
