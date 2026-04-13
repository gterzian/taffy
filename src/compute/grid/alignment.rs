//! Alignment of tracks and final positioning of items
use super::types::GridTrack;
use crate::compute::common::alignment::{apply_alignment_fallback, compute_alignment_offset};
use crate::geometry::{InBothAbsAxis, Line, Point, Rect, Size};
use crate::style::{AlignContent, AlignItems, AlignSelf, AvailableSpace, CoreStyle, Dimension, GridItemStyle, Overflow, Position};
use crate::tree::{Layout, LayoutPartialTreeExt, NodeId, SizingMode};
use crate::util::sys::f32_max;
use crate::util::{MaybeMath, MaybeResolve, ResolveOrZero};

#[cfg(feature = "content_size")]
use crate::compute::common::content_size::compute_content_size_contribution;
use crate::{BoxSizing, Direction, GridAxisKind, LayoutGridContainer};
use crate::CompactLength;

fn preferred_size_constraint(size: Dimension) -> Option<AvailableSpace> {
    match size.tag() {
        CompactLength::MIN_CONTENT_TAG => Some(AvailableSpace::MinContent),
        CompactLength::MAX_CONTENT_TAG => Some(AvailableSpace::MaxContent),
        _ => None,
    }
}

fn ignore_subgrid_axis_constraints(
    mut size: Size<Option<f32>>,
    horizontal_axis_kind: GridAxisKind,
    vertical_axis_kind: GridAxisKind,
    subgrid_min_size: Size<Option<f32>>,
) -> Size<Option<f32>> {
    if horizontal_axis_kind == GridAxisKind::Subgrid {
        size.width = subgrid_min_size.width;
    }
    if vertical_axis_kind == GridAxisKind::Subgrid {
        size.height = subgrid_min_size.height;
    }

    size
}

/// Align the grid tracks within the grid according to the align-content (rows) or
/// justify-content (columns) property. This only does anything if the size of the
/// grid is not equal to the size of the grid container in the axis being aligned.
pub(super) fn align_tracks(
    grid_container_content_box_size: f32,
    padding: Line<f32>,
    border: Line<f32>,
    tracks: &mut [GridTrack],
    track_alignment_style: AlignContent,
    axis_is_reversed: bool,
) {
    let used_size: f32 = tracks.iter().map(|track| track.base_size).sum();
    let free_space = grid_container_content_box_size - used_size;
    let origin = padding.start + border.start;

    // Count the number of non-collapsed tracks (not counting gutters)
    let num_tracks = tracks.iter().skip(1).step_by(2).filter(|track| !track.is_collapsed).count();

    // Grid layout treats gaps as full tracks rather than applying them at alignment so we
    // simply pass zero here. Grid layout is never reversed.
    let gap = 0.0;
    let layout_is_reversed = false;
    let is_safe = false; // TODO: Implement safe alignment
    let track_alignment = apply_alignment_fallback(free_space, num_tracks, track_alignment_style, is_safe);
    let track_alignment = if axis_is_reversed { track_alignment.reversed() } else { track_alignment };

    // Compute offsets
    let mut total_offset = origin;
    let mut seen_non_collapsed_track = false;
    tracks.iter_mut().enumerate().for_each(|(i, track)| {
        // Odd tracks are gutters (but slices are zero-indexed, so odd tracks have even indices)
        let is_gutter = i % 2 == 0;
        let is_non_collapsed_track = !is_gutter && !track.is_collapsed;

        // Alignment offsets should be applied only to non-collapsed tracks.
        let is_first = is_non_collapsed_track && !seen_non_collapsed_track;

        let offset = if is_non_collapsed_track {
            compute_alignment_offset(free_space, num_tracks, gap, track_alignment, layout_is_reversed, is_first)
        } else {
            0.0
        };

        track.offset = total_offset + offset;
        total_offset = total_offset + offset + track.base_size;
        if is_non_collapsed_track {
            seen_non_collapsed_track = true;
        }
    });
}

