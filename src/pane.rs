//! tmux 风格的分屏布局树。
//!
//! 一棵二叉树：`Leaf` 是一个面板，`Split` 把一块区域按 `Axis` 切成两半，
//! 面板自己的内容（editor / terminal / dashboard）由调用方作为负载带进来。
//! 这个模块只管「有哪些面板、谁在焦点上、各自占哪块矩形、分隔线拖到哪」，
//! 不认识 egui 的控件，也不认识应用里的文件。
//!
//! 和 tmux 对齐的几条行为：
//! - 分屏发生在被指定的那块面板上，新面板拿到焦点
//! - 关闭面板时父 `Split` 自动收起，兄弟顶上来，区域不留空洞
//! - 只剩一块面板时树自然退化成一个 `Leaf`
//!
//! id 用自增 `u64` 而不是 `egui::Id`：`Id::from_hash_of` 的哈希种子每进程随机，
//! 「同一条 Split 对应同一条分隔线」这种跨帧关系就没法断言，也没法在测试里比对。
//! 需要 `egui::Id` 的地方自己 `Id::new(("pane", id.0))` 派生即可。

use eframe::egui::{pos2, Rect};

/// 面板身份。在一棵树的整个生命周期内唯一且稳定。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(pub u64);

/// 一条分隔线（即一个 `Split` 节点）的身份。拖动时要靠它找回对应的那个比例。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SplitId(pub u64);

/// 分割方向。命名看「子面板的排列轴向」：
/// `Horizontal` = 左右并排（分隔线是竖的），`Vertical` = 上下堆叠（分隔线是横的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// 方位键找邻居用的四个方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

pub enum Node<T> {
    /// `payload` 在面板被关闭的那一瞬间被取走，留下 `None` 表示「这块已空」；
    /// 紧接着 [`PaneTree::remove`] 会做一次 [`collapse`] 把空位收掉，
    /// 所以调用方不会看到空叶子。
    Leaf { id: PaneId, payload: Option<T> },
    Split {
        id: SplitId,
        axis: Axis,
        /// `a` 分到可分配长度的比例，0~1。
        ratio: f32,
        a: Box<Node<T>>,
        b: Box<Node<T>>,
    },
}

/// 一块面板占的矩形。
#[derive(Clone, Copy, Debug)]
pub struct PaneRect {
    pub id: PaneId,
    pub rect: Rect,
}

/// 一条分隔线占的矩形，外加「比例是相对哪段距离算的」，拖动时直接换算成新比例。
#[derive(Clone, Copy, Debug)]
pub struct DividerRect {
    pub id: SplitId,
    pub axis: Axis,
    pub rect: Rect,
    /// 这条分割的起点（`Horizontal` 取 x，`Vertical` 取 y）。
    pub start: f32,
    /// `a` + `b` 可分配的总长，不含分隔线自身宽度。
    pub extent: f32,
}

/// [`PaneTree::layout`] 的结果。
#[derive(Clone, Debug, Default)]
pub struct TreeLayout {
    pub panes: Vec<PaneRect>,
    pub dividers: Vec<DividerRect>,
}

impl TreeLayout {
    pub fn pane_rect(&self, id: PaneId) -> Option<Rect> {
        self.panes.iter().find(|p| p.id == id).map(|p| p.rect)
    }
}

/// 分隔线热区宽度（左右、上下共用）。
pub const DIVIDER: f32 = 6.0;

/// 拖到多窄就停住，免得面板被压成 0 宽后拖不回来。
const MIN_RATIO: f32 = 0.05;

/// 方向键找邻居时允许的边界重叠容差：两块面板紧贴时浮点上会差一点点。
const GAP_TOLERANCE: f32 = 1.0;

impl<T> Default for PaneTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PaneTree<T> {
    root: Option<Node<T>>,
    next_pane: u64,
    next_split: u64,
    focus: Option<PaneId>,
}

impl<T> PaneTree<T> {
    pub fn new() -> Self {
        Self {
            root: None,
            next_pane: 1,
            next_split: 1,
            focus: None,
        }
    }

    pub fn count(&self) -> usize {
        self.pane_ids().len()
    }

