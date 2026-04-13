//! This module is a partial implementation of the CSS Grid Level 1 specification
//! <https://www.w3.org/TR/css-grid-1>
use crate::geometry::{AbsoluteAxis, AbstractAxis, InBothAbsAxis};
use crate::geometry::{Line, Point, Rect, Size};
use crate::style::{AlignItems, AlignSelf, AvailableSpace, Overflow, Position};
use crate::tree::{Layout, LayoutInput, LayoutOutput, LayoutPartialTreeExt, NodeId, RunMode, SizingMode};
use crate::util::debug::debug_log;
use crate::util::sys::{f32_max, f32_min, GridTrackVec, Vec};
use crate::util::MaybeMath;
use crate::util::{MaybeResolve, ResolveOrZero};
use crate::{
    style_helpers::*, AlignContent, BoxGenerationMode, BoxSizing, CoreStyle, Direction, GridContainerStyle,
    GridAreaAxis, GridItemStyle, GridAxisKind, JustifyContent, LayoutGridContainer, SubgridAxisContext,
    SubgridContext,
};
use alignment::{align_and_position_item, align_tracks};
use explicit_grid::{compute_explicit_grid_size_in_axis, initialize_grid_tracks, AutoRepeatStrategy};
use implicit_grid::compute_grid_size_estimate;
use placement::place_grid_items;
use track_sizing::{
    determine_if_item_crosses_flexible_or_intrinsic_tracks, resolve_item_track_indexes, track_sizing_algorithm,
};
use types::{CellOccupancyMatrix, GridTrack, NamedLineResolver, TrackCounts};

#[cfg(feature = "detailed_layout_info")]
use types::{GridItem, GridTrackKind};

pub(crate) use types::{GridCoordinate, GridLine, OriginZeroLine};

mod alignment;
mod explicit_grid;
mod implicit_grid;
mod placement;
mod track_sizing;
mod types;
mod util;

fn track_sizes_for_item_span(tracks: &[GridTrack], placement_indexes: Line<u16>) -> (Vec<f32>, Vec<f32>) {
    let mut track_sizes = Vec::new();
    let mut gutter_sizes = Vec::new();
    for (index, track) in tracks[(placement_indexes.start as usize + 1)..(placement_indexes.end as usize)]
        .iter()
        .enumerate()
    {
        if index % 2 == 0 {
            track_sizes.push(track.base_size);
        } else {
            gutter_sizes.push(track.base_size);
        }
    }
    (track_sizes, gutter_sizes)
}

fn track_span_size(tracks: &[GridTrack], placement_indexes: Line<u16>) -> f32 {
    tracks[(placement_indexes.start as usize + 1)..(placement_indexes.end as usize)]
        .iter()
        .map(|track| track.base_size)
        .sum()
}

fn place_in_flow_grid_children<Tree: LayoutGridContainer>(tree: &mut Tree, node: NodeId) -> Vec<GridItem> {
    let style = tree.get_grid_container_style(node);
    let subgrid_context = tree.get_grid_container_subgrid_context(node).cloned();
    let direction = style.direction();

    let child_styles_iter = || tree.child_ids(node).map(|child_node: NodeId| tree.get_grid_child_style(child_node));
    let (col_auto_repetition_count, grid_template_col_count) = compute_explicit_grid_size_in_axis(
        &style,
        None,
        AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
        |val, basis| tree.calc(val, basis),
        subgrid_context.as_ref().and_then(|context| context.columns.as_ref()),
        AbsoluteAxis::Horizontal,
    );
    let (row_auto_repetition_count, grid_template_row_count) = compute_explicit_grid_size_in_axis(
        &style,
        None,
        AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
        |val, basis| tree.calc(val, basis),
        subgrid_context.as_ref().and_then(|context| context.rows.as_ref()),
        AbsoluteAxis::Vertical,
    );

    let mut name_resolver =
        NamedLineResolver::new(&style, col_auto_repetition_count, row_auto_repetition_count, subgrid_context.as_ref());

    let explicit_col_count = subgrid_context
        .as_ref()
        .and_then(|context| context.columns.as_ref())
        .map(|context| context.track_count)
        .unwrap_or_else(|| grid_template_col_count.max(name_resolver.area_column_count()));
    let explicit_row_count = subgrid_context
        .as_ref()
        .and_then(|context| context.rows.as_ref())
        .map(|context| context.track_count)
        .unwrap_or_else(|| grid_template_row_count.max(name_resolver.area_row_count()));

    name_resolver.set_explicit_column_count(explicit_col_count);
    name_resolver.set_explicit_row_count(explicit_row_count);

    let (est_col_counts, est_row_counts) =
        compute_grid_size_estimate(explicit_col_count, explicit_row_count, direction, child_styles_iter());
    let mut child_items = Vec::with_capacity(tree.child_count(node));
    let mut cell_occupancy_matrix = CellOccupancyMatrix::with_track_counts(est_col_counts, est_row_counts);
    let in_flow_children_iter = || {
        tree.child_ids(node)
            .enumerate()
            .map(|(index, child_node)| (index, child_node, tree.get_grid_child_style(child_node)))
            .filter(|(_, _, style)| {
                style.box_generation_mode() != BoxGenerationMode::None && style.position() != Position::Absolute
            })
    };
    place_grid_items(
        &mut cell_occupancy_matrix,
        &mut child_items,
        in_flow_children_iter,
        direction,
        style.grid_auto_flow(),
        style.align_items().unwrap_or(AlignItems::Stretch),
        style.justify_items().unwrap_or(AlignItems::Stretch),
        &name_resolver,
    );

    let final_col_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Horizontal);
    let final_row_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Vertical);
    resolve_item_track_indexes(&mut child_items, final_col_counts, final_row_counts);
    child_items
}

fn update_child_subgrid_contexts<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    name_resolver: &NamedLineResolver<Tree::CustomIdent>,
    direction: Direction,
    columns: &[GridTrack],
    rows: &[GridTrack],
    items: &[GridItem],
    export_used_inline_sizes: bool,
    export_used_block_sizes: bool,
) {
    for item in items {
        let child_style = tree.get_grid_child_style(item.node);
        let horizontal_is_subgrid = child_style.subgrid_axis_kind(AbsoluteAxis::Horizontal) == GridAxisKind::Subgrid;
        let vertical_is_subgrid = child_style.subgrid_axis_kind(AbsoluteAxis::Vertical) == GridAxisKind::Subgrid;
        drop(child_style);

        let mut child_subgrid_context = SubgridContext::default();

        if horizontal_is_subgrid {
            child_subgrid_context.columns = Some(SubgridAxisContext {
                track_count: item.column.span(),
                line_names: name_resolver.line_names_for_span(GridAreaAxis::Column, item.column),
                track_sizes: if export_used_inline_sizes {
                    track_sizes_for_item_span(columns, item.column_indexes).0
                } else {
                    Vec::new()
                },
                gutter_sizes: if export_used_inline_sizes {
                    track_sizes_for_item_span(columns, item.column_indexes).1
                } else {
                    Vec::new()
                },
                inherited_sizing_adjustment: if direction.is_rtl() {
                    Line {
                        start: item.subgrid_margin_adjustment.right,
                        end: item.subgrid_margin_adjustment.left,
                    }
                } else {
                    Line {
                        start: item.subgrid_margin_adjustment.left,
                        end: item.subgrid_margin_adjustment.right,
                    }
                },
                inherited_layout_margin_adjustment: if direction.is_rtl() {
                    Line {
                        start: item.subgrid_layout_margin_adjustment.right,
                        end: item.subgrid_layout_margin_adjustment.left,
                    }
                } else {
                    Line {
                        start: item.subgrid_layout_margin_adjustment.left,
                        end: item.subgrid_layout_margin_adjustment.right,
                    }
                },
            });
        }

        if vertical_is_subgrid {
            child_subgrid_context.rows = Some(SubgridAxisContext {
                track_count: item.row.span(),
                line_names: name_resolver.line_names_for_span(GridAreaAxis::Row, item.row),
                track_sizes: if export_used_block_sizes {
                    track_sizes_for_item_span(rows, item.row_indexes).0
                } else {
                    Vec::new()
                },
                gutter_sizes: if export_used_block_sizes {
                    track_sizes_for_item_span(rows, item.row_indexes).1
                } else {
                    Vec::new()
                },
                inherited_sizing_adjustment: Line {
                    start: item.subgrid_margin_adjustment.top,
                    end: item.subgrid_margin_adjustment.bottom,
                },
                inherited_layout_margin_adjustment: Line {
                    start: item.subgrid_layout_margin_adjustment.top,
                    end: item.subgrid_layout_margin_adjustment.bottom,
                },
            });
        }

        let child_subgrid_context = if child_subgrid_context.columns.is_some() || child_subgrid_context.rows.is_some() {
            Some(child_subgrid_context)
        } else {
            None
        };
        tree.set_grid_child_subgrid_context(item.node, child_subgrid_context);
    }
}

