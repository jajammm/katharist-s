//! Treemap layout (squarified, Bruls-Huizing-van Wijk) and the items fed to it.

use crate::scan::{NodeId, ScanTree};

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Lays `sizes_desc` out inside `area`, one rect per size and in the same order.
/// Each rect's area is proportional to its size. Zero sizes get a zero-area rect
/// at the area's origin. Sizes are expected in descending order for good aspect
/// ratios, but any order stays correct.
pub fn squarify(sizes_desc: &[u64], area: Rect) -> Vec<Rect> {
    let zero = Rect {
        x: area.x,
        y: area.y,
        w: 0.0,
        h: 0.0,
    };
    let mut out = vec![zero; sizes_desc.len()];
    let total: f64 = sizes_desc.iter().map(|&s| s as f64).sum();
    let (w, h) = (f64::from(area.w), f64::from(area.h));
    let usable = total > 0.0 && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0;
    if !usable {
        return out;
    }
    let scale = w * h / total;
    let items: Vec<(usize, f64)> = sizes_desc
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s > 0)
        .map(|(i, &s)| (i, s as f64 * scale))
        .collect();

    let mut free = (f64::from(area.x), f64::from(area.y), w, h);
    let mut start = 0;
    while start < items.len() {
        let side = free.2.min(free.3);
        let mut end = start + 1;
        let mut sum = items[start].1;
        while end < items.len() {
            let next = sum + items[end].1;
            if worst(&items[start..=end], next, side) > worst(&items[start..end], sum, side) {
                break;
            }
            sum = next;
            end += 1;
        }
        free = lay_row(&items[start..end], sum, free, &mut out);
        start = end;
    }
    out
}

/// Worst aspect ratio of a row of areas laid along a side of length `side`.
fn worst(row: &[(usize, f64)], sum: f64, side: f64) -> f64 {
    let (mut lo, mut hi) = (f64::MAX, 0.0f64);
    for &(_, a) in row {
        lo = lo.min(a);
        hi = hi.max(a);
    }
    let (s2, side2) = (sum * sum, side * side);
    (side2 * hi / s2).max(s2 / (side2 * lo))
}

/// Places a row along the shorter side of `free` and returns what is left.
fn lay_row(
    row: &[(usize, f64)],
    sum: f64,
    (x, y, w, h): (f64, f64, f64, f64),
    out: &mut [Rect],
) -> (f64, f64, f64, f64) {
    let tall = w >= h;
    let thickness = if tall { sum / h } else { sum / w };
    let mut offset = 0.0;
    for &(i, a) in row {
        let len = a / thickness;
        let (rx, ry, rw, rh) = if tall {
            (x, y + offset, thickness, len)
        } else {
            (x + offset, y, len, thickness)
        };
        out[i] = Rect {
            x: finite(rx),
            y: finite(ry),
            w: finite(rw),
            h: finite(rh),
        };
        offset += len;
    }
    if tall {
        (x + thickness, y, (w - thickness).max(0.0), h)
    } else {
        (x, y + thickness, w, (h - thickness).max(0.0))
    }
}

/// Narrows to `f32`, mapping NaN, infinity and negatives to 0.
fn finite(v: f64) -> f32 {
    let v = v as f32;
    if v.is_finite() {
        v.max(0.0)
    } else {
        0.0
    }
}

/// One tile of the disk map. `node: None` is the "Lainnya" tile that groups the rest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapItem {
    pub node: Option<NodeId>,
    pub size: u64,
}

