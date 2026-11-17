//! FORK: per-row commit graph for the VS Code style history section.
//!
//! Each row is drawn on its own, the way VS Code's source control graph does
//! it: lanes keep their column for as long as they live, so a row only needs
//! to know which lanes pass through it, which ones end in its commit and which
//! ones start from it. That keeps the history a plain `uniform_list`.

use git::{Oid, repository::InitialGraphCommitData};
use gpui::{
    App, Bounds, Hsla, IntoElement, PathBuilder, Pixels, Styled as _, Window, canvas, point, px,
};
use smallvec::SmallVec;
use std::sync::Arc;
use theme::AccentColors;

pub(super) const LANE_WIDTH: Pixels = px(12.);
const LEFT_PADDING: Pixels = px(4.);
const LINE_WIDTH: Pixels = px(1.5);
const DOT_RADIUS: Pixels = px(3.5);

/// A lane is a column index; colors are indices into the theme's accents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LaneRef {
    pub lane: usize,
    pub color: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct GraphRow {
    pub lane: usize,
    pub color: usize,
    pub is_merge: bool,
    /// Lanes that continue straight through this row.
    pub through: SmallVec<[LaneRef; 4]>,
    /// Lanes that end in this row's commit, entering from the top.
    pub incoming: SmallVec<[LaneRef; 2]>,
    /// Lanes that leave this row's commit towards its parents.
    pub outgoing: SmallVec<[LaneRef; 2]>,
    /// Number of columns the row needs.
    pub width: usize,
}

pub(super) fn compute_rows(
    commits: &[Arc<InitialGraphCommitData>],
    color_count: usize,
) -> Vec<GraphRow> {
    let color_count = color_count.max(1);
    let mut lanes: Vec<Option<(Oid, usize)>> = Vec::new();
    let mut next_color = 0;
    let mut new_color = || {
        let color = next_color;
        next_color = (next_color + 1) % color_count;
        color
    };
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let waiting: SmallVec<[usize; 2]> = lanes
            .iter()
            .enumerate()
            .filter_map(|(ix, lane)| (lane.map(|(sha, _)| sha) == Some(commit.sha)).then_some(ix))
            .collect();

        let lane = match waiting.first() {
            Some(&lane) => lane,
            None => free_lane(&mut lanes, None),
        };
        let color = match lanes[lane] {
            Some((_, color)) if waiting.contains(&lane) => color,
            _ => new_color(),
        };

        let incoming = waiting
            .iter()
            .map(|&ix| LaneRef {
                lane: ix,
                color: lanes[ix].map_or(color, |(_, color)| color),
            })
            .collect();
        let through = lanes
            .iter()
            .enumerate()
            .filter(|(ix, _)| !waiting.contains(ix))
            .filter_map(|(ix, lane)| lane.map(|(_, color)| LaneRef { lane: ix, color }))
            .collect();

        for &ix in &waiting {
            lanes[ix] = None;
        }

        let mut outgoing = SmallVec::new();
        for (parent_ix, parent) in commit.parents.iter().enumerate() {
            // The first parent always continues in the commit's own lane, even
            // when another lane already waits for it: the two lines then meet
            // at the parent, which reads better than bending into a far lane.
            if parent_ix == 0 {
                lanes[lane] = Some((*parent, color));
                outgoing.push(LaneRef { lane, color });
                continue;
            }
            let existing = lanes
                .iter()
                .position(|lane| lane.map(|(sha, _)| sha) == Some(*parent));
            if let Some(ix) = existing {
                let color = lanes[ix].map_or(color, |(_, color)| color);
                outgoing.push(LaneRef { lane: ix, color });
            } else {
                let ix = free_lane(&mut lanes, Some(lane));
                let color = new_color();
                lanes[ix] = Some((*parent, color));
                outgoing.push(LaneRef { lane: ix, color });
            }
        }

        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }

        let mut row = GraphRow {
            lane,
            color,
            is_merge: commit.parents.len() > 1,
            through,
            incoming,
            outgoing,
            width: 0,
        };
        row.width = row
            .through
            .iter()
            .chain(&row.incoming)
            .chain(&row.outgoing)
            .map(|lane| lane.lane)
            .chain([lane])
            .max()
            .unwrap_or(0)
            + 1;
        rows.push(row);
    }

    rows
}

fn free_lane(lanes: &mut Vec<Option<(Oid, usize)>>, skip: Option<usize>) -> usize {
    match lanes
        .iter()
        .enumerate()
        .position(|(ix, lane)| lane.is_none() && Some(ix) != skip)
    {
        Some(ix) => ix,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

pub(super) fn graph_width(lanes: usize) -> Pixels {
    LEFT_PADDING * 2. + LANE_WIDTH * lanes as f32
}

/// The graph cell of one history row. `is_head` draws the commit as a ring,
/// like VS Code does for the current commit.
pub(super) fn render_row_graph(
    row: GraphRow,
    lanes: usize,
    is_head: bool,
    accents: AccentColors,
    background: Hsla,
) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window: &mut Window, _: &mut App| {
            paint_row(&row, bounds, is_head, &accents, background, window);
        },
    )
    .flex_none()
    .w(graph_width(lanes))
    .h_full()
}

