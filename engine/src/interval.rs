//! 有序、不相交、可合并的字节区间集合。
//!
//! 下载引擎用它记录"哪些字节已经落盘"。断点续传的事实来源就是它：
//! 已完成区间落库，重启后把未完成区间重新交给引擎。

/// 半开区间 `[start, end)`。
pub type Range = (u64, u64);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntervalSet {
    /// 有序、两两不相交、互不相邻的区间列表。
    ranges: Vec<Range>,
}

impl IntervalSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 插入区间 `[start, end)`，与已有区间合并。
    /// 空区间或 `end <= start` 会被忽略。
    pub fn insert(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }

        // 第一个可能与新区间相交或相接的下标：end >= start 的第一个。
        let idx = self.ranges.partition_point(|&(_s, e)| e < start);

        let mut new_start = start;
        let mut new_end = end;
        let mut first = idx;
        let mut removed = 0usize;

        // 向前：前一个区间可能与新区间相交或相接。
        if idx > 0 {
            let (s, e) = self.ranges[idx - 1];
            if e >= start {
                new_start = new_start.min(s);
                new_end = new_end.max(e);
                first = idx - 1;
                removed += 1;
            }
        }

        // 向后：吃掉所有与新区间（或已合并部分）相交的区间。
        for i in idx..self.ranges.len() {
            let (s, e) = self.ranges[i];
            if s > new_end {
                break;
            }
            new_start = new_start.min(s);
            new_end = new_end.max(e);
            removed += 1;
        }

        self.ranges
            .splice(first..first + removed, [(new_start, new_end)]);
    }

    /// 已覆盖的字节总数。
    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|(s, e)| e - s).sum()
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    /// `[0, total)` 中尚未被覆盖的部分。
    pub fn complement(&self, total: u64) -> Vec<Range> {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        for &(s, e) in &self.ranges {
            if s > cursor {
                out.push((cursor, s));
            }
            cursor = cursor.max(e);
        }
        if cursor < total {
            out.push((cursor, total));
        }
        out
    }

    /// `[start, end)` 是否被完整覆盖。
    pub fn covers(&self, start: u64, end: u64) -> bool {
        if end <= start {
            return true;
        }
        self.ranges
            .iter()
            .any(|&(s, e)| s <= start && e >= end)
    }

    pub fn contains(&self, point: u64) -> bool {
        self.ranges.iter().any(|&(s, e)| s <= point && point < e)
    }

    pub fn to_vec(&self) -> Vec<Range> {
        self.ranges.clone()
    }

    pub fn from_ranges<I: IntoIterator<Item = Range>>(iter: I) -> Self {
        let mut set = Self::new();
        for (s, e) in iter {
            set.insert(s, e);
        }
        set
    }
}

impl std::fmt::Display for IntervalSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.ranges.iter().map(|(s, e)| format!("{s}-{e}")).collect();
        write!(f, "[{}]", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_and_merges() {
        let mut set = IntervalSet::new();
        set.insert(10, 20);
        set.insert(0, 5);
        assert_eq!(set.ranges(), &[(0, 5), (10, 20)]);

        // 相接的区间应当合并
        set.insert(5, 10);
        assert_eq!(set.ranges(), &[(0, 20)]);
        assert_eq!(set.total(), 20);
    }

    #[test]
    fn overlapping_insert_merges_three_ways() {
        let mut set = IntervalSet::new();
        set.insert(10, 20);
        set.insert(0, 15);
        assert_eq!(set.ranges(), &[(0, 20)]);

        let mut set = IntervalSet::new();
        set.insert(10, 20);
        set.insert(15, 30);
        assert_eq!(set.ranges(), &[(10, 30)]);
    }

    #[test]
    fn ignores_empty_range() {
        let mut set = IntervalSet::new();
        set.insert(5, 5);
        set.insert(9, 3);
        assert!(set.is_empty());
    }

    #[test]
    fn complement_fills_gaps() {
        let set = IntervalSet::from_ranges([(0, 10), (20, 30)]);
        assert_eq!(set.complement(40), vec![(10, 20), (30, 40)]);
        assert_eq!(set.complement(30), vec![(10, 20)]);
    }

    #[test]
    fn covers_and_contains() {
        let set = IntervalSet::from_ranges([(10, 20)]);
        assert!(set.covers(12, 18));
        assert!(!set.covers(5, 15));
        assert!(set.contains(19));
        assert!(!set.contains(20));
    }

    #[test]
    fn total_accumulates() {
        let mut set = IntervalSet::new();
        for i in 0..100u64 {
            set.insert(i * 2, i * 2 + 1);
        }
        assert_eq!(set.total(), 100);
        assert_eq!(set.len(), 100);
    }
}