/// Children of `parent`, biggest first, at most `max_items` of them. Children
/// smaller than `min_fraction` of the parent's total, and those past the limit,
/// are merged into one `node: None` item. That item is placed by size like any
/// other, so the result can go straight into `squarify`. Zero-size children
/// are dropped, so the sizes always add up to the sum of the children.
pub fn map_items(
    tree: &ScanTree,
    parent: NodeId,
    min_fraction: f32,
    max_items: usize,
) -> Vec<MapItem> {
    let kids: Vec<(NodeId, u64)> = tree
        .children_by_size(parent)
        .into_iter()
        .map(|c| (c, tree.nodes[c].size))
        .filter(|&(_, s)| s > 0)
        .collect();
    let total: u64 = kids.iter().map(|&(_, s)| s).sum();
    let floor = f64::from(min_fraction.max(0.0)) * total as f64;

    let mut kept: Vec<MapItem> = Vec::new();
    let mut rest = 0u64;
    let mut grouped = false;
    for &(node, size) in &kids {
        if kept.len() < max_items && size as f64 >= floor {
            kept.push(MapItem {
                node: Some(node),
                size,
            });
        } else {
            rest += size;
            grouped = true;
        }
    }
    if grouped {
        // Keep the item count within the limit by giving the last slot to "Lainnya".
        if kept.len() >= max_items {
            if let Some(last) = kept.pop() {
                rest += last.size;
            }
        }
        let at = kept.partition_point(|m| m.size >= rest);
        kept.insert(
            at,
            MapItem {
                node: None,
                size: rest,
            },
        );
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanOptions, ScanProgress};
    use std::fs;

    const AREA: Rect = Rect {
        x: 0.,
        y: 0.,
        w: 100.,
        h: 50.,
    };

    fn overlap(a: &Rect, b: &Rect) -> bool {
        let eps = 1e-3;
        a.x + a.w > b.x + eps
            && b.x + b.w > a.x + eps
            && a.y + a.h > b.y + eps
            && b.y + b.h > a.y + eps
    }

    fn assert_valid(r: &[Rect]) {
        for q in r {
            assert!([q.x, q.y, q.w, q.h].iter().all(|v| v.is_finite()));
            assert!(q.w >= 0. && q.h >= 0.);
            assert!(q.x >= AREA.x - 1e-3 && q.y >= AREA.y - 1e-3);
            assert!(q.x + q.w <= AREA.x + AREA.w + 1e-3);
            assert!(q.y + q.h <= AREA.y + AREA.h + 1e-3);
        }
    }

    #[test]
    fn squarify_fills_area_without_overlap() {
        let r = squarify(&[60, 30, 10], AREA);
        assert_eq!(r.len(), 3);
        let area: f32 = r.iter().map(|q| q.w * q.h).sum();
        assert!((area - 5000.).abs() < 1.);
        assert!((r[0].w * r[0].h - 3000.).abs() < 1.);
        assert!((r[1].w * r[1].h - 1500.).abs() < 1.);
        assert!((r[2].w * r[2].h - 500.).abs() < 1.);
        assert_valid(&r);
        for i in 0..r.len() {
            for j in i + 1..r.len() {
                assert!(!overlap(&r[i], &r[j]), "{:?} overlaps {:?}", r[i], r[j]);
            }
        }
    }

    #[test]
    fn squarify_many_items_tiles_exactly() {
        let sizes: Vec<u64> = (1..=40).rev().map(|n| n * n * 1000).collect();
        let r = squarify(&sizes, AREA);
        assert_eq!(r.len(), 40);
        assert_valid(&r);
        let area: f32 = r.iter().map(|q| q.w * q.h).sum();
        assert!((area - 5000.).abs() < 1.);
        for i in 0..r.len() {
            for j in i + 1..r.len() {
                assert!(!overlap(&r[i], &r[j]));
            }
        }
    }

    #[test]
    fn squarify_handles_zero_and_empty() {
        assert!(squarify(&[], AREA).is_empty());
        let r = squarify(&[0, 0], AREA);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|q| q.w == 0. && q.h == 0.));
        assert_valid(&r);
    }

    #[test]
    fn squarify_keeps_zero_sizes_in_place_and_stays_finite() {
        let r = squarify(&[1_000_000_000_000, 1, 0], AREA);
        assert_eq!(r.len(), 3);
        assert_valid(&r);
        assert_eq!(r[2].w * r[2].h, 0.);
        assert!((r[0].w * r[0].h - 5000.).abs() < 1.);
        let degenerate = squarify(&[5, 5], Rect { w: 0., ..AREA });
        assert_eq!(degenerate.len(), 2);
        assert!(degenerate.iter().all(|q| q.w == 0. && q.h == 0.));
    }

    fn tree_with(files: &[(&str, usize)]) -> (tempfile::TempDir, ScanTree) {
        let dir = tempfile::tempdir().unwrap();
        for (name, len) in files {
            fs::write(dir.path().join(name), vec![1u8; *len]).unwrap();
        }
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        let t = scan(dir.path(), &opts, &ScanProgress::default()).unwrap();
        (dir, t)
    }

    #[test]
    fn map_items_groups_small_into_other() {
        let (_d, t) = tree_with(&[
            ("big", 1 << 20),
            ("mid", 1 << 19),
            ("tiny1", 10),
            ("tiny2", 10),
        ]);
        let items = map_items(&t, t.root, 0.005, 40);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().filter(|m| m.node.is_none()).count(), 1);
        let other = items.iter().find(|m| m.node.is_none()).unwrap();
        let tiny: u64 = t.nodes[t.root]
            .children
            .iter()
            .filter(|&&c| t.nodes[c].name.starts_with("tiny"))
            .map(|&c| t.nodes[c].size)
            .sum();
        assert_eq!(other.size, tiny);
        assert!(items.windows(2).all(|w| w[0].size >= w[1].size));
        let names: Vec<_> = items
            .iter()
            .filter_map(|m| m.node)
            .map(|n| t.nodes[n].name.as_str())
            .collect();
        assert_eq!(names, ["big", "mid"]);
    }

    #[test]
    fn map_items_respects_max_items() {
        let files: Vec<(String, usize)> = (1..=6).map(|n| (format!("f{n}"), n * 8192)).collect();
        let refs: Vec<(&str, usize)> = files.iter().map(|(n, l)| (n.as_str(), *l)).collect();
        let (_d, t) = tree_with(&refs);
        let items = map_items(&t, t.root, 0.0, 3);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().filter(|m| m.node.is_none()).count(), 1);
        let total: u64 = items.iter().map(|m| m.size).sum();
        assert_eq!(total, t.nodes[t.root].size);
        let none = map_items(&t, t.root, 0.0, 0);
        assert!(!none.is_empty() && none.iter().all(|m| m.node.is_none()));
    }

    #[test]
    fn map_items_keeps_total_equal_to_parent() {
        let (_d, t) = tree_with(&[("a", 1 << 20), ("b", 100), ("c", 100), ("empty", 0)]);
        for (frac, max) in [(0.005, 40), (0.0, 40), (0.5, 40), (0.005, 1), (1.0, 5)] {
            let items = map_items(&t, t.root, frac, max);
            let total: u64 = items.iter().map(|m| m.size).sum();
            assert_eq!(total, t.nodes[t.root].size, "frac {frac} max {max}");
            assert!(items.iter().all(|m| m.size > 0));
        }
        let no_group = map_items(&t, t.root, 0.0, 40);
        assert!(no_group.iter().all(|m| m.node.is_some()));
        assert_eq!(no_group.len(), 3);
    }
}
