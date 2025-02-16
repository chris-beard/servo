/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::cmp::Ordering;

use app_units::Au;
use atomic_refcell::AtomicRefMut;
use itertools::izip;
use style::logical_geometry::WritingMode;
use style::properties::longhands::align_content::computed_value::T as AlignContent;
use style::properties::longhands::align_items::computed_value::T as AlignItems;
use style::properties::longhands::align_self::computed_value::T as AlignSelf;
use style::properties::longhands::box_sizing::computed_value::T as BoxSizing;
use style::properties::longhands::flex_direction::computed_value::T as FlexDirection;
use style::properties::longhands::flex_wrap::computed_value::T as FlexWrap;
use style::properties::longhands::justify_content::computed_value::T as JustifyContent;
use style::properties::ComputedValues;
use style::values::computed::length::Size;
use style::values::computed::Length;
use style::values::generics::flex::GenericFlexBasis as FlexBasis;
use style::values::generics::length::{GenericLengthPercentageOrAuto, LengthPercentageOrNormal};
use style::values::specified::align::AlignFlags;
use style::Zero;

use super::geom::{
    FlexAxis, FlexRelativeRect, FlexRelativeSides, FlexRelativeVec2, MainStartCrossStart,
};
use super::{FlexContainer, FlexItemBox, FlexLevelBox};
use crate::cell::ArcRefCell;
use crate::context::LayoutContext;
use crate::formatting_contexts::{Baselines, IndependentFormattingContext, IndependentLayout};
use crate::fragment_tree::{BoxFragment, CollapsedBlockMargins, Fragment};
use crate::geom::{AuOrAuto, LogicalRect, LogicalSides, LogicalVec2};
use crate::positioned::{AbsolutelyPositionedBox, PositioningContext, PositioningContextLength};
use crate::sizing::ContentSizes;
use crate::style_ext::{Clamp, ComputedValuesExt};
use crate::ContainingBlock;

// FIMXE: “Flex items […] `z-index` values other than `auto` create a stacking context
// even if `position` is `static` (behaving exactly as if `position` were `relative`).”
// https://drafts.csswg.org/css-flexbox/#painting
// (likely in `display_list/stacking_context.rs`)

/// Layout parameters and intermediate results about a flex container,
/// grouped to avoid passing around many parameters
struct FlexContext<'a> {
    layout_context: &'a LayoutContext<'a>,
    positioning_context: &'a mut PositioningContext,
    containing_block: &'a ContainingBlock<'a>, // For items
    container_is_single_line: bool,
    container_min_cross_size: Au,
    container_max_cross_size: Option<Au>,
    flex_axis: FlexAxis,
    flex_direction_is_reversed: bool,
    flex_wrap_reverse: bool,
    main_start_cross_start_sides_are: MainStartCrossStart,
    container_definite_inner_size: FlexRelativeVec2<Option<Au>>,
    align_content: AlignContent,
    align_items: AlignItems,
    justify_content: JustifyContent,
}

/// A flex item with some intermediate results
struct FlexItem<'a> {
    box_: &'a mut IndependentFormattingContext,
    content_box_size: FlexRelativeVec2<AuOrAuto>,
    content_min_size: FlexRelativeVec2<Au>,
    content_max_size: FlexRelativeVec2<Option<Au>>,
    padding: FlexRelativeSides<Au>,
    border: FlexRelativeSides<Au>,
    margin: FlexRelativeSides<AuOrAuto>,

    /// Sum of padding, border, and margin (with `auto` assumed to be zero) in each axis.
    /// This is the difference between an outer and inner size.
    pbm_auto_is_zero: FlexRelativeVec2<Au>,

    /// <https://drafts.csswg.org/css-flexbox/#algo-main-item>
    flex_base_size: Au,

    /// <https://drafts.csswg.org/css-flexbox/#algo-main-item>
    hypothetical_main_size: Au,
    /// This is `align-self`, defaulting to `align-items` if `auto`
    align_self: AlignItems,
}

/// Child of a FlexContainer. Can either be absolutely positioned, or not. If not,
/// a placeholder is used and flex content is stored outside of this enum.
enum FlexContent {
    AbsolutelyPositionedBox(ArcRefCell<AbsolutelyPositionedBox>),
    FlexItemPlaceholder,
}

/// A flex line with some intermediate results
struct FlexLine<'a> {
    items: &'a mut [FlexItem<'a>],
    outer_hypothetical_main_sizes_sum: Au,
}

/// Return type of `FlexItem::layout`
struct FlexItemLayoutResult {
    hypothetical_cross_size: Au,
    fragments: Vec<Fragment>,
    positioning_context: PositioningContext,

    // Either the first or the last baseline, depending on ‘align-self’.
    baseline_relative_to_margin_box: Option<Au>,
}

impl FlexItemLayoutResult {
    fn get_or_synthesize_baseline_with_block_size(&self, block_size: Au, item: &FlexItem) -> Au {
        self.baseline_relative_to_margin_box
            .unwrap_or_else(|| item.synthesized_baseline_relative_to_margin_box(block_size))
    }
}

/// Return type of `FlexLine::layout`
struct FlexLineLayoutResult {
    cross_size: Au,
    /// The [`BoxFragment`]s and [`PositioningContext`]s of all flex items,
    /// one per flex item in "order-modified document order."
    item_fragments: Vec<(BoxFragment, PositioningContext)>,
    /// The 'shared alignment baseline' of this flex line. This is the baseline used for
    /// baseline-aligned items if there are any, otherwise `None`.
    shared_alignment_baseline: Option<Au>,
    /// This is the baseline of the first and last items with compatible writing mode, regardless of
    /// whether they particpate in baseline alignement. This is used as a fallback baseline for the
    /// container, if there are no items participating in baseline alignment in the first or last
    /// flex lines.
    all_baselines: Baselines,
}

impl FlexContext<'_> {
    fn vec2_to_flex_relative<T>(&self, x: LogicalVec2<T>) -> FlexRelativeVec2<T> {
        self.flex_axis.vec2_to_flex_relative(x)
    }

    fn sides_to_flex_relative<T>(&self, x: LogicalSides<T>) -> FlexRelativeSides<T> {
        self.main_start_cross_start_sides_are
            .sides_to_flex_relative(x)
    }

    fn sides_to_flow_relative<T>(&self, x: FlexRelativeSides<T>) -> LogicalSides<T> {
        self.main_start_cross_start_sides_are
            .sides_to_flow_relative(x)
    }

    fn rect_to_flow_relative(
        &self,
        base_rect_size: FlexRelativeVec2<Au>,
        rect: FlexRelativeRect<Au>,
    ) -> LogicalRect<Au> {
        super::geom::rect_to_flow_relative(
            self.flex_axis,
            self.main_start_cross_start_sides_are,
            base_rect_size,
            rect,
        )
    }

    fn align_for(&self, align_self: &AlignSelf) -> AlignItems {
        let value = align_self.0 .0.value();
        let mapped_value = match value {
            AlignFlags::AUTO | AlignFlags::NORMAL => self.align_items.0,
            _ => value,
        };
        AlignItems(mapped_value)
    }
}

#[derive(Default)]
struct DesiredFlexFractionAndGrowOrShrinkFactor {
    desired_flex_fraction: f32,
    flex_grow_or_shrink_factor: f32,
}

#[derive(Default)]
struct FlexItemBoxInlineContentSizesInfo {
    outer_flex_base_size: Au,
    content_min_size_no_auto: FlexRelativeVec2<Au>,
    content_max_size: FlexRelativeVec2<Option<Au>>,
    pbm_auto_is_zero: FlexRelativeVec2<Au>,
    min_flex_factors: DesiredFlexFractionAndGrowOrShrinkFactor,
    max_flex_factors: DesiredFlexFractionAndGrowOrShrinkFactor,
    min_content_main_size_for_multiline_container: Au,
}