/// Align and size a grid item into it's final position
pub(super) fn align_and_position_item(
    tree: &mut impl LayoutGridContainer,
    node: NodeId,
    order: u32,
    grid_area: Rect<f32>,
    container_alignment_styles: InBothAbsAxis<Option<AlignItems>>,
    baseline_shim: f32,
    subgrid_margin_adjustment: Rect<f32>,
    subgrid_layout_margin_adjustment: Rect<f32>,
    direction: Direction,
) -> (Size<f32>, f32, f32) {
    let grid_area_size = Size { width: grid_area.right - grid_area.left, height: grid_area.bottom - grid_area.top };

    let style = tree.get_grid_child_style(node);

    let overflow = style.overflow();
    let scrollbar_width = style.scrollbar_width();
    let aspect_ratio = style.aspect_ratio();
    let justify_self = style.justify_self();
    let align_self = style.align_self();
    let horizontal_axis_kind = style.subgrid_axis_kind(crate::geometry::AbsoluteAxis::Horizontal);
    let vertical_axis_kind = style.subgrid_axis_kind(crate::geometry::AbsoluteAxis::Vertical);

    let position = style.position();
    let inset_horizontal = style
        .inset()
        .horizontal_components()
        .map(|size| size.resolve_to_option(grid_area_size.width, |val, basis| tree.calc(val, basis)));
    let inset_vertical = style
        .inset()
        .vertical_components()
        .map(|size| size.resolve_to_option(grid_area_size.height, |val, basis| tree.calc(val, basis)));
    let padding =
        style.padding().map(|p| p.resolve_or_zero(Some(grid_area_size.width), |val, basis| tree.calc(val, basis)));
    let border =
        style.border().map(|p| p.resolve_or_zero(Some(grid_area_size.width), |val, basis| tree.calc(val, basis)));
    let padding_border_size = (padding + border).sum_axes();

    let box_sizing_adjustment =
        if style.box_sizing() == BoxSizing::ContentBox { padding_border_size } else { Size::ZERO };
    let preferred_inline_constraint = preferred_size_constraint(style.size().width);
    let preferred_block_constraint = preferred_size_constraint(style.size().height);

    let subgrid_min_size = padding_border_size.map(Some);

    let inherent_size = ignore_subgrid_axis_constraints(
        style
            .size()
            .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
            .maybe_apply_aspect_ratio(aspect_ratio)
            .maybe_add(box_sizing_adjustment),
        horizontal_axis_kind,
        vertical_axis_kind,
        Size { width: None, height: None },
    );
    let min_size = ignore_subgrid_axis_constraints(
        style
            .min_size()
            .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
            .maybe_add(box_sizing_adjustment)
            .or(subgrid_min_size)
            .maybe_max(padding_border_size)
            .maybe_apply_aspect_ratio(aspect_ratio),
        horizontal_axis_kind,
        vertical_axis_kind,
        subgrid_min_size,
    );
    let max_size = ignore_subgrid_axis_constraints(
        style
            .max_size()
            .maybe_resolve(grid_area_size, |val, basis| tree.calc(val, basis))
            .maybe_apply_aspect_ratio(aspect_ratio)
            .maybe_add(box_sizing_adjustment),
        horizontal_axis_kind,
        vertical_axis_kind,
        Size { width: None, height: None },
    );

    // Resolve default alignment styles if they are set on neither the parent or the node itself
    // Note: if the child has a preferred aspect ratio but neither width or height are set, then the width is stretched
    // and the then height is calculated from the width according the aspect ratio
    // See: https://www.w3.org/TR/css-grid-1/#grid-item-sizing
    let alignment_styles = InBothAbsAxis {
        horizontal: if horizontal_axis_kind == GridAxisKind::Subgrid {
            AlignSelf::Stretch
        } else {
            justify_self.or(container_alignment_styles.horizontal).unwrap_or_else(|| {
                if inherent_size.width.is_some() || preferred_inline_constraint.is_some() {
                    AlignSelf::Start
                } else {
                    AlignSelf::Stretch
                }
            })
        },
        vertical: if vertical_axis_kind == GridAxisKind::Subgrid {
            AlignSelf::Stretch
        } else {
            align_self.or(container_alignment_styles.vertical).unwrap_or_else(|| {
                if inherent_size.height.is_some() || preferred_block_constraint.is_some() || aspect_ratio.is_some() {
                    AlignSelf::Start
                } else {
                    AlignSelf::Stretch
                }
            })
        },
    };

    // Note: This is not a bug. It is part of the CSS spec that both horizontal and vertical margins
    // resolve against the WIDTH of the grid area.
    let authored_margin = style.margin().map(|margin| {
        margin.resolve_to_option(grid_area_size.width, |val, basis| tree.calc(val, basis))
    });
    let margin_for_size = Rect {
        left: authored_margin.left.map(|value| value + subgrid_margin_adjustment.left).or_else(|| {
            if subgrid_margin_adjustment.left != 0.0 {
                Some(subgrid_margin_adjustment.left)
            } else {
                None
            }
        }),
        right: authored_margin.right.map(|value| value + subgrid_margin_adjustment.right).or_else(|| {
            if subgrid_margin_adjustment.right != 0.0 {
                Some(subgrid_margin_adjustment.right)
            } else {
                None
            }
        }),
        top: authored_margin.top.map(|value| value + subgrid_margin_adjustment.top).or_else(|| {
            if subgrid_margin_adjustment.top != 0.0 {
                Some(subgrid_margin_adjustment.top)
            } else {
                None
            }
        }),
        bottom: authored_margin.bottom.map(|value| value + subgrid_margin_adjustment.bottom).or_else(|| {
            if subgrid_margin_adjustment.bottom != 0.0 {
                Some(subgrid_margin_adjustment.bottom)
            } else {
                None
            }
        }),
    };

    // Final placement still needs the subgrid's projected edge margins and gutter deltas,
    // otherwise descendant items keep the parent grid's shared gutters visually even when the
    // subgrid overrides them, such as `gap: 0` on a row subgrid.
    // Spec anchors:
    // https://drafts.csswg.org/css-grid-2/#subgrid-margins
    // https://drafts.csswg.org/css-grid-2/#subgrid-grid-alignment
    let margin_for_alignment = Rect {
        left: authored_margin.left.map(|value| value + subgrid_layout_margin_adjustment.left).or_else(|| {
            if subgrid_layout_margin_adjustment.left != 0.0 {
                Some(subgrid_layout_margin_adjustment.left)
            } else {
                None
            }
        }),
        right: authored_margin.right.map(|value| value + subgrid_layout_margin_adjustment.right).or_else(|| {
            if subgrid_layout_margin_adjustment.right != 0.0 {
                Some(subgrid_layout_margin_adjustment.right)
            } else {
                None
            }
        }),
        top: authored_margin.top.map(|value| value + subgrid_layout_margin_adjustment.top).or_else(|| {
            if subgrid_layout_margin_adjustment.top != 0.0 {
                Some(subgrid_layout_margin_adjustment.top)
            } else {
                None
            }
        }),
        bottom: authored_margin.bottom.map(|value| value + subgrid_layout_margin_adjustment.bottom).or_else(|| {
            if subgrid_layout_margin_adjustment.bottom != 0.0 {
                Some(subgrid_layout_margin_adjustment.bottom)
            } else {
                None
            }
        }),
    };

    let grid_area_minus_item_margins_size = Size {
        width: grid_area_size.width.maybe_sub(margin_for_size.left).maybe_sub(margin_for_size.right),
        height: grid_area_size.height.maybe_sub(margin_for_size.top).maybe_sub(margin_for_size.bottom) - baseline_shim,
    };

    // If node is absolutely positioned and width is not set explicitly, then deduce it
    // from left, right and container_content_box if both are set.
    let width = if horizontal_axis_kind == GridAxisKind::Subgrid && position != Position::Absolute {
        // https://drafts.csswg.org/css-grid-2/#subgrid-box-alignment
        Some(grid_area_minus_item_margins_size.width)
    } else {
        inherent_size.width.or_else(|| {
            // Apply width derived from both the left and right properties of an absolutely
            // positioned element being set
            if position == Position::Absolute {
                if let (Some(left), Some(right)) = (inset_horizontal.start, inset_horizontal.end) {
                    return Some(f32_max(grid_area_minus_item_margins_size.width - left - right, 0.0));
                }
            }

            // Apply width based on stretch alignment if:
            //  - Alignment style is "stretch"
            //  - The node is not absolutely positioned
            //  - The node does not have auto margins in this axis.
            if preferred_inline_constraint.is_none() &&
                authored_margin.left.is_some()
                && authored_margin.right.is_some()
                && alignment_styles.horizontal == AlignSelf::Stretch
                && position != Position::Absolute
            {
                return Some(grid_area_minus_item_margins_size.width);
            }

            None
        })
    };

    // Reapply aspect ratio after stretch and absolute position width adjustments
    let size_before_aspect_ratio = Size { width, height: inherent_size.height };
    let Size { mut width, mut height } = size_before_aspect_ratio.maybe_apply_aspect_ratio(aspect_ratio);
    if horizontal_axis_kind == GridAxisKind::Subgrid {
        width = size_before_aspect_ratio.width;
    }
    if vertical_axis_kind == GridAxisKind::Subgrid {
        height = size_before_aspect_ratio.height;
    }

    let height = if vertical_axis_kind == GridAxisKind::Subgrid && position != Position::Absolute {
        // https://drafts.csswg.org/css-grid-2/#subgrid-box-alignment
        Some(grid_area_minus_item_margins_size.height)
    } else {
        height.or_else(|| {
            if position == Position::Absolute {
                if let (Some(top), Some(bottom)) = (inset_vertical.start, inset_vertical.end) {
                    return Some(f32_max(grid_area_minus_item_margins_size.height - top - bottom, 0.0));
                }
            }

            // Apply height based on stretch alignment if:
            //  - Alignment style is "stretch"
            //  - The node is not absolutely positioned
            //  - The node does not have auto margins in this axis.
            if preferred_block_constraint.is_none() &&
                authored_margin.top.is_some()
                && authored_margin.bottom.is_some()
                && alignment_styles.vertical == AlignSelf::Stretch
                && position != Position::Absolute
            {
                return Some(grid_area_minus_item_margins_size.height);
            }

            None
        })
    };
    // Reapply aspect ratio after stretch and absolute position height adjustments
    let size_before_final_aspect_ratio = Size { width, height };
    let Size { mut width, mut height } = size_before_final_aspect_ratio.maybe_apply_aspect_ratio(aspect_ratio);
    if horizontal_axis_kind == GridAxisKind::Subgrid {
        width = size_before_final_aspect_ratio.width;
    }
    if vertical_axis_kind == GridAxisKind::Subgrid {
        height = size_before_final_aspect_ratio.height;
    }

    // Clamp size by min and max width/height
    let Size { width, height } = Size { width, height }.maybe_clamp(min_size, max_size);

    // Layout node
    drop(style);

    let mut size = Size { width, height };

    if let Some(preferred_constraint) = preferred_inline_constraint {
        let mut available_space = grid_area_minus_item_margins_size.map(AvailableSpace::Definite);
        available_space.width = preferred_constraint;
        size.width = Some(tree.measure_child_size(
            node,
            Size { width: None, height: size.height },
            grid_area_size.map(Option::Some),
            available_space,
            SizingMode::InherentSize,
            crate::geometry::AbsoluteAxis::Horizontal,
            Line::FALSE,
        ));
    }

    if let Some(preferred_constraint) = preferred_block_constraint {
        let mut available_space = grid_area_minus_item_margins_size.map(AvailableSpace::Definite);
        available_space.height = preferred_constraint;
        size.height = Some(tree.measure_child_size(
            node,
            Size { width: size.width, height: None },
            grid_area_size.map(Option::Some),
            available_space,
            SizingMode::InherentSize,
            crate::geometry::AbsoluteAxis::Vertical,
            Line::FALSE,
        ));
    }

    let size = if position == Position::Absolute && (size.width.is_none() || size.height.is_none()) {
        tree.measure_child_size_both(
            node,
            size,
            grid_area_size.map(Option::Some),
            grid_area_minus_item_margins_size.map(AvailableSpace::Definite),
            SizingMode::InherentSize,
            Line::FALSE,
        )
        .map(Some)
    } else {
        size
    };

    let layout_output = tree.perform_child_layout(
        node,
        size,
        grid_area_size.map(Option::Some),
        grid_area_minus_item_margins_size.map(AvailableSpace::Definite),
        SizingMode::InherentSize,
        Line::FALSE,
    );

    // Resolve final size
    let Size { width, height } = size.unwrap_or(layout_output.size).maybe_clamp(min_size, max_size);

    let (x, x_margin) = align_item_within_area(
        Line { start: grid_area.left, end: grid_area.right },
        if horizontal_axis_kind == GridAxisKind::Subgrid {
            AlignSelf::Stretch
        } else {
            justify_self.unwrap_or(alignment_styles.horizontal)
        },
        width,
        position,
        inset_horizontal,
        margin_for_alignment.horizontal_components(),
        0.0,
        direction,
    );
    let (y, y_margin) = align_item_within_area(
        Line { start: grid_area.top, end: grid_area.bottom },
        if vertical_axis_kind == GridAxisKind::Subgrid {
            AlignSelf::Stretch
        } else {
            align_self.unwrap_or(alignment_styles.vertical)
        },
        height,
        position,
        inset_vertical,
        margin_for_alignment.vertical_components(),
        baseline_shim,
        Direction::Ltr,
    );

    let scrollbar_size = Size {
        width: if overflow.y == Overflow::Scroll { scrollbar_width } else { 0.0 },
        height: if overflow.x == Overflow::Scroll { scrollbar_width } else { 0.0 },
    };

    let resolved_margin = Rect { left: x_margin.start, right: x_margin.end, top: y_margin.start, bottom: y_margin.end };

    tree.set_unrounded_layout(
        node,
        &Layout {
            order,
            location: Point { x, y },
            size: Size { width, height },
            #[cfg(feature = "content_size")]
            content_size: layout_output.content_size,
            scrollbar_size,
            padding,
            border,
            margin: resolved_margin,
        },
    );

    #[cfg(feature = "content_size")]
    let contribution = compute_content_size_contribution(
        Point { x: x - grid_area.left, y: y - grid_area.top },
        Size { width, height },
        layout_output.content_size,
        overflow,
    );
    #[cfg(not(feature = "content_size"))]
    let contribution = Size::ZERO;

    (contribution, y, height)
}

