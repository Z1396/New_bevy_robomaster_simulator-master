//! 共享内存区域封装 (基于 memmap2)
//!
//! 使用内存映射文件替代 POSIX 共享内存，纯 Rust 实现。
//!
//! 【教学】"共享内存"在各平台的真实机制并不统一，这里用**内存映射文件**抹平差异：
//! 把磁盘/临时目录下的一个文件 `mmap` 进进程地址空间，读写这段内存就等于读写文件；
//! 两个进程映射**同一个文件**，就看到同一份字节，从而零拷贝地交换数据。
//! - 跨平台：Windows 映射到 `%TEMP%` 下文件、Unix 映射到 `/tmp` 下文件（见 `shm_path`）；
//! - RAII：`ShmRegion` 析构（`Drop`）时，若是创建者则删除文件，避免残留；
//! - `as_ref` / `as_mut` 是 `unsafe`：把裸内存强行解释成 `ShmMetaRegion` 等结构，
//!   其安全性完全依赖 layout.rs 里那份"布局契约"与调用方的单写者约定。

use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::PathBuf;

/// 共享内存相关错误。`IoError` 变体携带底层 `io::Error` 载荷（其他变体无载荷）。
#[derive(Debug)]
pub enum ShmError {
    IoError(io::Error), // 文件打开/映射等系统调用失败
    MapFailed,          // 内存映射失败
    InvalidSize,        // 文件大小不满足要求
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShmError::IoError(e) => write!(f, "IO error: {}", e),
            ShmError::MapFailed => write!(f, "mmap failed"),
            ShmError::InvalidSize => write!(f, "invalid size"),
        }
    }
}

// 实现标准错误 trait：这样 `ShmError` 才能被 `Box<dyn Error>` 容纳、与其它错误统一处理。
impl std::error::Error for ShmError {}

// `From<io::Error>`：让 `?` 在遇到 `io::Error` 时自动转成 `ShmError`（无需手写 map_err）。
impl From<io::Error> for ShmError {
    fn from(e: io::Error) -> Self {
        ShmError::IoError(e)
    }
}

/// 获取共享内存文件路径（按平台选择临时目录）。
fn shm_path(name: &str) -> PathBuf {
    let clean_name = name.trim_start_matches('/'); // 去掉可能的前导 '/'（Windows 文件名不允许）
    #[cfg(target_os = "windows")]
    // Windows 使用临时目录(TempDir)
    // 注意：Windows 上的共享内存文件名不能以 '/' 开头
    return std::env::temp_dir().join(clean_name);
    #[cfg(not(target_os = "windows"))]
    // Unix 使用 /tmp 目录
    PathBuf::from("/tmp").join(clean_name)
}

/// RAII 封装的共享内存区域
/// （RAII = 资源获取即初始化：构造时获得映射，`Drop` 时自动释放，无需手动清理。）
pub struct ShmRegion {
    mmap: MmapMut, // memmap2 的可写内存映射句柄
    path: PathBuf, // 对应文件路径（Drop 时删除）
    is_owner: bool, // 是否由本进程创建（只有创建者负责删除文件）
}

// 手动声明跨线程可安全共享：底层是裸内存映射，由上层三缓冲/原子控制字保证并发安全。
unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// 创建新的共享内存区域 (生产者)
    pub fn create(name: &str, size: usize) -> Result<Self, ShmError> {
        let path = shm_path(name);

        // 创建或覆盖文件
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;

        // 设置文件大小
        file.set_len(size as u64)?;

        // 写入零填充
        // ⚠ 零填充会把三缓冲的控制字清成 0（错误的初始状态），所以 publisher 在创建后
        //   会显式重新初始化三缓冲（见 publisher.rs 的 init_triple_buffer 与 layout.rs 的 Default）。
        file.write_all(&vec![0u8; size])?;
        file.sync_all()?;

        // 重新打开并映射
        // （上一步用的是带 create/truncate 的写句柄；这里重新以读写方式打开，再建立内存映射。）
        let file = OpenOptions::new().read(true).write(true).open(&path)?;

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(Self {
            mmap,
            path,
            is_owner: true,
        })
    }

    /// 打开已存在的共享内存区域 (消费者)
    pub fn open(name: &str, size: usize) -> Result<Self, ShmError> {
        let path = shm_path(name);

        let file = OpenOptions::new().read(true).write(true).open(&path)?;

        // 验证大小
        let metadata = file.metadata()?;
        if metadata.len() < size as u64 {
            return Err(ShmError::InvalidSize);
        }

        let mmap = unsafe { MmapMut::map_mut(&file)? };

        Ok(Self {
            mmap,
            path,
            is_owner: false,
        })
    }

    /// 获取指向共享内存的指针
    /// （裸指针，供图像池那种"按 offset 直接写大块像素"的零拷贝场景使用。）
    pub fn as_ptr(&self) -> *mut u8 {
        self.mmap.as_ptr() as *mut u8
    }

    /// 获取共享内存大小
    pub fn size(&self) -> usize {
        self.mmap.len()
    }

    /// 将共享内存解释为指定类型
    ///
    /// # Safety
    /// 调用者必须确保类型 T 与共享内存布局匹配
    pub unsafe fn as_ref<T>(&self) -> &T {
        unsafe { &*(self.mmap.as_ptr() as *const T) }
    }

    /// 将共享内存解释为指定类型 (可变)
    ///
    /// # Safety
    /// 调用者必须确保类型 T 与共享内存布局匹配
    pub unsafe fn as_mut<T>(&mut self) -> &mut T {
        unsafe { &mut *(self.mmap.as_ptr() as *mut T) }
    }

    /// 刷新内存映射到文件（把脏页写回，保证对端能看到最新字节）。
    pub fn flush(&self) -> Result<(), ShmError> {
        self.mmap.flush()?;
        Ok(())
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        if self.is_owner {
            // 删除文件
            // 仅创建者删除；消费者只断开映射，不影响对方。
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