fn update_non_empty_subgrid_missing_edge_placeholders<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    items: &mut [GridItem],
) {
    fn child_contributes_edge_in_axis<Tree: LayoutGridContainer>(tree: &Tree, child: &GridItem, axis: AbstractAxis) -> bool {
        !(child.subgrids_axis(axis) && tree.child_count(child.node) == 0)
    }

    for item in items.iter_mut().filter(|item| item.row_axis_kind == GridAxisKind::Subgrid || item.column_axis_kind == GridAxisKind::Subgrid) {
        if tree.child_count(item.node) == 0 {
            continue;
        }
        let child_items = place_in_flow_grid_children(tree, item.node);

        let explicit_col_count = item.column.span();
        let explicit_row_count = item.row.span();

        if item.column_axis_kind == GridAxisKind::Subgrid && !child_items.is_empty() {
            item.subgrid_missing_edge_placeholders.left =
                !child_items
                    .iter()
                    .any(|child| child.column.start.0 <= 0 && child_contributes_edge_in_axis(tree, child, AbstractAxis::Inline));
            item.subgrid_missing_edge_placeholders.right =
                !child_items.iter().any(|child| {
                    child.column.end.0 >= explicit_col_count as i16
                        && child_contributes_edge_in_axis(tree, child, AbstractAxis::Inline)
                });
        }

        if item.row_axis_kind == GridAxisKind::Subgrid && !child_items.is_empty() {
            item.subgrid_missing_edge_placeholders.top =
                !child_items
                    .iter()
                    .any(|child| child.row.start.0 <= 0 && child_contributes_edge_in_axis(tree, child, AbstractAxis::Block));
            item.subgrid_missing_edge_placeholders.bottom =
                !child_items.iter().any(|child| {
                    child.row.end.0 >= explicit_row_count as i16
                        && child_contributes_edge_in_axis(tree, child, AbstractAxis::Block)
                });
        }
    }
}

fn apply_subgrid_item_margin_adjustments<S: crate::CheapCloneStr>(
    direction: Direction,
    content_box_inset: Rect<f32>,
    resolved_margin: Rect<f32>,
    subgrid_context: Option<&SubgridContext<S>>,
    resolved_gap: Size<f32>,
    items: &mut [GridItem],
) {
    // Project the parent-facing edge and gutter-delta space that Section 9 requires a
    // subgrid to expose through its descendants during sizing. The projected space is
    // stored as extra margin-like adjustment because the existing contribution code is
    // already expressed in terms of outer sizes.
    // Spec anchors:
    // https://drafts.csswg.org/css-grid-2/#subgrid-item-contribution
    // https://drafts.csswg.org/css-grid-2/#subgrid-margins
    fn apply_axis(
        axis: AbstractAxis,
        inherited_gutter_sizes: &[f32],
        current_gap: f32,
        edge_adjustment: Line<f32>,
        items: &mut [GridItem],
    ) {
        for item in items.iter_mut() {
            let placement = item.placement(axis);

            let start_adjustment = if placement.start.0 <= 0 {
                edge_adjustment.start
            } else {
                let parent_gutter = inherited_gutter_sizes.get((placement.start.0 - 1) as usize).copied().unwrap_or(0.0);
                (current_gap - parent_gutter) * 0.5
            };

            let end_adjustment = if placement.end.0 >= inherited_gutter_sizes.len() as i16 + 1 {
                edge_adjustment.end
            } else {
                let parent_gutter = inherited_gutter_sizes.get((placement.end.0 - 1) as usize).copied().unwrap_or(0.0);
                (current_gap - parent_gutter) * 0.5
            };

            match axis {
                AbstractAxis::Inline => {
                    item.subgrid_margin_adjustment.left += start_adjustment;
                    item.subgrid_margin_adjustment.right += end_adjustment;
                }
                AbstractAxis::Block => {
                    item.subgrid_margin_adjustment.top += start_adjustment;
                    item.subgrid_margin_adjustment.bottom += end_adjustment;
                }
            }
        }
    }

    fn apply_layout_margin_axis(
        axis: AbstractAxis,
        inherited_gutter_sizes: &[f32],
        current_gap: f32,
        explicit_line_end: i16,
        edge_margin: Line<f32>,
        items: &mut [GridItem],
    ) {
        for item in items.iter_mut() {
            let placement = item.placement(axis);

            let start_adjustment = if placement.start.0 <= 0 {
                edge_margin.start
            } else {
                let parent_gutter = inherited_gutter_sizes.get((placement.start.0 - 1) as usize).copied().unwrap_or(0.0);
                (current_gap - parent_gutter) * 0.5
            };

            let end_adjustment = if placement.end.0 >= explicit_line_end {
                edge_margin.end
            } else {
                let parent_gutter = inherited_gutter_sizes.get((placement.end.0 - 1) as usize).copied().unwrap_or(0.0);
                (current_gap - parent_gutter) * 0.5
            };

            match axis {
                AbstractAxis::Inline => {
                    item.subgrid_layout_margin_adjustment.left += start_adjustment;
                    item.subgrid_layout_margin_adjustment.right += end_adjustment;
                }
                AbstractAxis::Block => {
                    item.subgrid_layout_margin_adjustment.top += start_adjustment;
                    item.subgrid_layout_margin_adjustment.bottom += end_adjustment;
                }
            }
        }
    }

    if subgrid_context.and_then(|context| context.columns.as_ref()).is_some() {
        let inherited = subgrid_context.and_then(|context| context.columns.as_ref());
        let inherited_gutters = inherited.map(|context| context.gutter_sizes.as_slice()).unwrap_or(&[]);
        let inherited_sizing_adjustment = inherited
            .map(|context| context.inherited_sizing_adjustment)
            .unwrap_or(Line { start: 0.0, end: 0.0 });
        let inherited_layout_margin_adjustment = inherited
            .map(|context| context.inherited_layout_margin_adjustment)
            .unwrap_or(Line { start: 0.0, end: 0.0 });
        let inline_sizing_adjustment = if direction.is_rtl() {
            Line {
                start: inherited_sizing_adjustment.start + content_box_inset.right + resolved_margin.right,
                end: inherited_sizing_adjustment.end + content_box_inset.left + resolved_margin.left,
            }
        } else {
            Line {
                start: inherited_sizing_adjustment.start + content_box_inset.left + resolved_margin.left,
                end: inherited_sizing_adjustment.end + content_box_inset.right + resolved_margin.right,
            }
        };
        let inline_margin_adjustment = if direction.is_rtl() {
            Line {
                start: inherited_layout_margin_adjustment.start + resolved_margin.right,
                end: inherited_layout_margin_adjustment.end + resolved_margin.left,
            }
        } else {
            Line {
                start: inherited_layout_margin_adjustment.start + resolved_margin.left,
                end: inherited_layout_margin_adjustment.end + resolved_margin.right,
            }
        };
        apply_axis(
            AbstractAxis::Inline,
            inherited_gutters,
            resolved_gap.width,
            inline_sizing_adjustment,
            items,
        );
        apply_layout_margin_axis(
            AbstractAxis::Inline,
            inherited_gutters,
            resolved_gap.width,
            inherited_gutters.len() as i16 + 1,
            inline_margin_adjustment,
            items,
        );
    }

    if subgrid_context.and_then(|context| context.rows.as_ref()).is_some() {
        let inherited = subgrid_context.and_then(|context| context.rows.as_ref());
        let inherited_gutters = inherited.map(|context| context.gutter_sizes.as_slice()).unwrap_or(&[]);
        let inherited_sizing_adjustment = inherited
            .map(|context| context.inherited_sizing_adjustment)
            .unwrap_or(Line { start: 0.0, end: 0.0 });
        let inherited_layout_margin_adjustment = inherited
            .map(|context| context.inherited_layout_margin_adjustment)
            .unwrap_or(Line { start: 0.0, end: 0.0 });
        apply_axis(
            AbstractAxis::Block,
            inherited_gutters,
            resolved_gap.height,
            Line {
                start: inherited_sizing_adjustment.start + content_box_inset.top,
                end: inherited_sizing_adjustment.end + content_box_inset.bottom,
            },
            items,
        );
        apply_layout_margin_axis(
            AbstractAxis::Block,
            inherited_gutters,
            resolved_gap.height,
            inherited_gutters.len() as i16 + 1,
            Line {
                start: inherited_layout_margin_adjustment.start + resolved_margin.top,
                end: inherited_layout_margin_adjustment.end + resolved_margin.bottom,
            },
            items,
        );
    }
}

