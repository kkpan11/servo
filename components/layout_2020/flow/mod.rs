/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Flow layout, also known as block-and-inline layout.

use crate::cell::ArcRefCell;
use crate::context::LayoutContext;
use crate::flow::float::{ClearSide, ContainingBlockPositionInfo, FloatBox, SequentialLayoutState};
use crate::flow::inline::InlineFormattingContext;
use crate::formatting_contexts::{
    IndependentFormattingContext, IndependentLayout, NonReplacedFormattingContext,
};
use crate::fragment_tree::{
    BaseFragmentInfo, BoxFragment, CollapsedBlockMargins, CollapsedMargin, Fragment,
};
use crate::geom::flow_relative::{Rect, Sides, Vec2};
use crate::positioned::{AbsolutelyPositionedBox, PositioningContext};
use crate::replaced::ReplacedContent;
use crate::sizing::{self, ContentSizes};
use crate::style_ext::{ComputedValuesExt, PaddingBorderMargin};
use crate::ContainingBlock;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use servo_arc::Arc;
use style::computed_values::clear::T as Clear;
use style::computed_values::float::T as Float;
use style::logical_geometry::WritingMode;
use style::properties::ComputedValues;
use style::values::computed::{Length, LengthOrAuto};
use style::Zero;

mod construct;
pub mod float;
pub mod inline;
mod root;

pub use root::{BoxTree, CanvasBackground};

#[derive(Debug, Serialize)]
pub(crate) struct BlockFormattingContext {
    pub contents: BlockContainer,
    pub contains_floats: bool,
}

#[derive(Debug, Serialize)]
pub(crate) enum BlockContainer {
    BlockLevelBoxes(Vec<ArcRefCell<BlockLevelBox>>),
    InlineFormattingContext(InlineFormattingContext),
}

#[derive(Debug, Serialize)]
pub(crate) enum BlockLevelBox {
    SameFormattingContextBlock {
        base_fragment_info: BaseFragmentInfo,
        #[serde(skip_serializing)]
        style: Arc<ComputedValues>,
        contents: BlockContainer,
    },
    OutOfFlowAbsolutelyPositionedBox(ArcRefCell<AbsolutelyPositionedBox>),
    OutOfFlowFloatBox(FloatBox),
    Independent(IndependentFormattingContext),
}

impl BlockLevelBox {
    fn find_block_margin_collapsing_with_parent(
        &self,
        collected_margin: &mut CollapsedMargin,
        containing_block: &ContainingBlock,
    ) -> bool {
        let style = match self {
            BlockLevelBox::SameFormattingContextBlock { ref style, .. } => &style,
            BlockLevelBox::OutOfFlowAbsolutelyPositionedBox(_) |
            BlockLevelBox::OutOfFlowFloatBox(_) => return true,
            BlockLevelBox::Independent(ref context) => context.style(),
        };

        // FIXME: This should only return false when 'clear' causes clearance.
        if style.get_box().clear != Clear::None {
            return false;
        }

        let pbm = style.padding_border_margin(containing_block);
        let start_margin = pbm.margin.block_start.auto_is(Length::zero);
        collected_margin.adjoin_assign(&CollapsedMargin::new(start_margin));

        let contents = match self {
            BlockLevelBox::SameFormattingContextBlock { ref contents, .. } => contents,
            _ => return false,
        };

        if pbm.padding.block_start != Length::zero() || pbm.border.block_start != Length::zero() {
            return false;
        }

        match contents {
            BlockContainer::BlockLevelBoxes(boxes) => {
                let min_inline_size = style
                    .content_min_box_size(containing_block, &pbm)
                    .auto_is(Length::zero)
                    .inline;
                let max_inline_size = style.content_max_box_size(containing_block, &pbm).inline;
                let inline_size = style
                    .content_box_size(containing_block, &pbm)
                    .inline
                    .auto_is(|| {
                        let margin_inline_start = pbm.margin.inline_start.auto_is(Length::zero);
                        let margin_inline_end = pbm.margin.inline_end.auto_is(Length::zero);
                        containing_block.inline_size -
                            pbm.padding_border_sums.inline -
                            margin_inline_start -
                            margin_inline_end
                    })
                    .clamp_between_extremums(min_inline_size, max_inline_size);

                // The block size is irrelevant here.
                let block_size = LengthOrAuto::Auto;

                let containing_block_for_children = ContainingBlock {
                    inline_size,
                    block_size,
                    style,
                };

                if !Self::find_block_margin_collapsing_with_parent_from_slice(
                    &boxes,
                    collected_margin,
                    &containing_block_for_children,
                ) {
                    return false;
                }
            },
            BlockContainer::InlineFormattingContext(_) => return false,
        }

        let block_size_zero =
            style.content_block_size().is_definitely_zero() || style.content_block_size().is_auto();
        let min_block_size_zero =
            style.min_block_size().is_definitely_zero() || style.min_block_size().is_auto();
        if !min_block_size_zero ||
            !block_size_zero ||
            pbm.padding_border_sums.block != Length::zero()
        {
            return false;
        }

        let end_margin = pbm.margin.block_end.auto_is(Length::zero);
        collected_margin.adjoin_assign(&CollapsedMargin::new(end_margin));

        true
    }

