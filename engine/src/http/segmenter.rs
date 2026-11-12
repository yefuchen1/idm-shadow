//! 动态重分割的核心。
//!
//! 引擎把"还没下载的字节"表示成若干互不相交的**区间**（`Work`）。
//! 每个区间要么待分配（`owner = None`），要么由某个 worker 持有。
//! worker 从自己区间的起点向后连续推进，推进过的字节记入 `done`。
//!
//! 不变量（由属性测试保证）：
//! - `done` 与所有 `Work` 区间互不相交；
//! - `done` 并上所有 `Work` 区间等于 `[0, total)`，不重叠、不留洞。
//!
//! 分段边界在下载过程中会移动：慢的 worker 被淘汰、区间被切走；
//! 文件尾部（关键路径）被细分给最快的几个 worker。

use crate::interval::IntervalSet;

pub type WorkerId = usize;

/// 一段尚未下载的字节区间 `[start, end)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Work {
    pub start: u64,
    pub end: u64,
    pub owner: Option<WorkerId>,
}

impl Work {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

#[derive(Debug, Clone)]
pub struct Segmenter {
    total: u64,
    /// 未下载区间，按 start 升序，两两不相交。
    works: Vec<Work>,
    /// 已完成区间。
    done: IntervalSet,
}

impl Segmenter {
    /// 从零开始的任务。
    pub fn new(total: u64) -> Self {
        let mut works = Vec::new();
        if total > 0 {
            works.push(Work {
                start: 0,
                end: total,
                owner: None,
            });
        }
        Self {
            total,
            works,
            done: IntervalSet::new(),
        }
    }

    /// 断点续传：`done` 是上次已经落盘的字节，其余区间重新进入待分配池。
    pub fn resume(total: u64, done: IntervalSet) -> Self {
        let works = done
            .complement(total)
            .into_iter()
            .map(|(start, end)| Work {
                start,
                end,
                owner: None,
            })
            .collect();
        Self {
            total,
            works,
            done,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn downloaded(&self) -> u64 {
        self.done.total()
    }

    pub fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.done.total())
    }

    pub fn works(&self) -> &[Work] {
        &self.works
    }

    pub fn done(&self) -> &IntervalSet {
        &self.done
    }

    /// 某个 worker 当前持有的区间。
    pub fn current_range(&self, worker: WorkerId) -> Option<(u64, u64)> {
        self.works
            .iter()
            .find(|w| w.owner == Some(worker))
            .map(|w| (w.start, w.end))
    }

    /// 某个区间的持有者。
    pub fn owner_of(&self, start: u64) -> Option<WorkerId> {
        self.works
            .iter()
            .find(|w| start >= w.start && start < w.end)
            .and_then(|w| w.owner)
    }

    /// worker 从自己区间的当前起点下载了 `n` 字节。
    ///
    /// 返回实际计入完成集的字节数：若区间已被重分割缩短，超出部分
    /// 属于别人的区间，不予计入（那些字节由新持有者负责）。
    pub fn complete(&mut self, worker: WorkerId, n: u64) -> u64 {
        let idx = match self.works.iter().position(|w| w.owner == Some(worker)) {
            Some(i) => i,
            None => return 0,
        };
        let avail = self.works[idx].len();
        let n = n.min(avail);
        if n == 0 {
            return 0;
        }
        let start = self.works[idx].start;
        self.done.insert(start, start + n);
        self.works[idx].start = start + n;
        if self.works[idx].is_empty() {
            self.works.remove(idx);
        }
        n
    }

    /// worker 放弃自己手上的区间，区间回到待分配池。
    /// 用于淘汰拖后腿的连接，或连接出错后重新排队。
    pub fn release(&mut self, worker: WorkerId) {
        if let Some(w) = self.works.iter_mut().find(|w| w.owner == Some(worker)) {
            w.owner = None;
        }
    }