fn apply_subgrid_item_margin_adjustments_from_container<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    node: NodeId,
    parent_inline_size: f32,
    items: &mut [GridItem],
) {
    let style = tree.get_grid_container_style(node);
    let direction = style.direction();
    let percentage_basis = Some(parent_inline_size);
    let padding = style.padding().resolve_or_zero(percentage_basis, |val, basis| tree.calc(val, basis));
    let border = style.border().resolve_or_zero(percentage_basis, |val, basis| tree.calc(val, basis));
    let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
        Overflow::Scroll => style.scrollbar_width(),
        _ => 0.0,
    });
    let mut content_box_inset = padding + border;
    content_box_inset.bottom += scrollbar_gutter.y;
    match direction {
        Direction::Ltr => content_box_inset.right += scrollbar_gutter.x,
        Direction::Rtl => content_box_inset.left += scrollbar_gutter.x,
    }

    let resolved_margin = style
        .margin()
        .map(|margin| margin.resolve_or_zero(percentage_basis, |val, basis| tree.calc(val, basis)));
    let resolved_gap = style.gap().map(|gap| gap.resolve_or_zero(percentage_basis, |val, basis| tree.calc(val, basis)));
    let subgrid_context = tree.get_grid_container_subgrid_context(node).cloned();
    drop(style);

    apply_subgrid_item_margin_adjustments(
        direction,
        content_box_inset,
        resolved_margin,
        subgrid_context.as_ref(),
        resolved_gap,
        items,
    );
}

fn build_axis_sizing_items<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    axis: AbstractAxis,
    columns: &[GridTrack],
    rows: &[GridTrack],
    items: &[GridItem],
) -> Vec<GridItem> {
    fn translated_placement(
        parent: Line<OriginZeroLine>,
        child: Line<OriginZeroLine>,
    ) -> Line<OriginZeroLine> {
        Line {
            start: OriginZeroLine(parent.start.0 + child.start.0),
            end: OriginZeroLine(parent.start.0 + child.end.0),
        }
    }

    fn set_placement(item: &mut GridItem, axis: AbstractAxis, placement: Line<OriginZeroLine>) {
        match axis {
            AbstractAxis::Inline => item.column = placement,
            AbstractAxis::Block => item.row = placement,
        }
    }

    fn collect_from_item<Tree: LayoutGridContainer>(
        tree: &mut Tree,
        axis: AbstractAxis,
        columns: &[GridTrack],
        rows: &[GridTrack],
        item: &GridItem,
        output: &mut Vec<GridItem>,
    ) {
        if !item.subgrids_axis(axis) || tree.child_count(item.node) == 0 {
            output.push(item.clone());
            return;
        }

        let parent_inline_size = track_span_size(columns, item.column_indexes);
        let mut child_items = place_in_flow_grid_children(tree, item.node);
        if child_items.is_empty() {
            output.push(item.clone());
            return;
        }

        let subgrid_axis_span = item.span(axis) as i16;
        let has_baseline_participant = child_items.iter().any(|child| child.align_self == AlignSelf::Baseline);
        let needs_axis_local_hoist = child_items.iter().any(|child| {
            let placement = child.placement(axis);
            placement.start.0 > 0 || placement.end.0 < subgrid_axis_span
        });
        if has_baseline_participant || !needs_axis_local_hoist {
            output.push(item.clone());
            return;
        }

        apply_subgrid_item_margin_adjustments_from_container(tree, item.node, parent_inline_size, &mut child_items);
        update_non_empty_subgrid_missing_edge_placeholders(tree, &mut child_items);

        let other_axis = axis.other();
        let other_axis_tracks = match other_axis {
            AbstractAxis::Inline => columns,
            AbstractAxis::Block => rows,
        };
        let other_axis_parent_size = track_span_size(other_axis_tracks, item.placement_indexes(other_axis));

        for mut child in child_items {
            let child_axis_placement = child.placement(axis);
            set_placement(
                &mut child,
                axis,
                translated_placement(item.placement(axis), child_axis_placement),
            );

            if item.subgrids_axis(other_axis) {
                let child_other_axis_placement = child.placement(other_axis);
                set_placement(
                    &mut child,
                    other_axis,
                    translated_placement(item.placement(other_axis), child_other_axis_placement),
                );
                child.grid_area_size_override.set(other_axis, None);
            } else {
                set_placement(&mut child, other_axis, item.placement(other_axis));
                child.grid_area_size_override.set(other_axis, Some(other_axis_parent_size));
            }

            child.row_indexes = Line { start: 0, end: 0 };
            child.column_indexes = Line { start: 0, end: 0 };
            child.crosses_flexible_row = false;
            child.crosses_flexible_column = false;
            child.crosses_intrinsic_row = false;
            child.crosses_intrinsic_column = false;
            child.available_space_cache = None;
            child.min_content_contribution_cache = Size::NONE;
            child.max_content_contribution_cache = Size::NONE;
            child.minimum_contribution_cache = Size::NONE;
            child.grid_area_size_override.set(axis, None);

            collect_from_item(tree, axis, columns, rows, &child, output);
        }
    }

    let mut axis_sizing_items = Vec::new();
    for item in items {
        collect_from_item(tree, axis, columns, rows, item, &mut axis_sizing_items);
    }
    axis_sizing_items
}