    fn find_block_margin_collapsing_with_parent_from_slice(
        boxes: &[ArcRefCell<BlockLevelBox>],
        margin: &mut CollapsedMargin,
        containing_block: &ContainingBlock,
    ) -> bool {
        boxes.iter().all(|block_level_box| {
            block_level_box
                .borrow()
                .find_block_margin_collapsing_with_parent(margin, containing_block)
        })
    }
}

struct FlowLayout {
    pub fragments: Vec<Fragment>,
    pub content_block_size: Length,
    pub collapsible_margins_in_children: CollapsedBlockMargins,
}

#[derive(Clone, Copy)]
struct CollapsibleWithParentStartMargin(bool);

impl BlockFormattingContext {
    pub(super) fn layout(
        &self,
        layout_context: &LayoutContext,
        positioning_context: &mut PositioningContext,
        containing_block: &ContainingBlock,
    ) -> IndependentLayout {
        let mut sequential_layout_state = if self.contains_floats || !layout_context.use_rayon {
            Some(SequentialLayoutState::new(containing_block.inline_size))
        } else {
            None
        };

        let flow_layout = self.contents.layout(
            layout_context,
            positioning_context,
            containing_block,
            sequential_layout_state.as_mut(),
            CollapsibleWithParentStartMargin(false),
        );
        debug_assert!(
            !flow_layout
                .collapsible_margins_in_children
                .collapsed_through
        );

        // The content height of a BFC root should include any float participating in that BFC
        // (https://drafts.csswg.org/css2/#root-height), we implement this by imagining there is
        // an element with `clear: both` after the actual contents.
        let clearance = sequential_layout_state.and_then(|sequential_layout_state| {
            sequential_layout_state.calculate_clearance(ClearSide::Both, &CollapsedMargin::zero())
        });

        IndependentLayout {
            fragments: flow_layout.fragments,
            content_block_size: flow_layout.content_block_size +
                flow_layout.collapsible_margins_in_children.end.solve() +
                clearance.unwrap_or_else(Length::zero),
        }
    }
}

/// Finds the min/max-content inline size of the block-level children of a block container.
/// The in-flow boxes will stack vertically, so we only need to consider the maximum size.
/// But floats can flow horizontally depending on 'clear', so we may need to sum their sizes.
/// CSS 2 does not define the exact algorithm, this logic is based on the behavior observed
/// on Gecko and Blink.
fn calculate_inline_content_size_for_block_level_boxes(
    boxes: &[ArcRefCell<BlockLevelBox>],
    layout_context: &LayoutContext,
    writing_mode: WritingMode,
) -> ContentSizes {
    let get_box_info = |box_: &ArcRefCell<BlockLevelBox>| {
        let size = box_
            .borrow_mut()
            .inline_content_sizes(layout_context, writing_mode);
        if let BlockLevelBox::OutOfFlowFloatBox(ref float_box) = *box_.borrow_mut() {
            let style_box = &float_box.contents.style().get_box();
            (size, style_box.float, style_box.clear)
        } else {
            // The element may in fact have clearance, but the logic below ignores it,
            // so don't bother retrieving it from the style.
            (size, Float::None, Clear::None)
        }
    };

    /// When iterating the block-level boxes to compute the inline content sizes,
    /// this struct contains the data accumulated up to the current box.
    struct AccumulatedData {
        /// The maximum size seen so far, not including trailing uncleared floats.
        max_size: ContentSizes,
        /// The size of the trailing uncleared floats with 'float: left'.
        left_floats: ContentSizes,
        /// The size of the trailing uncleared floats with 'float: right'.
        right_floats: ContentSizes,
    }

    impl AccumulatedData {
        fn max_size_including_uncleared_floats(&self) -> ContentSizes {
            self.max_size.max(self.left_floats.add(&self.right_floats))
        }
        fn clear_floats(&mut self, clear: Clear) {
            match clear {
                Clear::Left => {
                    self.max_size = self.max_size_including_uncleared_floats();
                    self.left_floats = ContentSizes::zero();
                },
                Clear::Right => {
                    self.max_size = self.max_size_including_uncleared_floats();
                    self.right_floats = ContentSizes::zero();
                },
                Clear::Both => {
                    self.max_size = self.max_size_including_uncleared_floats();
                    self.left_floats = ContentSizes::zero();
                    self.right_floats = ContentSizes::zero();
                },
                Clear::None => {},
            };
        }
    }

    let accumulate = |mut data: AccumulatedData, (size, float, clear)| {
        if float == Float::None {
            // TODO: The first BFC root after a sequence of floats should appear next to them
            // (if it doesn't have clearance).
            data.clear_floats(Clear::Both);
            data.max_size = data.max_size.max(size);
        } else {
            data.clear_floats(clear);
            match float {
                Float::Left => data.left_floats = data.left_floats.add(&size),
                Float::Right => data.right_floats = data.right_floats.add(&size),
                Float::None => unreachable!(),
            }
        }
        data
    };
    let zero = AccumulatedData {
        max_size: ContentSizes::zero(),
        left_floats: ContentSizes::zero(),
        right_floats: ContentSizes::zero(),
    };
    let data = if layout_context.use_rayon {
        boxes
            .par_iter()
            .map(get_box_info)
            .collect::<Vec<_>>()
            .into_iter()
            .fold(zero, accumulate)
    } else {
        boxes.iter().map(get_box_info).fold(zero, accumulate)
    };
    data.max_size_including_uncleared_floats()
}

