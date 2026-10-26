#[derive(Clone, Copy, Debug)]
pub struct SelectionRect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryLayout {
    pub width: f32,
    pub height: f32,
    pub gap: f32,
    pub columns: usize,
}

impl EntryLayout {

    pub fn intersecting(self, rect: SelectionRect, count: usize) -> Vec<usize> {
        if count == 0
            || self.columns == 0
            || self.columns > 24
            || ![
                self.width,
                self.height,
                self.gap,
                rect.x1,
                rect.x2,
                rect.y1,
                rect.y2,
            ]
            .iter()
            .all(|value| value.is_finite())
            || self.width <= 0.
            || self.height <= 0.
            || self.gap < 0.
        {
            return vec![];
        }
        let left = rect.x1.min(rect.x2);
        let top = rect.y1.min(rect.y2);
        let right = rect.x1.max(rect.x2).max(left + 1.);
        let bottom = rect.y1.max(rect.y2).max(top + 1.);
        let step_x = self.width + self.gap;
        let step_y = self.height + self.gap;
        let rows = count.div_ceil(self.columns);
        if right <= 0. || bottom <= 0. || left >= self.columns as f32 * step_x {
            return vec![];
        }
        let first_row = (top.max(0.) / step_y).floor() as usize;
        let last_row = ((bottom.max(0.) / step_y).floor() as usize).min(rows - 1);
        if first_row > last_row {
            return vec![];
        }
        let first_column = ((left.max(0.) / step_x).floor() as usize).min(self.columns - 1);
        let last_column = ((right.max(0.) / step_x).floor() as usize).min(self.columns - 1);
        let mut hits = vec![];
        for row in first_row..=last_row {
            let cell_top = row as f32 * step_y;
            if cell_top >= bottom || cell_top + self.height <= top {
                continue;
            }
            for column in first_column..=last_column {
                let cell_left = column as f32 * step_x;
                let index = row * self.columns + column;
                if index < count && cell_left < right && cell_left + self.width > left {
                    hits.push(index);
                }
            }
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> SelectionRect {
        SelectionRect { x1, y1, x2, y2 }
    }

    #[test]
    fn reverse_grid_drag_selects_intersections_without_padding_or_gutters() {
        let grid = EntryLayout {
            width: 100.,
            height: 100.,
            gap: 20.,
            columns: 3,
        };
        assert_eq!(
            grid.intersecting(rect(330., 225., 125., 10.), 5),
            vec![1, 2, 4]
        );
        assert!(grid
            .intersecting(rect(103., 103., 117., 117.), 5)
            .is_empty());
        assert!(grid
            .intersecting(rect(245., 125., 330., 210.), 5)
            .is_empty());
        assert_eq!(grid.intersecting(rect(-50., -20., 20., 25.), 5), vec![0]);
    }

    #[test]
    fn list_selection_uses_content_positions_after_scrolling() {
        let list = EntryLayout {
            width: 700.,
            height: 40.,
            gap: 0.,
            columns: 1,
        };
        assert_eq!(
            list.intersecting(rect(500., 150., 550., 250.), 20),
            vec![3, 4, 5, 6]
        );
        assert_eq!(list.intersecting(rect(10., 120., 30., 160.), 20), vec![3]);
        assert!(list.intersecting(rect(710., 5., 740., 100.), 20).is_empty());
    }

    #[test]
    fn invalid_geometry_is_rejected_and_large_offscreen_rectangles_are_bounded() {
        let grid = EntryLayout {
            width: 80.,
            height: 80.,
            gap: 8.,
            columns: 2,
        };
        assert!(grid
            .intersecting(rect(f32::NAN, 0., 10., 10.), 4)
            .is_empty());
        assert!(grid
            .intersecting(rect(0., f32::INFINITY, 10., 10.), 4)
            .is_empty());
        assert_eq!(
            grid.intersecting(rect(-1e30, -1e30, 1e30, 1e30), 4),
            vec![0, 1, 2, 3]
        );
        assert!(grid.intersecting(rect(0., 1e30, 10., 2e30), 4).is_empty());
    }
}