impl FlexContainer {
    pub fn inline_content_sizes(
        &mut self,
        layout_context: &LayoutContext,
        writing_mode: WritingMode,
    ) -> ContentSizes {
        // - TODO: calculate intrinsic cross sizes when container is a column
        // (and check for ‘writing-mode’?)
        // - TODO: Collapsed flex items need to be skipped for intrinsic size calculation.

        // <https://drafts.csswg.org/css-flexbox/#intrinsic-cross-sizes>
        // > It is calculated, considering only non-collapsed flex items, by:
        // > 1. For each flex item, subtract its outer flex base size from its max-content
        // > contribution size.
        let mut chosen_max_flex_fraction = f32::NEG_INFINITY;
        let mut chosen_min_flex_fraction = f32::NEG_INFINITY;
        let mut sum_of_flex_grow_factors = 0.0;
        let mut sum_of_flex_shrink_factors = 0.0;
        let mut item_infos = vec![];

        let container_is_horizontal = self.style.writing_mode.is_horizontal();
        let flex_direction = used_flex_direction(&*self.style);
        let flex_axis = FlexAxis::from(flex_direction);
        let flex_wrap = self.style.get_position().flex_wrap;
        let flex_wrap_reverse = match flex_wrap {
            FlexWrap::Nowrap | FlexWrap::Wrap => false,
            FlexWrap::WrapReverse => true,
        };
        let main_start_cross_start = MainStartCrossStart::from(flex_direction, flex_wrap_reverse);

        for kid in self.children.iter() {
            let kid = &mut *kid.borrow_mut();
            match kid {
                FlexLevelBox::FlexItem(item) => {
                    sum_of_flex_grow_factors += item.style().get_position().flex_grow.0;
                    sum_of_flex_shrink_factors += item.style().get_position().flex_shrink.0;

                    let info = item.inline_content_size_info(
                        layout_context,
                        writing_mode,
                        container_is_horizontal,
                        flex_axis,
                        main_start_cross_start,
                    );

                    // > 2. Place all flex items into lines of infinite length. Within
                    // > each line, find the greatest (most > positive) desired flex
                    // > fraction among all the flex items. This is the line’s chosen flex
                    // > fraction.
                    chosen_max_flex_fraction =
                        chosen_max_flex_fraction.max(info.max_flex_factors.desired_flex_fraction);
                    chosen_min_flex_fraction =
                        chosen_min_flex_fraction.max(info.min_flex_factors.desired_flex_fraction);

                    item_infos.push(info)
                },
                FlexLevelBox::OutOfFlowAbsolutelyPositionedBox(_) => {},
            }
        }

        let normalize_flex_fraction = |chosen_flex_fraction| {
            if chosen_flex_fraction > 0.0 && sum_of_flex_grow_factors < 1.0 {
                // > 3. If the chosen flex fraction is positive, and the sum of the line’s
                // > flex grow factors is less than 1, > divide the chosen flex fraction by that
                // > sum.
                chosen_flex_fraction / sum_of_flex_grow_factors
            } else if chosen_flex_fraction < 0.0 && sum_of_flex_shrink_factors < 1.0 {
                // > If the chosen flex fraction is negative, and the sum of the line’s flex
                // > shrink factors is less than 1, > multiply the chosen flex fraction by that
                // > sum.
                chosen_flex_fraction * sum_of_flex_shrink_factors
            } else {
                chosen_flex_fraction
            }
        };

        let chosen_min_flex_fraction = normalize_flex_fraction(chosen_min_flex_fraction);
        let chosen_max_flex_fraction = normalize_flex_fraction(chosen_max_flex_fraction);

        let column_gap = match self.style.clone_column_gap() {
            LengthPercentageOrNormal::LengthPercentage(length_percentage) => {
                length_percentage.to_used_value(Au::zero())
            },
            LengthPercentageOrNormal::Normal => Au::zero(),
        };
        let extra_space_from_column_gap = column_gap * (item_infos.len() as i32 - 1);
        let mut container_max_content_size = extra_space_from_column_gap;
        let mut container_min_content_size = if flex_wrap == FlexWrap::Nowrap {
            extra_space_from_column_gap
        } else {
            Au::zero()
        };

        for FlexItemBoxInlineContentSizesInfo {
            outer_flex_base_size,
            content_min_size_no_auto,
            content_max_size,
            pbm_auto_is_zero,
            min_flex_factors,
            max_flex_factors,
            min_content_main_size_for_multiline_container,
        } in item_infos.iter()
        {
            // > 4. Add each item’s flex base size to the product of its flex grow factor (scaled flex shrink
            // > factor, if shrinking) and the chosen flex fraction, then clamp that result by the max main size
            // > floored by the min main size.
            let outer_min_main_size = content_min_size_no_auto.main + pbm_auto_is_zero.main;
            let outer_max_main_size = content_max_size.main.map(|v| v + pbm_auto_is_zero.main);

            // > 5. The flex container’s max-content size is the largest sum (among all the lines) of the
            // > afore-calculated sizes of all items within a single line.
            container_max_content_size += (*outer_flex_base_size +
                Au::from_f32_px(
                    max_flex_factors.flex_grow_or_shrink_factor * chosen_max_flex_fraction,
                ))
            .clamp_between_extremums(outer_min_main_size, outer_max_main_size);

            // > The min-content main size of a single-line flex container is calculated
            // > identically to the max-content main size, except that the flex items’
            // > min-content contributions are used instead of their max-content contributions.
            //
            // > However, for a multi-line container, the min-content main size is simply the
            // > largest min-content contribution of all the non-collapsed flex items in the
            // > flex container. For this purpose, each item’s contribution is capped by the
            // > item’s flex base size if the item is not growable, floored by the item’s flex
            // > base size if the item is not shrinkable, and then further clamped by the item’s
            // > min and max main sizes.
            if flex_wrap == FlexWrap::Nowrap {
                container_min_content_size += (*outer_flex_base_size +
                    Au::from_f32_px(
                        min_flex_factors.flex_grow_or_shrink_factor * chosen_min_flex_fraction,
                    ))
                .clamp_between_extremums(outer_min_main_size, outer_max_main_size);
            } else {
                container_min_content_size
                    .max_assign(*min_content_main_size_for_multiline_container);
            }
        }

        ContentSizes {
            min_content: container_min_content_size,
            max_content: container_max_content_size,
        }
    }