impl BlockContainer {
    fn layout(
        &self,
        layout_context: &LayoutContext,
        positioning_context: &mut PositioningContext,
        containing_block: &ContainingBlock,
        sequential_layout_state: Option<&mut SequentialLayoutState>,
        collapsible_with_parent_start_margin: CollapsibleWithParentStartMargin,
    ) -> FlowLayout {
        match self {
            BlockContainer::BlockLevelBoxes(child_boxes) => layout_block_level_children(
                layout_context,
                positioning_context,
                child_boxes,
                containing_block,
                sequential_layout_state,
                collapsible_with_parent_start_margin,
            ),
            BlockContainer::InlineFormattingContext(ifc) => ifc.layout(
                layout_context,
                positioning_context,
                containing_block,
                sequential_layout_state,
            ),
        }
    }

    pub(super) fn inline_content_sizes(
        &self,
        layout_context: &LayoutContext,
        writing_mode: WritingMode,
    ) -> ContentSizes {
        match &self {
            Self::BlockLevelBoxes(boxes) => calculate_inline_content_size_for_block_level_boxes(
                boxes,
                layout_context,
                writing_mode,
            ),
            Self::InlineFormattingContext(context) => {
                context.inline_content_sizes(layout_context, writing_mode)
            },
        }
    }
}

fn layout_block_level_children(
    layout_context: &LayoutContext,
    positioning_context: &mut PositioningContext,
    child_boxes: &[ArcRefCell<BlockLevelBox>],
    containing_block: &ContainingBlock,
    mut sequential_layout_state: Option<&mut SequentialLayoutState>,
    collapsible_with_parent_start_margin: CollapsibleWithParentStartMargin,
) -> FlowLayout {
    match sequential_layout_state {
        Some(ref mut sequential_layout_state) => layout_block_level_children_sequentially(
            layout_context,
            positioning_context,
            child_boxes,
            containing_block,
            sequential_layout_state,
            collapsible_with_parent_start_margin,
        ),
        None => layout_block_level_children_in_parallel(
            layout_context,
            positioning_context,
            child_boxes,
            containing_block,
            collapsible_with_parent_start_margin,
        ),
    }
}

fn layout_block_level_children_in_parallel(
    layout_context: &LayoutContext,
    positioning_context: &mut PositioningContext,
    child_boxes: &[ArcRefCell<BlockLevelBox>],
    containing_block: &ContainingBlock,
    collapsible_with_parent_start_margin: CollapsibleWithParentStartMargin,
) -> FlowLayout {
    let collects_for_nearest_positioned_ancestor =
        positioning_context.collects_for_nearest_positioned_ancestor();
    let layout_results: Vec<(Fragment, PositioningContext)> = child_boxes
        .par_iter()
        .map(|child_box| {
            let mut child_positioning_context =
                PositioningContext::new_for_subtree(collects_for_nearest_positioned_ancestor);
            let fragment = child_box.borrow_mut().layout(
                layout_context,
                &mut child_positioning_context,
                containing_block,
                /* sequential_layout_state = */ None,
                /* collapsible_with_parent_start_margin = */ None,
            );
            (fragment, child_positioning_context)
        })
        .collect();

    let mut placement_state = PlacementState::new(collapsible_with_parent_start_margin);
    let fragments = layout_results
        .into_iter()
        .map(|(mut fragment, mut child_positioning_context)| {
            placement_state.place_fragment(&mut fragment, None);
            child_positioning_context.adjust_static_position_of_hoisted_fragments(&fragment);
            positioning_context.append(child_positioning_context);
            fragment
        })
        .collect();

    let (content_block_size, collapsible_margins_in_children) = placement_state.finish();
    FlowLayout {
        fragments,
        content_block_size,
        collapsible_margins_in_children,
    }
}

fn layout_block_level_children_sequentially(
    layout_context: &LayoutContext,
    positioning_context: &mut PositioningContext,
    child_boxes: &[ArcRefCell<BlockLevelBox>],
    containing_block: &ContainingBlock,
    sequential_layout_state: &mut SequentialLayoutState,
    collapsible_with_parent_start_margin: CollapsibleWithParentStartMargin,
) -> FlowLayout {
    let mut placement_state = PlacementState::new(collapsible_with_parent_start_margin);
    let collects_for_nearest_positioned_ancestor =
        positioning_context.collects_for_nearest_positioned_ancestor();

    // Because floats are involved, we do layout for this block formatting context in tree
    // order without parallelism. This enables mutable access to a `SequentialLayoutState` that
    // tracks every float encountered so far (again in tree order).
    let fragments = child_boxes
        .iter()
        .map(|child_box| {
            let mut child_positioning_context =
                PositioningContext::new_for_subtree(collects_for_nearest_positioned_ancestor);
            let mut fragment = child_box.borrow_mut().layout(
                layout_context,
                &mut child_positioning_context,
                containing_block,
                Some(&mut *sequential_layout_state),
                Some(CollapsibleWithParentStartMargin(
                    placement_state.next_in_flow_margin_collapses_with_parent_start_margin,
                )),
            );

            placement_state.place_fragment(&mut fragment, Some(sequential_layout_state));

            child_positioning_context.adjust_static_position_of_hoisted_fragments(&fragment);
            positioning_context.append(child_positioning_context);

            fragment
        })
        .collect();

    let (content_block_size, collapsible_margins_in_children) = placement_state.finish();
    FlowLayout {
        fragments,
        content_block_size,
        collapsible_margins_in_children,
    }
}

