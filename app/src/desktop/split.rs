use std::num::NonZeroU32;

use lune_terminal::{PixelPosition, PixelSize};
use winit::dpi::PhysicalPosition;

use crate::PixelRect;

use super::PaneId;

const MIN_RATIO: f32 = 0.15;
const MAX_RATIO: f32 = 0.85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Side-by-side panes separated by a vertical divider.
    Columns,
    /// Stacked panes separated by a horizontal divider.
    Rows,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub(super) const fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Columns,
            Self::Up | Self::Down => Axis::Rows,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SplitPath {
    bits: u64,
    depth: u8,
}

impl SplitPath {
    const ROOT: Self = Self { bits: 0, depth: 0 };

    fn push(self, second: bool) -> Option<Self> {
        if self.depth >= 64 {
            return None;
        }
        let bit = u64::from(second) << self.depth;
        Some(Self {
            bits: self.bits | bit,
            depth: self.depth + 1,
        })
    }

    fn is_second(self, level: u8) -> bool {
        (self.bits >> level) & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PaneRect {
    pub(super) id: PaneId,
    pub(super) rect: PixelRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Divider {
    pub(super) path: SplitPath,
    pub(super) axis: Axis,
    pub(super) rect: PixelRect,
    pub(super) parent_start: u32,
    pub(super) parent_span: u32,
}

impl Divider {
    pub(super) fn ratio_for(&self, position: PhysicalPosition<f64>) -> f32 {
        let coord = match self.axis {
            Axis::Columns => position.x,
            Axis::Rows => position.y,
        };
        let span = f64::from(self.parent_span.max(1));
        let ratio = ((coord - f64::from(self.parent_start)) / span) as f32;
        ratio.clamp(MIN_RATIO, MAX_RATIO)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SplitLayout {
    pub(super) panes: Vec<PaneRect>,
    pub(super) dividers: Vec<Divider>,
}

impl SplitLayout {
    pub(super) fn rect_of(&self, id: PaneId) -> Option<PixelRect> {
        self.panes
            .iter()
            .find(|pane| pane.id == id)
            .map(|pane| pane.rect)
    }

    pub(super) fn pane_at(&self, position: PhysicalPosition<f64>) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|pane| rect_contains(pane.rect, position))
            .map(|pane| pane.id)
    }

    pub(super) fn divider_at(
        &self,
        position: PhysicalPosition<f64>,
        margin: f64,
    ) -> Option<Divider> {
        self.dividers
            .iter()
            .copied()
            .find(|divider| match divider.axis {
                Axis::Columns => {
                    let left = f64::from(divider.rect.origin.x) - margin;
                    let right =
                        f64::from(divider.rect.origin.x + divider.rect.size.width.get()) + margin;
                    let top = f64::from(divider.rect.origin.y);
                    let bottom = f64::from(divider.rect.origin.y + divider.rect.size.height.get());
                    position.x >= left
                        && position.x < right
                        && position.y >= top
                        && position.y < bottom
                }
                Axis::Rows => {
                    let left = f64::from(divider.rect.origin.x);
                    let right = f64::from(divider.rect.origin.x + divider.rect.size.width.get());
                    let top = f64::from(divider.rect.origin.y) - margin;
                    let bottom =
                        f64::from(divider.rect.origin.y + divider.rect.size.height.get()) + margin;
                    position.x >= left
                        && position.x < right
                        && position.y >= top
                        && position.y < bottom
                }
            })
    }
}

fn rect_contains(rect: PixelRect, position: PhysicalPosition<f64>) -> bool {
    let x = f64::from(rect.origin.x);
    let y = f64::from(rect.origin.y);
    position.x >= x
        && position.x < x + f64::from(rect.size.width.get())
        && position.y >= y
        && position.y < y + f64::from(rect.size.height.get())
}

#[derive(Clone, Debug)]
enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct SplitTree {
    root: Node,
}

impl SplitTree {
    pub(super) fn new(root: PaneId) -> Self {
        Self {
            root: Node::Leaf(root),
        }
    }

    pub(super) fn contains(&self, id: PaneId) -> bool {
        self.root.contains(id)
    }

    pub(super) fn split(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        self.root.split(target, axis, new_id)
    }

    /// Removes `target` from the tree, collapsing its parent split into its sibling.
    /// Returns the preferred surviving pane to focus, or `None` if `target` was the root.
    pub(super) fn remove(&mut self, target: PaneId) -> Option<PaneId> {
        match &self.root {
            Node::Leaf(id) if *id == target => None,
            _ => {
                let (next_root, next_focus) =
                    remove_node(std::mem::replace(&mut self.root, Node::Leaf(0)), target);
                self.root = next_root;
                next_focus
            }
        }
    }

    pub(super) fn layout(
        &self,
        area: PixelRect,
        divider: u32,
        zoomed: Option<PaneId>,
    ) -> SplitLayout {
        if let Some(id) = zoomed
            && self.contains(id)
        {
            return SplitLayout {
                panes: vec![PaneRect { id, rect: area }],
                dividers: Vec::new(),
            };
        }
        let mut layout = SplitLayout::default();
        self.root
            .layout(area, divider.max(1), SplitPath::ROOT, &mut layout);
        layout
    }

    pub(super) fn focus_neighbor(
        current: PaneId,
        direction: Direction,
        layout: &SplitLayout,
    ) -> Option<PaneId> {
        let origin = layout.rect_of(current)?;
        let (ox0, oy0, ox1, oy1) = rect_bounds(origin);
        let ocx = (ox0 + ox1) / 2;
        let ocy = (oy0 + oy1) / 2;

        let mut best: Option<(PaneId, (u32, u32, u32))> = None;
        for pane in &layout.panes {
            if pane.id == current {
                continue;
            }
            let (x0, y0, x1, y1) = rect_bounds(pane.rect);
            let (primary_gap, overlap, center_dist) = match direction {
                Direction::Left => {
                    if x1 > ox0 {
                        continue;
                    }
                    let overlap = y1.min(oy1).saturating_sub(y0.max(oy0));
                    if overlap == 0 {
                        continue;
                    }
                    (ox0 - x1, overlap, ocy.abs_diff((y0 + y1) / 2))
                }
                Direction::Right => {
                    if x0 < ox1 {
                        continue;
                    }
                    let overlap = y1.min(oy1).saturating_sub(y0.max(oy0));
                    if overlap == 0 {
                        continue;
                    }
                    (x0 - ox1, overlap, ocy.abs_diff((y0 + y1) / 2))
                }
                Direction::Up => {
                    if y1 > oy0 {
                        continue;
                    }
                    let overlap = x1.min(ox1).saturating_sub(x0.max(ox0));
                    if overlap == 0 {
                        continue;
                    }
                    (oy0 - y1, overlap, ocx.abs_diff((x0 + x1) / 2))
                }
                Direction::Down => {
                    if y0 < oy1 {
                        continue;
                    }
                    let overlap = x1.min(ox1).saturating_sub(x0.max(ox0));
                    if overlap == 0 {
                        continue;
                    }
                    (y0 - oy1, overlap, ocx.abs_diff((x0 + x1) / 2))
                }
            };
            // Prefer smallest gap, then center alignment, then larger overlap.
            let score = (primary_gap, center_dist, u32::MAX - overlap);
            if best.is_none_or(|(_, best_score)| score < best_score) {
                best = Some((pane.id, score));
            }
        }
        best.map(|(id, _)| id)
    }

    pub(super) fn resize(&mut self, target: PaneId, direction: Direction, delta: f32) -> bool {
        let axis = direction.axis();
        let signed = match direction {
            Direction::Left | Direction::Up => -delta,
            Direction::Right | Direction::Down => delta,
        };
        self.root.resize(target, axis, signed)
    }

    pub(super) fn set_ratio(&mut self, path: SplitPath, ratio: f32) -> bool {
        let clamped = ratio.clamp(MIN_RATIO, MAX_RATIO);
        self.root.set_ratio(path, 0, clamped)
    }

    pub(super) fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second || !self.contains(first) || !self.contains(second) {
            return false;
        }
        self.root.swap_panes(first, second);
        true
    }
}

impl Node {
    fn contains(&self, target: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn first_leaf(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    fn last_leaf(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { second, .. } => second.last_leaf(),
        }
    }

    fn split(&mut self, target: PaneId, axis: Axis, new_id: PaneId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                let existing = *id;
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(existing)),
                    second: Box::new(Self::Leaf(new_id)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, axis, new_id) || second.split(target, axis, new_id)
            }
        }
    }