    /// <https://drafts.csswg.org/css-flexbox/#layout-algorithm>
    pub(crate) fn layout(
        &self,
        layout_context: &LayoutContext,
        positioning_context: &mut PositioningContext,
        containing_block: &ContainingBlock,
        containing_block_for_container: &ContainingBlock,
    ) -> IndependentLayout {
        // Actual length may be less, but we guess that usually not by a lot
        let mut flex_items = Vec::with_capacity(self.children.len());

        // Absolutely-positioned children of the flex container may be interleaved
        // with flex items. We need to preserve their relative order for correct painting order,
        // which is the order of `Fragment`s in this function’s return value.
        //
        // Example:
        // absolutely_positioned_items_with_original_order = [Some(item), Some(item), None, Some(item), None]
        // flex_items                                      =                         [item,             item]
        let absolutely_positioned_items_with_original_order = self
            .children
            .iter()
            .map(|arcrefcell| {
                let borrowed = arcrefcell.borrow_mut();
                match &*borrowed {
                    FlexLevelBox::OutOfFlowAbsolutelyPositionedBox(absolutely_positioned) => {
                        FlexContent::AbsolutelyPositionedBox(absolutely_positioned.clone())
                    },
                    FlexLevelBox::FlexItem(_) => {
                        let item = AtomicRefMut::map(borrowed, |child| match child {
                            FlexLevelBox::FlexItem(item) => item,
                            _ => unreachable!(),
                        });
                        flex_items.push(item);
                        FlexContent::FlexItemPlaceholder
                    },
                }
            })
            .collect::<Vec<_>>();

        let container_style = containing_block.style;
        let flex_direction = used_flex_direction(container_style);

        let flex_axis = FlexAxis::from(flex_direction);
        let (container_min_cross_size, container_max_cross_size) = self
            .available_cross_space_for_flex_items(
                container_style,
                flex_axis,
                containing_block_for_container,
            );

        let flex_wrap = container_style.get_position().flex_wrap;
        let container_is_single_line = match flex_wrap {
            FlexWrap::Nowrap => true,
            FlexWrap::Wrap | FlexWrap::WrapReverse => false,
        };
        let flex_direction_is_reversed = match flex_direction {
            FlexDirection::Row | FlexDirection::Column => false,
            FlexDirection::RowReverse | FlexDirection::ColumnReverse => true,
        };
        let flex_wrap_reverse = match flex_wrap {
            FlexWrap::Nowrap | FlexWrap::Wrap => false,
            FlexWrap::WrapReverse => true,
        };

        let align_content = container_style.clone_align_content();
        let align_items = AlignItems(match container_style.clone_align_items().0 {
            AlignFlags::AUTO | AlignFlags::NORMAL => AlignFlags::STRETCH,
            align => align,
        });
        let justify_content = container_style.clone_justify_content();

        let mut flex_context = FlexContext {
            layout_context,
            positioning_context,
            containing_block,
            container_min_cross_size,
            container_max_cross_size,
            container_is_single_line,
            flex_axis,
            flex_direction_is_reversed,
            flex_wrap_reverse,
            align_content,
            align_items,
            justify_content,
            main_start_cross_start_sides_are: MainStartCrossStart::from(
                flex_direction,
                flex_wrap_reverse,
            ),
            // https://drafts.csswg.org/css-flexbox/#definite-sizes
            container_definite_inner_size: flex_axis.vec2_to_flex_relative(LogicalVec2 {
                inline: Some(containing_block.inline_size),
                block: containing_block.block_size.non_auto(),
            }),
        };

        let flex_item_boxes = flex_items.iter_mut().map(|child| &mut **child);
        let mut flex_items = flex_item_boxes
            .map(|flex_item_box| FlexItem::new(&flex_context, flex_item_box))
            .collect::<Vec<_>>();

        // “Determine the main size of the flex container”
        // https://drafts.csswg.org/css-flexbox/#algo-main-container
        let container_main_size = match flex_axis {
            FlexAxis::Row => containing_block.inline_size,
            FlexAxis::Column => {
                // FIXME “using the rules of the formatting context in which it participates”
                // but if block-level with `block-size: max-auto` that requires
                // layout of the content to be fully done:
                // https://github.com/w3c/csswg-drafts/issues/4905
                // Gecko reportedly uses `block-size: fit-content` in this case
                // (which requires running another pass of the "full" layout algorithm)
                todo!()
                // Note: this panic shouldn’t happen since the start of `FlexContainer::layout`
                // forces `FlexAxis::Row`.
            },
        };

        let row_gap = container_style.clone_row_gap();
        let column_gap = container_style.clone_column_gap();
        let (cross_gap, main_gap) = match flex_context.flex_axis {
            FlexAxis::Row => (row_gap, column_gap),
            FlexAxis::Column => (column_gap, row_gap),
        };
        let cross_gap = match cross_gap {
            LengthPercentageOrNormal::LengthPercentage(length_percent) => length_percent
                .maybe_to_used_value(flex_context.container_definite_inner_size.cross)
                .unwrap_or_default(),
            LengthPercentageOrNormal::Normal => Au::zero(),
        };
        let main_gap = match main_gap {
            LengthPercentageOrNormal::LengthPercentage(length_percent) => length_percent
                .maybe_to_used_value(flex_context.container_definite_inner_size.main)
                .unwrap_or_default(),
            LengthPercentageOrNormal::Normal => Au::zero(),
        };

        // “Resolve the flexible lengths of all the flex items to find their *used main size*.”
        // https://drafts.csswg.org/css-flexbox/#algo-flex
        let flex_lines = collect_flex_lines(
            &mut flex_context,
            container_main_size,
            &mut flex_items,
            main_gap,
        );

        let line_count = flex_lines.len();
        let content_cross_size = flex_lines.iter().map(|line| line.cross_size).sum::<Au>() +
            cross_gap * (line_count as i32 - 1);

        // https://drafts.csswg.org/css-flexbox/#algo-cross-container
        let container_cross_size = flex_context
            .container_definite_inner_size
            .cross
            .unwrap_or(content_cross_size)
            .clamp_between_extremums(
                flex_context.container_min_cross_size,
                flex_context.container_max_cross_size,
            );

        // https://drafts.csswg.org/css-flexbox/#algo-line-align
        // Align all flex lines per `align-content`.
        let mut cross_start_position_cursor = Au::zero();
        let mut line_interval = cross_gap;

        if let Some(cross_size) = flex_context.container_definite_inner_size.cross {
            let free_space = cross_size - content_cross_size;
            let layout_is_flex_reversed = flex_context.flex_wrap_reverse;

            // Implement fallback alignment.
            //
            // In addition to the spec at https://www.w3.org/TR/css-align-3/ this implementation follows
            // the resolution of https://github.com/w3c/csswg-drafts/issues/10154
            let resolved_align_content: AlignFlags = {
                let align_content_style = flex_context.align_content.0.primary();

                // Inital values from the style system
                let mut resolved_align_content = align_content_style.value();
                let mut is_safe = align_content_style.flags() == AlignFlags::SAFE;

                // Fallback occurs in two cases:

                // 1. If there is only a single item being aligned and alignment is a distributed alignment keyword
                //    https://www.w3.org/TR/css-align-3/#distribution-values
                if line_count <= 1 || free_space <= Au::zero() {
                    (resolved_align_content, is_safe) = match resolved_align_content {
                        AlignFlags::STRETCH => (AlignFlags::FLEX_START, true),
                        AlignFlags::SPACE_BETWEEN => (AlignFlags::FLEX_START, true),
                        AlignFlags::SPACE_AROUND => (AlignFlags::CENTER, true),
                        AlignFlags::SPACE_EVENLY => (AlignFlags::CENTER, true),
                        _ => (resolved_align_content, is_safe),
                    }
                };

                // 2. If free space is negative the "safe" alignment variants all fallback to Start alignment
                if free_space <= Au::zero() && is_safe {
                    resolved_align_content = AlignFlags::START;
                }

                resolved_align_content
            };

            // Implement "unsafe" alignment. "safe" alignment is handled by the fallback process above.
            cross_start_position_cursor = match resolved_align_content {
                AlignFlags::START => Au::zero(),
                AlignFlags::FLEX_START => {
                    if layout_is_flex_reversed {
                        free_space
                    } else {
                        Au::zero()
                    }
                },
                AlignFlags::END => free_space,
                AlignFlags::FLEX_END => {
                    if layout_is_flex_reversed {
                        Au::zero()
                    } else {
                        free_space
                    }
                },
                AlignFlags::CENTER => free_space / 2,
                AlignFlags::STRETCH => Au::zero(),
                AlignFlags::SPACE_BETWEEN => Au::zero(),
                AlignFlags::SPACE_AROUND => free_space / line_count as i32 / 2,
                AlignFlags::SPACE_EVENLY => free_space / (line_count + 1) as i32,

                // TODO: Implement all alignments. Note: not all alignment values are valid for content distribution
                _ => Au::zero(),
            };

            line_interval += match resolved_align_content {
                AlignFlags::START => Au::zero(),
                AlignFlags::FLEX_START => Au::zero(),
                AlignFlags::END => Au::zero(),
                AlignFlags::FLEX_END => Au::zero(),
                AlignFlags::CENTER => Au::zero(),
                AlignFlags::STRETCH => Au::zero(),
                AlignFlags::SPACE_BETWEEN => free_space / (line_count - 1) as i32,
                AlignFlags::SPACE_AROUND => free_space / line_count as i32,
                AlignFlags::SPACE_EVENLY => free_space / (line_count + 1) as i32,

                // TODO: Implement all alignments. Note: not all alignment values are valid for content distribution
                _ => Au::zero(),
            };
        };

        let line_cross_start_positions = flex_lines
            .iter()
            .map(|line| {
                let cross_start = cross_start_position_cursor;
                let cross_end = cross_start + line.cross_size + line_interval;
                cross_start_position_cursor = cross_end;
                cross_start
            })
            .collect::<Vec<_>>();

        let content_block_size = match flex_context.flex_axis {
            FlexAxis::Row => {
                // `container_main_size` ends up unused here but in this case that’s fine
                // since it was already exactly the one decided by the outer formatting context.
                container_cross_size
            },
            FlexAxis::Column => {
                // FIXME: `container_cross_size` ends up unused here, which is a bug.
                // It is meant to be the used inline-size, but the parent formatting context
                // has already decided a possibly-different used inline-size.
                // The spec is missing something to resolve this conflict:
                // https://github.com/w3c/csswg-drafts/issues/5190
                // And we’ll need to change the signature of `IndependentFormattingContext::layout`
                // to allow the inner formatting context to “negotiate” a used inline-size
                // with the outer one somehow.
                container_main_size
            },
        };

        let mut baseline_alignment_participating_baselines = Baselines::default();
        let mut all_baselines = Baselines::default();
        let num_lines = flex_lines.len();
        let mut flex_item_fragments = izip!(flex_lines.into_iter(), line_cross_start_positions)
            .enumerate()
            .flat_map(|(index, (mut line, line_cross_start_position))| {
                let flow_relative_line_position = match (flex_axis, flex_wrap_reverse) {
                    (FlexAxis::Row, false) => LogicalVec2 {
                        block: line_cross_start_position,
                        inline: Au::zero(),
                    },
                    (FlexAxis::Row, true) => LogicalVec2 {
                        block: container_cross_size - line_cross_start_position - line.cross_size,
                        inline: Au::zero(),
                    },
                    (FlexAxis::Column, false) => LogicalVec2 {
                        block: Au::zero(),
                        inline: line_cross_start_position,
                    },
                    (FlexAxis::Column, true) => LogicalVec2 {
                        block: Au::zero(),
                        inline: container_cross_size - line_cross_start_position - line.cross_size,
                    },
                };

                let line_shared_alignment_baseline = line
                    .shared_alignment_baseline
                    .map(|baseline| baseline + flow_relative_line_position.block);
                let line_all_baselines =
                    line.all_baselines.offset(flow_relative_line_position.block);
                if index == 0 {
                    baseline_alignment_participating_baselines.first =
                        line_shared_alignment_baseline;
                    all_baselines.first = line_all_baselines.first;
                }
                if index == num_lines - 1 {
                    baseline_alignment_participating_baselines.last =
                        line_shared_alignment_baseline;
                    all_baselines.last = line_all_baselines.last;
                }

                for (fragment, _) in &mut line.item_fragments {
                    fragment.content_rect.start_corner += flow_relative_line_position
                }
                line.item_fragments
            });

        let fragments = absolutely_positioned_items_with_original_order
            .into_iter()
            .map(|child_as_abspos| match child_as_abspos {
                FlexContent::AbsolutelyPositionedBox(absolutely_positioned) => {
                    let hoisted_box = AbsolutelyPositionedBox::to_hoisted(
                        absolutely_positioned,
                        LogicalVec2::zero(),
                        containing_block,
                    );
                    let hoisted_fragment = hoisted_box.fragment.clone();
                    positioning_context.push(hoisted_box);
                    Fragment::AbsoluteOrFixedPositioned(hoisted_fragment)
                },
                FlexContent::FlexItemPlaceholder => {
                    // The `flex_item_fragments` iterator yields one fragment
                    // per flex item, in the original order.
                    let (fragment, mut child_positioning_context) =
                        flex_item_fragments.next().unwrap();
                    let fragment = Fragment::Box(fragment);
                    child_positioning_context.adjust_static_position_of_hoisted_fragments(
                        &fragment,
                        PositioningContextLength::zero(),
                    );
                    positioning_context.append(child_positioning_context);
                    fragment
                },
            })
            .collect::<Vec<_>>();

        // There should be no more flex items
        assert!(flex_item_fragments.next().is_none());

        let baselines = Baselines {
            first: baseline_alignment_participating_baselines
                .first
                .or(all_baselines.first),
            last: baseline_alignment_participating_baselines
                .last
                .or(all_baselines.last),
        };

        IndependentLayout {
            fragments,
            content_block_size,
            content_inline_size_for_table: None,
            baselines,
        }
    }

    fn available_cross_space_for_flex_items(
        &self,
        style: &ComputedValues,
        flex_axis: FlexAxis,
        containing_block_for_container: &ContainingBlock,
    ) -> (Au, Option<Au>) {
        let pbm = style.padding_border_margin(containing_block_for_container);
        let max_box_size = style.content_max_box_size(containing_block_for_container, &pbm);
        let min_box_size = style
            .content_min_box_size(containing_block_for_container, &pbm)
            .auto_is(Length::zero);

        let max_box_size = flex_axis.vec2_to_flex_relative(max_box_size);
        let min_box_size = flex_axis.vec2_to_flex_relative(min_box_size);

        (
            min_box_size.cross.into(),
            max_box_size.cross.map(Into::into),
        )
    }
}

