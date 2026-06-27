//! Pure layout + hit-testing for the app drawer's categorized view. No Win32 / Direct2D
//! here — just turning (entry keys, categories) into ordered sections and cell rectangles
//! in the drawer's content coordinate space, so the geometry can be unit-tested on its
//! own. The drawer renders these rects and routes clicks / drops back through them.

use crate::categories::Categories;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SectionKind {
    Custom(usize), // index into Categories.categories
    Uncategorized,
}

pub struct Section {
    pub name: String,
    pub kind: SectionKind,
    pub entries: Vec<usize>, // indices into the drawer's entry list
}

/// Resolve (entry keys, categories) into ordered sections. Custom categories come first
/// in saved order (members in saved drag order, with keys whose program is gone skipped);
/// the "未分类" catch-all holds the rest in incoming (alphabetical) order. The catch-all is
/// always present when any custom category exists (so it stays a drop target), and
/// otherwise only when it actually has members.
pub fn sectionize(entry_keys: &[String], categories: &Categories) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut claimed = vec![false; entry_keys.len()];

    for (ci, cat) in categories.categories.iter().enumerate() {
        let mut entries = Vec::new();
        for key in &cat.items {
            if let Some(idx) = entry_keys.iter().position(|k| k == key) {
                if !claimed[idx] {
                    claimed[idx] = true;
                    entries.push(idx);
                }
            }
        }
        sections.push(Section {
            name: cat.name.clone(),
            kind: SectionKind::Custom(ci),
            entries,
        });
    }

    let leftovers: Vec<usize> = (0..entry_keys.len()).filter(|&i| !claimed[i]).collect();
    if !leftovers.is_empty() || !categories.categories.is_empty() {
        sections.push(Section {
            name: "未分类".to_string(),
            kind: SectionKind::Uncategorized,
            entries: leftovers,
        });
    }
    sections
}

/// Pixel dimensions (device px) the geometry is laid out with.
pub struct Metrics {
    pub width: f32,
    pub cols: usize,
    pub lpad: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    pub gap_x: f32,
    pub gap_y: f32,
    pub header_h: f32,
    pub section_gap: f32,
    pub add_h: f32,
}

#[derive(Clone, Copy)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

pub struct Cell {
    pub entry: usize,
    pub rect: Rect,
}

/// One section's vertical span: its header plus the grid band below it.
pub struct Band {
    pub section: usize,
    pub header: Rect,
    pub grid_top: f32,
    pub grid_bottom: f32,
}

pub struct Layout {
    pub cells: Vec<Cell>,
    pub bands: Vec<Band>,
    pub add_button: Rect,
    pub content_h: f32,
}