impl BlockLevelBox {
    fn layout(
        &mut self,
        layout_context: &LayoutContext,
        positioning_context: &mut PositioningContext,
        containing_block: &ContainingBlock,
        sequential_layout_state: Option<&mut SequentialLayoutState>,
        collapsible_with_parent_start_margin: Option<CollapsibleWithParentStartMargin>,
    ) -> Fragment {
        match self {
            BlockLevelBox::SameFormattingContextBlock {
                base_fragment_info: tag,
                style,
                contents,
            } => Fragment::Box(positioning_context.layout_maybe_position_relative_fragment(
                layout_context,
                containing_block,
                style,
                |positioning_context| {
                    layout_in_flow_non_replaced_block_level_same_formatting_context(
                        layout_context,
                        positioning_context,
                        containing_block,
                        *tag,
                        style,
                        contents,
                        sequential_layout_state,
                        collapsible_with_parent_start_margin,
                    )
                },
            )),
            BlockLevelBox::Independent(independent) => match independent {
                IndependentFormattingContext::Replaced(replaced) => {
                    Fragment::Box(positioning_context.layout_maybe_position_relative_fragment(
                        layout_context,
                        containing_block,
                        &replaced.style,
                        |_positioning_context| {
                            layout_in_flow_replaced_block_level(
                                containing_block,
                                replaced.base_fragment_info,
                                &replaced.style,
                                &replaced.contents,
                                sequential_layout_state,
                            )
                        },
                    ))
                },
                IndependentFormattingContext::NonReplaced(non_replaced) => {
                    Fragment::Box(positioning_context.layout_maybe_position_relative_fragment(
                        layout_context,
                        containing_block,
                        &non_replaced.style,
                        |positioning_context| {
                            layout_in_flow_non_replaced_block_level_independent_formatting_context(
                                layout_context,
                                positioning_context,
                                containing_block,
                                non_replaced.base_fragment_info,
                                &non_replaced.style,
                                non_replaced,
                                sequential_layout_state,
                            )
                        },
                    ))
                },
            },
            BlockLevelBox::OutOfFlowAbsolutelyPositionedBox(box_) => {
                let hoisted_box = AbsolutelyPositionedBox::to_hoisted(
                    box_.clone(),
                    // This is incorrect, however we do not know the
                    // correct positioning until later, in place_block_level_fragment,
                    // and this value will be adjusted there
                    Vec2::zero(),
                    containing_block,
                );
                let hoisted_fragment = hoisted_box.fragment.clone();
                positioning_context.push(hoisted_box);
                Fragment::AbsoluteOrFixedPositioned(hoisted_fragment)
            },
            BlockLevelBox::OutOfFlowFloatBox(float_box) => Fragment::Float(float_box.layout(
                layout_context,
                positioning_context,
                containing_block,
            )),
        }
    }

    fn inline_content_sizes(
        &mut self,
        layout_context: &LayoutContext,
        containing_block_writing_mode: WritingMode,
    ) -> ContentSizes {
        match self {
            Self::SameFormattingContextBlock {
                style, contents, ..
            } => sizing::outer_inline(style, containing_block_writing_mode, || {
                contents.inline_content_sizes(layout_context, style.writing_mode)
            }),
            Self::Independent(independent) => independent
                .outer_inline_content_sizes(layout_context, containing_block_writing_mode),
            BlockLevelBox::OutOfFlowAbsolutelyPositionedBox(_) => ContentSizes::zero(),
            BlockLevelBox::OutOfFlowFloatBox(float_box) => float_box
                .contents
                .outer_inline_content_sizes(layout_context, containing_block_writing_mode),
        }
    }
}