/// Align and size a grid item along a single axis
#[allow(clippy::too_many_arguments)]
pub(super) fn align_item_within_area(
    grid_area: Line<f32>,
    alignment_style: AlignSelf,
    resolved_size: f32,
    position: Position,
    inset: Line<Option<f32>>,
    margin: Line<Option<f32>>,
    baseline_shim: f32,
    direction: Direction,
) -> (f32, Line<f32>) {
    // Calculate grid area dimension in the axis
    let non_auto_margin = Line { start: margin.start.unwrap_or(0.0) + baseline_shim, end: margin.end.unwrap_or(0.0) };
    let grid_area_size = f32_max(grid_area.end - grid_area.start, 0.0);
    let free_space = f32_max(grid_area_size - resolved_size - non_auto_margin.sum(), 0.0);

    // Expand auto margins to fill available space
    let auto_margin_count = margin.start.is_none() as u8 + margin.end.is_none() as u8;
    let auto_margin_size = if auto_margin_count > 0 { free_space / auto_margin_count as f32 } else { 0.0 };
    let resolved_margin = Line {
        start: margin.start.unwrap_or(auto_margin_size) + baseline_shim,
        end: margin.end.unwrap_or(auto_margin_size),
    };

    // Compute offset in the axis
    let alignment_based_offset = match alignment_style {
        // TODO: Add support for baseline alignment. For now we treat it as "start".
        AlignSelf::Start | AlignSelf::FlexStart | AlignSelf::Baseline | AlignSelf::Stretch => {
            if direction.is_rtl() {
                grid_area_size - resolved_size - resolved_margin.end
            } else {
                resolved_margin.start
            }
        }
        AlignSelf::End | AlignSelf::FlexEnd => {
            if direction.is_rtl() {
                resolved_margin.start
            } else {
                grid_area_size - resolved_size - resolved_margin.end
            }
        }
        AlignSelf::Center => (grid_area_size - resolved_size + resolved_margin.start - resolved_margin.end) / 2.0,
    };

    let offset_within_area = if position == Position::Absolute {
        match (inset.start, inset.end) {
            (Some(start), Some(end)) => {
                if direction.is_rtl() {
                    grid_area_size - end - resolved_size - non_auto_margin.end
                } else {
                    start + non_auto_margin.start
                }
            }
            (Some(start), None) => start + non_auto_margin.start,
            (None, Some(end)) => grid_area_size - end - resolved_size - non_auto_margin.end,
            (None, None) => alignment_based_offset,
        }
    } else {
        alignment_based_offset
    };

    let mut start = grid_area.start + offset_within_area;
    if position == Position::Relative {
        let relative_inset = if direction.is_rtl() {
            inset.end.map(|pos| -pos).or(inset.start)
        } else {
            inset.start.or(inset.end.map(|pos| -pos))
        };
        start += relative_inset.unwrap_or(0.0);
    }

    (start, resolved_margin)
}