/// Grid layout algorithm
/// This consists of a few phases:
///   - Resolving the explicit grid
///   - Placing items (which also resolves the implicit grid)
///   - Track (row/column) sizing
///   - Alignment & Final item placement
pub fn compute_grid_layout<Tree: LayoutGridContainer>(
    tree: &mut Tree,
    node: NodeId,
    inputs: LayoutInput,
) -> LayoutOutput {
    let LayoutInput { known_dimensions, parent_size, available_space, run_mode, .. } = inputs;

    let style = tree.get_grid_container_style(node);
    let subgrid_context = tree.get_grid_container_subgrid_context(node).cloned();
    let direction = style.direction();

    // 1. Compute "available grid space"
    // https://www.w3.org/TR/css-grid-1/#available-grid-space
    let aspect_ratio = style.aspect_ratio();
    let padding = style.padding().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
    let border = style.border().resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis));
    let padding_border = padding + border;
    let padding_border_size = padding_border.sum_axes();
    let box_sizing_adjustment =
        if style.box_sizing() == BoxSizing::ContentBox { padding_border_size } else { Size::ZERO };

    let min_size = style
        .min_size()
        .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
        .maybe_apply_aspect_ratio(aspect_ratio)
        .maybe_add(box_sizing_adjustment);
    let max_size = style
        .max_size()
        .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
        .maybe_apply_aspect_ratio(aspect_ratio)
        .maybe_add(box_sizing_adjustment);
    let preferred_size = if inputs.sizing_mode == SizingMode::InherentSize {
        style
            .size()
            .maybe_resolve(parent_size, |val, basis| tree.calc(val, basis))
            .maybe_apply_aspect_ratio(style.aspect_ratio())
            .maybe_add(box_sizing_adjustment)
    } else {
        Size::NONE
    };

    // Scrollbar gutters are reserved when the `overflow` property is set to `Overflow::Scroll`.
    // However, the axis are switched (transposed) because a node that scrolls vertically needs
    // *horizontal* space to be reserved for a scrollbar
    let scrollbar_gutter = style.overflow().transpose().map(|overflow| match overflow {
        Overflow::Scroll => style.scrollbar_width(),
        _ => 0.0,
    });
    let mut content_box_inset = padding_border;
    content_box_inset.bottom += scrollbar_gutter.y;

    match direction {
        Direction::Ltr => content_box_inset.right += scrollbar_gutter.x,
        Direction::Rtl => content_box_inset.left += scrollbar_gutter.x,
    };

    // CSS Grid Level 2 requires subgrid descendants in a subgridded axis to inherit the
    // subgrid container's used margin, border, padding, and scrollbar gutter as extra
    // margin-like space at the corresponding edges.
    // https://drafts.csswg.org/css-grid-2/#subgrid-item-contribution
    // https://drafts.csswg.org/css-grid-2/#subgrid-margins
    let resolved_margin = style.margin().map(|margin| {
        margin.resolve_or_zero(parent_size.width, |val, basis| tree.calc(val, basis))
    });
    let subgrid_edge_adjustment = content_box_inset;

    let align_content = style.align_content().unwrap_or(AlignContent::Stretch);
    let justify_content = style.justify_content().unwrap_or(JustifyContent::Stretch);
    let align_items = style.align_items();
    let justify_items = style.justify_items();

    // Note: we avoid accessing the grid rows/columns methods more than once as this can
    // cause an expensive-ish computation
    let grid_template_columns = style.grid_template_columns();
    let grid_template_rows = style.grid_template_rows();
    let grid_auto_columns = style.grid_auto_columns();
    let grid_auto_rows = style.grid_auto_rows();

    let constrained_available_space = known_dimensions
        .or(preferred_size)
        .map(|size| size.map(AvailableSpace::Definite))
        .unwrap_or(available_space)
        .maybe_clamp(min_size, max_size)
        .maybe_max(padding_border_size);

    let available_grid_space = Size {
        width: constrained_available_space
            .width
            .map_definite_value(|space| space - content_box_inset.horizontal_axis_sum()),
        height: constrained_available_space
            .height
            .map_definite_value(|space| space - content_box_inset.vertical_axis_sum()),
    };

    let outer_node_size =
        known_dimensions.or(preferred_size).maybe_clamp(min_size, max_size).maybe_max(padding_border_size);
    let mut inner_node_size = Size {
        width: outer_node_size.width.map(|space| space - content_box_inset.horizontal_axis_sum()),
        height: outer_node_size.height.map(|space| space - content_box_inset.vertical_axis_sum()),
    };
    let resolved_gap = style
        .gap()
        .map(|gap| gap.resolve_or_zero(inner_node_size.width, |val, basis| tree.calc(val, basis)));

    debug_log!("parent_size", dbg:parent_size);
    debug_log!("outer_node_size", dbg:outer_node_size);
    debug_log!("inner_node_size", dbg:inner_node_size);

    if let (RunMode::ComputeSize, Some(width), Some(height)) = (run_mode, outer_node_size.width, outer_node_size.height)
    {
        return LayoutOutput::from_outer_size(Size { width, height });
    }

    let get_child_styles_iter =
        |node| tree.child_ids(node).map(|child_node: NodeId| tree.get_grid_child_style(child_node));
    let child_styles_iter = get_child_styles_iter(node);

    // 2. Resolve the explicit grid

    // This is very similar to the inner_node_size except if the inner_node_size is not definite but the node
    // has a min- or max- size style then that will be used in it's place.
    let auto_fit_container_size = outer_node_size
        .or(max_size)
        .or(min_size)
        .maybe_clamp(min_size, max_size)
        .maybe_max(padding_border_size)
        .maybe_sub(content_box_inset.sum_axes());

    // If the grid container has a definite size or max size in the relevant axis:
    //   - then the number of repetitions is the largest possible positive integer that does not cause the grid to overflow the content
    //     box of its grid container.
    // Otherwise, if the grid container has a definite min size in the relevant axis:
    //   - then the number of repetitions is the smallest possible positive integer that fulfills that minimum requirement
    // Otherwise, the specified track list repeats only once.
    let auto_repeat_fit_strategy = outer_node_size.or(max_size).map(|val| match val {
        Some(_) => AutoRepeatStrategy::MaxRepetitionsThatDoNotOverflow,
        None => AutoRepeatStrategy::MinRepetitionsThatDoOverflow,
    });

    // Compute the number of rows and columns in the explicit grid *template*
    // (explicit tracks from grid_areas are computed separately below)
    let (col_auto_repetition_count, grid_template_col_count) = compute_explicit_grid_size_in_axis(
        &style,
        auto_fit_container_size.width,
        auto_repeat_fit_strategy.width,
        |val, basis| tree.calc(val, basis),
        subgrid_context.as_ref().and_then(|context| context.columns.as_ref()),
        AbsoluteAxis::Horizontal,
    );
    let (row_auto_repetition_count, grid_template_row_count) = compute_explicit_grid_size_in_axis(
        &style,
        auto_fit_container_size.height,
        auto_repeat_fit_strategy.height,
        |val, basis| tree.calc(val, basis),
        subgrid_context.as_ref().and_then(|context| context.rows.as_ref()),
        AbsoluteAxis::Vertical,
    );

    // type CustomIdent<'a> = <<Tree as LayoutPartialTree>::CoreContainerStyle<'_> as CoreStyle>::CustomIdent;
    let mut name_resolver =
        NamedLineResolver::new(&style, col_auto_repetition_count, row_auto_repetition_count, subgrid_context.as_ref());

    let explicit_col_count = subgrid_context
        .as_ref()
        .and_then(|context| context.columns.as_ref())
        .map(|context| context.track_count)
        .unwrap_or_else(|| grid_template_col_count.max(name_resolver.area_column_count()));
    let explicit_row_count = subgrid_context
        .as_ref()
        .and_then(|context| context.rows.as_ref())
        .map(|context| context.track_count)
        .unwrap_or_else(|| grid_template_row_count.max(name_resolver.area_row_count()));

    name_resolver.set_explicit_column_count(explicit_col_count);
    name_resolver.set_explicit_row_count(explicit_row_count);

    // 3. Implicit Grid: Estimate Track Counts
    // Estimate the number of rows and columns in the implicit grid (= the entire grid)
    // This is necessary as part of placement. Doing it early here is a perf optimisation to reduce allocations.
    let (est_col_counts, est_row_counts) =
        compute_grid_size_estimate(explicit_col_count, explicit_row_count, direction, child_styles_iter);

    // 4. Grid Item Placement
    // Match items (children) to a definite grid position (row start/end and column start/end position)
    let mut items = Vec::with_capacity(tree.child_count(node));
    let mut cell_occupancy_matrix = CellOccupancyMatrix::with_track_counts(est_col_counts, est_row_counts);
    let in_flow_children_iter = || {
        tree.child_ids(node)
            .enumerate()
            .map(|(index, child_node)| (index, child_node, tree.get_grid_child_style(child_node)))
            .filter(|(_, _, style)| {
                style.box_generation_mode() != BoxGenerationMode::None && style.position() != Position::Absolute
            })
    };
    place_grid_items(
        &mut cell_occupancy_matrix,
        &mut items,
        in_flow_children_iter,
        direction,
        style.grid_auto_flow(),
        align_items.unwrap_or(AlignItems::Stretch),
        justify_items.unwrap_or(AlignItems::Stretch),
        &name_resolver,
    );

    let mut child_subgrid_contexts = Vec::with_capacity(items.len());
    for item in &items {
        let child_style = tree.get_grid_child_style(item.node);
        let mut child_subgrid_context = SubgridContext::default();

        if child_style.subgrid_axis_kind(AbsoluteAxis::Horizontal) == GridAxisKind::Subgrid {
            child_subgrid_context.columns = Some(SubgridAxisContext {
                track_count: item.column.span(),
                line_names: name_resolver.line_names_for_span(GridAreaAxis::Column, item.column),
                track_sizes: Vec::new(),
                gutter_sizes: Vec::new(),
                inherited_sizing_adjustment: Line { start: 0.0, end: 0.0 },
                inherited_layout_margin_adjustment: Line { start: 0.0, end: 0.0 },
            });
        }

        if child_style.subgrid_axis_kind(AbsoluteAxis::Vertical) == GridAxisKind::Subgrid {
            child_subgrid_context.rows = Some(SubgridAxisContext {
                track_count: item.row.span(),
                line_names: name_resolver.line_names_for_span(GridAreaAxis::Row, item.row),
                track_sizes: Vec::new(),
                gutter_sizes: Vec::new(),
                inherited_sizing_adjustment: Line { start: 0.0, end: 0.0 },
                inherited_layout_margin_adjustment: Line { start: 0.0, end: 0.0 },
            });
        }

        let child_subgrid_context = if child_subgrid_context.columns.is_some() || child_subgrid_context.rows.is_some() {
            Some(child_subgrid_context)
        } else {
            None
        };
        child_subgrid_contexts.push((item.node, child_subgrid_context));
    }

    // Extract track counts from previous step (auto-placement can expand the number of tracks)
    let final_col_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Horizontal);
    let final_row_counts = *cell_occupancy_matrix.track_counts(AbsoluteAxis::Vertical);

    // 5. Initialize Tracks
    // Initialize (explicit and implicit) grid tracks (and gutters)
    // This resolves the min and max track sizing functions for all tracks and gutters
    let mut columns = GridTrackVec::new();
    let mut rows = GridTrackVec::new();
    let mut column_track_counts_for_init = final_col_counts;
    if direction.is_rtl() && final_col_counts.explicit <= 1 {
        column_track_counts_for_init.negative_implicit = final_col_counts.positive_implicit;
        column_track_counts_for_init.positive_implicit = final_col_counts.negative_implicit;
    }
    initialize_grid_tracks(
        &mut columns,
        column_track_counts_for_init,
        &style,
        subgrid_context.as_ref().and_then(|context| context.columns.as_ref()),
        AbsoluteAxis::Horizontal,
        |column_index| {
            let occupancy_index = if direction.is_rtl() {
                rtl_column_occupancy_index_for_initialization(column_index, final_col_counts)
            } else {
                column_index
            };
            cell_occupancy_matrix.column_is_occupied(occupancy_index)
        },
    );
    initialize_grid_tracks(
        &mut rows,
        final_row_counts,
        &style,
        subgrid_context.as_ref().and_then(|context| context.rows.as_ref()),
        AbsoluteAxis::Vertical,
        |row_index| cell_occupancy_matrix.row_is_occupied(row_index),
    );
    if direction.is_rtl() {
        reverse_non_gutter_tracks(&mut columns, final_col_counts);
    }

    drop(grid_template_rows);
    drop(grid_template_columns);
    drop(grid_auto_rows);
    drop(grid_auto_columns);
    drop(style);

    for (child_node, child_subgrid_context) in child_subgrid_contexts {
        tree.set_grid_child_subgrid_context(child_node, child_subgrid_context);
    }

    // 6. Track Sizing

    // Convert grid placements in origin-zero coordinates to indexes into the GridTrack (rows and columns) vectors
    // This computation is relatively trivial, but it requires the final number of negative (implicit) tracks in
    // each axis, and doing it up-front here means we don't have to keep repeating that calculation
    resolve_item_track_indexes(&mut items, final_col_counts, final_row_counts);
    apply_subgrid_item_margin_adjustments(
        direction,
        subgrid_edge_adjustment,
        resolved_margin,
        subgrid_context.as_ref(),
        resolved_gap,
        &mut items,
    );
    update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, false, false);
    update_non_empty_subgrid_missing_edge_placeholders(tree, &mut items);
    // For each item, and in each axis, determine whether the item crosses any flexible (fr) tracks
    // Record this as a boolean (per-axis) on each item for later use in the track-sizing algorithm
    determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut items, &columns, &rows);

    let mut inline_sizing_items = build_axis_sizing_items(tree, AbstractAxis::Inline, &columns, &rows, &items);
    resolve_item_track_indexes(&mut inline_sizing_items, final_col_counts, final_row_counts);
    determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut inline_sizing_items, &columns, &rows);

    // Determine if the grid has any baseline aligned items
    let has_baseline_aligned_item = inline_sizing_items.iter().any(|item| item.align_self == AlignSelf::Baseline);

    // Run track sizing algorithm for Inline axis
    track_sizing_algorithm(
        tree,
        AbstractAxis::Inline,
        min_size.get(AbstractAxis::Inline),
        max_size.get(AbstractAxis::Inline),
        justify_content,
        align_content,
        available_grid_space,
        inner_node_size,
        &mut columns,
        &mut rows,
        &mut inline_sizing_items,
        |track: &GridTrack, parent_size: Option<f32>, tree: &Tree| {
            track.max_track_sizing_function.definite_value(parent_size, |val, basis| tree.calc(val, basis))
        },
        has_baseline_aligned_item,
    );
    let initial_column_sum = columns.iter().map(|track| track.base_size).sum::<f32>();
    inner_node_size.width = inner_node_size.width.or_else(|| initial_column_sum.into());
    update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, true, false);

    items.iter_mut().for_each(|item| item.available_space_cache = None);

    let mut block_sizing_items = build_axis_sizing_items(tree, AbstractAxis::Block, &columns, &rows, &items);
    resolve_item_track_indexes(&mut block_sizing_items, final_col_counts, final_row_counts);
    determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut block_sizing_items, &columns, &rows);

    // Run track sizing algorithm for Block axis
    track_sizing_algorithm(
        tree,
        AbstractAxis::Block,
        min_size.get(AbstractAxis::Block),
        max_size.get(AbstractAxis::Block),
        align_content,
        justify_content,
        available_grid_space,
        inner_node_size,
        &mut rows,
        &mut columns,
        &mut block_sizing_items,
        |track: &GridTrack, _, _| Some(track.base_size),
        false, // TODO: Support baseline alignment in the vertical axis
    );
    update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, true, true);
    let initial_row_sum = rows.iter().map(|track| track.base_size).sum::<f32>();
    inner_node_size.height = inner_node_size.height.or_else(|| initial_row_sum.into());

    debug_log!("initial_column_sum", dbg:initial_column_sum);
    debug_log!(dbg: columns.iter().map(|track| track.base_size).collect::<Vec<_>>());
    debug_log!("initial_row_sum", dbg:initial_row_sum);
    debug_log!(dbg: rows.iter().map(|track| track.base_size).collect::<Vec<_>>());

    // 6. Compute container size
    let resolved_style_size = known_dimensions.or(preferred_size);
    let compute_container_boxes = |column_sum: f32, row_sum: f32| {
        let container_border_box = Size {
            width: resolved_style_size
                .get(AbstractAxis::Inline)
                .unwrap_or_else(|| column_sum + content_box_inset.horizontal_axis_sum())
                .maybe_clamp(min_size.width, max_size.width)
                .max(padding_border_size.width),
            height: resolved_style_size
                .get(AbstractAxis::Block)
                .unwrap_or_else(|| row_sum + content_box_inset.vertical_axis_sum())
                .maybe_clamp(min_size.height, max_size.height)
                .max(padding_border_size.height),
        };
        let container_content_box = Size {
            width: f32_max(0.0, container_border_box.width - content_box_inset.horizontal_axis_sum()),
            height: f32_max(0.0, container_border_box.height - content_box_inset.vertical_axis_sum()),
        };
        (container_border_box, container_content_box)
    };
    let (mut container_border_box, mut container_content_box) =
        compute_container_boxes(initial_column_sum, initial_row_sum);

    // 7. Resolve percentage track base sizes
    // In the case of an indefinitely sized container these resolve to zero during the "Initialise Tracks" step
    // and therefore need to be re-resolved here based on the content-sized content box of the container
    if !available_grid_space.width.is_definite() {
        for column in &mut columns {
            let min: Option<f32> = column
                .min_track_sizing_function
                .resolved_percentage_size(container_content_box.width, |val, basis| tree.calc(val, basis));
            let max: Option<f32> = column
                .max_track_sizing_function
                .resolved_percentage_size(container_content_box.width, |val, basis| tree.calc(val, basis));
            column.base_size = column.base_size.maybe_clamp(min, max);
        }
    }
    if !available_grid_space.height.is_definite() {
        for row in &mut rows {
            let min: Option<f32> = row
                .min_track_sizing_function
                .resolved_percentage_size(container_content_box.height, |val, basis| tree.calc(val, basis));
            let max: Option<f32> = row
                .max_track_sizing_function
                .resolved_percentage_size(container_content_box.height, |val, basis| tree.calc(val, basis));
            row.base_size = row.base_size.maybe_clamp(min, max);
        }
    }

    // Column sizing must be re-run (once) if:
    //   - The grid container's width was initially indefinite and there are any columns with percentage track sizing functions
    //   - Any grid item crossing an intrinsically sized track's min content contribution width has changed
    // TODO: Only rerun sizing for tracks that actually require it rather than for all tracks if any need it.
    let mut rerun_column_sizing;
    let mut intrinsic_column_contribution_changed = false;

    let has_percentage_column = columns.iter().any(|track| track.uses_percentage());
    let has_percentage_row = rows.iter().any(|track| track.uses_percentage());
    let parent_width_indefinite = !available_space.width.is_definite();
    let mut inline_sizing_items = build_axis_sizing_items(tree, AbstractAxis::Inline, &columns, &rows, &items);
    resolve_item_track_indexes(&mut inline_sizing_items, final_col_counts, final_row_counts);
    determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut inline_sizing_items, &columns, &rows);
    rerun_column_sizing = parent_width_indefinite && has_percentage_column;

    if !rerun_column_sizing {
        intrinsic_column_contribution_changed =
            inline_sizing_items
                .iter_mut()
                .filter(|item| item.crosses_intrinsic_column)
                .any(|item| {
                let available_space = item.available_space(
                    AbstractAxis::Inline,
                    &rows,
                    inner_node_size.height,
                    |track: &GridTrack, _| Some(track.base_size),
                );
                let grid_area_size = Size {
                    width: item.definite_grid_area_size_in_axis(
                        AbstractAxis::Inline,
                        &columns,
                        inner_node_size.width,
                        &|val, basis| tree.calc(val, basis),
                    ),
                    height: available_space.height,
                };
                let new_min_content_contribution = item.min_content_contribution(
                    AbstractAxis::Inline,
                    tree,
                    grid_area_size,
                    available_space,
                    inner_node_size,
                );

                let has_changed = Some(new_min_content_contribution) != item.min_content_contribution_cache.width;

                item.available_space_cache = Some(available_space);
                item.min_content_contribution_cache.width = Some(new_min_content_contribution);
                item.max_content_contribution_cache.width = None;
                item.minimum_contribution_cache.width = None;

                has_changed
            });
        rerun_column_sizing = intrinsic_column_contribution_changed;
    } else {
        // Clear intrinsic width caches
        inline_sizing_items.iter_mut().for_each(|item| {
            item.available_space_cache = None;
            item.min_content_contribution_cache.width = None;
            item.max_content_contribution_cache.width = None;
            item.minimum_contribution_cache.width = None;
        });
    }

    let mut intrinsic_row_contribution_changed = false;

    if rerun_column_sizing {
        // Re-run track sizing algorithm for Inline axis
        track_sizing_algorithm(
            tree,
            AbstractAxis::Inline,
            min_size.get(AbstractAxis::Inline),
            max_size.get(AbstractAxis::Inline),
            justify_content,
            align_content,
            available_grid_space,
            inner_node_size,
            &mut columns,
            &mut rows,
            &mut inline_sizing_items,
            |track: &GridTrack, _, _| Some(track.base_size),
            has_baseline_aligned_item,
        );
        update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, true, false);

        // Row sizing must be re-run (once) if:
        //   - The grid container's height was initially indefinite and there are any rows with percentage track sizing functions
        //   - Any grid item crossing an intrinsically sized track's min content contribution height has changed
        // TODO: Only rerun sizing for tracks that actually require it rather than for all tracks if any need it.
        let mut rerun_row_sizing;
        let mut block_sizing_items = build_axis_sizing_items(tree, AbstractAxis::Block, &columns, &rows, &items);
        resolve_item_track_indexes(&mut block_sizing_items, final_col_counts, final_row_counts);
        determine_if_item_crosses_flexible_or_intrinsic_tracks(&mut block_sizing_items, &columns, &rows);

        let parent_height_indefinite = !available_space.height.is_definite();
        rerun_row_sizing = parent_height_indefinite && has_percentage_row;

        if !rerun_row_sizing {
            intrinsic_row_contribution_changed =
                block_sizing_items
                    .iter_mut()
                    .filter(|item| item.crosses_intrinsic_row)
                    .any(|item| {
                    let available_space = item.available_space(
                        AbstractAxis::Block,
                        &columns,
                        inner_node_size.width,
                        |track: &GridTrack, _| Some(track.base_size),
                    );
                    let grid_area_size = Size {
                        width: available_space.width,
                        height: item.definite_grid_area_size_in_axis(
                            AbstractAxis::Block,
                            &rows,
                            inner_node_size.height,
                            &|val, basis| tree.calc(val, basis),
                        ),
                    };
                    let new_min_content_contribution = item.min_content_contribution(
                        AbstractAxis::Block,
                        tree,
                        grid_area_size,
                        available_space,
                        inner_node_size,
                    );

                    let has_changed = Some(new_min_content_contribution) != item.min_content_contribution_cache.height;

                    item.available_space_cache = Some(available_space);
                    item.min_content_contribution_cache.height = Some(new_min_content_contribution);
                    item.max_content_contribution_cache.height = None;
                    item.minimum_contribution_cache.height = None;

                    has_changed
                });
            rerun_row_sizing = intrinsic_row_contribution_changed;
        } else {
            block_sizing_items.iter_mut().for_each(|item| {
                // Clear intrinsic height caches
                item.available_space_cache = None;
                item.min_content_contribution_cache.height = None;
                item.max_content_contribution_cache.height = None;
                item.minimum_contribution_cache.height = None;
            });
        }

        if rerun_row_sizing {
            // Re-run track sizing algorithm for Block axis
            track_sizing_algorithm(
                tree,
                AbstractAxis::Block,
                min_size.get(AbstractAxis::Block),
                max_size.get(AbstractAxis::Block),
                align_content,
                justify_content,
                available_grid_space,
                inner_node_size,
                &mut rows,
                &mut columns,
                &mut block_sizing_items,
                |track: &GridTrack, _, _| Some(track.base_size),
                false, // TODO: Support baseline alignment in the vertical axis
            );
            update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, true, true);
        }
    }

    if (intrinsic_column_contribution_changed && !has_percentage_column)
        || (intrinsic_row_contribution_changed && !has_percentage_row)
    {
        let final_column_sum = columns.iter().map(|track| track.base_size).sum::<f32>();
        let final_row_sum = rows.iter().map(|track| track.base_size).sum::<f32>();

        if intrinsic_column_contribution_changed && !has_percentage_column {
            container_border_box.width = resolved_style_size
                .get(AbstractAxis::Inline)
                .unwrap_or_else(|| final_column_sum + content_box_inset.horizontal_axis_sum())
                .maybe_clamp(min_size.width, max_size.width)
                .max(padding_border_size.width);
            container_content_box.width =
                f32_max(0.0, container_border_box.width - content_box_inset.horizontal_axis_sum());
        }

        if intrinsic_row_contribution_changed && !has_percentage_row {
            container_border_box.height = resolved_style_size
                .get(AbstractAxis::Block)
                .unwrap_or_else(|| final_row_sum + content_box_inset.vertical_axis_sum())
                .maybe_clamp(min_size.height, max_size.height)
                .max(padding_border_size.height);
            container_content_box.height =
                f32_max(0.0, container_border_box.height - content_box_inset.vertical_axis_sum());
        }
    }

    // If only the container's size has been requested
    if run_mode == RunMode::ComputeSize {
        return LayoutOutput::from_outer_size(container_border_box);
    }

    // 8. Track Alignment

    // Align columns
    let inline_size_without_scrollbar = f32_max(container_border_box.width - padding_border_size.width, 0.0);
    let inline_scrollbar_gutter_for_alignment = f32_min(scrollbar_gutter.x, inline_size_without_scrollbar);
    align_tracks(
        container_content_box.get(AbstractAxis::Inline),
        Line {
            start: padding.left + if direction.is_rtl() { inline_scrollbar_gutter_for_alignment } else { 0.0 },
            end: padding.right + if direction.is_rtl() { 0.0 } else { inline_scrollbar_gutter_for_alignment },
        },
        Line { start: border.left, end: border.right },
        &mut columns,
        justify_content,
        direction.is_rtl(),
    );
    // Align rows
    align_tracks(
        container_content_box.get(AbstractAxis::Block),
        Line { start: padding.top, end: padding.bottom },
        Line { start: border.top, end: border.bottom },
        &mut rows,
        align_content,
        false,
    );

    update_child_subgrid_contexts(tree, &name_resolver, direction, &columns, &rows, &items, true, true);

    // 9. Size, Align, and Position Grid Items

    #[cfg_attr(not(feature = "content_size"), allow(unused_mut))]
    let mut item_content_size_contribution = Size::ZERO;

    // Sort items back into original order to allow them to be matched up with styles
    items.sort_by_key(|item| item.source_order);

    let container_alignment_styles = InBothAbsAxis { horizontal: justify_items, vertical: align_items };

    // Position in-flow children (stored in items vector)
    for (index, item) in items.iter_mut().enumerate() {
        let grid_area = Rect {
            top: rows[item.row_indexes.start as usize + 1].offset,
            bottom: rows[item.row_indexes.end as usize].offset,
            left: columns[item.column_indexes.start as usize + 1].offset,
            right: columns[item.column_indexes.end as usize].offset,
        };
        #[cfg_attr(not(feature = "content_size"), allow(unused_variables))]
        let (content_size_contribution, y_position, height) = align_and_position_item(
            tree,
            item.node,
            index as u32,
            grid_area,
            container_alignment_styles,
            item.baseline_shim,
            item.subgrid_margin_adjustment,
            item.subgrid_layout_margin_adjustment,
            direction,
        );
        item.y_position = y_position;
        item.height = height;

        #[cfg(feature = "content_size")]
        {
            item_content_size_contribution = item_content_size_contribution.f32_max(content_size_contribution);
        }
    }

    // Position hidden and absolutely positioned children
    let mut order = items.len() as u32;

    (0..tree.child_count(node)).for_each(|index| {
        let child = tree.get_child_id(node, index);
        let child_style = tree.get_grid_child_style(child);

        // Position hidden child
        if child_style.box_generation_mode() == BoxGenerationMode::None {
            drop(child_style);
            tree.set_unrounded_layout(child, &Layout::with_order(order));
            tree.perform_child_layout(
                child,
                Size::NONE,
                Size::NONE,
                Size::MAX_CONTENT,
                SizingMode::InherentSize,
                Line::FALSE,
            );
            order += 1;
            return;
        }

        // Position absolutely positioned child
        if child_style.position() == Position::Absolute {
            // Convert grid-col-{start/end} into Option's of indexes into the columns vector
            // The Option is None if the style property is Auto and an unresolvable Span
            let maybe_col_indexes = name_resolver
                .resolve_column_names(&child_style.grid_column())
                .into_origin_zero(final_col_counts.explicit)
                .resolve_absolutely_positioned_grid_tracks()
                .map(|maybe_grid_line| {
                    maybe_grid_line
                        .map(|line: OriginZeroLine| {
                            if direction.is_rtl() {
                                OriginZeroLine(final_col_counts.explicit as i16 - line.0)
                            } else {
                                line
                            }
                        })
                        .and_then(|line| line.try_into_track_vec_index(final_col_counts))
                });
            let maybe_col_indexes = if direction.is_rtl() {
                Line { start: maybe_col_indexes.end, end: maybe_col_indexes.start }
            } else {
                maybe_col_indexes
            };
            // Convert grid-row-{start/end} into Option's of indexes into the row vector
            // The Option is None if the style property is Auto and an unresolvable Span
            let maybe_row_indexes = name_resolver
                .resolve_row_names(&child_style.grid_row())
                .into_origin_zero(final_row_counts.explicit)
                .resolve_absolutely_positioned_grid_tracks()
                .map(|maybe_grid_line| {
                    maybe_grid_line.and_then(|line: OriginZeroLine| line.try_into_track_vec_index(final_row_counts))
                });

            let grid_area = Rect {
                top: maybe_row_indexes.start.map(|index| rows[index].offset).unwrap_or(border.top),
                bottom: maybe_row_indexes
                    .end
                    .map(|index| rows[index].offset)
                    .unwrap_or(container_border_box.height - border.bottom - scrollbar_gutter.y),
                left: maybe_col_indexes.start.map(|index| columns[index].offset).unwrap_or_else(|| {
                    if direction.is_rtl() {
                        border.left + scrollbar_gutter.x
                    } else {
                        border.left
                    }
                }),
                right: maybe_col_indexes.end.map(|index| columns[index].offset).unwrap_or_else(|| {
                    if direction.is_rtl() {
                        container_border_box.width - border.right
                    } else {
                        container_border_box.width - border.right - scrollbar_gutter.x
                    }
                }),
            };
            drop(child_style);

            // TODO: Baseline alignment support for absolutely positioned items (should check if is actually specified)
            #[cfg_attr(not(feature = "content_size"), allow(unused_variables))]
            let (content_size_contribution, _, _) =
                align_and_position_item(
                    tree,
                    child,
                    order,
                    grid_area,
                    container_alignment_styles,
                    0.0,
                    Rect::ZERO,
                    Rect::ZERO,
                    direction,
                );
            #[cfg(feature = "content_size")]
            {
                item_content_size_contribution = item_content_size_contribution.f32_max(content_size_contribution);
            }

            order += 1;
        }
    });

    // Set detailed grid information
    #[cfg(feature = "detailed_layout_info")]
    tree.set_detailed_grid_info(
        node,
        DetailedGridInfo {
            rows: DetailedGridTracksInfo::from_grid_tracks_and_track_count(final_row_counts, rows),
            columns: DetailedGridTracksInfo::from_grid_tracks_and_track_count(final_col_counts, columns),
            items: items.iter().map(DetailedGridItemsInfo::from_grid_item).collect(),
        },
    );

    // If there are not items then return just the container size (no baseline)
    if items.is_empty() {
        return LayoutOutput::from_outer_size(container_border_box);
    }

    // Determine the grid container baseline(s) (currently we only compute the first baseline)
    let grid_container_baseline: f32 = {
        // Sort items by row start position so that we can iterate items in groups which are in the same row
        items.sort_by_key(|item| item.row_indexes.start);

        // Get the row index of the first row containing items
        let first_row = items[0].row_indexes.start;

        // Create a slice of all of the items start in this row (taking advantage of the fact that we have just sorted the array)
        let first_row_items = &items[0..].split(|item| item.row_indexes.start != first_row).next().unwrap();

        // Check if any items in *this row* are baseline aligned
        let row_has_baseline_item = first_row_items.iter().any(|item| item.align_self == AlignSelf::Baseline);

        let item = if row_has_baseline_item {
            first_row_items.iter().find(|item| item.align_self == AlignSelf::Baseline).unwrap()
        } else {
            &first_row_items[0]
        };

        item.y_position + item.baseline.unwrap_or(item.height)
    };

    LayoutOutput::from_sizes_and_baselines(
        container_border_box,
        item_content_size_contribution,
        Point { x: None, y: Some(grid_container_baseline) },
    )
}