/// Lay out a normal flow non-replaced block that does not establish a new formatting
/// context.
///
/// - https://drafts.csswg.org/css2/visudet.html#blockwidth
/// - https://drafts.csswg.org/css2/visudet.html#normal-block
fn layout_in_flow_non_replaced_block_level_same_formatting_context(
    layout_context: &LayoutContext,
    positioning_context: &mut PositioningContext,
    containing_block: &ContainingBlock,
    base_fragment_info: BaseFragmentInfo,
    style: &Arc<ComputedValues>,
    contents: &BlockContainer,
    mut sequential_layout_state: Option<&mut SequentialLayoutState>,
    collapsible_with_parent_start_margin: Option<CollapsibleWithParentStartMargin>,
) -> BoxFragment {
    let ContainingBlockPaddingBorderAndMargin {
        containing_block: containing_block_for_children,
        pbm,
        min_box_size,
        max_box_size,
        margin,
    } = solve_containing_block_padding_border_and_margin_for_in_flow_box(containing_block, style);

    let computed_block_size = style.content_block_size();
    let start_margin_can_collapse_with_children =
        pbm.padding.block_start == Length::zero() && pbm.border.block_start == Length::zero();
    let end_margin_can_collapse_with_children = pbm.padding.block_end == Length::zero() &&
        pbm.border.block_end == Length::zero() &&
        computed_block_size.is_auto();

    let mut clearance = None;
    let parent_containing_block_position_info;
    match sequential_layout_state {
        None => parent_containing_block_position_info = None,
        Some(ref mut sequential_layout_state) => {
            let mut block_start_margin = CollapsedMargin::new(margin.block_start);

            // The block start margin may collapse with content margins,
            // compute the resulting one in order to place floats correctly.
            // Only need to do this if the element isn't also collapsing with its parent,
            // otherwise we should have already included the margin in an ancestor.
            // Note this lookahead stops when finding a descendant whose `clear` isn't `none`
            // (since clearance prevents collapsing margins with the parent).
            // But then we have to decide whether to actually add clearance or not,
            // so look forward again regardless of `collapsible_with_parent_start_margin`.
            // TODO: This isn't completely right: if we don't add actual clearance,
            // the margin should have been included in the parent (or some ancestor).
            // The lookahead should stop for actual clearance, not just for `clear`.
            let collapsible_with_parent_start_margin = collapsible_with_parent_start_margin.expect(
                "We should know whether we are collapsing the block start margin with the parent \
                when laying out sequentially",
            ).0 && style.get_box().clear == Clear::None;
            if !collapsible_with_parent_start_margin && start_margin_can_collapse_with_children {
                if let BlockContainer::BlockLevelBoxes(child_boxes) = contents {
                    BlockLevelBox::find_block_margin_collapsing_with_parent_from_slice(
                        &child_boxes,
                        &mut block_start_margin,
                        containing_block,
                    );
                }
            }

            // Introduce clearance if necessary.
            clearance = sequential_layout_state
                .calculate_clearance_and_adjoin_margin(style, &block_start_margin);
            if !start_margin_can_collapse_with_children {
                sequential_layout_state.collapse_margins();
            }

            // NB: This will be a no-op if we're collapsing margins with our children since that
            // can only happen if we have no block-start padding and border.
            sequential_layout_state.advance_block_position(
                pbm.padding.block_start +
                    pbm.border.block_start +
                    clearance.unwrap_or_else(Length::zero),
            );

            // We are about to lay out children. Update the offset between the block formatting
            // context and the containing block that we create for them. This offset is used to
            // ajust BFC relative coordinates to coordinates that are relative to our content box.
            // Our content box establishes the containing block for non-abspos children, including
            // floats.
            let inline_start = sequential_layout_state
                .floats
                .containing_block_info
                .inline_start +
                pbm.padding.inline_start +
                pbm.border.inline_start +
                margin.inline_start;
            let new_cb_offsets = ContainingBlockPositionInfo {
                block_start: sequential_layout_state.bfc_relative_block_position,
                block_start_margins_not_collapsed: sequential_layout_state.current_margin,
                inline_start,
                inline_end: inline_start + containing_block_for_children.inline_size,
            };
            parent_containing_block_position_info = Some(
                sequential_layout_state.replace_containing_block_position_info(new_cb_offsets),
            );
        },
    };

    let flow_layout = contents.layout(
        layout_context,
        positioning_context,
        &containing_block_for_children,
        sequential_layout_state.as_mut().map(|x| &mut **x),
        CollapsibleWithParentStartMargin(start_margin_can_collapse_with_children),
    );
    let mut content_block_size = flow_layout.content_block_size;

    // Update margins.
    let mut block_margins_collapsed_with_children = CollapsedBlockMargins::from_margin(&margin);
    let mut collapsible_margins_in_children = flow_layout.collapsible_margins_in_children;
    if start_margin_can_collapse_with_children {
        block_margins_collapsed_with_children
            .start
            .adjoin_assign(&collapsible_margins_in_children.start);
        if collapsible_margins_in_children.collapsed_through {
            block_margins_collapsed_with_children
                .start
                .adjoin_assign(&std::mem::replace(
                    &mut collapsible_margins_in_children.end,
                    CollapsedMargin::zero(),
                ));
        }
    }
    if end_margin_can_collapse_with_children {
        block_margins_collapsed_with_children
            .end
            .adjoin_assign(&collapsible_margins_in_children.end);
    } else {
        content_block_size += collapsible_margins_in_children.end.solve();
    }

    let computed_min_block_size = style.min_block_size();
    block_margins_collapsed_with_children.collapsed_through = collapsible_margins_in_children
        .collapsed_through &&
        pbm.padding_border_sums.block == Length::zero() &&
        (computed_block_size.is_definitely_zero() || computed_block_size.is_auto()) &&
        (computed_min_block_size.is_definitely_zero() || computed_min_block_size.is_auto());

    let block_size = containing_block_for_children.block_size.auto_is(|| {
        content_block_size.clamp_between_extremums(min_box_size.block, max_box_size.block)
    });

    if let Some(ref mut sequential_layout_state) = sequential_layout_state {
        // Now that we're done laying out our children, we can restore the
        // parent's containing block position information.
        sequential_layout_state
            .replace_containing_block_position_info(parent_containing_block_position_info.unwrap());

        // Account for padding and border. We also might have to readjust the
        // `bfc_relative_block_position` if it was different from the content size (i.e. was
        // non-`auto` and/or was affected by min/max block size).
        sequential_layout_state.advance_block_position(
            (block_size - content_block_size) + pbm.padding.block_end + pbm.border.block_end,
        );

        if !end_margin_can_collapse_with_children {
            sequential_layout_state.collapse_margins();
        }
        sequential_layout_state.adjoin_assign(&CollapsedMargin::new(margin.block_end));
    }

    let content_rect = Rect {
        start_corner: Vec2 {
            block: pbm.padding.block_start +
                pbm.border.block_start +
                clearance.unwrap_or_else(Length::zero),
            inline: pbm.padding.inline_start + pbm.border.inline_start + margin.inline_start,
        },
        size: Vec2 {
            block: block_size,
            inline: containing_block_for_children.inline_size,
        },
    };

    BoxFragment::new(
        base_fragment_info,
        style.clone(),
        flow_layout.fragments,
        content_rect,
        pbm.padding,
        pbm.border,
        margin,
        clearance,
        block_margins_collapsed_with_children,
    )
}