impl<'a> FlexItem<'a> {
    fn new(flex_context: &FlexContext, box_: &'a mut FlexItemBox) -> Self {
        let containing_block = flex_context.containing_block;

        // https://drafts.csswg.org/css-writing-modes/#orthogonal-flows
        assert_eq!(
            containing_block.style.writing_mode,
            box_.style().writing_mode,
            "Mixed writing modes are not supported yet"
        );

        let container_is_horizontal = containing_block.style.writing_mode.is_horizontal();
        let item_is_horizontal = box_.style().writing_mode.is_horizontal();
        let cross_axis_is_item_block_axis = cross_axis_is_item_block_axis(
            container_is_horizontal,
            item_is_horizontal,
            flex_context.flex_axis,
        );

        let pbm = box_.style().padding_border_margin(containing_block);
        let content_box_size = box_
            .style()
            .content_box_size(containing_block, &pbm)
            .map(|v| v.map(Au::from));
        let max_size = box_
            .style()
            .content_max_box_size(containing_block, &pbm)
            .map(|v| v.map(Au::from));
        let min_size = box_
            .style()
            .content_min_box_size(containing_block, &pbm)
            .map(|v| v.map(Au::from));

        let min_size = LogicalVec2 {
            inline: min_size.inline.auto_is(|| {
                box_.automatic_min_size(
                    flex_context.layout_context,
                    cross_axis_is_item_block_axis,
                    content_box_size,
                    min_size,
                    max_size,
                )
            }),
            block: min_size.block.auto_is(Au::zero),
        };
        let margin_auto_is_zero = pbm.margin.auto_is(Au::zero);

        let content_box_size = flex_context.vec2_to_flex_relative(content_box_size);
        let content_max_size = flex_context.vec2_to_flex_relative(max_size);
        let content_min_size = flex_context.vec2_to_flex_relative(min_size);
        let margin_auto_is_zero = flex_context.sides_to_flex_relative(margin_auto_is_zero);
        let padding = flex_context.sides_to_flex_relative(pbm.padding);
        let border = flex_context.sides_to_flex_relative(pbm.border);
        let padding_border = padding.sum_by_axis() + border.sum_by_axis();
        let pbm_auto_is_zero = FlexRelativeVec2 {
            main: padding_border.main,
            cross: padding_border.cross,
        } + margin_auto_is_zero.sum_by_axis();

        let align_self = flex_context.align_for(&box_.style().clone_align_self());

        let flex_base_size = flex_base_size(
            flex_context.layout_context,
            flex_context.container_definite_inner_size,
            &mut box_.independent_formatting_context,
            cross_axis_is_item_block_axis,
            content_box_size,
            padding_border,
        );

        let hypothetical_main_size =
            flex_base_size.clamp_between_extremums(content_min_size.main, content_max_size.main);
        let margin: FlexRelativeSides<AuOrAuto> = flex_context.sides_to_flex_relative(pbm.margin);

        Self {
            box_: &mut box_.independent_formatting_context,
            content_box_size,
            content_min_size,
            content_max_size,
            padding,
            border,
            margin,
            pbm_auto_is_zero,
            flex_base_size,
            hypothetical_main_size,
            align_self,
        }
    }
}

/// <https://drafts.csswg.org/css-flexbox/#algo-main-item>
fn flex_base_size(
    layout_context: &LayoutContext,
    container_definite_inner_size: FlexRelativeVec2<Option<Au>>,
    flex_item: &mut IndependentFormattingContext,
    cross_axis_is_item_block_axis: bool,
    content_box_size: FlexRelativeVec2<AuOrAuto>,
    padding_border_sums: FlexRelativeVec2<Au>,
) -> Au {
    let used_flex_basis = match &flex_item.style().get_position().flex_basis {
        FlexBasis::Content => FlexBasis::Content,
        FlexBasis::Size(Size::LengthPercentage(length_percentage)) => {
            let apply_box_sizing = |length: Au| {
                match flex_item.style().get_position().box_sizing {
                    BoxSizing::ContentBox => length,
                    BoxSizing::BorderBox => {
                        // This may make `length` negative,
                        // but it will be clamped in the hypothetical main size
                        length - padding_border_sums.main
                    },
                }
            };
            // “For example, percentage values of flex-basis are resolved
            //  against the flex item’s containing block (i.e. its flex container);”
            match container_definite_inner_size.main {
                Some(container_definite_main_size) => {
                    let length = length_percentage
                        .0
                        .percentage_relative_to(container_definite_main_size.into());
                    FlexBasis::Size(apply_box_sizing(length.into()))
                },
                None => {
                    if let Some(length) = length_percentage.0.to_length() {
                        FlexBasis::Size(apply_box_sizing(length.into()))
                    } else {
                        // “and if that containing block’s size is indefinite,
                        //  the used value for `flex-basis` is `content`.”
                        // https://drafts.csswg.org/css-flexbox/#flex-basis-property
                        FlexBasis::Content
                    }
                },
            }
        },
        FlexBasis::Size(Size::Auto) => {
            // “When specified on a flex item, the `auto` keyword retrieves
            //  the value of the main size property as the used `flex-basis`.”
            match content_box_size.main {
                AuOrAuto::LengthPercentage(length) => FlexBasis::Size(length),
                // “If that value is itself `auto`, then the used value is `content`.”
                AuOrAuto::Auto => FlexBasis::Content,
            }
        },
    };

    // NOTE: at this point the flex basis is either `content` or a definite length.
    // However when we add support for additional values for `width` and `height`
    // from https://drafts.csswg.org/css-sizing/#preferred-size-properties,
    // it could have those values too.

    match used_flex_basis {
        FlexBasis::Size(length) => {
            // Case A: definite flex basis
            length
        },
        FlexBasis::Content => {
            // FIXME: implement cases B, C, D.

            // Case E: everything else
            // “treating a value of content as max-content.”
            if cross_axis_is_item_block_axis {
                // The main axis is the inline axis
                flex_item.inline_content_sizes(layout_context).max_content
            } else {
                // FIXME: block-axis content sizing requires another pass
                // of "full" layout
                todo!()
                // Note: this panic shouldn’t happen since the start of `FlexContainer::layout`
                // forces `FlexAxis::Row` and the `writing-mode` property is disabled.
            }
        },
    }
}

fn cross_axis_is_item_block_axis(
    container_is_horizontal: bool,
    item_is_horizontal: bool,
    flex_axis: FlexAxis,
) -> bool {
    let item_is_orthogonal = item_is_horizontal != container_is_horizontal;
    let container_is_row = flex_axis == FlexAxis::Row;

    container_is_row ^ item_is_orthogonal
}

fn used_flex_direction(container_style: &ComputedValues) -> FlexDirection {
    // Column flex containers are not fully implemented yet,
    // so give a different layout instead of panicking.
    // FIXME: implement `todo!`s for FlexAxis::Column below, and remove this
    match container_style.clone_flex_direction() {
        FlexDirection::Row | FlexDirection::Column => FlexDirection::Row,
        FlexDirection::RowReverse | FlexDirection::ColumnReverse => FlexDirection::RowReverse,
    }
}

// “Collect flex items into flex lines”
// https://drafts.csswg.org/css-flexbox/#algo-line-break
fn collect_flex_lines<'items>(
    flex_context: &mut FlexContext,
    container_main_size: Au,
    mut items: &'items mut [FlexItem<'items>],
    main_gap: Au,
) -> Vec<FlexLineLayoutResult> {
    if flex_context.container_is_single_line {
        let mut line = FlexLine {
            outer_hypothetical_main_sizes_sum: items
                .iter()
                .map(|item| item.hypothetical_main_size + item.pbm_auto_is_zero.main)
                .sum(),
            items,
        };
        vec![line.layout(flex_context, container_main_size, main_gap)]
    } else {
        let mut lines = Vec::new();
        let mut line_size_so_far = Au::zero();
        let mut line_so_far_is_empty = true;
        let mut index = 0;
        while let Some(item) = items.get(index) {
            let item_size = item.hypothetical_main_size + item.pbm_auto_is_zero.main;
            let mut line_size_would_be = line_size_so_far + item_size;
            if !line_so_far_is_empty {
                line_size_would_be += main_gap;
            }
            let item_fits = line_size_would_be <= container_main_size;
            if item_fits || line_so_far_is_empty {
                line_size_so_far = line_size_would_be;
                line_so_far_is_empty = false;
                index += 1;
            } else {
                // We found something that doesn’t fit. This line ends *before* this item.
                let (line_items, rest) = items.split_at_mut(index);
                let mut line = FlexLine {
                    items: line_items,
                    outer_hypothetical_main_sizes_sum: line_size_so_far,
                };
                items = rest;
                lines.push(line.layout(flex_context, container_main_size, main_gap));
                // The next line has this item.
                line_size_so_far = item_size;
                index = 1;
            }
        }
        // The last line is added even without finding an item that doesn’t fit
        let mut line = FlexLine {
            items,
            outer_hypothetical_main_sizes_sum: line_size_so_far,
        };
        lines.push(line.layout(flex_context, container_main_size, main_gap));
        lines
    }
}