/// Reverses only non-gutter column tracks in-place while preserving line/gutter slots.
fn reverse_non_gutter_tracks(tracks: &mut [GridTrack], track_counts: TrackCounts) {
    // When the explicit grid has 0/1 tracks, visual RTL mirroring is entirely determined by implicit tracks.
    // Reverse all non-gutter tracks in that case.
    if track_counts.explicit <= 1 {
        const MIN_TRACK_VEC_LEN_TO_REVERSE_COLUMNS: usize = 5;
        if tracks.len() < MIN_TRACK_VEC_LEN_TO_REVERSE_COLUMNS {
            return;
        }
        let mut left = 1;
        let mut right = tracks.len() - 2;
        while left < right {
            tracks.swap(left, right);
            left += 2;
            right = right.saturating_sub(2);
        }
        return;
    }

    let explicit_track_count = track_counts.explicit as usize;
    if explicit_track_count < 2 {
        return;
    }

    let mut left = track_counts.negative_implicit as usize;
    let mut right = left + explicit_track_count - 1;
    while left < right {
        tracks.swap((2 * left) + 1, (2 * right) + 1);
        left += 1;
        right = right.saturating_sub(1);
    }
}

/// Maps initialized column indexes to occupancy-matrix indexes for auto-fit collapsing in RTL.
fn rtl_column_occupancy_index_for_initialization(column_index: usize, track_counts: TrackCounts) -> usize {
    if track_counts.explicit <= 1 {
        return track_counts.len() - column_index - 1;
    }

    let explicit_start = track_counts.negative_implicit as usize;
    let explicit_end = explicit_start + track_counts.explicit as usize;
    if (explicit_start..explicit_end).contains(&column_index) {
        explicit_start + (explicit_end - column_index - 1)
    } else {
        column_index
    }
}