/// Lay out a normal flow non-replaced block that establishes and independent formatting
/// context in its containing formatting context.
///
/// - https://drafts.csswg.org/css2/visudet.html#blockwidth
/// - https://drafts.csswg.org/css2/visudet.html#normal-block
fn layout_in_flow_non_replaced_block_level_independent_formatting_context(
    layout_context: &LayoutContext,
    positioning_context: &mut PositioningContext,
    containing_block: &ContainingBlock,
    base_fragment_info: BaseFragmentInfo,
    style: &Arc<ComputedValues>,
    independent_formatting_context: &NonReplacedFormattingContext,
    mut sequential_layout_state: Option<&mut SequentialLayoutState>,
) -> BoxFragment {
    let ContainingBlockPaddingBorderAndMargin {
        containing_block: containing_block_for_children,
        pbm,
        min_box_size,
        max_box_size,
        margin,
    } = solve_containing_block_padding_border_and_margin_for_in_flow_box(containing_block, style);

    let layout = independent_formatting_context.layout(
        layout_context,
        positioning_context,
        &containing_block_for_children,
    );

    let content_block_size = layout.content_block_size;
    let block_size = containing_block_for_children.block_size.auto_is(|| {
        content_block_size.clamp_between_extremums(min_box_size.block, max_box_size.block)
    });

    let mut clearance = None;
    if let Some(ref mut sequential_layout_state) = sequential_layout_state {
        clearance = sequential_layout_state.calculate_clearance_and_adjoin_margin(
            style,
            &CollapsedMargin::new(margin.block_start),
        );
        sequential_layout_state.collapse_margins();

        // Account for padding and border. We also might have to readjust the
        // `bfc_relative_block_position` if it was different from the content size (i.e. was
        // non-`auto` and/or was affected by min/max block size).
        sequential_layout_state.advance_block_position(
            (block_size - content_block_size) + pbm.padding.block_sum() + pbm.border.block_sum(),
        );

        sequential_layout_state.adjoin_assign(&CollapsedMargin::new(margin.block_end));
    }

    let content_rect = Rect {
        start_corner: Vec2 {
            block: pbm.padding.block_start +
                pbm.border.block_start +
                clearance.unwrap_or_else(Length::zero),
            inline: pbm.padding.inline_start + pbm.border.inline_start + margin.inline_start,
        },
        size: Vec2 {
            block: block_size,
            inline: containing_block_for_children.inline_size,
        },
    };

    let block_margins_collapsed_with_children = CollapsedBlockMargins::from_margin(&margin);
    BoxFragment::new(
        base_fragment_info,
        style.clone(),
        layout.fragments,
        content_rect,
        pbm.padding,
        pbm.border,
        margin,
        clearance,
        block_margins_collapsed_with_children,
    )
}

