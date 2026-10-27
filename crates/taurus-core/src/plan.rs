//! Segment planning and work stealing for multi-connection downloads.

/// Smallest segment worth a connection of its own (1 MiB).
pub const MIN_SEGMENT: u64 = 1_048_576;
/// Default cap on parallel connections per download.
pub const DEFAULT_CONNECTIONS: u32 = 16;

/// A byte range `[start, end)` of the file; `done` bytes from `start` are complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl Segment {
    /// Bytes still to download. Saturating, so an open-ended
    /// `[0, u64::MAX)` segment cannot overflow.
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.cursor())
    }

    /// Next byte offset to fetch (`start + done`).
    pub fn cursor(&self) -> u64 {
        self.start.saturating_add(self.done)
    }
}

/// Splits `[0, size)` into at most `max_connections` contiguous segments of
/// at least `MIN_SEGMENT` bytes each (fewer connections for small files).
/// Lengths differ by at most one byte; earlier segments take the extra byte.
pub fn plan_segments(size: u64, max_connections: u32) -> Vec<Segment> {
    let count = (size / MIN_SEGMENT).clamp(1, u64::from(max_connections.max(1)));
    let (base, extra) = (size / count, size % count);
    let mut start = 0;
    (0..count)
        .map(|i| {
            let end = start + base + u64::from(i < extra);
            let segment = Segment {
                start,
                end,
                done: 0,
            };
            start = end;
            segment
        })
        .collect()
}

/// Picks the segment with the most bytes remaining (lowest index on ties) and
/// returns `(index, split)`: the caller shrinks that segment to end at `split`
/// and starts a new one at `split`. `None` when the largest remainder is under
/// `2 * MIN_SEGMENT`.
pub fn steal(segments: &[Segment]) -> Option<(usize, u64)> {
    let mut best: Option<(usize, u64)> = None;
    for (i, s) in segments.iter().enumerate() {
        if best.is_none_or(|(_, remaining)| s.remaining() > remaining) {
            best = Some((i, s.remaining()));
        }
    }
    let (index, remaining) = best?;
    (remaining >= 2 * MIN_SEGMENT).then(|| (index, segments[index].cursor() + remaining / 2))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = MIN_SEGMENT;

    fn seg(start: u64, end: u64, done: u64) -> Segment {
        Segment { start, end, done }
    }

    #[test]
    fn zero_and_tiny_files_one_segment() {
        assert_eq!(plan_segments(0, 16), vec![seg(0, 0, 0)]);
        assert_eq!(plan_segments(500_000, 16), vec![seg(0, 500_000, 0)]);
    }

    #[test]
    fn even_split_respects_min() {
        let segs = plan_segments(10 * MIB, 16);
        assert_eq!(segs.len(), 10);
        assert!(segs.iter().all(|s| s.end - s.start == MIB && s.done == 0));

        let size = 100 * MIB + 7;
        let segs = plan_segments(size, 16);
        assert_eq!(segs.len(), 16);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs[15].end, size);
        assert!(segs.windows(2).all(|w| w[0].end == w[1].start));
        let lens: Vec<u64> = segs.iter().map(|s| s.end - s.start).collect();
        assert!(lens.iter().max().unwrap() - lens.iter().min().unwrap() <= 1);
        assert!(
            lens.windows(2).all(|w| w[0] >= w[1]),
            "extra bytes go first"
        );
    }

    #[test]
    fn zero_max_connections_means_one_segment() {
        assert_eq!(plan_segments(50 * MIB, 0), vec![seg(0, 50 * MIB, 0)]);
    }

    #[test]
    fn steal_picks_largest_remaining() {
        let segs = [
            seg(0, MIB, 0),
            seg(MIB, 6 * MIB, 0),
            seg(6 * MIB, 9 * MIB, 0),
        ];
        assert_eq!(steal(&segs), Some((1, MIB + 5 * MIB / 2)));

        let segs = [seg(0, 10 * MIB, 4 * MIB), seg(10 * MIB, 14 * MIB, 0)];
        assert_eq!(steal(&segs), Some((0, 4 * MIB + 3 * MIB)));
    }

    #[test]
    fn steal_ties_pick_lowest_index() {
        let segs = [seg(0, 4 * MIB, 0), seg(4 * MIB, 8 * MIB, 0)];
        assert_eq!(steal(&segs), Some((0, 2 * MIB)));
    }

    #[test]
    fn steal_refuses_small() {
        let segs = [seg(0, 2 * MIB - 1, 0), seg(2 * MIB, 6 * MIB, 3 * MIB)];
        assert_eq!(steal(&segs), None);
        assert_eq!(steal(&[]), None);
    }

    #[test]
    fn open_ended_segment_does_not_overflow() {
        let s = seg(0, u64::MAX, 5);
        assert_eq!((s.cursor(), s.remaining()), (5, u64::MAX - 5));
        assert_eq!(steal(&[s]), Some((0, 5 + (u64::MAX - 5) / 2)));
    }
}