/// Information from the computation of grid
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridInfo {
    /// <https://drafts.csswg.org/css-grid-1/#grid-row>
    pub rows: DetailedGridTracksInfo,
    /// <https://drafts.csswg.org/css-grid-1/#grid-column>
    pub columns: DetailedGridTracksInfo,
    /// <https://drafts.csswg.org/css-grid-1/#grid-items>
    pub items: Vec<DetailedGridItemsInfo>,
}

/// Information from the computation of grids tracks
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridTracksInfo {
    /// Number of leading implicit grid tracks
    pub negative_implicit_tracks: u16,
    /// Number of explicit grid tracks
    pub explicit_tracks: u16,
    /// Number of trailing implicit grid tracks
    pub positive_implicit_tracks: u16,

    /// Gutters between tracks
    pub gutters: Vec<f32>,
    /// The used size of the tracks
    pub sizes: Vec<f32>,
}

#[cfg(feature = "detailed_layout_info")]
impl DetailedGridTracksInfo {
    /// Get the base_size of [`GridTrack`] with a kind [`types::GridTrackKind`]
    #[inline(always)]
    fn grid_track_base_size_of_kind(grid_tracks: &[GridTrack], kind: GridTrackKind) -> Vec<f32> {
        grid_tracks
            .iter()
            .filter_map(|track| match track.kind == kind {
                true => Some(track.base_size),
                false => None,
            })
            .collect()
    }

