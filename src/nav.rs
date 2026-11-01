//! 光标位置历史：在读过 / 写过的位置之间前后跳转。
//!
//! 仿 IDEA 的 Navigate Back / Forward。关键是**不能**每按一下方向键就记一条，
//! 所以这里加了「停留」判定：光标在某个位置待够 [`DWELL`] 才算一个真正的落脚点。
//!
//! 另外跳转回来之后的那一次光标同步必须忽略，否则会把刚跳到的位置又记一遍，
//! 变成来回横跳。这个由 `landed` 标志挡住。

use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NavPoint {
    pub path: PathBuf,
    pub byte: usize,
    pub line: usize,
}

/// 历史最多保留多少个落脚点，超了从最旧的开始丢。
pub const MAX_POINTS: usize = 200;

/// 光标在同一个位置停留多久才算「停下来了」。
pub const DWELL: Duration = Duration::from_millis(350);

pub struct CaretHistory {
    points: Vec<NavPoint>,
    /// 当前所在条目下标；`points[cursor]` 就是「现在这个位置」
    cursor: usize,
    /// 正在等待确认的位置
    pending: Option<NavPoint>,
    since: Instant,
    /// 刚通过历史跳转落到某个点，下一次同步到同一位置时不要重复记录
    landed: bool,
}

impl Default for CaretHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl CaretHistory {
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            cursor: 0,
            pending: None,
            since: Instant::now(),
            landed: false,
        }
    }

    /// 每帧把当前光标位置喂进来。
    pub fn poll(&mut self, point: NavPoint) {
        if self.points.is_empty() {
            self.points.push(point.clone());
            self.cursor = 0;
            self.pending = Some(point);
            self.since = Instant::now();
            return;
        }

        if self.landed {
            if self.points.get(self.cursor) == Some(&point) {
                // 还停在刚跳到的位置，别记录
                self.pending = None;
                return;
            }
            self.landed = false;
        }

        match &self.pending {
            Some(p) if *p == point => {
                if self.since.elapsed() >= DWELL && self.points.get(self.cursor) != Some(&point) {
                    self.push(point);
                }
            }
            _ => {
                self.pending = Some(point);
                self.since = Instant::now();
            }
        }
    }

    /// 还有没有「等待确认」的位置没落进历史（需要再等一会儿才记录）。
    pub fn waiting(&self) -> bool {
        match &self.pending {
            Some(p) => self.points.get(self.cursor) != Some(p),
            None => false,
        }
    }

    fn push(&mut self, point: NavPoint) {
        // 同一行内左右移动不单独记一个点，只把当前点更新到最新偏移，
        // 否则「后退」会变成在同一行里挪几列。
        if let Some(cur) = self.points.get(self.cursor) {
            if cur.path == point.path && cur.line == point.line {
                if self.cursor + 1 == self.points.len() {
                    self.points[self.cursor] = point;
                }
                return;
            }
        }
        // 在历史中间又走了新路，前面的分支作废（和浏览器一致）
        self.points.truncate(self.cursor + 1);
        self.points.push(point);
        if self.points.len() > MAX_POINTS {
            self.points.remove(0);
        }
        self.cursor = self.points.len() - 1;
    }

    pub fn back(&mut self) -> Option<NavPoint> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        self.landed = true;
        self.pending = None;
        self.points.get(self.cursor).cloned()
    }

    pub fn forward(&mut self) -> Option<NavPoint> {
        if self.cursor + 1 >= self.points.len() {
            return None;
        }
        self.cursor += 1;
        self.landed = true;
        self.pending = None;
        self.points.get(self.cursor).cloned()
    }

    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.points.len()
    }

    pub fn back_count(&self) -> usize {
        self.cursor
    }

    pub fn forward_count(&self) -> usize {
        self.points.len().saturating_sub(self.cursor + 1)
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: usize) -> NavPoint {
        NavPoint {
            path: PathBuf::from("a.txt"),
            byte: line * 10,
            line,
        }
    }

    /// 直接跳过等待：测试里不能真 sleep 350ms
    fn settle(h: &mut CaretHistory, point: NavPoint) {
        h.poll(point.clone());
        h.since = Instant::now() - DWELL - Duration::from_millis(1);
        h.poll(point);
    }

    #[test]
    fn first_position_is_recorded_immediately() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        assert_eq!(h.len(), 1);
        assert!(!h.can_back());
        assert!(!h.can_forward());
    }

    #[test]
    fn records_only_after_dwell() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        // 光标扫过去，没有停留，不该记
        h.poll(p(2));
        h.poll(p(3));
        assert_eq!(h.len(), 1);
        assert!(h.waiting());

        settle(&mut h, p(3));
        assert_eq!(h.len(), 2);
        assert!(h.can_back());
    }

    #[test]
    fn same_line_movement_does_not_add_entry() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        settle(&mut h, p(5));
        let before = h.len();
        // 同一行里挪列
        settle(&mut h, NavPoint { path: PathBuf::from("a.txt"), byte: 57, line: 5 });
        assert_eq!(h.len(), before);
        // 但当前点更新成了新偏移，后退再前进应落在新偏移上
        h.back();
        assert_eq!(h.forward().unwrap().byte, 57);
    }

    #[test]
    fn back_and_forward_walk_the_history() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        settle(&mut h, p(5));
        settle(&mut h, p(9));
        assert_eq!(h.len(), 3);

        assert_eq!(h.back().unwrap().line, 5);
        assert_eq!(h.back().unwrap().line, 1);
        assert!(h.back().is_none());

        assert_eq!(h.forward().unwrap().line, 5);
        assert_eq!(h.forward().unwrap().line, 9);
        assert!(h.forward().is_none());
    }

    #[test]
    fn landing_does_not_duplicate() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        settle(&mut h, p(5));
        let target = h.back().unwrap();
        // 跳转后编辑器把光标设回 target，多次同步也不能新增条目
        for _ in 0..5 {
            h.poll(target.clone());
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.forward_count(), 1);
    }

    #[test]
    fn moving_after_back_drops_forward_branch() {
        let mut h = CaretHistory::new();
        h.poll(p(1));
        settle(&mut h, p(5));
        settle(&mut h, p(9));
        h.back(); // 回到 5
        settle(&mut h, p(7)); // 在 5 的位置又去了 7
        assert_eq!(h.len(), 3);
        assert!(!h.can_forward());
    }
}