    fn swap_panes(&mut self, a: PaneId, b: PaneId) {
        match self {
            Self::Leaf(id) => {
                if *id == a {
                    *id = b;
                } else if *id == b {
                    *id = a;
                }
            }
            Self::Split { first, second, .. } => {
                first.swap_panes(a, b);
                second.swap_panes(a, b);
            }
        }
    }

    fn layout(&self, area: PixelRect, divider: u32, path: SplitPath, out: &mut SplitLayout) {
        match self {
            Self::Leaf(id) => out.panes.push(PaneRect {
                id: *id,
                rect: area,
            }),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match *axis {
                Axis::Columns => {
                    let width = area.size.width.get();
                    if width <= divider + 2 {
                        first.layout(area, divider, path, out);
                        return;
                    }
                    let available = width - divider;
                    let first_w = ((available as f32) * *ratio)
                        .round()
                        .clamp(1.0, (available - 1) as f32)
                        as u32;
                    let second_w = available - first_w;
                    let first_rect = PixelRect {
                        origin: area.origin,
                        size: PixelSize {
                            width: NonZeroU32::new(first_w).unwrap(),
                            height: area.size.height,
                        },
                    };
                    let divider_rect = PixelRect {
                        origin: PixelPosition {
                            x: area.origin.x + first_w,
                            y: area.origin.y,
                        },
                        size: PixelSize {
                            width: NonZeroU32::new(divider).unwrap(),
                            height: area.size.height,
                        },
                    };
                    let second_rect = PixelRect {
                        origin: PixelPosition {
                            x: area.origin.x + first_w + divider,
                            y: area.origin.y,
                        },
                        size: PixelSize {
                            width: NonZeroU32::new(second_w).unwrap(),
                            height: area.size.height,
                        },
                    };
                    out.dividers.push(Divider {
                        path,
                        axis: Axis::Columns,
                        rect: divider_rect,
                        parent_start: area.origin.x,
                        parent_span: available,
                    });
                    if let Some(left_path) = path.push(false) {
                        first.layout(first_rect, divider, left_path, out);
                    }
                    if let Some(right_path) = path.push(true) {
                        second.layout(second_rect, divider, right_path, out);
                    }
                }
                Axis::Rows => {
                    let height = area.size.height.get();
                    if height <= divider + 2 {
                        first.layout(area, divider, path, out);
                        return;
                    }
                    let available = height - divider;
                    let first_h = ((available as f32) * *ratio)
                        .round()
                        .clamp(1.0, (available - 1) as f32)
                        as u32;
                    let second_h = available - first_h;
                    let first_rect = PixelRect {
                        origin: area.origin,
                        size: PixelSize {
                            width: area.size.width,
                            height: NonZeroU32::new(first_h).unwrap(),
                        },
                    };
                    let divider_rect = PixelRect {
                        origin: PixelPosition {
                            x: area.origin.x,
                            y: area.origin.y + first_h,
                        },
                        size: PixelSize {
                            width: area.size.width,
                            height: NonZeroU32::new(divider).unwrap(),
                        },
                    };
                    let second_rect = PixelRect {
                        origin: PixelPosition {
                            x: area.origin.x,
                            y: area.origin.y + first_h + divider,
                        },
                        size: PixelSize {
                            width: area.size.width,
                            height: NonZeroU32::new(second_h).unwrap(),
                        },
                    };
                    out.dividers.push(Divider {
                        path,
                        axis: Axis::Rows,
                        rect: divider_rect,
                        parent_start: area.origin.y,
                        parent_span: available,
                    });
                    if let Some(top_path) = path.push(false) {
                        first.layout(first_rect, divider, top_path, out);
                    }
                    if let Some(bottom_path) = path.push(true) {
                        second.layout(second_rect, divider, bottom_path, out);
                    }
                }
            },
        }
    }