    /// 给空闲 worker 分配一段未下载的区间，优先靠近文件尾部。
    ///
    /// worker 已经持有区间时返回 `None`——一个 worker 同时只做一段。
    pub fn assign(&mut self, worker: WorkerId, max_len: u64) -> Option<(u64, u64)> {
        if self.current_range(worker).is_some() {
            return None;
        }
        let idx = self
            .works
            .iter()
            .enumerate()
            .filter(|(_, w)| w.owner.is_none())
            .max_by_key(|(_, w)| w.end)
            .map(|(i, _)| i)?;

        let (w_start, w_end) = (self.works[idx].start, self.works[idx].end);
        let take = max_len.min(w_end - w_start).max(1);
        let new_start = w_end - take;

        self.works[idx].end = new_start;
        if self.works[idx].is_empty() {
            self.works.remove(idx);
        }

        self.insert_work(Work {
            start: new_start,
            end: w_end,
            owner: Some(worker),
        });
        let _ = w_start;
        Some((new_start, w_end))
    }

    /// 关键路径保护：把文件尾部切给若干空闲且更快的 worker。
    ///
    /// `weights` 是 `(worker, 速率)` 列表。返回新分配的 `(worker, 区间)`。
    /// 尾部区间的原持有者保留靠前的一段（它正在从那里往后读），
    /// 其余按速率比例分给空闲 worker。
    pub fn share_tail(&mut self, weights: &[(WorkerId, f64)]) -> Vec<(WorkerId, (u64, u64))> {
        let mut out = Vec::new();
        let tail_idx = match self
            .works
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| w.end)
            .map(|(i, _)| i)
        {
            Some(i) => i,
            None => return out,
        };

        let (t_start, t_end) = (self.works[tail_idx].start, self.works[tail_idx].end);
        let len = t_end - t_start;
        if len == 0 {
            return out;
        }

        let idle: Vec<(WorkerId, f64)> = weights
            .iter()
            .copied()
            .filter(|(w, _)| self.current_range(*w).is_none())
            .collect();
        if idle.is_empty() {
            return out;
        }

        match self.works[tail_idx].owner {
            None => {
                let (w, _) = idle
                    .iter()
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .copied()
                    .unwrap();
                self.works[tail_idx].owner = Some(w);
                out.push((w, (t_start, t_end)));
            }
            Some(owner) => {
                let owner_weight = weights
                    .iter()
                    .find(|(w, _)| *w == owner)
                    .map(|(_, v)| *v)
                    .unwrap_or(1.0)
                    .max(1e-6);

                let mut recipients: Vec<(WorkerId, f64)> = idle
                    .iter()
                    .copied()
                    .filter(|(w, _)| *w != owner)
                    .collect();
                recipients.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                recipients.truncate(3);

                if recipients.is_empty() || len < (recipients.len() + 1) as u64 {
                    return out;
                }

                // 按速率比例累出切点
                let mut prefix = Vec::with_capacity(recipients.len() + 1);
                let mut acc = owner_weight;
                prefix.push(acc);
                for (_, w) in &recipients {
                    acc += *w;
                    prefix.push(acc);
                }
                let total_w = acc;
                if total_w <= 0.0 {
                    return out;
                }

                let parts = recipients.len() + 1;
                let mut bounds = Vec::with_capacity(parts + 1);
                bounds.push(t_start);
                for i in 1..parts {
                    // prefix[i - 1] 是第 i 块之前的累积权重
                    let mut cut = t_start + ((len as f64) * (prefix[i - 1] / total_w)) as u64;
                    let last = *bounds.last().unwrap();
                    if cut <= last {
                        cut = last + 1;
                    }
                    if cut >= t_end {
                        cut = t_end;
                    }
                    bounds.push(cut);
                }
                bounds.push(t_end);

                self.works.remove(tail_idx);

                self.insert_work(Work {
                    start: bounds[0],
                    end: bounds[1],
                    owner: Some(owner),
                });

                for (i, (w, _)) in recipients.iter().enumerate() {
                    let start = bounds[i + 1];
                    let end = bounds[i + 2];
                    if start >= end {
                        continue;
                    }
                    self.insert_work(Work {
                        start,
                        end,
                        owner: Some(*w),
                    });
                    out.push((*w, (start, end)));
                }
            }
        }

        out
    }

    /// 保持 works 按 start 升序。
    fn insert_work(&mut self, work: Work) {
        let pos = self.works.partition_point(|w| w.start < work.start);
        self.works.insert(pos, work);
    }