    /// 空树时放入唯一一块面板。已有面板时什么都不做，只返回当前焦点。
    pub fn set_root(&mut self, payload: T) -> PaneId {
        if let Some(root) = &self.root {
            return self.focus.unwrap_or_else(|| first_leaf_of(root));
        }
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        self.root = Some(Node::Leaf {
            id,
            payload: Some(payload),
        });
        self.focus = Some(id);
        id
    }

    pub fn focus(&self) -> Option<PaneId> {
        self.focus
    }

    /// 把焦点交给某块面板。面板不存在时返回 `false` 并保持原焦点。
    pub fn set_focus(&mut self, id: PaneId) -> bool {
        if self.contains(id) {
            self.focus = Some(id);
            true
        } else {
            false
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        self.root.as_ref().is_some_and(|n| n.contains(id))
    }

    pub fn payload_of(&self, id: PaneId) -> Option<&T> {
        self.root.as_ref().and_then(|n| n.payload_of(id))
    }

    pub fn payload_of_mut(&mut self, id: PaneId) -> Option<&mut T> {
        self.root.as_mut().and_then(|n| n.payload_of_mut(id))
    }

    /// 按 DFS（先 `a` 后 `b`）顺序列出所有面板 id。
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.collect_ids(&mut out);
        }
        out
    }

    /// 在 `target` 这块面板上再切一刀，新面板落在 `b` 侧（`Horizontal` 的右边 /
    /// `Vertical` 的下边）。`target` 不存在时返回 `None` 且不改树。
    pub fn split(&mut self, target: PaneId, axis: Axis, payload: T) -> Option<PaneId> {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        let split_id = SplitId(self.next_split);
        self.next_split += 1;
        let leftover = match self.root.as_mut() {
            Some(root) => root.split_leaf(target, split_id, axis, id, Some(payload)),
            None => Some(payload),
        };
        if leftover.is_some() {
            return None;
        }
        self.focus = Some(id);
        Some(id)
    }

    /// 关闭面板，返回它的负载（调用方负责善后，比如杀进程）。
    ///
    /// 父 `Split` 收起、兄弟顶上来；焦点交给 DFS 顺序里的后一块，已经是最后一块
    /// 就交给前一块。关掉最后一块面板后树变空。
    pub fn remove(&mut self, id: PaneId) -> Option<T> {
        let ids = self.pane_ids();
        let at = ids.iter().position(|p| *p == id)?;
        let payload = self
            .root
            .as_mut()
            .and_then(|root| root.take_payload(id));
        if payload.is_some() {
            if let Some(root) = self.root.take() {
                self.root = collapse(root);
            }
        }
        self.focus = match &self.root {
            Some(root) => {
                let after = ids.get(at + 1).copied().filter(|p| root.contains(*p));
                let before = ids
                    .get(at.saturating_sub(1))
                    .copied()
                    .filter(|p| root.contains(*p));
                Some(after.or(before).unwrap_or_else(|| first_leaf_of(root)))
            }
            None => None,
        };
        payload
    }

    /// DFS 顺序循环切焦点，`delta` 可以为负。
    pub fn focus_cycle(&mut self, delta: isize) -> Option<PaneId> {
        let ids = self.pane_ids();
        if ids.is_empty() {
            self.focus = None;
            return None;
        }
        let len = ids.len() as isize;
        let base = self
            .focus
            .and_then(|f| ids.iter().position(|p| *p == f))
            .unwrap_or(0) as isize;
        let next = ((base + delta) % len + len) % len;
        let id = ids[next as usize];
        self.focus = Some(id);
        Some(id)
    }

    /// 按方位切焦点，返回切完之后的焦点；走不出去时保持原地
    /// （和 tmux / i3 一样按几何找，所以一整列通高的面板往下按不会跳到右下角）。
    ///
    /// 需要 [`PaneTree::layout`] 的结果，调用方每帧本来就要算它。
    pub fn focus_dir(&mut self, layout: &TreeLayout, dir: Dir) -> Option<PaneId> {
        let cur_id = self.focus?;
        let cur = layout.pane_rect(cur_id)?;
        let mut best: Option<(f32, f32, PaneId)> = None;
        for pane in &layout.panes {
            if pane.id == cur_id {
                continue;
            }
            let other = pane.rect;
            // 主轴上必须在 `dir` 那一侧，副轴偏移越小越优先
            let (gap, cross) = match dir {
                Dir::Right => (
                    other.min.x - cur.max.x,
                    (other.center().y - cur.center().y).abs(),
                ),
                Dir::Left => (
                    cur.min.x - other.max.x,
                    (other.center().y - cur.center().y).abs(),
                ),
                Dir::Down => (
                    other.min.y - cur.max.y,
                    (other.center().x - cur.center().x).abs(),
                ),
                Dir::Up => (
                    cur.min.y - other.max.y,
                    (other.center().x - cur.center().x).abs(),
                ),
            };
            if gap < -GAP_TOLERANCE {
                continue;
            }
            let better = match best {
                None => true,
                Some((best_cross, best_gap, _)) => {
                    cross < best_cross - GAP_TOLERANCE
                        || ((cross - best_cross).abs() <= GAP_TOLERANCE && gap < best_gap)
                }
            };
            if better {
                best = Some((cross, gap, pane.id));
            }
        }
        let id = match best {
            Some((_, _, id)) => id,
            None => return Some(cur_id),
        };
        self.focus = Some(id);
        Some(id)
    }

    /// 算出每块面板的矩形和每条分隔线的位置。
    pub fn layout(&self, area: Rect, divider: f32) -> TreeLayout {
        let mut out = TreeLayout::default();
        if let Some(root) = &self.root {
            root.layout_into(area, divider, &mut out);
        }
        out
    }

    /// 拖动分隔线：把指针位置（轴向坐标）换算成新比例写回。
    ///
    /// 返回 `false` 表示这条分隔线对应的 `Split` 已经不在了（面板刚被关掉）。
    pub fn drag_divider(&mut self, id: SplitId, divider: &DividerRect, pointer: f32) -> bool {
        if divider.extent <= 0.0 {
            return false;
        }
        self.set_ratio(id, (pointer - divider.start) / divider.extent)
    }

    pub fn set_ratio(&mut self, id: SplitId, ratio: f32) -> bool {
        let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        match self.root.as_mut() {
            Some(root) => root.set_ratio(id, ratio),
            None => false,
        }
    }

    /// 按 DFS 顺序遍历每块面板的负载。
    pub fn foreach_payload_mut(&mut self, mut f: impl FnMut(PaneId, &mut T)) {
        for id in self.pane_ids() {
            if let Some(payload) = self.payload_of_mut(id) {
                f(id, payload);
            }
        }
    }
}

