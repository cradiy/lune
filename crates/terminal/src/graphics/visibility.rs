use std::collections::HashSet;

use lune_kitty_graphics::ImageStore;

use super::{GraphicsPlaceholderCache, GraphicsPlaceholderRun, visit_graphics_placeholders};
use crate::{PixelSize, Row, RowRevision, Screen, StyleTable};

#[derive(Default)]
pub(super) struct AnimationVisibility {
    stamp: Option<VisibilityStamp>,
    rows: Vec<Option<RowRevision>>,
    virtual_placements: HashSet<u64>,
    parents: GraphicsPlaceholderCache,
    animated: Vec<usize>,
    has_virtual: bool,
    has_parents: bool,
}

#[derive(PartialEq, Eq)]
struct VisibilityStamp {
    graphics: Option<u64>,
    animations: u64,
    offset: usize,
    dimensions: [usize; 2],
    cell: PixelSize,
    history: Option<u64>,
}

impl AnimationVisibility {
    pub(super) fn invalidate(&mut self) {
        self.stamp = None;
    }

    pub(super) fn update(&mut self, screen: &Screen, styles: &StyleTable, images: &mut ImageStore) {
        let Some(animations) = images.animation_membership() else {
            self.invalidate();
            return;
        };
        let graphics = screen.graphics_revision();
        let placements = screen.graphics_placements();
        if self
            .stamp
            .as_ref()
            .is_none_or(|stamp| stamp.graphics != graphics || stamp.animations != animations)
        {
            self.animated.clear();
            self.has_virtual = false;
            self.has_parents = false;
            for (index, placement) in placements.iter().enumerate() {
                if images.is_animated(placement.placement.image) {
                    self.animated.push(index);
                    self.has_virtual |= placement.is_virtual();
                    self.has_parents |= placement.virtual_parent().is_some();
                }
            }
        }
        let view = screen.viewport();
        let has_parents = self.has_parents;
        let scans_rows = self.has_virtual || has_parents;
        let revisions = || {
            view.rows_for_graphics(has_parents)
                .map(Row::graphics_placeholder_revision)
        };
        let stamp = VisibilityStamp {
            graphics,
            animations,
            offset: view.offset(),
            dimensions: [screen.grid().columns(), view.rows()],
            cell: screen.graphics_cell_extent(),
            history: if has_parents {
                view.source().history_revision()
            } else {
                None
            },
        };
        if self.stamp.as_ref() == Some(&stamp)
            && (!scans_rows || revisions().eq(self.rows.iter().copied()))
        {
            return;
        }

        let [columns, rows] = stamp.dimensions.map(|size| size as i64);
        self.virtual_placements.clear();
        let visible = &mut self.virtual_placements;
        let record = |run: GraphicsPlaceholderRun| {
            let [column, row] = run.position;
            if column < columns && (0..rows).contains(&row) {
                visible.insert(run.placement);
            }
        };
        let scanned = if has_parents {
            self.parents
                .visit(
                    graphics.expect("A parent belongs to a placement"),
                    placements,
                    view,
                    styles,
                    stamp.cell,
                    self.has_virtual.then_some(record),
                )
                .is_ok()
        } else if self.has_virtual {
            visit_graphics_placeholders(placements, view.iter_rows(), styles, record).is_ok()
        } else {
            true
        };
        let cell = [stamp.cell.width.get(), stamp.cell.height.get()].map(f64::from);
        let area = [columns as f64 * cell[0], rows as f64 * cell[1]];
        images.set_animation_visibility(self.animated.iter().filter_map(|&index| {
            let placement = &placements[index];
            let key = placement.placement.image;
            if placement.is_virtual() {
                return visible.contains(&placement.serial()).then_some(key);
            }
            let origin =
                placement.origin_in_view(stamp.offset, |parent| self.parents.origin(parent))?;
            let layout = placement.layout(stamp.cell);
            (0..2)
                .all(|axis| {
                    let start = origin[axis] as f64 * cell[axis] + layout.offset[axis];
                    start < area[axis] && start + layout.size[axis] > 0.0
                })
                .then_some(key)
        }));
        self.rows.clear();
        if scans_rows {
            self.rows.extend(revisions());
        }
        self.stamp = scanned.then_some(stamp);
    }
}