impl FlexLine<'_> {
    fn layout(
        &mut self,
        flex_context: &mut FlexContext,
        container_main_size: Au,
        main_gap: Au,
    ) -> FlexLineLayoutResult {
        let item_count = self.items.len();
        let (item_used_main_sizes, mut free_space) =
            self.resolve_flexible_lengths(container_main_size - main_gap * (item_count as i32 - 1));

        // https://drafts.csswg.org/css-flexbox/#algo-cross-item
        let mut item_layout_results = self
            .items
            .iter_mut()
            .zip(&item_used_main_sizes)
            .map(|(item, &used_main_size)| item.layout(used_main_size, flex_context, None))
            .collect::<Vec<_>>();

        // https://drafts.csswg.org/css-flexbox/#algo-cross-line
        let line_cross_size = self.cross_size(&item_layout_results, flex_context);
        let line_size = FlexRelativeVec2 {
            main: container_main_size,
            cross: line_cross_size,
        };

        // FIXME: Handle `align-content: stretch`
        // https://drafts.csswg.org/css-flexbox/#algo-line-stretch

        // FIXME: Collapse `visibility: collapse` items
        // This involves “restart layout from the beginning” with a modified second round,
        // which will make structuring the code… interesting.
        // https://drafts.csswg.org/css-flexbox/#algo-visibility

        // Determine the used cross size of each flex item
        // https://drafts.csswg.org/css-flexbox/#algo-stretch
        let mut shared_alignment_baseline = None;
        let mut item_used_cross_sizes = Vec::with_capacity(item_count);
        let mut item_cross_margins = Vec::with_capacity(item_count);
        for (item, item_layout_result, used_main_size) in izip!(
            self.items.iter_mut(),
            item_layout_results.iter_mut(),
            &item_used_main_sizes
        ) {
            let has_stretch = item.align_self.0.value() == AlignFlags::STRETCH;
            let used_cross_size = if has_stretch &&
                item.content_box_size.cross.is_auto() &&
                !(item.margin.cross_start.is_auto() || item.margin.cross_end.is_auto())
            {
                (line_cross_size - item.pbm_auto_is_zero.cross).clamp_between_extremums(
                    item.content_min_size.cross,
                    item.content_max_size.cross,
                )
            } else {
                item_layout_result.hypothetical_cross_size
            };
            item_used_cross_sizes.push(used_cross_size);

            if has_stretch {
                // “If the flex item has `align-self: stretch`, redo layout for its contents,
                //  treating this used size as its definite cross size
                //  so that percentage-sized children can be resolved.”
                *item_layout_result =
                    item.layout(*used_main_size, flex_context, Some(used_cross_size));
            }

            // TODO: This also needs to check whether we have a compatible writing mode.
            let baseline = item_layout_result
                .get_or_synthesize_baseline_with_block_size(used_cross_size, item);
            if matches!(
                item.align_self.0.value(),
                AlignFlags::BASELINE | AlignFlags::LAST_BASELINE
            ) {
                shared_alignment_baseline =
                    Some(shared_alignment_baseline.unwrap_or(baseline).max(baseline));
            }
            item_layout_result.baseline_relative_to_margin_box = Some(baseline);

            // https://drafts.csswg.org/css-flexbox/#algo-cross-margins
            item_cross_margins.push(item.resolve_auto_cross_margins(
                flex_context,
                line_cross_size,
                used_cross_size,
            ));
        }

        // Layout of items is over. These should no longer be mutable.
        let item_layout_results = item_layout_results;

        // Distribute any remaining free space
        // https://drafts.csswg.org/css-flexbox/#algo-main-align
        let (item_main_margins, free_space_distributed) =
            self.resolve_auto_main_margins(free_space);
        if free_space_distributed {
            free_space = Au::zero();
        }

        // Align the items along the main-axis per justify-content.
        let layout_is_flex_reversed = flex_context.flex_direction_is_reversed;

        // Implement fallback alignment.
        //
        // In addition to the spec at https://www.w3.org/TR/css-align-3/ this implementation follows
        // the resolution of https://github.com/w3c/csswg-drafts/issues/10154
        let resolved_justify_content: AlignFlags = {
            let justify_content_style = flex_context.justify_content.0.primary();

            // Inital values from the style system
            let mut resolved_justify_content = justify_content_style.value();
            let mut is_safe = justify_content_style.flags() == AlignFlags::SAFE;

            // Fallback occurs in two cases:

            // 1. If there is only a single item being aligned and alignment is a distributed alignment keyword
            //    https://www.w3.org/TR/css-align-3/#distribution-values
            if item_count <= 1 || free_space <= Au::zero() {
                (resolved_justify_content, is_safe) = match resolved_justify_content {
                    AlignFlags::STRETCH => (AlignFlags::FLEX_START, true),
                    AlignFlags::SPACE_BETWEEN => (AlignFlags::FLEX_START, true),
                    AlignFlags::SPACE_AROUND => (AlignFlags::CENTER, true),
                    AlignFlags::SPACE_EVENLY => (AlignFlags::CENTER, true),
                    _ => (resolved_justify_content, is_safe),
                }
            };

            // 2. If free space is negative the "safe" alignment variants all fallback to Start alignment
            if free_space <= Au::zero() && is_safe {
                resolved_justify_content = AlignFlags::START;
            }

            resolved_justify_content
        };

        // Implement "unsafe" alignment. "safe" alignment is handled by the fallback process above.
        let main_start_position = match resolved_justify_content {
            AlignFlags::START => Au::zero(),
            AlignFlags::FLEX_START => {
                if layout_is_flex_reversed {
                    free_space
                } else {
                    Au::zero()
                }
            },
            AlignFlags::END => free_space,
            AlignFlags::FLEX_END => {
                if layout_is_flex_reversed {
                    Au::zero()
                } else {
                    free_space
                }
            },
            AlignFlags::CENTER => free_space / 2,
            AlignFlags::STRETCH => Au::zero(),
            AlignFlags::SPACE_BETWEEN => Au::zero(),
            AlignFlags::SPACE_AROUND => (free_space / item_count as i32) / 2,
            AlignFlags::SPACE_EVENLY => free_space / (item_count + 1) as i32,

            // TODO: Implement all alignments. Note: not all alignment values are valid for content distribution
            _ => Au::zero(),
        };

        let item_main_interval = match resolved_justify_content {
            AlignFlags::START => Au::zero(),
            AlignFlags::FLEX_START => Au::zero(),
            AlignFlags::END => Au::zero(),
            AlignFlags::FLEX_END => Au::zero(),
            AlignFlags::CENTER => Au::zero(),
            AlignFlags::STRETCH => Au::zero(),
            AlignFlags::SPACE_BETWEEN => free_space / (item_count - 1) as i32,
            AlignFlags::SPACE_AROUND => free_space / item_count as i32,
            AlignFlags::SPACE_EVENLY => free_space / (item_count + 1) as i32,

            // TODO: Implement all alignments. Note: not all alignment values are valid for content distribution
            _ => Au::zero(),
        };
        let item_main_interval = item_main_interval + main_gap;

        let mut all_baselines = Baselines::default();
        let mut main_position_cursor = main_start_position;
        let item_fragments = izip!(
            self.items.iter(),
            item_main_margins,
            item_cross_margins,
            &item_used_main_sizes,
            &item_used_cross_sizes,
            item_layout_results.into_iter()
        )
        .map(
            |(
                item,
                item_main_margins,
                item_cross_margins,
                item_used_main_size,
                item_used_cross_size,
                item_layout_result,
            )| {
                let item_margin = FlexRelativeSides {
                    main_start: item_main_margins.0,
                    main_end: item_main_margins.1,
                    cross_start: item_cross_margins.0,
                    cross_end: item_cross_margins.1,
                };

                // https://drafts.csswg.org/css-flexbox/#algo-main-align
                // “Align the items along the main-axis”
                main_position_cursor +=
                    item_margin.main_start + item.border.main_start + item.padding.main_start;
                let item_content_main_start_position = main_position_cursor;
                main_position_cursor += *item_used_main_size +
                    item.padding.main_end +
                    item.border.main_end +
                    item_margin.main_end +
                    item_main_interval;

                // https://drafts.csswg.org/css-flexbox/#algo-cross-align
                let item_content_cross_start_position = item.align_along_cross_axis(
                    &item_margin,
                    item_used_cross_size,
                    line_cross_size,
                    item_layout_result
                        .baseline_relative_to_margin_box
                        .unwrap_or_default(),
                    shared_alignment_baseline.unwrap_or_default(),
                );

                let start_corner = FlexRelativeVec2 {
                    main: item_content_main_start_position,
                    cross: item_content_cross_start_position,
                };
                let size = FlexRelativeVec2 {
                    main: *item_used_main_size,
                    cross: *item_used_cross_size,
                };

                // Need to collect both baselines from baseline participation and other baselines.
                let content_rect = flex_context
                    .rect_to_flow_relative(line_size, FlexRelativeRect { start_corner, size });
                let margin = flex_context.sides_to_flow_relative(item_margin);
                let collapsed_margin = CollapsedBlockMargins::from_margin(&margin);

                if let Some(item_baseline) =
                    item_layout_result.baseline_relative_to_margin_box.as_ref()
                {
                    let item_baseline = *item_baseline + item_content_cross_start_position -
                        item.border.cross_start -
                        item.padding.cross_start -
                        item_margin.cross_start;
                    all_baselines.first.get_or_insert(item_baseline);
                    all_baselines.last = Some(item_baseline);
                }

                (
                    BoxFragment::new(
                        item.box_.base_fragment_info(),
                        item.box_.style().clone(),
                        item_layout_result.fragments,
                        content_rect,
                        flex_context.sides_to_flow_relative(item.padding),
                        flex_context.sides_to_flow_relative(item.border),
                        margin,
                        None, /* clearance */
                        collapsed_margin,
                    ),
                    item_layout_result.positioning_context,
                )
            },
        )
        .collect();

        FlexLineLayoutResult {
            cross_size: line_cross_size,
            item_fragments,
            all_baselines,
            shared_alignment_baseline,
        }
    }

    /// Return the *main size* of each item, and the line’s remainaing free space
    /// <https://drafts.csswg.org/css-flexbox/#resolve-flexible-lengths>
    fn resolve_flexible_lengths(&self, container_main_size: Au) -> (Vec<Au>, Au) {
        let mut frozen = vec![false; self.items.len()];
        let mut target_main_sizes_vec = self
            .items
            .iter()
            .map(|item| item.flex_base_size)
            .collect::<Vec<_>>();

        // Using `Cell`s reconciles mutability with multiple borrows in closures
        let target_main_sizes = Cell::from_mut(&mut *target_main_sizes_vec).as_slice_of_cells();
        let frozen = Cell::from_mut(&mut *frozen).as_slice_of_cells();
        let frozen_count = Cell::new(0);

        let grow = self.outer_hypothetical_main_sizes_sum < container_main_size;
        let flex_factor = |item: &FlexItem| {
            let position_style = item.box_.style().get_position();
            if grow {
                position_style.flex_grow.0
            } else {
                position_style.flex_shrink.0
            }
        };
        let items = || self.items.iter().zip(target_main_sizes).zip(frozen);

        // “Size inflexible items”
        for ((item, target_main_size), frozen) in items() {
            let is_inflexible = flex_factor(item) == 0. ||
                if grow {
                    item.flex_base_size > item.hypothetical_main_size
                } else {
                    item.flex_base_size < item.hypothetical_main_size
                };
            if is_inflexible {
                frozen_count.set(frozen_count.get() + 1);
                frozen.set(true);
                target_main_size.set(item.hypothetical_main_size);
            }
        }

        let check_for_flexible_items = || frozen_count.get() < self.items.len();
        let free_space = || {
            container_main_size -
                items()
                    .map(|((item, target_main_size), frozen)| {
                        item.pbm_auto_is_zero.main +
                            if frozen.get() {
                                target_main_size.get()
                            } else {
                                item.flex_base_size
                            }
                    })
                    .sum()
        };
        // https://drafts.csswg.org/css-flexbox/#initial-free-space
        let initial_free_space = free_space();
        let unfrozen_items = || {
            items().filter_map(|(item_and_target_main_size, frozen)| {
                if !frozen.get() {
                    Some(item_and_target_main_size)
                } else {
                    None
                }
            })
        };
        loop {
            // https://drafts.csswg.org/css-flexbox/#remaining-free-space
            let mut remaining_free_space = free_space();
            if !check_for_flexible_items() {
                return (target_main_sizes_vec, remaining_free_space);
            }
            let unfrozen_items_flex_factor_sum: f32 =
                unfrozen_items().map(|(item, _)| flex_factor(item)).sum();
            // FIXME: I (Simon) transcribed the spec but I don’t yet understand why this algorithm
            if unfrozen_items_flex_factor_sum < 1. {
                let multiplied = initial_free_space.scale_by(unfrozen_items_flex_factor_sum);
                if multiplied.abs() < remaining_free_space.abs() {
                    remaining_free_space = multiplied
                }
            }

            // “Distribute free space proportional to the flex factors.”
            // FIXME: is it a problem if floating point precision errors accumulate
            // and we get not-quite-zero remaining free space when we should get zero here?
            if remaining_free_space != Au::zero() {
                if grow {
                    for (item, target_main_size) in unfrozen_items() {
                        let grow_factor = item.box_.style().get_position().flex_grow.0;
                        let ratio = grow_factor / unfrozen_items_flex_factor_sum;
                        target_main_size
                            .set(item.flex_base_size + remaining_free_space.scale_by(ratio));
                    }
                } else {
                    // https://drafts.csswg.org/css-flexbox/#scaled-flex-shrink-factor
                    let scaled_shrink_factor = |item: &FlexItem| {
                        let shrink_factor = item.box_.style().get_position().flex_shrink.0;
                        item.flex_base_size.scale_by(shrink_factor)
                    };
                    let scaled_shrink_factors_sum: Au = unfrozen_items()
                        .map(|(item, _)| scaled_shrink_factor(item))
                        .sum();
                    if scaled_shrink_factors_sum > Au::zero() {
                        for (item, target_main_size) in unfrozen_items() {
                            let ratio = scaled_shrink_factor(item).0 as f32 /
                                scaled_shrink_factors_sum.0 as f32;
                            target_main_size.set(
                                item.flex_base_size - remaining_free_space.abs().scale_by(ratio),
                            );
                        }
                    }
                }
            }

            // “Fix min/max violations.”
            let violation = |(item, target_main_size): (&FlexItem, &Cell<Au>)| {
                let size = target_main_size.get();
                let clamped = size.clamp_between_extremums(
                    item.content_min_size.main,
                    item.content_max_size.main,
                );
                clamped - size
            };

            // “Freeze over-flexed items.”
            let total_violation: Au = unfrozen_items().map(violation).sum();
            match total_violation.cmp(&Au::zero()) {
                Ordering::Equal => {
                    // “Freeze all items.”
                    // Return instead, as that’s what the next loop iteration would do.
                    let remaining_free_space =
                        container_main_size - target_main_sizes_vec.iter().cloned().sum();
                    return (target_main_sizes_vec, remaining_free_space);
                },
                Ordering::Greater => {
                    // “Freeze all the items with min violations.”
                    // “If the item’s target main size was made larger by [clamping],
                    //  it’s a min violation.”
                    for (item_and_target_main_size, frozen) in items() {
                        if violation(item_and_target_main_size) > Au::zero() {
                            let (item, target_main_size) = item_and_target_main_size;
                            target_main_size.set(item.content_min_size.main);
                            frozen_count.set(frozen_count.get() + 1);
                            frozen.set(true);
                        }
                    }
                },
                Ordering::Less => {
                    // Negative total violation
                    // “Freeze all the items with max violations.”
                    // “If the item’s target main size was made smaller by [clamping],
                    //  it’s a max violation.”
                    for (item_and_target_main_size, frozen) in items() {
                        if violation(item_and_target_main_size) < Au::zero() {
                            let (item, target_main_size) = item_and_target_main_size;
                            let Some(max_size) = item.content_max_size.main else {
                                unreachable!()
                            };
                            target_main_size.set(max_size);
                            frozen_count.set(frozen_count.get() + 1);
                            frozen.set(true);
                        }
                    }
                },
            }
        }
    }
}

