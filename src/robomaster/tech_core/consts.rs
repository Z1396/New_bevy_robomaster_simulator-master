//! tech_core 模块的内部常量表：**模型节点命名契约** + 灯光形状参数。
//!
//! 本文件零逻辑，只放"魔法字符串/魔法数字"并在此集中解释它们的来历。
//! 这些常量是代码与美术资源 `TECH_CORE.glb` 之间的契约——改模型节点名就必须同步改这里，
//! 否则 `construct::setup_tech_core` 会找不到灯（会打印 warn 并放弃绑定）。
//!
//! 命名契约（glb 场景里的实体 Name，大小写完全一致）：
//!   第一组灯（分左右两排、每排 18 段）：`BLUE_LIGHT_1` / `RED_LIGHT_1`
//!       整组兜底节点：`{TEAM}_LIGHT_1`
//!       单段节点：`{TEAM}_LIGHT_1_L_{1..=18}`（左）、`{TEAM}_LIGHT_1_R_{1..=18}`（右）
//!   第二组灯（整体一个节点）：`{TEAM}_LIGHT_2`
//!   第三组灯（整体一个节点）：`{TEAM}_LIGHT_3`
//! 其中 `{TEAM}` ∈ {BLUE, RED}，代码侧由 `BLUE_LIGHT_NAMES` / `RED_LIGHT_NAMES` 提供。
//!
//! `pub(super)`：可见性 = 只对父模块（tech_core）可见，construct 通过 `super::consts::…` 使用。

// 双方各自三组灯的名字前缀，顺序固定为 [第一组, 第二组, 第三组]。
// 调用点约定：`[0]` 同时作为第一组"分段灯"的检索前缀（见 naming 契约中的 _L_/_R_ 后缀）。
// `[&str; 3]`：固定长度 3 的数组字面量类型——长度是类型的一部分，编译器会检查个数。
pub(super) const BLUE_LIGHT_NAMES: [&str; 3] = ["BLUE_LIGHT_1", "BLUE_LIGHT_2", "BLUE_LIGHT_3"];
pub(super) const RED_LIGHT_NAMES: [&str; 3] = ["RED_LIGHT_1", "RED_LIGHT_2", "RED_LIGHT_3"];

// 第一组灯单侧的分段数量（左右各 18 段），单位：段（无量纲计数）。
// 用于数组长度、角度→段号换算、以及"跑马灯一圈来回"的步数上限。
pub(super) const FIRST_LIGHT_SEGMENT_COUNT: usize = 18;
// 跑马灯（Flow）每个段位前进一格的频率，单位：Hz（赫兹，次/秒）。
// 12Hz 表示每 1/12 s ≈ 83.3 ms 亮起下一段；它只描述"流光步进"节奏，
// 与 BlinkRate（1/3Hz 整组闪烁）是两套独立频率，勿混淆。
pub(super) const FLOW_SEGMENT_HZ: f64 = 12.0;
