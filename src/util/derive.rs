//! 两个"消除样板代码"的声明式宏。
//!
//! Rust 用 `macro_rules!` 写**声明式宏**（也叫宏规则）：它不是函数，而是在**编译期**按
//! 模式匹配源码 token 并展开成新代码。调用时宏名后必须加 `!`，如 `all_arg_constructor!(...)`。
//! `#[macro_export]` 把宏导出到 crate 根，于是可以用 `crate::all_arg_constructor!` 或（本项目里）
//! 直接 `all_arg_constructor!` 调用。
//!
//! 两个宏的用途：
//! - `all_arg_constructor!`：给一个结构体自动生成"接收所有字段的 `new(...)` 构造函数"；
//! - `arc_mutex!`：`Arc::new(Mutex::new(x))` 的简写，一行写出"多线程共享 + 可改写"的容器。

// `#[macro_export]`：让宏在 crate 根可见（否则只能在声明后、同模块内使用）。
#[macro_export]
// `macro_rules! 名字 { ... }` 定义宏；大括号里是一串"匹配臂"，从上往下尝试匹配。
macro_rules! all_arg_constructor {
    // 私有结构体分支：匹配 `struct 名字 { 字段: 类型, ... }`。
    // - `$name:ident`：捕获一个"标识符"（结构体名）；
    // - `$( $field:ident : $ty:ty ),*`：把"字段名: 类型"序列按逗号重复捕获，`*` 表示 0 次或多次；
    // - `$(,)?`：允许结尾多一个可选逗号（写不写 trailing comma 都能匹配）。
    (struct $name:ident { $( $field:ident : $ty:ty ),* $(,)? }) => {
        struct $name {
            // `$( ... )*` 重复展开：为每个被捕获的字段生成一行声明。
            $(
            $field: $ty,
            )*
        }
        impl $name {
            // 生成 `new`：参数与字段一一对应（`$( $field: $ty ),*`），
            // 函数体用字段简写 `Self { $field }`（字段名=变量名时可省略 `字段: `）。
            pub fn new($( $field: $ty ),*) -> Self {
                Self { $( $field ),* }
            }
        }
    };
    // 公开结构体分支：与上面完全相同，只是结构体定义前多了 `pub`。
    // 宏匹配臂是"文本模式"，所以 pub / 非 pub 只能各写一条臂。
    (pub struct $name:ident { $( $field:ident : $ty:ty ),* $(,)? }) => {
        pub struct $name {
            $(
            $field: $ty,
            )*
        }
        impl $name {
            pub fn new($( $field: $ty ),*) -> Self {
                Self { $( $field ),* }
            }
        }
    };
}

#[macro_export]
macro_rules! arc_mutex {
    // `$elem:expr`：捕获"一个表达式"（任意能求值的片段），然后包一层再返回。
    ($elem:expr) => {
        // 前导 `::` 表示"从 crate 根开始的绝对路径"，可避免调用方本地同名项造成的名字遮蔽。
        // 语义：`Arc`（原子引用计数，可跨线程共享所有权）内套 `Mutex`（互斥锁，保证独占改写的安全）。
        ::std::sync::Arc::new(::std::sync::Mutex::new($elem))
    };
}
