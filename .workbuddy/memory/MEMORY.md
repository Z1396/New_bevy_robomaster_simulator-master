# 项目长期须知（bevy_robomaster_simulator）

## 代码注释教学化改造（进行中）
- 目标：为"会编程但不懂 Rust/Bevy"的新手补教学型注释。项目：Bevy 0.19 + avian3d 0.7 + edition 2024。
- **风格金标准（唯一权威，开工前必读）**：`src/main.rs`、`src/robomaster/armor/collision.rs`。
- 三层结构：文件头 `//!`（5~15 行，模块职责+协作者+阅读顺序）/ 条目 `///`（用途+单位+调用时机+调度阶段）/ 行内 `//`（Rust 语法点、Bevy/avian API、魔法数字物理含义、跨文件契约）。
- 物理量必须标单位（米/秒/弧度/度/Hz）。禁止废话注释（`// 遍历`/`// 创建实体`/`// 返回`）。同一概念全项目只详解一次，之后简注或引用（如"同 main.rs 中 EventName 的说明"）。
- 硬约束：只增删注释行，禁止改代码逻辑、禁止移动代码行；`cargo check` 必须零错误零警告。

## 已确认的编辑口径（用户拍板）
- **允许把英文文件头 `//!` 改写为中文**（仅限文件头）。
- 其他既有注释（`///`、行内 `//`）一律**逐字保留**，中文教学注释**追加其后**；可像金标准那样给既有代码行**追加行尾注释**。

## 环境/工具
- `cargo` 不在 PATH：用 `export PATH="$PATH:/c/Users/86182/.cargo/bin"`。
- `cargo check` 用 `CARGO_INCREMENTAL=0`，否则 Windows 沙箱下会报增量锁文件"拒绝访问"的**环境噪音警告**（非代码问题）。
- 校验"只改注释"的脚本 `/tmp/strip_cmp.py`：剥离注释后对比 `git show HEAD:<file>` 与工作区，输出 CODE IDENTICAL。

## 批次进度
- 批次0（已完成，勿动）：main.rs、robomaster/armor/collision.rs（含【修改】记录，一律保留）。
- 批次1（已完成）：src/scene.rs、src/setup.rs。
- 待办：2=components/、3=systems/、4=robomaster/（除 armor/collision.rs）、5=capture/、6=util/+statistic+handler+config、7=talos/+crates/talos-ipc/。

## 已标注的跨文件契约（供后续批次引用，避免重复详解）
- glTF 节点契约：`<root>→VEHICLE→BASE/GIMBAL`，云台下 `SHOT_DIRECTION`/`CAM_DIRECTION`（setup.rs::setup_vehicle）。
- `HierarchyQuery`（util/entity_query.rs）：`of(root).any().exact("..").flatten()` 链式；`one()` 取恰好一个。
- `CAM_DIRECTION`(InfantryViewOffset) 与 `systems/camera.rs` Robot 模式一致。
- 场景线性异步加载：机器人必须最后生成，否则掉出地板（scene.rs）。