impl<'a> FlexItem<'a> {
    // Return the hypothetical cross size together with laid out contents of the fragment.
    // https://drafts.csswg.org/css-flexbox/#algo-cross-item
    // “performing layout as if it were an in-flow block-level box
    //  with the used main size and the given available space, treating `auto` as `fit-content`.”
    fn layout(
        &mut self,
        used_main_size: Au,
        flex_context: &mut FlexContext,
        used_cross_size_override: Option<Au>,
    ) -> FlexItemLayoutResult {
        let mut positioning_context = PositioningContext::new_for_subtree(
            flex_context
                .positioning_context
                .collects_for_nearest_positioned_ancestor(),
        );
        match flex_context.flex_axis {
            FlexAxis::Row => {
                // The main axis is the container’s inline axis

                // https://drafts.csswg.org/css-writing-modes/#orthogonal-flows
                assert_eq!(
                    flex_context.containing_block.style.writing_mode,
                    self.box_.style().writing_mode,
                    "Mixed writing modes are not supported yet"
                );
                // … and also the item’s inline axis.

                match self.box_ {
                    IndependentFormattingContext::Replaced(replaced) => {
                        let pbm = replaced
                            .style
                            .padding_border_margin(flex_context.containing_block);
                        let box_size = used_cross_size_override.map(|size| LogicalVec2 {
                            inline: replaced
                                .style
                                .content_box_size(flex_context.containing_block, &pbm)
                                .inline
                                .map(Au::from),
                            block: AuOrAuto::LengthPercentage(size),
                        });
                        let size = replaced.contents.used_size_as_if_inline_element(
                            flex_context.containing_block,
                            &replaced.style,
                            box_size,
                            &pbm,
                        );
                        let cross_size = flex_context.vec2_to_flex_relative(size).cross;
                        let fragments = replaced.contents.make_fragments(&replaced.style, size);

                        FlexItemLayoutResult {
                            hypothetical_cross_size: cross_size,
                            fragments,
                            positioning_context,

                            // We will need to synthesize the baseline, but since the used cross
                            // size can differ from the hypothetical cross size, we should defer
                            // synthesizing until needed.
                            baseline_relative_to_margin_box: None,
                        }
                    },
                    IndependentFormattingContext::NonReplaced(non_replaced) => {
                        let block_size = match used_cross_size_override {
                            Some(s) => AuOrAuto::LengthPercentage(s),
                            None => self.content_box_size.cross.map(|t| t),
                        };

                        let item_as_containing_block = ContainingBlock {
                            inline_size: used_main_size,
                            block_size,
                            style: &non_replaced.style,
                        };
                        let IndependentLayout {
                            fragments,
                            content_block_size,
                            baselines: content_box_baselines,
                            ..
                        } = non_replaced.layout(
                            flex_context.layout_context,
                            &mut positioning_context,
                            &item_as_containing_block,
                            flex_context.containing_block,
                        );

                        let baselines_relative_to_margin_box =
                            self.layout_baselines_relative_to_margin_box(&content_box_baselines);

                        let baseline_relative_to_margin_box = match self.align_self.0.value() {
                            // ‘baseline’ computes to ‘first baseline’.
                            AlignFlags::BASELINE => baselines_relative_to_margin_box.first,
                            AlignFlags::LAST_BASELINE => baselines_relative_to_margin_box.last,
                            _ => None,
                        };

                        let hypothetical_cross_size = self
                            .content_box_size
                            .cross
                            .auto_is(|| content_block_size)
                            .clamp_between_extremums(
                                self.content_min_size.cross,
                                self.content_max_size.cross,
                            );

                        FlexItemLayoutResult {
                            hypothetical_cross_size,
                            fragments,
                            positioning_context,
                            baseline_relative_to_margin_box,
                        }
                    },
                }
            },
            FlexAxis::Column => {
                todo!()
                // Note: this panic shouldn’t happen since the start of `FlexContainer::layout`
                // forces `FlexAxis::Row`.
            },
        }
    }

