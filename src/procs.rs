//! 进程树的一层薄封装：关一块终端面板时，把 shell 自己起的进程一并收走。
//!
//! portable-pty 的 `kill()` 只对 shell 本身 `TerminateProcess`。在终端里跑
//! `npm run dev`、`vim`、一条还在输出的构建命令时，那些孙进程会留在原地占着端口和
//! 文件句柄，而界面上已经没有任何东西指向它们。这里按 `(pid, ppid)` 把子树数出来，
//! 从最深的那一层往上杀。

use std::collections::HashSet;

/// `rows` 是全系统的 `(pid, ppid)`。返回 `root` 的所有子孙，**深的排在前面**：
/// 先断子进程，父进程不会在被杀的那一瞬间还有机会再拉起一个。
/// 同一层之间的先后无所谓，它们彼此没有父子关系。
///
/// 进程号会被复用，抓快照又不是原子的，所以成环（A 是 B 的父、B 又是 A 的父）真能撞上；
/// `seen` 保证每个 pid 只收一次，环到这儿就断。
pub fn descendants(root: u32, rows: &[(u32, u32)]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    seen.insert(root);
    let mut level = vec![root];
    while !level.is_empty() {
        let mut next = Vec::new();
        for &parent in &level {
            for &(pid, ppid) in rows {
                if ppid == parent && seen.insert(pid) {
                    next.push(pid);
                    out.push(pid);
                }
            }
        }
        level = next;
    }
    out.reverse();
    out
}

/// 杀掉 `root` 的子孙，`root` 自己留给调用方（shell 那条句柄本来就在手上，
/// 走它的 `kill()` 顺带把状态收干净）。
#[cfg(windows)]
pub fn kill_descendants(root: u32) {
    for pid in descendants(root, &win::snapshot()) {
        win::terminate(pid);
    }
}

/// 类 Unix 上终端是会话首进程，pty 断开时整个前台进程组一起吃 SIGHUP，不用管。
#[cfg(not(windows))]
pub fn kill_descendants(_root: u32) {}

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const MAX_PATH: usize = 260;

    /// `PROCESSENTRY32W`。字段顺序和对齐照 C 头文件，`th32DefaultHeapID` 是唯一
    /// 一个指针宽度字段（早年的字段，值没人用），但少了它后面全部错位。
    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; MAX_PATH],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut c_void;
        fn Process32FirstW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: *mut c_void, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    /// 全系统进程的 `(pid, ppid)`。拿不到快照就交回空表——多出来的那几个进程
    /// 等下一次关页签或退出时再收，总好过为这点事打扰用户。
    pub fn snapshot() -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() || snap as isize == -1 {
                return out;
            }
            let mut entry: ProcessEntry32W = std::mem::zeroed();
            let size = std::mem::size_of::<ProcessEntry32W>() as u32;
            entry.dw_size = size;
            let mut ok = Process32FirstW(snap, &mut entry) != 0;
            while ok {
                out.push((entry.th32_process_id, entry.th32_parent_process_id));
                // Next 同样要求调用前把 dw_size 填好，否则返回 ERROR_BAD_LENGTH
                entry.dw_size = size;
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    pub fn terminate(pid: u32) {
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                // 已经退出了，或者是不让碰的受保护进程：两种都不该报给用户
                return;
            }
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 9 → 100（shell）→ 101 → 102；100 另外还直接带一个 103。隔壁 4 那棵不许被碰到。
    const ROWS: &[(u32, u32)] = &[
        (0, 0),
        (4, 0),
        (9, 4),
        (100, 9),
        (101, 100),
        (103, 100),
        (102, 101),
    ];

    /// 顺序要求只有一条：谁的子进程排在它前面，杀的时候才轮得到从最深往上收。
    #[test]
    fn takes_the_whole_subtree_deepest_first() {
        assert_eq!(descendants(100, ROWS), vec![102, 103, 101]);
    }

    #[test]
    fn leaves_other_trees_alone() {
        assert!(descendants(102, ROWS).is_empty());
        assert!(descendants(100_000, ROWS).is_empty());
        // 根自己不算在内，它由调用方的那条句柄收
        assert_eq!(descendants(4, ROWS), vec![102, 103, 101, 100, 9]);
    }

    /// 进程号复用加上快照不是原子抓取，ppid 成环是会出现的：不能死循环，
    /// 也不能把同一个进程交两遍。
    #[test]
    fn survives_a_cycle_in_the_snapshot() {
        let rows = [(1u32, 2u32), (2, 1), (3, 2)];
        assert_eq!(descendants(1, &rows), vec![3, 2]);
    }

    /// 真起一层进程树试一次（会真的开 cmd 和 ping，所以默认跳过）：
    /// `cargo test -- --ignored --nocapture procs`
    #[test]
    #[ignore = "要起真实进程"]
    fn kill_descendants_reaches_the_grandchild() {
        let mut root = std::process::Command::new("cmd")
            .args(["/C", "start /b ping -n 120 127.0.0.1 & ping -n 120 127.0.0.1"])
            .spawn()
            .expect("起 cmd");
        std::thread::sleep(std::time::Duration::from_millis(800));
        let pid = root.id();
        let before = descendants(pid, &win::snapshot());
        assert!(!before.is_empty(), "没抓到 cmd 起的孙进程");
        kill_descendants(pid);
        let _ = root.kill();
        let _ = root.wait();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let after = win::snapshot();
        for gone in before {
            assert!(
                !after.iter().any(|(p, _)| *p == gone),
                "{gone} 还活着，没杀干净"
            );
        }
    }
}