    fn resize(&mut self, target: PaneId, axis: Axis, delta: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                axis: node_axis,
                ratio,
                first,
                second,
            } => {
                if first.contains(target) {
                    if first.resize(target, axis, delta) {
                        return true;
                    }
                    if *node_axis == axis {
                        let next = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
                        let changed = (next - *ratio).abs() > f32::EPSILON;
                        *ratio = next;
                        return changed;
                    }
                } else if second.contains(target) {
                    if second.resize(target, axis, delta) {
                        return true;
                    }
                    if *node_axis == axis {
                        let next = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
                        let changed = (next - *ratio).abs() > f32::EPSILON;
                        *ratio = next;
                        return changed;
                    }
                }
                false
            }
        }
    }

    fn set_ratio(&mut self, path: SplitPath, level: u8, next_ratio: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if level == path.depth {
                    let changed = (next_ratio - *ratio).abs() > f32::EPSILON;
                    *ratio = next_ratio;
                    changed
                } else if path.is_second(level) {
                    second.set_ratio(path, level + 1, next_ratio)
                } else {
                    first.set_ratio(path, level + 1, next_ratio)
                }
            }
        }
    }
}

fn remove_node(node: Node, target: PaneId) -> (Node, Option<PaneId>) {
    match node {
        Node::Leaf(id) => (Node::Leaf(id), None),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            if matches!(*first, Node::Leaf(id) if id == target) {
                let focus = second.first_leaf();
                return (*second, Some(focus));
            }
            if matches!(*second, Node::Leaf(id) if id == target) {
                let focus = first.last_leaf();
                return (*first, Some(focus));
            }
            let (first, focus) = remove_node(*first, target);
            if focus.is_some() {
                return (
                    Node::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second,
                    },
                    focus,
                );
            }
            let (second, focus) = remove_node(*second, target);
            (
                Node::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                },
                focus,
            )
        }
    }
}