fn paint_row(
    row: &GraphRow,
    bounds: Bounds<Pixels>,
    is_head: bool,
    accents: &AccentColors,
    background: Hsla,
    window: &mut Window,
) {
    let x =
        |lane: usize| bounds.origin.x + LEFT_PADDING + LANE_WIDTH * lane as f32 + LANE_WIDTH / 2.;
    let top = bounds.origin.y;
    let bottom = bounds.origin.y + bounds.size.height;
    let center = bounds.origin.y + bounds.size.height / 2.;
    let color = |index: usize| accents.color_for_index(index as u32);

    let mut stroke = |from: gpui::Point<Pixels>,
                      to: gpui::Point<Pixels>,
                      ctrl: Option<gpui::Point<Pixels>>,
                      color: Hsla| {
        let mut builder = PathBuilder::stroke(LINE_WIDTH);
        builder.move_to(from);
        match ctrl {
            Some(ctrl) => builder.curve_to(to, ctrl),
            None => builder.line_to(to),
        }
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    };

    for lane in &row.through {
        stroke(
            point(x(lane.lane), top),
            point(x(lane.lane), bottom),
            None,
            color(lane.color),
        );
    }
    for lane in &row.incoming {
        let from = point(x(lane.lane), top);
        let to = point(x(row.lane), center);
        let ctrl = (lane.lane != row.lane).then(|| point(x(lane.lane), center));
        stroke(from, to, ctrl, color(lane.color));
    }
    for lane in &row.outgoing {
        let from = point(x(row.lane), center);
        let to = point(x(lane.lane), bottom);
        let ctrl = (lane.lane != row.lane).then(|| point(x(lane.lane), center));
        stroke(from, to, ctrl, color(lane.color));
    }

    let dot_color = color(row.color);
    let cx = x(row.lane);
    fill_circle(cx, center, DOT_RADIUS + px(1.), background, window);
    fill_circle(cx, center, DOT_RADIUS, dot_color, window);
    if is_head || row.is_merge {
        fill_circle(cx, center, DOT_RADIUS - LINE_WIDTH, background, window);
    }
    if row.is_merge && !is_head {
        fill_circle(cx, center, px(1.5), dot_color, window);
    }
}

fn fill_circle(cx: Pixels, cy: Pixels, radius: Pixels, color: Hsla, window: &mut Window) {
    if radius <= px(0.) {
        return;
    }
    let mut builder = PathBuilder::fill();
    builder.move_to(point(cx + radius, cy));
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(cx - radius, cy),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        point(cx + radius, cy),
    );
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(n: u8) -> Oid {
        format!("{:040x}", n).parse().unwrap()
    }

    fn commit(n: u8, parents: &[u8]) -> Arc<InitialGraphCommitData> {
        Arc::new(InitialGraphCommitData {
            sha: sha(n),
            parents: parents.iter().map(|&p| sha(p)).collect(),
            ref_names: Vec::new(),
        })
    }

    fn lanes(refs: &[LaneRef]) -> Vec<usize> {
        refs.iter().map(|lane| lane.lane).collect()
    }

    #[test]
    fn test_linear_history_stays_in_one_lane() {
        let rows = compute_rows(&[commit(3, &[2]), commit(2, &[1]), commit(1, &[])], 4);
        assert!(rows.iter().all(|row| row.lane == 0 && row.width == 1));
        assert!(rows[0].incoming.is_empty());
        assert_eq!(lanes(&rows[1].incoming), [0]);
        assert!(rows[2].outgoing.is_empty());
    }

    #[test]
    fn test_merge_opens_and_closes_a_side_lane() {
        // 4 merges 3 (side branch) into 2's line; both come from 1.
        let rows = compute_rows(
            &[
                commit(4, &[2, 3]),
                commit(3, &[1]),
                commit(2, &[1]),
                commit(1, &[]),
            ],
            4,
        );
        assert!(rows[0].is_merge);
        assert_eq!(lanes(&rows[0].outgoing), [0, 1]);
        // The side branch commit sits in lane 1, lane 0 passes through.
        assert_eq!(rows[1].lane, 1);
        assert_eq!(lanes(&rows[1].through), [0]);
        // Both lanes wait for 1 now; 2 continues in lane 0 and 1 passes by.
        assert_eq!(rows[2].lane, 0);
        assert_eq!(lanes(&rows[2].through), [1]);
        assert_eq!(lanes(&rows[3].incoming), [0, 1]);
        assert_eq!(rows[3].lane, 0);
        assert_ne!(rows[0].outgoing[0].color, rows[0].outgoing[1].color);
    }

    #[test]
    fn test_rows_of_a_prefix_match_the_full_history() {
        let history = [
            commit(6, &[5, 4]),
            commit(4, &[2]),
            commit(5, &[3, 2]),
            commit(3, &[1]),
            commit(2, &[1]),
            commit(1, &[]),
        ];
        let full = compute_rows(&history, 4);
        for len in 0..=history.len() {
            assert_eq!(compute_rows(&history[..len], 4), full[..len]);
        }
    }
}