    fn layout_baselines_relative_to_margin_box(
        &self,
        baselines_relative_to_content_box: &Baselines,
    ) -> Baselines {
        baselines_relative_to_content_box.offset(
            self.margin.cross_start.auto_is(Au::zero) +
                self.padding.cross_start +
                self.border.cross_start,
        )
    }

    fn synthesized_baseline_relative_to_margin_box(&self, content_size: Au) -> Au {
        // If the item does not have a baseline in the necessary axis,
        // then one is synthesized from the flex item’s border box.
        // https://drafts.csswg.org/css-flexbox/#valdef-align-items-baseline
        content_size +
            self.margin.cross_start.auto_is(Au::zero) +
            self.padding.cross_start +
            self.border.cross_start +
            self.border.cross_end +
            self.padding.cross_end
    }
}

impl<'items> FlexLine<'items> {
    /// <https://drafts.csswg.org/css-flexbox/#algo-cross-line>
    fn cross_size(
        &self,
        item_layout_results: &[FlexItemLayoutResult],
        flex_context: &FlexContext,
    ) -> Au {
        if flex_context.container_is_single_line {
            if let Some(size) = flex_context.container_definite_inner_size.cross {
                return size;
            }
        }

        let mut max_ascent = Au::zero();
        let mut max_descent = Au::zero();
        let mut max_outer_hypothetical_cross_size = Au::zero();
        for (item_result, item) in item_layout_results.iter().zip(&*self.items) {
            // TODO: check inline-axis is parallel to main axis, check no auto cross margins
            if matches!(
                item.align_self.0.value(),
                AlignFlags::BASELINE | AlignFlags::LAST_BASELINE
            ) {
                let baseline = item_result.get_or_synthesize_baseline_with_block_size(
                    item_result.hypothetical_cross_size,
                    item,
                );
                let hypothetical_margin_box_cross_size =
                    item_result.hypothetical_cross_size + item.pbm_auto_is_zero.cross;
                max_ascent = max_ascent.max(baseline);
                max_descent = max_descent.max(hypothetical_margin_box_cross_size - baseline);
            } else {
                max_outer_hypothetical_cross_size = max_outer_hypothetical_cross_size
                    .max(item_result.hypothetical_cross_size + item.pbm_auto_is_zero.cross);
            }
        }

        // FIXME: add support for `align-self: baseline`
        // and computing the baseline of flex items.
        // https://drafts.csswg.org/css-flexbox/#baseline-participation
        let largest = max_outer_hypothetical_cross_size.max(max_ascent + max_descent);
        if flex_context.container_is_single_line {
            largest.clamp_between_extremums(
                flex_context.container_min_cross_size,
                flex_context.container_max_cross_size,
            )
        } else {
            largest
        }
    }

    // Return the main-start and main-end margin of each item in the line,
    // with `auto` values resolved,
    // and return whether free space has been distributed.
    fn resolve_auto_main_margins(
        &self,
        remaining_free_space: Au,
    ) -> (impl Iterator<Item = (Au, Au)> + '_, bool) {
        let each_auto_margin = if remaining_free_space > Au::zero() {
            let auto_margins_count = self
                .items
                .iter()
                .map(|item| {
                    item.margin.main_start.is_auto() as u32 + item.margin.main_end.is_auto() as u32
                })
                .sum::<u32>();
            if auto_margins_count > 0 {
                remaining_free_space / auto_margins_count as i32
            } else {
                Au::zero()
            }
        } else {
            Au::zero()
        };
        (
            self.items.iter().map(move |item| {
                (
                    item.margin.main_start.auto_is(|| each_auto_margin),
                    item.margin.main_end.auto_is(|| each_auto_margin),
                )
            }),
            each_auto_margin > Au::zero(),
        )
    }
}

impl FlexItem<'_> {
    /// Return the cross-start and cross-end margin, with `auto` values resolved.
    /// <https://drafts.csswg.org/css-flexbox/#algo-cross-margins>
    fn resolve_auto_cross_margins(
        &self,
        flex_context: &FlexContext,
        line_cross_size: Au,
        item_cross_content_size: Au,
    ) -> (Au, Au) {
        let auto_count = match (self.margin.cross_start, self.margin.cross_end) {
            (AuOrAuto::LengthPercentage(start), AuOrAuto::LengthPercentage(end)) => {
                return (start, end);
            },
            (AuOrAuto::Auto, AuOrAuto::Auto) => 2,
            _ => 1,
        };
        let outer_size = self.pbm_auto_is_zero.cross + item_cross_content_size;
        let available = line_cross_size - outer_size;
        let start;
        let end;
        if available > Au::zero() {
            let each_auto_margin = available / auto_count;
            start = self.margin.cross_start.auto_is(|| each_auto_margin);
            end = self.margin.cross_end.auto_is(|| each_auto_margin);
        } else {
            // “the block-start or inline-start margin (whichever is in the cross axis)”
            // This margin is the cross-end on iff `flex-wrap` is `wrap-reverse`,
            // cross-start otherwise.
            // We know this because:
            // https://drafts.csswg.org/css-flexbox/#flex-wrap-property
            // “For the values that are not wrap-reverse,
            //  the cross-start direction is equivalent to
            //  either the inline-start or block-start direction of the current writing mode
            //  (whichever is in the cross axis)
            //  and the cross-end direction is the opposite direction of cross-start.
            //  When flex-wrap is wrap-reverse,
            //  the cross-start and cross-end directions are swapped.”
            let flex_wrap = flex_context.containing_block.style.get_position().flex_wrap;
            let flex_wrap_reverse = match flex_wrap {
                FlexWrap::Nowrap | FlexWrap::Wrap => false,
                FlexWrap::WrapReverse => true,
            };
            // “if the block-start or inline-start margin (whichever is in the cross axis) is auto,
            //  set it to zero. Set the opposite margin so that the outer cross size of the item
            //  equals the cross size of its flex line.”
            if flex_wrap_reverse {
                start = self.margin.cross_start.auto_is(|| available);
                end = self.margin.cross_end.auto_is(Au::zero);
            } else {
                start = self.margin.cross_start.auto_is(Au::zero);
                end = self.margin.cross_end.auto_is(|| available);
            }
        }
        (start, end)
    }

    /// Return the coordinate of the cross-start side of the content area
    fn align_along_cross_axis(
        &self,
        margin: &FlexRelativeSides<Au>,
        used_cross_size: &Au,
        line_cross_size: Au,
        propagated_baseline: Au,
        max_propagated_baseline: Au,
    ) -> Au {
        let outer_cross_start =
            if self.margin.cross_start.is_auto() || self.margin.cross_end.is_auto() {
                Au::zero()
            } else {
                match self.align_self.0.value() {
                    AlignFlags::STRETCH | AlignFlags::FLEX_START => Au::zero(),
                    AlignFlags::FLEX_END => {
                        let margin_box_cross = *used_cross_size + self.pbm_auto_is_zero.cross;
                        line_cross_size - margin_box_cross
                    },
                    AlignFlags::CENTER => {
                        let margin_box_cross = *used_cross_size + self.pbm_auto_is_zero.cross;
                        (line_cross_size - margin_box_cross) / 2
                    },
                    AlignFlags::BASELINE | AlignFlags::LAST_BASELINE => {
                        max_propagated_baseline - propagated_baseline
                    },
                    _ => Au::zero(),
                }
            };
        outer_cross_start + margin.cross_start + self.border.cross_start + self.padding.cross_start
    }
}