    /// Get the sizes of the gutters
    fn gutters_from_grid_track_layout(grid_tracks: &[GridTrack]) -> Vec<f32> {
        DetailedGridTracksInfo::grid_track_base_size_of_kind(grid_tracks, GridTrackKind::Gutter)
    }

    /// Get the sizes of the tracks
    fn sizes_from_grid_track_layout(grid_tracks: &[GridTrack]) -> Vec<f32> {
        DetailedGridTracksInfo::grid_track_base_size_of_kind(grid_tracks, GridTrackKind::Track)
    }

    /// Construct DetailedGridTracksInfo from TrackCounts and GridTracks
    fn from_grid_tracks_and_track_count(track_count: TrackCounts, grid_tracks: Vec<GridTrack>) -> Self {
        DetailedGridTracksInfo {
            negative_implicit_tracks: track_count.negative_implicit,
            explicit_tracks: track_count.explicit,
            positive_implicit_tracks: track_count.positive_implicit,
            gutters: DetailedGridTracksInfo::gutters_from_grid_track_layout(&grid_tracks),
            sizes: DetailedGridTracksInfo::sizes_from_grid_track_layout(&grid_tracks),
        }
    }
}

/// Grid area information from the placement algorithm
///
/// The values is 1-indexed grid line numbers bounding the area.
/// This matches the Chrome and Firefox's format as of 2nd Jan 2024.
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "detailed_layout_info")]
pub struct DetailedGridItemsInfo {
    /// row-start with 1-indexed grid line numbers
    pub row_start: u16,
    /// row-end with 1-indexed grid line numbers
    pub row_end: u16,
    /// column-start with 1-indexed grid line numbers
    pub column_start: u16,
    /// column-end with 1-indexed grid line numbers
    pub column_end: u16,
}

/// Grid area information from the placement algorithm
#[cfg(feature = "detailed_layout_info")]
impl DetailedGridItemsInfo {
    /// Construct from GridItems
    #[inline(always)]
    fn from_grid_item(grid_item: &GridItem) -> Self {
        /// Conversion from the indexes of Vec<GridTrack> into 1-indexed grid line numbers. See [`GridItem::row_indexes`] or [`GridItem::column_indexes`]
        #[inline(always)]
        fn to_one_indexed_grid_line(grid_track_index: u16) -> u16 {
            grid_track_index / 2 + 1
        }

        DetailedGridItemsInfo {
            row_start: to_one_indexed_grid_line(grid_item.row_indexes.start),
            row_end: to_one_indexed_grid_line(grid_item.row_indexes.end),
            column_start: to_one_indexed_grid_line(grid_item.column_indexes.start),
            column_end: to_one_indexed_grid_line(grid_item.column_indexes.end),
        }
    }
}