/// https://drafts.csswg.org/css2/visudet.html#block-replaced-width
/// https://drafts.csswg.org/css2/visudet.html#inline-replaced-width
/// https://drafts.csswg.org/css2/visudet.html#inline-replaced-height
fn layout_in_flow_replaced_block_level<'a>(
    containing_block: &ContainingBlock,
    base_fragment_info: BaseFragmentInfo,
    style: &Arc<ComputedValues>,
    replaced: &ReplacedContent,
    mut sequential_layout_state: Option<&mut SequentialLayoutState>,
) -> BoxFragment {
    let pbm = style.padding_border_margin(containing_block);
    let size = replaced.used_size_as_if_inline_element(containing_block, style, None, &pbm);

    let (margin_inline_start, margin_inline_end) =
        solve_inline_margins_for_in_flow_block_level(containing_block, &pbm, size.inline);
    let margin = Sides {
        inline_start: margin_inline_start,
        inline_end: margin_inline_end,
        block_start: pbm.margin.block_start.auto_is(Length::zero),
        block_end: pbm.margin.block_end.auto_is(Length::zero),
    };
    let fragments = replaced.make_fragments(style, size.clone());

    let mut clearance = None;
    if let Some(ref mut sequential_layout_state) = sequential_layout_state {
        clearance = sequential_layout_state.calculate_clearance_and_adjoin_margin(
            style,
            &CollapsedMargin::new(margin.block_start),
        );
        sequential_layout_state.collapse_margins();
        sequential_layout_state.advance_block_position(
            pbm.border.block_sum() +
                pbm.padding.block_sum() +
                size.block +
                clearance.unwrap_or_else(Length::zero),
        );
        sequential_layout_state.adjoin_assign(&CollapsedMargin::new(margin.block_end));
    };

    let content_rect = Rect {
        start_corner: Vec2 {
            block: pbm.padding.block_start +
                pbm.border.block_start +
                clearance.unwrap_or_else(Length::zero),
            inline: pbm.padding.inline_start + pbm.border.inline_start + margin.inline_start,
        },
        size,
    };
    let block_margins_collapsed_with_children = CollapsedBlockMargins::from_margin(&margin);

    BoxFragment::new(
        base_fragment_info,
        style.clone(),
        fragments,
        content_rect,
        pbm.padding,
        pbm.border,
        margin,
        clearance,
        block_margins_collapsed_with_children,
    )
}

struct ContainingBlockPaddingBorderAndMargin<'a> {
    containing_block: ContainingBlock<'a>,
    pbm: PaddingBorderMargin,
    min_box_size: Vec2<Length>,
    max_box_size: Vec2<Option<Length>>,
    margin: Sides<Length>,
}

/// Given the style for in in flow box and its containing block, determine the containing
/// block for its children and the margin it should use.
fn solve_containing_block_padding_border_and_margin_for_in_flow_box<'a>(
    containing_block: &ContainingBlock<'_>,
    style: &'a Arc<ComputedValues>,
) -> ContainingBlockPaddingBorderAndMargin<'a> {
    let pbm = style.padding_border_margin(containing_block);
    let box_size = style.content_box_size(containing_block, &pbm);
    let max_box_size = style.content_max_box_size(containing_block, &pbm);
    let min_box_size = style
        .content_min_box_size(containing_block, &pbm)
        .auto_is(Length::zero);

    let (mut inline_size, mut inline_margins) =
        if let Some(inline_size) = box_size.inline.non_auto() {
            (
                inline_size,
                solve_inline_margins_for_in_flow_block_level(containing_block, &pbm, inline_size),
            )
        } else {
            let margin_inline_start = pbm.margin.inline_start.auto_is(Length::zero);
            let margin_inline_end = pbm.margin.inline_end.auto_is(Length::zero);
            let inline_size = containing_block.inline_size -
                pbm.padding_border_sums.inline -
                margin_inline_start -
                margin_inline_end;
            (inline_size, (margin_inline_start, margin_inline_end))
        };

    // https://drafts.csswg.org/css2/visudet.html#min-max-widths
    if let Some(max_inline_size) = max_box_size.inline {
        if inline_size > max_inline_size {
            inline_size = max_inline_size;
            inline_margins =
                solve_inline_margins_for_in_flow_block_level(containing_block, &pbm, inline_size);
        }
    }

    if inline_size < min_box_size.inline {
        inline_size = min_box_size.inline;
        inline_margins =
            solve_inline_margins_for_in_flow_block_level(containing_block, &pbm, inline_size);
    }

    let margin = Sides {
        inline_start: inline_margins.0,
        inline_end: inline_margins.1,
        block_start: pbm.margin.block_start.auto_is(Length::zero),
        block_end: pbm.margin.block_end.auto_is(Length::zero),
    };

    // https://drafts.csswg.org/css2/visudet.html#min-max-heights
    let mut block_size = box_size.block;
    if let LengthOrAuto::LengthPercentage(ref mut block_size) = block_size {
        *block_size = block_size.clamp_between_extremums(min_box_size.block, max_box_size.block);
    }

    let containing_block_for_children = ContainingBlock {
        inline_size,
        block_size,
        style,
    };
    // https://drafts.csswg.org/css-writing-modes/#orthogonal-flows
    assert_eq!(
        containing_block.style.writing_mode, containing_block_for_children.style.writing_mode,
        "Mixed writing modes are not supported yet"
    );
    ContainingBlockPaddingBorderAndMargin {
        containing_block: containing_block_for_children,
        pbm,
        min_box_size,
        max_box_size,
        margin,
    }
}

fn solve_inline_margins_for_in_flow_block_level(
    containing_block: &ContainingBlock,
    pbm: &PaddingBorderMargin,
    inline_size: Length,
) -> (Length, Length) {
    let available = containing_block.inline_size - pbm.padding_border_sums.inline - inline_size;
    match (pbm.margin.inline_start, pbm.margin.inline_end) {
        (LengthOrAuto::Auto, LengthOrAuto::Auto) => (available / 2., available / 2.),
        (LengthOrAuto::Auto, LengthOrAuto::LengthPercentage(end)) => (available - end, end),
        (LengthOrAuto::LengthPercentage(start), _) => (start, available - start),
    }
}