impl FlexItemBox {
    fn inline_content_size_info(
        &mut self,
        layout_context: &LayoutContext,
        container_writing_mode: WritingMode,
        container_is_horizontal: bool,
        flex_axis: FlexAxis,
        main_start_cross_start: MainStartCrossStart,
    ) -> FlexItemBoxInlineContentSizesInfo {
        let style = self.style().clone();
        let item_writing_mode = style.writing_mode;
        let item_is_horizontal = item_writing_mode.is_horizontal();
        let cross_axis_is_item_block_axis =
            cross_axis_is_item_block_axis(container_is_horizontal, item_is_horizontal, flex_axis);

        let pbm = style.padding_border_margin_for_intrinsic_size(item_writing_mode);
        let box_size = style
            .box_size(item_writing_mode)
            .map(|v| v.percentage_relative_to(Length::zero()));
        let content_box_size = style
            .content_box_size_for_box_size(box_size, &pbm)
            .map(|v| v.map(Au::from));
        let min_size = style
            .min_box_size(item_writing_mode)
            .map(|v| v.percentage_relative_to(Length::zero()));
        let content_min_size = style
            .content_min_box_size_for_min_size(min_size, &pbm)
            .map(|v| v.map(Au::from));
        let max_size = style
            .max_box_size(item_writing_mode)
            .map(|v| v.map(|v| v.percentage_relative_to(Length::zero())));
        let content_max_size = style
            .content_max_box_size_for_max_size(max_size, &pbm)
            .map(|v| v.map(Au::from));
        let automatic_min_size = self.automatic_min_size(
            layout_context,
            cross_axis_is_item_block_axis,
            content_box_size,
            content_min_size,
            content_max_size,
        );

        let content_box_size = flex_axis.vec2_to_flex_relative(content_box_size);
        let content_min_size_no_auto = LogicalVec2 {
            inline: content_min_size.inline.auto_is(|| automatic_min_size),
            block: content_min_size.block.auto_is(Au::zero),
        };
        let content_min_size_no_auto = flex_axis.vec2_to_flex_relative(content_min_size_no_auto);
        let content_max_size = flex_axis.vec2_to_flex_relative(content_max_size);

        let padding = main_start_cross_start.sides_to_flex_relative(pbm.padding);
        let border = main_start_cross_start.sides_to_flex_relative(pbm.border);
        let padding_border = padding.sum_by_axis() + border.sum_by_axis();
        let margin_auto_is_zero = pbm.margin.auto_is(Au::zero);
        let margin_auto_is_zero =
            main_start_cross_start.sides_to_flex_relative(margin_auto_is_zero);
        let pbm_auto_is_zero = FlexRelativeVec2 {
            main: padding_border.main,
            cross: padding_border.cross,
        } + margin_auto_is_zero.sum_by_axis();

        let flex_base_size = flex_base_size(
            layout_context,
            FlexRelativeVec2 {
                main: None,
                cross: None,
            },
            &mut self.independent_formatting_context,
            cross_axis_is_item_block_axis,
            content_box_size,
            padding_border,
        );

        let content_contribution_sizes = self.inline_content_sizes(
            layout_context,
            container_writing_mode,
            content_box_size,
            content_min_size_no_auto,
            content_max_size,
            pbm_auto_is_zero,
        );
        let outer_flex_base_size = flex_base_size + pbm_auto_is_zero.main;
        let max_flex_factors = self.desired_flex_factors_for_preferred_width(
            content_contribution_sizes.max_content,
            flex_base_size,
            outer_flex_base_size,
        );

        // > The min-content main size of a single-line flex container is calculated
        // > identically to the max-content main size, except that the flex items’
        // > min-content contributions are used instead of their max-content contributions.
        let min_flex_factors = self.desired_flex_factors_for_preferred_width(
            content_contribution_sizes.min_content,
            flex_base_size,
            outer_flex_base_size,
        );

        // > However, for a multi-line container, the min-content main size is simply the
        // > largest min-content contribution of all the non-collapsed flex items in the
        // > flex container. For this purpose, each item’s contribution is capped by the
        // > item’s flex base size if the item is not growable, floored by the item’s flex
        // > base size if the item is not shrinkable, and then further clamped by the item’s
        // > min and max main sizes.
        let mut min_content_main_size_for_multiline_container =
            content_contribution_sizes.min_content;
        if style.get_position().flex_grow.is_zero() {
            min_content_main_size_for_multiline_container.min_assign(flex_base_size);
        }
        if style.get_position().flex_shrink.is_zero() {
            min_content_main_size_for_multiline_container.max_assign(flex_base_size);
        }
        min_content_main_size_for_multiline_container =
            min_content_main_size_for_multiline_container
                .clamp_between_extremums(content_min_size_no_auto.main, content_max_size.main);

        FlexItemBoxInlineContentSizesInfo {
            outer_flex_base_size,
            content_min_size_no_auto,
            content_max_size,
            pbm_auto_is_zero,
            min_flex_factors,
            max_flex_factors,
            min_content_main_size_for_multiline_container,
        }
    }

    fn desired_flex_factors_for_preferred_width(
        &self,
        preferred_width: Au,
        flex_base_size: Au,
        outer_flex_base_size: Au,
    ) -> DesiredFlexFractionAndGrowOrShrinkFactor {
        let difference = (preferred_width - outer_flex_base_size).to_f32_px();
        let (flex_grow_or_scaled_flex_shrink_factor, desired_flex_fraction) = if difference > 0.0 {
            // > If that result is positive, divide it by the item’s flex
            // > grow factor if the flex grow > factor is ≥ 1, or multiply
            // > it by the flex grow factor if the flex grow factor is < 1;
            let flex_grow_factor = self.style().get_position().flex_grow.0;

            (
                flex_grow_factor,
                if flex_grow_factor >= 1.0 {
                    difference / flex_grow_factor
                } else {
                    difference * flex_grow_factor
                },
            )
        } else if difference < 0.0 {
            // > if the result is negative, divide it by the item’s scaled
            // > flex shrink factor (if dividing > by zero, treat the result
            // > as negative infinity).
            let flex_shrink_factor = self.style().get_position().flex_shrink.0;
            let scaled_flex_shrink_factor = flex_shrink_factor * flex_base_size.to_f32_px();

            (
                scaled_flex_shrink_factor,
                if scaled_flex_shrink_factor != 0.0 {
                    difference / scaled_flex_shrink_factor
                } else {
                    f32::NEG_INFINITY
                },
            )
        } else {
            (0.0, 0.0)
        };

        DesiredFlexFractionAndGrowOrShrinkFactor {
            desired_flex_fraction,
            flex_grow_or_shrink_factor: flex_grow_or_scaled_flex_shrink_factor,
        }
    }

    fn inline_content_sizes(
        &mut self,
        layout_context: &LayoutContext,
        writing_mode: WritingMode,
        content_box_size: FlexRelativeVec2<GenericLengthPercentageOrAuto<Au>>,
        content_min_size: FlexRelativeVec2<Au>,
        content_max_size: FlexRelativeVec2<Option<Au>>,
        pbm_auto_is_zero: FlexRelativeVec2<Au>,
    ) -> ContentSizes {
        // TODO: use cross sizes when container is a column
        // (and check for ‘writing-mode’?)

        // <https://drafts.csswg.org/css-flexbox/#intrinsic-item-contributions>
        let outer_inline_content_sizes = self
            .independent_formatting_context
            .outer_inline_content_sizes(layout_context, writing_mode);
        let outer_preferred_size = content_box_size
            .main
            .non_auto()
            .map(|preferred_size| preferred_size + pbm_auto_is_zero.main);
        let outer_min_main_size = content_min_size.main + pbm_auto_is_zero.main;
        let outer_max_main_size = content_max_size
            .main
            .map(|max_main_size| max_main_size + pbm_auto_is_zero.main);

        // > The main-size min-content contribution of a flex item is the larger of its
        // > outer min-content size and outer preferred size if that is not auto, clamped by
        // > its min/max main size.
        let min_content_contribution = outer_preferred_size
            .map_or(
                outer_inline_content_sizes.min_content,
                |outer_preferred_size| {
                    outer_preferred_size.max(outer_inline_content_sizes.min_content)
                },
            )
            .clamp_between_extremums(outer_min_main_size, outer_max_main_size);

        // > The main-size max-content contribution of a flex item is the larger of its
        // > outer max-content size and outer preferred size if that is not auto, clamped by
        // > its min/max main size.
        let max_content_contribution = outer_preferred_size
            .map_or(
                outer_inline_content_sizes.max_content,
                |outer_preferred_size| {
                    outer_preferred_size.max(outer_inline_content_sizes.max_content)
                },
            )
            .clamp_between_extremums(outer_min_main_size, outer_max_main_size);

        ContentSizes {
            min_content: min_content_contribution,
            max_content: max_content_contribution,
        }
    }

    /// This is an implementation of <https://drafts.csswg.org/css-flexbox/#min-size-auto>.
    fn automatic_min_size(
        &mut self,
        layout_context: &LayoutContext,
        cross_axis_is_item_block_axis: bool,
        content_box_size: LogicalVec2<AuOrAuto>,
        min_size: LogicalVec2<GenericLengthPercentageOrAuto<Au>>,
        max_size: LogicalVec2<Option<Au>>,
    ) -> Au {
        // FIXME(stshine): Consider more situations when auto min size is not needed.
        if self
            .independent_formatting_context
            .style()
            .get_box()
            .overflow_x
            .is_scrollable()
        {
            return Au::zero();
        }

        if cross_axis_is_item_block_axis {
            let specified_size_suggestion = content_box_size.inline;

            let transferred_size_suggestion = match self.independent_formatting_context {
                IndependentFormattingContext::NonReplaced(_) => None,
                IndependentFormattingContext::Replaced(ref bfc) => {
                    match (
                        bfc.contents.inline_size_over_block_size_intrinsic_ratio(
                            self.independent_formatting_context.style(),
                        ),
                        content_box_size.block,
                    ) {
                        (Some(ratio), AuOrAuto::LengthPercentage(block_size)) => {
                            let block_size = block_size.clamp_between_extremums(
                                min_size.block.auto_is(Au::zero),
                                max_size.block,
                            );
                            Some(block_size.scale_by(ratio))
                        },
                        _ => None,
                    }
                },
            };

            let inline_content_size = self
                .independent_formatting_context
                .inline_content_sizes(layout_context)
                .min_content;
            let content_size_suggestion = match self.independent_formatting_context {
                IndependentFormattingContext::NonReplaced(_) => inline_content_size,
                IndependentFormattingContext::Replaced(ref replaced) => {
                    if let Some(ratio) = replaced
                        .contents
                        .inline_size_over_block_size_intrinsic_ratio(
                            self.independent_formatting_context.style(),
                        )
                    {
                        inline_content_size.clamp_between_extremums(
                            min_size.block.auto_is(Au::zero).scale_by(ratio),
                            max_size.block.map(|l| l.scale_by(ratio)),
                        )
                    } else {
                        inline_content_size
                    }
                },
            };

            let result = match specified_size_suggestion {
                AuOrAuto::LengthPercentage(l) => l.min(content_size_suggestion),
                AuOrAuto::Auto => {
                    if let Some(l) = transferred_size_suggestion {
                        l.min(content_size_suggestion)
                    } else {
                        content_size_suggestion
                    }
                },
            };
            result.clamp_below_max(max_size.inline)
        } else {
            // FIXME(stshine): Implement this when main axis is item's block axis.
            Au::zero()
        }
    }
}
