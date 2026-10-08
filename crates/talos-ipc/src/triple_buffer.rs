//! 无锁三缓冲读写原语（SPSC：单生产者 / 单消费者，跨线程或跨进程）。
//!
//! 【要解决什么】一个线程写、另一个线程读同一份数据。用锁（Mutex）会让"写"经常等"读"，
//! 高频图像场景下帧率被拖垮。**三缓冲**用三个槽 + 一个原子控制字实现：
//! - **写者不阻塞读者**：写者只往自己私有的 write 槽写，从不碰别人正在读的槽，故永不等待；
//! - **读者总拿到最新完整帧**：`publish` 原子地把"刚写好的槽"标为 ready，读者每次取走
//!   整个槽，永远不会读到"写了一半"的撕裂帧（只会跳过中间被覆盖的帧——这正是想要的）。
//!
//! 【三槽的分工】任一时刻相邻的"状态字节"只编码两件事：低 2 位 = ready 槽下标，
//! 最高位 `FLAG_NEW` = 是否有尚未被读走的新数据。三个槽在"写 / 就绪 / 已读"三种角色间
//! 轮转，本原语就是维护这个轮转。初始状态见 layout.rs 的三缓冲 Default 实现。

use crate::layout::{FLAG_NEW, INDEX_MASK};
use std::sync::atomic::Ordering;

/// 生产者（写者）句柄：持有对共享控制字与三槽的可变引用，生命周期内只能有一个。
pub struct TripleBufferProducer<'a, S> {
    state: &'a std::sync::atomic::AtomicU8, // 共享的原子控制字（跨线程/进程）
    write_idx: &'a mut u8,                  // 本生产者私有的可写槽下标
    slots: &'a mut [S; 3],                  // 三个数据槽
}

impl<'a, S> TripleBufferProducer<'a, S> {
    /// 创建生产者
    ///
    /// # Safety
    /// 调用者必须确保只有一个生产者存在
    /// （`unsafe` 的原因：要保证"单写者"才能无锁安全，编译器无法验证这一点。）
    pub unsafe fn new(
        state: &'a std::sync::atomic::AtomicU8,
        write_idx: &'a mut u8,
        slots: &'a mut [S; 3],
    ) -> Self {
        Self {
            state,
            write_idx,
            slots,
        }
    }

    /// 获取可写槽位的可变引用
    /// 写入期间不触碰共享状态，所以随便写多久都不会阻塞读者。
    pub fn borrow_mut(&mut self) -> &mut S {
        &mut self.slots[*self.write_idx as usize]
    }

    /// 发布数据
    /// `swap`：原子地"把 state 换成 `write_idx | FLAG_NEW`（我刚写好的槽 = 新就绪槽），
    /// 同时拿回旧 state"；旧 state 里的就绪槽（`old & INDEX_MASK`）转而成为我的下一个
    /// 可写槽——三槽就此完成一次角色轮转。
    pub fn publish(&mut self) {
        let old = self
            .state
            .swap(*self.write_idx | FLAG_NEW, Ordering::AcqRel);
        *self.write_idx = old & INDEX_MASK;
    }
}

/// TripleBuffer 消费者操作
/// 消费者（读者）句柄：同样要求全局唯一。
pub struct TripleBufferConsumer<'a, S> {
    state: &'a std::sync::atomic::AtomicU8,
    read_idx: &'a mut u8, // 本消费者上次读的槽下标（换出去给生产者复用）
    slots: &'a [S; 3],
}

impl<'a, S> TripleBufferConsumer<'a, S> {
    /// # Safety
    /// caller must ensure only no more than one consumer exists
    pub unsafe fn new(
        state: &'a std::sync::atomic::AtomicU8,
        read_idx: &'a mut u8,
        slots: &'a [S; 3],
    ) -> Self {
        Self {
            state,
            read_idx,
            slots,
        }
    }

    /// 取最新一帧；无新数据时返回 `None`。
    /// 用 CAS（compare-and-swap）循环与生产者竞争修改 `state`：成功即"认领"就绪槽，
    /// 并把自己的旧读槽作为 `desired` 写回 state（同时清掉 FLAG_NEW），让生产者回收利用。
    pub fn borrow(&mut self) -> Option<&S> {
        let mut expected = self.state.load(Ordering::Acquire);

        // 没有新数据
        if (expected & FLAG_NEW) == 0 {
            return None;
        }

        let mut ready_idx = expected & INDEX_MASK; // 当前就绪槽
        let mut desired = *self.read_idx; // 我要换回去的槽（下次 producer 用它写）

        // 第一次 CAS
        // `compare_exchange_weak`：若 state 仍等于 expected 就换成 desired（成功），
        // 否则返回当前值（失败，说明生产者刚动过 state）。weak 版允许伪失败，故有重试。
        match self.state.compare_exchange_weak(
            expected,
            desired,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // 认领成功：就绪槽变为我当前读槽，返回该槽整体（完整一帧，不会撕裂）。
                *self.read_idx = ready_idx;
                Some(&self.slots[ready_idx as usize])
            }
            Err(new_expected) => {
                // 生产者刚发布了新数据，再试一次
                expected = new_expected;
                if (expected & FLAG_NEW) == 0 {
                    return None;
                }
                ready_idx = expected & INDEX_MASK;
                desired = *self.read_idx;

                match self.state.compare_exchange_weak(
                    expected,
                    desired,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        *self.read_idx = ready_idx;
                        Some(&self.slots[ready_idx as usize])
                    }
                    Err(_) => None, // 又竞争失败：本帧放弃，返回 None（下一帧再取最新即可）
                }
            }
        }
    }

    /// Check if new data is available without consuming it
    ///
    /// This is useful for non-blocking polling of data availability.
    /// Returns `true` if a call to `borrow()` would return `Some(_)`.
    /// 只查 FLAG_NEW、不改变状态，因此可反复轮询而不"消费"数据。
    /// `#[must_use]`：返回值被忽略时编译器会警告（提醒别漏判结果）；
    /// `#[allow(dead_code)]`：本 crate 暂时没人调用，先别报"未使用"警告。
    #[must_use]
    #[allow(dead_code)]
    pub fn has_new_data(&self) -> bool {
        (self.state.load(Ordering::Acquire) & FLAG_NEW) != 0
    }
}