/// State that we maintain when placing blocks.
///
/// In parallel mode, this placement is done after all child blocks are laid out. In
/// sequential mode, this is done right after each block is laid out.
struct PlacementState {
    next_in_flow_margin_collapses_with_parent_start_margin: bool,
    last_in_flow_margin_collapses_with_parent_end_margin: bool,
    start_margin: CollapsedMargin,
    current_margin: CollapsedMargin,
    current_block_direction_position: Length,
}

impl PlacementState {
    fn new(
        collapsible_with_parent_start_margin: CollapsibleWithParentStartMargin,
    ) -> PlacementState {
        PlacementState {
            next_in_flow_margin_collapses_with_parent_start_margin:
                collapsible_with_parent_start_margin.0,
            last_in_flow_margin_collapses_with_parent_end_margin: true,
            start_margin: CollapsedMargin::zero(),
            current_margin: CollapsedMargin::zero(),
            current_block_direction_position: Length::zero(),
        }
    }

    /// Place a single [Fragment] in a block level context using the state so far and
    /// information gathered from the [Fragment] itself.
    fn place_fragment(
        &mut self,
        fragment: &mut Fragment,
        sequential_layout_state: Option<&mut SequentialLayoutState>,
    ) {
        match fragment {
            Fragment::Box(fragment) => {
                let fragment_block_margins = &fragment.block_margins_collapsed_with_children;
                let mut fragment_block_size = fragment.padding.block_sum() +
                    fragment.border.block_sum() +
                    fragment.content_rect.size.block;
                // We use `last_in_flow_margin_collapses_with_parent_end_margin` to implement
                // this quote from https://drafts.csswg.org/css2/#collapsing-margins
                // > If the top and bottom margins of an element with clearance are adjoining,
                // > its margins collapse with the adjoining margins of following siblings but that
                // > resulting margin does not collapse with the bottom margin of the parent block.
                if let Some(clearance) = fragment.clearance {
                    fragment_block_size += clearance;
                    // Margins can't be adjoining if they are separated by clearance.
                    // Setting `next_in_flow_margin_collapses_with_parent_start_margin` to false
                    // prevents collapsing with the start margin of the parent, and will set
                    // `collapsed_through` to false, preventing the parent from collapsing through.
                    self.current_block_direction_position += self.current_margin.solve();
                    self.current_margin = CollapsedMargin::zero();
                    self.next_in_flow_margin_collapses_with_parent_start_margin = false;
                    if fragment_block_margins.collapsed_through {
                        self.last_in_flow_margin_collapses_with_parent_end_margin = false;
                    }
                } else if !fragment_block_margins.collapsed_through {
                    self.last_in_flow_margin_collapses_with_parent_end_margin = true;
                }

                if self.next_in_flow_margin_collapses_with_parent_start_margin {
                    debug_assert_eq!(self.current_margin.solve(), Length::zero());
                    self.start_margin
                        .adjoin_assign(&fragment_block_margins.start);
                    if fragment_block_margins.collapsed_through {
                        self.start_margin.adjoin_assign(&fragment_block_margins.end);
                        return;
                    }
                    self.next_in_flow_margin_collapses_with_parent_start_margin = false;
                } else {
                    self.current_margin
                        .adjoin_assign(&fragment_block_margins.start);
                }

                fragment.content_rect.start_corner.block +=
                    self.current_margin.solve() + self.current_block_direction_position;

                if fragment_block_margins.collapsed_through {
                    // `fragment_block_size` is typically zero when collapsing through,
                    // but we still need to consider it in case there is clearance.
                    self.current_block_direction_position += fragment_block_size;
                    self.current_margin
                        .adjoin_assign(&fragment_block_margins.end);
                } else {
                    self.current_block_direction_position +=
                        self.current_margin.solve() + fragment_block_size;
                    self.current_margin = fragment_block_margins.end;
                }
            },
            Fragment::AbsoluteOrFixedPositioned(fragment) => {
                let offset = Vec2 {
                    block: self.current_margin.solve() + self.current_block_direction_position,
                    inline: Length::new(0.),
                };
                fragment.borrow_mut().adjust_offsets(offset);
            },
            Fragment::Float(box_fragment) => {
                let sequential_layout_state = sequential_layout_state
                    .expect("Found float fragment without SequentialLayoutState");
                let block_offset_from_containing_block_top =
                    self.current_block_direction_position + self.current_margin.solve();
                sequential_layout_state.place_float_fragment(
                    box_fragment,
                    self.start_margin,
                    block_offset_from_containing_block_top,
                );
            },
            Fragment::Anonymous(_) => {},
            _ => unreachable!(),
        }
    }

    fn finish(mut self) -> (Length, CollapsedBlockMargins) {
        if !self.last_in_flow_margin_collapses_with_parent_end_margin {
            self.current_block_direction_position += self.current_margin.solve();
            self.current_margin = CollapsedMargin::zero();
        }
        (
            self.current_block_direction_position,
            CollapsedBlockMargins {
                collapsed_through: self.next_in_flow_margin_collapses_with_parent_start_margin,
                start: self.start_margin,
                end: self.current_margin,
            },
        )
    }
}