    /// 校验不变式。返回 Err 说明区间出现了重叠或空洞。
    pub fn check_invariants(&self) -> Result<(), String> {
        let expected = self.done.complement(self.total);

        // works 之间允许相邻（不同持有者），但不允许重叠。
        for pair in self.works.windows(2) {
            if pair[0].end > pair[1].start {
                return Err(format!("区间重叠：{:?} 与 {:?}", pair[0], pair[1]));
            }
        }

        // 把相邻的 works 合并成极大区间，再和补集比较。
        let mut actual: Vec<(u64, u64)> = Vec::with_capacity(self.works.len());
        for w in &self.works {
            match actual.last_mut() {
                Some(last) if w.start <= last.1 => last.1 = last.1.max(w.end),
                _ => actual.push((w.start, w.end)),
            }
        }

        if actual != expected {
            return Err(format!(
                "区间划分不一致：期望 {:?}，实际 {:?}（done={}）",
                expected, actual, self.done
            ));
        }

        let undone: u64 = actual.iter().map(|(s, e)| e - s).sum();
        if self.done.total() + undone != self.total {
            return Err("已完成字节数与未完成字节数之和 != 总大小".to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_covers_everything() {
        let seg = Segmenter::new(1000);
        assert_eq!(seg.remaining(), 1000);
        assert_eq!(seg.downloaded(), 0);
        seg.check_invariants().unwrap();
    }

    #[test]
    fn assign_takes_from_the_tail() {
        let mut seg = Segmenter::new(1000);
        let r = seg.assign(0, 100).unwrap();
        assert_eq!(r, (900, 1000));
        seg.check_invariants().unwrap();

        let r = seg.assign(1, 100).unwrap();
        assert_eq!(r, (800, 900));
        seg.check_invariants().unwrap();
    }

    #[test]
    fn complete_marks_bytes_done() {
        let mut seg = Segmenter::new(1000);
        seg.assign(0, 100).unwrap();
        assert_eq!(seg.complete(0, 40), 40);
        assert_eq!(seg.downloaded(), 40);
        assert_eq!(seg.current_range(0), Some((940, 1000)));
        seg.check_invariants().unwrap();
    }

    #[test]
    fn complete_clamps_to_owned_range() {
        let mut seg = Segmenter::new(1000);
        seg.assign(0, 100).unwrap();
        assert_eq!(seg.complete(0, 250), 100);
        assert_eq!(seg.downloaded(), 100);
        seg.check_invariants().unwrap();
    }

    #[test]
    fn release_returns_range_to_pool() {
        let mut seg = Segmenter::new(1000);
        seg.assign(0, 100).unwrap();
        seg.release(0);
        assert_eq!(seg.current_range(0), None);
        seg.check_invariants().unwrap();
    }

    #[test]
    fn share_tail_splits_between_fast_workers() {
        let mut seg = Segmenter::new(1000);
        seg.assign(0, 1000).unwrap();
        assert_eq!(seg.current_range(0), Some((0, 1000)));

        let out = seg.share_tail(&[(0, 1.0), (1, 3.0)]);
        assert_eq!(out, vec![(1, (250, 1000))]);
        assert_eq!(seg.current_range(0), Some((0, 250)));
        assert_eq!(seg.current_range(1), Some((250, 1000)));
        seg.check_invariants().unwrap();
    }

    #[test]
    fn share_tail_gives_unowned_tail_to_fastest() {
        let mut seg = Segmenter::new(1000);
        let out = seg.share_tail(&[(0, 1.0), (1, 5.0)]);
        assert_eq!(out, vec![(1, (0, 1000))]);
        seg.check_invariants().unwrap();
    }

    #[test]
    fn resume_from_completed_intervals() {
        let mut done = IntervalSet::new();
        done.insert(0, 300);
        done.insert(500, 1000);
        let mut seg = Segmenter::resume(1000, done);
        assert_eq!(seg.downloaded(), 800);
        assert_eq!(seg.remaining(), 200);
        seg.check_invariants().unwrap();

        let r = seg.assign(0, 1000).unwrap();
        assert_eq!(r, (300, 500));
        seg.check_invariants().unwrap();
    }

    #[test]
    fn many_workers_keep_the_file_consistent() {
        let mut seg = Segmenter::new(10_000);
        for w in 0..8 {
            seg.assign(w, 500).unwrap();
        }
        for _ in 0..50 {
            for w in 0..8 {
                seg.complete(w, 10);
            }
            seg.share_tail(&[(0, 1.0), (1, 2.0), (2, 0.5)]);
        }
        seg.check_invariants().unwrap();
    }

    /// 把整个文件真的下完：完成字节数必须等于总大小。
    #[test]
    fn full_download_covers_every_byte() {
        let total = 5_000u64;
        let mut seg = Segmenter::new(total);
        let mut guard = 0;

        while seg.remaining() > 0 && guard < 10_000 {
            guard += 1;
            for w in 0..4 {
                if seg.current_range(w).is_none() {
                    // 没有待分配的区间时，这个 worker 只能先闲着
                    let _ = seg.assign(w, 300);
                }
                seg.complete(w, 100);
            }
            seg.share_tail(&[(0, 3.0), (1, 1.0), (2, 2.0), (3, 0.5)]);
        }

        assert_eq!(seg.downloaded(), total);
        assert_eq!(seg.remaining(), 0);
        assert!(seg.done().covers(0, total));
        seg.check_invariants().unwrap();
    }

    mod property {
        use super::*;
        use proptest::prelude::*;

        const WORKERS: usize = 4;

        #[derive(Debug, Clone)]
        enum Op {
            Complete(usize, u64),
            Release(usize),
            Assign(usize, u64),
            ShareTail(Vec<(usize, f64)>),
        }

        fn arb_op() -> impl Strategy<Value = Op> {
            prop_oneof![
                (0usize..WORKERS, 1u64..1000).prop_map(|(w, n)| Op::Complete(w, n)),
                (0usize..WORKERS).prop_map(Op::Release),
                (0usize..WORKERS, 1u64..5000).prop_map(|(w, l)| Op::Assign(w, l)),
                prop::collection::vec((0usize..WORKERS, 0.1f64..10.0), 1..4)
                    .prop_map(Op::ShareTail),
            ]
        }

        proptest! {
            /// 无论怎么折腾，区间划分都不重叠、不留洞。
            #[test]
            fn invariants_survive_random_operations(ops in prop::collection::vec(arb_op(), 1..300)) {
                let total = 100_000u64;
                let mut seg = Segmenter::new(total);

                for op in ops {
                    match op {
                        Op::Complete(w, n) => { seg.complete(w, n); }
                        Op::Release(w) => seg.release(w),
                        Op::Assign(w, l) => { seg.assign(w, l); }
                        Op::ShareTail(weights) => { seg.share_tail(&weights); }
                    }

                    prop_assert!(
                        seg.check_invariants().is_ok(),
                        "不变式被破坏: {} / {:?}",
                        seg.check_invariants().unwrap_err(),
                        seg.works()
                    );
                }
            }

            /// 已完成 + 未完成 恒等于总大小。
            #[test]
            fn byte_accounting_is_exact(ops in prop::collection::vec(arb_op(), 1..200)) {
                let total = 50_000u64;
                let mut seg = Segmenter::new(total);

                for op in ops {
                    match op {
                        Op::Complete(w, n) => { seg.complete(w, n); }
                        Op::Release(w) => seg.release(w),
                        Op::Assign(w, l) => { seg.assign(w, l); }
                        Op::ShareTail(weights) => { seg.share_tail(&weights); }
                    }
                }

                let undone: u64 = seg.works().iter().map(|w| w.len()).sum();
                prop_assert_eq!(seg.downloaded() + undone, total);
            }

            /// 续传之后，未完成区间恰好是原来的补集。
            #[test]
            fn resume_matches_complement(gaps in prop::collection::vec((0u64..1000, 0u64..500), 0..40)) {
                let total = 10_000u64;
                let mut done = IntervalSet::new();
                for (s, len) in gaps {
                    let s = s.min(total);
                    done.insert(s, (s + len).min(total));
                }

                let seg = Segmenter::resume(total, done.clone());
                let undone: u64 = seg.works().iter().map(|w| w.len()).sum();
                prop_assert_eq!(seg.downloaded(), done.total());
                prop_assert_eq!(seg.downloaded() + undone, total);
                prop_assert!(seg.check_invariants().is_ok());
            }
        }
    }
}