impl<T> Node<T> {
    fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf { id: own, .. } => *own == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    fn collect_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf { id, payload } => {
                if payload.is_some() {
                    out.push(*id);
                }
            }
            Node::Split { a, b, .. } => {
                a.collect_ids(out);
                b.collect_ids(out);
            }
        }
    }

    fn payload_of(&self, id: PaneId) -> Option<&T> {
        match self {
            Node::Leaf { id: own, payload } if *own == id => payload.as_ref(),
            Node::Split { a, b, .. } => a.payload_of(id).or_else(|| b.payload_of(id)),
            Node::Leaf { .. } => None,
        }
    }

    fn payload_of_mut(&mut self, id: PaneId) -> Option<&mut T> {
        match self {
            Node::Leaf { id: own, payload } if *own == id => payload.as_mut(),
            Node::Split { a, b, .. } => a.payload_of_mut(id).or_else(|| b.payload_of_mut(id)),
            Node::Leaf { .. } => None,
        }
    }

    /// 把 `target` 那片叶子换成一个 `Split`。成功返回 `None`；
    /// 没找到就原样把负载还回来，调用方据此判断要不要改焦点。
    fn split_leaf(
        &mut self,
        target: PaneId,
        split_id: SplitId,
        axis: Axis,
        new_id: PaneId,
        payload: Option<T>,
    ) -> Option<T> {
        match self {
            Node::Leaf { id, payload: old } => {
                if *id != target {
                    return payload;
                }
                let Some(payload) = payload else {
                    return None;
                };
                let old_leaf = Node::Leaf {
                    id: target,
                    payload: old.take(),
                };
                *self = Node::Split {
                    id: split_id,
                    axis,
                    ratio: 0.5,
                    a: Box::new(old_leaf),
                    b: Box::new(Node::Leaf {
                        id: new_id,
                        payload: Some(payload),
                    }),
                };
                None
            }
            Node::Split { a, b, .. } => match a.split_leaf(target, split_id, axis, new_id, payload)
            {
                None => None,
                Some(rest) => b.split_leaf(target, split_id, axis, new_id, Some(rest)),
            },
        }
    }

    /// 把负载取走，留下空叶子；结构性收起由外层的 [`collapse`] 负责。
    fn take_payload(&mut self, id: PaneId) -> Option<T> {
        match self {
            Node::Leaf { id: own, payload } => {
                if *own == id {
                    payload.take()
                } else {
                    None
                }
            }
            Node::Split { a, b, .. } => a.take_payload(id).or_else(|| b.take_payload(id)),
        }
    }

    fn layout_into(&self, area: Rect, divider: f32, out: &mut TreeLayout) {
        match self {
            Node::Leaf { id, payload } => {
                if payload.is_some() {
                    out.panes.push(PaneRect {
                        id: *id,
                        rect: area,
                    });
                }
            }
            Node::Split {
                id,
                axis,
                ratio,
                a,
                b,
            } => {
                let (ra, drect, rb, start, extent) = split_rects(area, *axis, *ratio, divider);
                out.dividers.push(DividerRect {
                    id: *id,
                    axis: *axis,
                    rect: drect,
                    start,
                    extent,
                });
                a.layout_into(ra, divider, out);
                b.layout_into(rb, divider, out);
            }
        }
    }

    fn set_ratio(&mut self, id: SplitId, ratio: f32) -> bool {
        match self {
            Node::Leaf { .. } => false,
            Node::Split {
                id: own,
                ratio: own_ratio,
                a,
                b,
                ..
            } => {
                if *own == id {
                    *own_ratio = ratio;
                    true
                } else {
                    a.set_ratio(id, ratio) || b.set_ratio(id, ratio)
                }
            }
        }
    }
}