fn rect_bounds(rect: PixelRect) -> (u32, u32, u32, u32) {
    (
        rect.origin.x,
        rect.origin.y,
        rect.origin.x + rect.size.width.get(),
        rect.origin.y + rect.size.height.get(),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use lune_terminal::{PixelPosition, PixelSize};
    use winit::dpi::PhysicalPosition;

    use crate::PixelRect;

    use super::{Axis, Direction, SplitTree};

    fn rect(x: u32, y: u32, width: u32, height: u32) -> PixelRect {
        PixelRect {
            origin: PixelPosition { x, y },
            size: PixelSize {
                width: NonZeroU32::new(width).unwrap(),
                height: NonZeroU32::new(height).unwrap(),
            },
        }
    }

    #[test]
    fn split_layout_navigation_resize_zoom_and_remove() {
        let mut tree = SplitTree::new(1);
        assert!(tree.split(1, Axis::Columns, 2));
        assert!(tree.split(2, Axis::Rows, 3));

        let area = rect(0, 0, 1002, 602);
        let layout = tree.layout(area, 2, None);
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.dividers.len(), 2);
        assert_eq!(layout.rect_of(1), Some(rect(0, 0, 500, 602)));
        assert_eq!(layout.rect_of(2), Some(rect(502, 0, 500, 300)));
        assert_eq!(layout.rect_of(3), Some(rect(502, 302, 500, 300)));

        // Spatial navigation.
        assert_eq!(
            SplitTree::focus_neighbor(1, Direction::Right, &layout),
            Some(2)
        );
        assert_eq!(
            SplitTree::focus_neighbor(2, Direction::Down, &layout),
            Some(3)
        );
        assert_eq!(
            SplitTree::focus_neighbor(3, Direction::Up, &layout),
            Some(2)
        );
        assert_eq!(
            SplitTree::focus_neighbor(3, Direction::Left, &layout),
            Some(1)
        );
        assert_eq!(SplitTree::focus_neighbor(1, Direction::Left, &layout), None);

        // Divider hit test and drag resize.
        let main_divider = layout
            .divider_at(PhysicalPosition::new(501.0, 200.0), 4.0)
            .unwrap();
        assert_eq!(main_divider.axis, Axis::Columns);
        assert!(tree.set_ratio(main_divider.path, 0.7));
        let resized = tree.layout(area, 2, None);
        assert_eq!(resized.rect_of(1).unwrap().size.width.get(), 700);
        assert_eq!(resized.rect_of(2).unwrap().size.width.get(), 300);

        // Keyboard resize adjusts nearest ancestor on that axis.
        assert!(tree.resize(3, Direction::Down, 0.1));
        let after_kb = tree.layout(area, 2, None);
        assert_eq!(after_kb.rect_of(2).unwrap().size.height.get(), 360);
        assert_eq!(after_kb.rect_of(3).unwrap().size.height.get(), 240);

        // Swapping two leaves exchanges their rectangles while preserving split ratios.
        let before_1 = after_kb.rect_of(1);
        let before_3 = after_kb.rect_of(3);
        assert!(tree.swap_panes(1, 3));
        let swapped = tree.layout(area, 2, None);
        assert_eq!(swapped.rect_of(1), before_3);
        assert_eq!(swapped.rect_of(3), before_1);
        assert!(tree.swap_panes(1, 3));

        // Zoom collapses layout to a single full-area pane with no dividers.
        let zoomed = tree.layout(area, 2, Some(3));
        assert_eq!(zoomed.panes.len(), 1);
        assert_eq!(zoomed.rect_of(3), Some(area));
        assert!(zoomed.dividers.is_empty());

        // Removing a pane collapses its parent split and returns sibling leaf.
        assert_eq!(tree.remove(3), Some(2));
        let collapsed = tree.layout(area, 2, None);
        assert_eq!(collapsed.panes.len(), 2);
        assert_eq!(collapsed.rect_of(2).unwrap().size.height.get(), 602);
        assert_eq!(tree.remove(2), Some(1));
        assert_eq!(tree.remove(1), None);
    }
}