fn inside(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

fn rows_for(count: usize, cols: usize) -> usize {
    // Empty sections still reserve one row so there's something to drop into.
    if count == 0 {
        1
    } else {
        count.div_ceil(cols)
    }
}

/// Lay sections out top-to-bottom in content space (y = 0 at the first header).
pub fn compute(sections: &[Section], m: &Metrics) -> Layout {
    let mut cells = Vec::new();
    let mut bands = Vec::new();
    let row_stride = m.cell_h + m.gap_y;
    let mut y = 0.0f32;
    for (si, sec) in sections.iter().enumerate() {
        let header = Rect {
            left: m.lpad,
            top: y,
            right: m.width - m.lpad,
            bottom: y + m.header_h,
        };
        let grid_top = y + m.header_h;
        for (i, &entry) in sec.entries.iter().enumerate() {
            let col = (i % m.cols) as f32;
            let row = (i / m.cols) as f32;
            let left = m.lpad + col * (m.cell_w + m.gap_x);
            let top = grid_top + row * row_stride;
            cells.push(Cell {
                entry,
                rect: Rect {
                    left,
                    top,
                    right: left + m.cell_w,
                    bottom: top + m.cell_h,
                },
            });
        }
        let rows = rows_for(sec.entries.len(), m.cols) as f32;
        let grid_bottom = grid_top + rows * m.cell_h + (rows - 1.0) * m.gap_y;
        bands.push(Band {
            section: si,
            header,
            grid_top,
            grid_bottom,
        });
        y = grid_bottom + m.section_gap;
    }
    let add_button = Rect {
        left: m.lpad,
        top: y,
        right: m.width - m.lpad,
        bottom: y + m.add_h,
    };
    Layout {
        cells,
        bands,
        add_button,
        content_h: y + m.add_h,
    }
}

/// The cell index under a content-space point, if any.
pub fn hit_cell(layout: &Layout, x: f32, y: f32) -> Option<usize> {
    layout.cells.iter().position(|c| inside(&c.rect, x, y))
}

/// The (section index, insertion index) a content-space point maps to for a drop:
/// the section whose band contains `y` (clamped to the nearest), and the grid slot the
/// cursor is over. Insertion index is in the section's own entry order, 0..=len.
pub fn drop_target(
    sections: &[Section],
    layout: &Layout,
    m: &Metrics,
    x: f32,
    y: f32,
) -> Option<(usize, usize)> {
    if layout.bands.is_empty() {
        return None;
    }
    let mut chosen = layout.bands.len() - 1;
    for (bi, band) in layout.bands.iter().enumerate() {
        if y < band.grid_bottom + m.section_gap * 0.5 {
            chosen = bi;
            break;
        }
    }
    let band = &layout.bands[chosen];
    let count = sections[band.section].entries.len();
    // Drop on a cell's right half inserts after it (the +cell_w*0.5 bias).
    let col = (((x - m.lpad + m.cell_w * 0.5) / (m.cell_w + m.gap_x)).floor() as i32)
        .clamp(0, m.cols as i32) as usize;
    let rel_y = (y - band.grid_top).max(0.0);
    let row = (rel_y / (m.cell_h + m.gap_y)).floor() as usize;
    let index = (row * m.cols + col).min(count);
    Some((band.section, index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::categories::Categories;

    fn metrics() -> Metrics {
        Metrics {
            width: 100.0,
            cols: 5,
            lpad: 0.0,
            cell_w: 20.0,
            cell_h: 20.0,
            gap_x: 0.0,
            gap_y: 0.0,
            header_h: 10.0,
            section_gap: 0.0,
            add_h: 10.0,
        }
    }

    #[test]
    fn sectionize_orders_custom_first_then_uncategorized() {
        let keys: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let mut cats = Categories::default();
        let g = cats.add("games");
        cats.move_item("c", Some(g), 0);
        cats.move_item("a", Some(g), 1);

        let sections = sectionize(&keys, &cats);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].kind, SectionKind::Custom(0));
        assert_eq!(sections[0].entries, vec![2, 0]); // c, a in saved order
        assert_eq!(sections[1].kind, SectionKind::Uncategorized);
        assert_eq!(sections[1].entries, vec![1, 3]); // b, d leftover
    }

    #[test]
    fn empty_uncategorized_kept_as_drop_target_when_categories_exist() {
        let keys: Vec<String> = vec!["a".to_string()];
        let mut cats = Categories::default();
        let g = cats.add("games");
        cats.move_item("a", Some(g), 0);
        let sections = sectionize(&keys, &cats);
        assert_eq!(sections.len(), 2);
        assert!(sections[1].entries.is_empty());
    }

    #[test]
    fn drop_target_maps_into_the_right_section_and_slot() {
        let keys: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let cats = Categories::default(); // all uncategorized
        let sections = sectionize(&keys, &cats);
        let m = metrics();
        let layout = compute(&sections, &m);
        // Header is y in [0,10), grid starts at 10. First cell center ~ (10,20).
        let (sec, idx) = drop_target(&sections, &layout, &m, 5.0, 20.0).unwrap();
        assert_eq!(sec, 0);
        assert_eq!(idx, 0); // left half of first cell → before it
        let (_, idx_after) = drop_target(&sections, &layout, &m, 18.0, 20.0).unwrap();
        assert_eq!(idx_after, 1); // right half of first cell → after it
    }
}