fn first_leaf_of<T>(node: &Node<T>) -> PaneId {
    match node {
        Node::Leaf { id, .. } => *id,
        Node::Split { a, .. } => first_leaf_of(a),
    }
}

/// 收掉空叶子：`Split` 只剩一个非空孩子时用它顶替自己，全空则整棵消失。
fn collapse<T>(node: Node<T>) -> Option<Node<T>> {
    match node {
        Node::Leaf { id, payload } => payload.map(|p| Node::Leaf {
            id,
            payload: Some(p),
        }),
        Node::Split { id, axis, ratio, a, b } => match (collapse(*a), collapse(*b)) {
            (Some(a), Some(b)) => Some(Node::Split {
                id,
                axis,
                ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

/// 把一个矩形按 `axis` / `ratio` 切成两块，中间留 `divider` 宽。
///
/// 返回 `(a 区, 分隔线区, b 区, a 起点, 可分配总长)`。
fn split_rects(area: Rect, axis: Axis, ratio: f32, divider: f32) -> (Rect, Rect, Rect, f32, f32) {
    let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
    match axis {
        Axis::Horizontal => {
            let extent = (area.width() - divider).max(0.0);
            let x = area.min.x + extent * ratio;
            let split_x = x.min(area.max.x);
            (
                Rect::from_min_max(area.min, pos2(x, area.max.y)),
                Rect::from_min_max(pos2(x, area.min.y), pos2(x + divider, area.max.y)),
                Rect::from_min_max(pos2(split_x + divider, area.min.y), area.max),
                area.min.x,
                extent,
            )
        }
        Axis::Vertical => {
            let extent = (area.height() - divider).max(0.0);
            let y = area.min.y + extent * ratio;
            let split_y = y.min(area.max.y);
            (
                Rect::from_min_max(area.min, pos2(area.max.x, y)),
                Rect::from_min_max(pos2(area.min.x, y), pos2(area.max.x, y + divider)),
                Rect::from_min_max(pos2(area.min.x, split_y + divider), area.max),
                area.min.y,
                extent,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 600.0))
    }

    /// 三块面板：左边 editor，右边上下两块 terminal。
    fn three() -> (PaneTree<char>, [PaneId; 3]) {
        let mut tree = PaneTree::<char>::new();
        let a = tree.set_root('a');
        let b = tree.split(a, Axis::Horizontal, 'b').unwrap();
        let c = tree.split(b, Axis::Vertical, 'c').unwrap();
        (tree, [a, b, c])
    }

    #[test]
    fn split_adds_a_child_and_takes_focus() {
        let mut tree = PaneTree::<char>::new();
        let first = tree.set_root('a');
        assert_eq!(tree.focus(), Some(first));
        assert_eq!(tree.count(), 1);

        let second = tree.split(first, Axis::Horizontal, 'b').unwrap();
        assert_ne!(first, second);
        assert_eq!(tree.count(), 2);
        assert_eq!(tree.focus(), Some(second));
        assert_eq!(tree.payload_of(first), Some(&'a'));
        assert_eq!(tree.payload_of(second), Some(&'b'));
        // 新面板总在 b 侧，所以 DFS 顺序就是创建顺序
        assert_eq!(tree.pane_ids(), vec![first, second]);
    }

    #[test]
    fn split_on_missing_target_changes_nothing() {
        let (mut tree, ids) = three();
        assert_eq!(tree.split(PaneId(999), Axis::Horizontal, 'x'), None);
        assert_eq!(tree.count(), 3);
        assert_eq!(tree.pane_ids(), ids.to_vec());
    }

    #[test]
    fn set_root_on_nonempty_tree_keeps_focus() {
        let (mut tree, ids) = three();
        assert_eq!(Some(tree.set_root('z')), tree.focus());
        assert_eq!(tree.count(), 3);
        assert_eq!(tree.payload_of(ids[0]), Some(&'a'));
    }

    #[test]
    fn remove_collapses_the_parent_split() {
        let (mut tree, [a, b, c]) = three();
        tree.set_focus(a);
        assert_eq!(tree.remove(b), Some('b'));
        // b 和 c 同属一个 Split，去掉 b 后那个 Split 收起，只剩 a / c 左右两块
        assert_eq!(tree.count(), 2);
        assert_eq!(tree.pane_ids(), vec![a, c]);
        assert!(!tree.contains(b));
        assert_eq!(tree.focus(), Some(c));
        let layout = tree.layout(area(), DIVIDER);
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.dividers.len(), 1);
        // 收起后重新铺满整块区域
        assert_eq!(layout.pane_rect(c).unwrap().max, area().max);
    }

    #[test]
    fn remove_last_leaf_empties_the_tree() {
        let mut tree = PaneTree::<char>::new();
        let only = tree.set_root('a');
        assert_eq!(tree.remove(only), Some('a'));
        assert_eq!(tree.focus(), None);
        assert_eq!(tree.count(), 0);
        assert!(tree.layout(area(), DIVIDER).panes.is_empty());
    }

    #[test]
    fn remove_falls_back_to_the_previous_pane() {
        let (mut tree, [_, b, c]) = three();
        assert_eq!(tree.remove(PaneId(999)), None);
        assert_eq!(tree.focus(), Some(c));
        tree.set_focus(b);
        assert_eq!(tree.remove(b), Some('b'));
        // b 后面只剩 c
        assert_eq!(tree.focus(), Some(c));
        assert_eq!(tree.remove(c), Some('c'));
        assert_eq!(tree.focus(), Some(PaneId(1)));
        assert_eq!(tree.count(), 1);
    }

    #[test]
    fn layout_respects_ratio_and_divider() {
        let mut tree = PaneTree::<char>::new();
        let a = tree.set_root('a');
        let b = tree.split(a, Axis::Horizontal, 'b').unwrap();
        let split = tree.layout(area(), DIVIDER).dividers[0].id;
        tree.set_ratio(split, 0.25);

        let layout = tree.layout(area(), DIVIDER);
        let pa = layout.pane_rect(a).unwrap();
        let pb = layout.pane_rect(b).unwrap();
        let d = &layout.dividers[0];
        // 可分配 994，a 占 1/4 ≈ 248.5，b 拿剩下的
        assert!((pa.width() - 248.5).abs() < 0.1, "{pa:?}");
        assert!((pb.width() - 745.5).abs() < 0.1, "{pb:?}");
        assert!((d.rect.width() - DIVIDER).abs() < 0.001);
        assert!((d.extent - 994.0).abs() < 0.001);
        // 三块严丝合缝，不重叠也不留缝
        assert!((pa.max.x - d.rect.min.x).abs() < 0.001);
        assert!((d.rect.max.x - pb.min.x).abs() < 0.001);
        assert_eq!(pb.max, area().max);
    }

    #[test]
    fn nested_layout_stacks_vertically_inside_the_right_half() {
        let (tree, [a, b, c]) = three();
        let layout = tree.layout(area(), DIVIDER);
        let pa = layout.pane_rect(a).unwrap();
        let pb = layout.pane_rect(b).unwrap();
        let pc = layout.pane_rect(c).unwrap();
        assert_eq!(pa.min, area().min);
        assert!(pb.min.x > pa.max.x);
        assert!(pc.min.x > pa.max.x);
        assert!((pb.min.x - pc.min.x).abs() < 0.001);
        assert!(pb.max.y <= pc.min.y);
        assert_eq!(pc.max, area().max);
        assert_eq!(layout.dividers.len(), 2);
    }

    #[test]
    fn drag_divider_maps_pointer_to_ratio() {
        let (mut tree, [a, b, _]) = three();
        let layout = tree.layout(area(), DIVIDER);
        let h = layout
            .dividers
            .iter()
            .find(|d| d.axis == Axis::Horizontal)
            .unwrap();
        assert!(tree.drag_divider(h.id, h, 100.0));
        let layout = tree.layout(area(), DIVIDER);
        assert!(layout.pane_rect(a).unwrap().width() < layout.pane_rect(b).unwrap().width());
        // 越界的指针被夹到安全范围，面板不会被压没
        assert!(tree.drag_divider(h.id, h, -5000.0));
        let layout = tree.layout(area(), DIVIDER);
        assert!(layout.pane_rect(a).unwrap().width() > 0.0);
        assert!(tree.drag_divider(h.id, h, 5000.0));
        let layout = tree.layout(area(), DIVIDER);
        assert!(layout.pane_rect(b).unwrap().width() > 0.0);
    }

    #[test]
    fn drag_a_dead_or_flat_divider_is_ignored() {
        let (mut tree, _) = three();
        let dead = DividerRect {
            id: SplitId(42),
            axis: Axis::Horizontal,
            rect: Rect::from_min_max(pos2(0.0, 0.0), pos2(6.0, 600.0)),
            start: 0.0,
            extent: 994.0,
        };
        assert!(!tree.drag_divider(dead.id, &dead, 300.0));
        assert!(!tree.set_ratio(SplitId(42), 0.5));
        let flat = DividerRect {
            extent: 0.0,
            ..dead
        };
        assert!(!tree.drag_divider(SplitId(1), &flat, 10.0));
    }

    #[test]
    fn focus_cycle_wraps_both_ways() {
        let (mut tree, [a, b, c]) = three();
        tree.set_focus(a);
        assert_eq!(tree.focus_cycle(1), Some(b));
        assert_eq!(tree.focus_cycle(1), Some(c));
        assert_eq!(tree.focus_cycle(1), Some(a));
        assert_eq!(tree.focus_cycle(-1), Some(c));
        // 退 4 步 = 退 1 步再多绕一圈
        assert_eq!(tree.focus_cycle(-4), Some(b));
        assert_eq!(tree.focus_cycle(-2), Some(c));
    }

    #[test]
    fn focus_cycle_on_empty_tree_clears_focus() {
        let mut tree = PaneTree::<char>::new();
        assert_eq!(tree.focus_cycle(1), None);
    }

    #[test]
    fn focus_dir_uses_geometry() {
        let (mut tree, [a, b, c]) = three();
        let layout = tree.layout(area(), DIVIDER);

        // a 的右边是贴着分隔线的 b
        tree.set_focus(a);
        assert_eq!(tree.focus_dir(&layout, Dir::Right), Some(b));
        // b 往下是 c
        tree.set_focus(b);
        assert_eq!(tree.focus_dir(&layout, Dir::Down), Some(c));
        // c 往左回到通高的 a
        tree.set_focus(c);
        assert_eq!(tree.focus_dir(&layout, Dir::Left), Some(a));
        // a 占满整列高度，往下没有面板
        tree.set_focus(a);
        assert_eq!(tree.focus_dir(&layout, Dir::Down), Some(a));
        assert_eq!(tree.focus(), Some(a));
        // c 在右下角，再往右下都没有
        tree.set_focus(c);
        assert_eq!(tree.focus_dir(&layout, Dir::Down), Some(c));
        assert_eq!(tree.focus_dir(&layout, Dir::Right), Some(c));
        // b 往右也出界
        tree.set_focus(b);
        assert_eq!(tree.focus_dir(&layout, Dir::Right), Some(b));
    }

    #[test]
    fn four_way_grid_navigates() {
        let mut tree = PaneTree::<char>::new();
        let tl = tree.set_root('a');
        let tr = tree.split(tl, Axis::Horizontal, 'b').unwrap();
        let bl = tree.split(tl, Axis::Vertical, 'c').unwrap();
        let br = tree.split(tr, Axis::Vertical, 'd').unwrap();
        assert_eq!(tree.count(), 4);
        assert_eq!(tree.payload_of(bl), Some(&'c'));
        assert_eq!(tree.payload_of(br), Some(&'d'));

        let layout = tree.layout(area(), DIVIDER);
        assert_eq!(layout.panes.len(), 4);
        for p in &layout.panes {
            assert!(p.rect.is_finite());
            assert!(p.rect.width() > 0.0 && p.rect.height() > 0.0);
        }
        // 四块正好拼成 2x2，左上角从原点铺开，右下角收到边界
        assert_eq!(layout.pane_rect(tl).unwrap().min, area().min);
        assert_eq!(layout.pane_rect(br).unwrap().max, area().max);

        let go = |tree: &mut PaneTree<char>, dir: Dir| {
            let l = tree.layout(area(), DIVIDER);
            tree.focus_dir(&l, dir)
        };
        tree.set_focus(tl);
        assert_eq!(go(&mut tree, Dir::Right), Some(tr));
        tree.set_focus(tl);
        assert_eq!(go(&mut tree, Dir::Down), Some(bl));
        tree.set_focus(br);
        assert_eq!(go(&mut tree, Dir::Left), Some(bl));
        tree.set_focus(br);
        assert_eq!(go(&mut tree, Dir::Up), Some(tr));
        tree.set_focus(br);
        assert_eq!(go(&mut tree, Dir::Right), Some(br));
    }

    #[test]
    fn ids_are_never_reused() {
        let mut tree = PaneTree::<char>::new();
        let a = tree.set_root('a');
        let b = tree.split(a, Axis::Horizontal, 'b').unwrap();
        tree.remove(a);
        let c = tree.split(b, Axis::Horizontal, 'c').unwrap();
        assert_ne!(c, a);
        assert!(!tree.contains(a));
        assert_eq!(tree.count(), 2);
    }

    #[test]
    fn unknown_focus_is_rejected() {
        let (mut tree, [a, _, _]) = three();
        tree.set_focus(PaneId(3));
        assert!(!tree.set_focus(PaneId(404)));
        assert_eq!(tree.focus(), Some(PaneId(3)));
        assert!(tree.set_focus(a));
        assert_eq!(tree.payload_of(a), Some(&'a'));
    }

    #[test]
    fn payload_mutation_reaches_the_stored_value() {
        let (mut tree, [a, _, _]) = three();
        *tree.payload_of_mut(a).unwrap() = 'z';
        assert_eq!(tree.payload_of(a), Some(&'z'));
        let mut seen = Vec::new();
        tree.foreach_payload_mut(|id, p| seen.push((id, *p)));
        assert_eq!(seen.len(), 3);
    }
}
