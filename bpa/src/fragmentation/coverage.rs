//! Byte range coverage tracking for fragment reassembly.
//!
//! Tracks which byte ranges of an ADU have been received,
//! handling overlaps and out-of-order arrival.

/// Tracks received byte ranges, merging overlaps.
///
/// Ranges are stored as sorted, non-overlapping `(start, end)` intervals.
/// Inserting a new range merges it with any adjacent or overlapping intervals.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct Coverage {
    /// Sorted, non-overlapping intervals: `[(start, end), ...]`
    ranges: Vec<(u64, u64)>,
}

impl Coverage {
    pub fn new() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Insert a byte range, merging with existing intervals.
    pub fn insert(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }

        let end = start + len;
        let mut new_start = start;
        let mut new_end = end;

        // Find all intervals that overlap or are adjacent to [start, end)
        let mut first = self.ranges.len();
        let mut last = 0;

        for (i, &(s, e)) in self.ranges.iter().enumerate() {
            // Overlaps or is adjacent if: s <= new_end && e >= new_start
            if s <= new_end && e >= new_start {
                if first > i {
                    first = i;
                }
                last = i + 1;
                new_start = new_start.min(s);
                new_end = new_end.max(e);
            }
        }

        if first >= last {
            // No overlap — find insertion point to keep sorted
            let pos = self.ranges.partition_point(|&(s, _)| s < start);
            self.ranges.insert(pos, (new_start, new_end));
        } else {
            // Replace overlapping range(s) with merged interval
            self.ranges[first] = (new_start, new_end);
            if last > first + 1 {
                self.ranges.drain((first + 1)..last);
            }
        }
    }

    /// Check if coverage spans the entire range `[0, total_length)`.
    pub fn is_complete(&self, total_length: u64) -> bool {
        if total_length == 0 {
            return true;
        }
        self.ranges.len() == 1 && self.ranges[0] == (0, total_length)
    }

    /// Total number of unique bytes covered.
    #[cfg(test)]
    fn covered_bytes(&self) -> u64 {
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }

    /// Number of intervals (1 when complete, more when fragmented).
    #[cfg(test)]
    fn interval_count(&self) -> usize {
        self.ranges.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_coverage() {
        let c = Coverage::new();
        assert!(c.is_complete(0));
        assert!(!c.is_complete(10));
        assert_eq!(c.covered_bytes(), 0);
    }

    #[test]
    fn single_insert_covers_all() {
        let mut c = Coverage::new();
        c.insert(0, 10);
        assert!(c.is_complete(10));
        assert_eq!(c.covered_bytes(), 10);
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn two_adjacent_fragments() {
        let mut c = Coverage::new();
        c.insert(0, 5);
        c.insert(5, 5);
        assert!(c.is_complete(10));
        assert_eq!(c.covered_bytes(), 10);
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn out_of_order() {
        let mut c = Coverage::new();
        c.insert(5, 5);
        c.insert(0, 5);
        assert!(c.is_complete(10));
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn gap_remains() {
        let mut c = Coverage::new();
        c.insert(0, 3);
        c.insert(7, 3);
        assert!(!c.is_complete(10));
        assert_eq!(c.covered_bytes(), 6);
        assert_eq!(c.interval_count(), 2);
    }

    #[test]
    fn fill_gap() {
        let mut c = Coverage::new();
        c.insert(0, 3);
        c.insert(7, 3);
        c.insert(3, 4);
        assert!(c.is_complete(10));
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn overlapping_fragments() {
        let mut c = Coverage::new();
        c.insert(0, 6);
        c.insert(4, 6);
        assert!(c.is_complete(10));
        assert_eq!(c.covered_bytes(), 10);
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn duplicate_fragment() {
        let mut c = Coverage::new();
        c.insert(0, 5);
        c.insert(0, 5);
        assert_eq!(c.covered_bytes(), 5);
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn zero_length_insert_ignored() {
        let mut c = Coverage::new();
        c.insert(5, 0);
        assert_eq!(c.covered_bytes(), 0);
        assert_eq!(c.interval_count(), 0);
    }

    #[test]
    fn three_fragments_merge_all() {
        let mut c = Coverage::new();
        c.insert(0, 3);
        c.insert(6, 4);
        c.insert(2, 5); // bridges the gap, overlaps both
        assert!(c.is_complete(10));
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn partial_coverage_not_complete() {
        let mut c = Coverage::new();
        c.insert(0, 9);
        assert!(!c.is_complete(10));
        assert_eq!(c.covered_bytes(), 9);
    }

    #[test]
    fn many_small_fragments() {
        let mut c = Coverage::new();
        for i in 0..100 {
            c.insert(i * 10, 10);
        }
        assert!(c.is_complete(1000));
        assert_eq!(c.interval_count(), 1);
    }

    #[test]
    fn many_small_fragments_reverse() {
        let mut c = Coverage::new();
        for i in (0..100).rev() {
            c.insert(i * 10, 10);
        }
        assert!(c.is_complete(1000));
        assert_eq!(c.interval_count(), 1);
    }
}
