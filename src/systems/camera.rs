//! 相机控制与跟随：三种模式（第一人称 Robot / 第三人称 / 自由），以及自由相机操作。
/// 相机控制系统：三种模式（第一人称 Robot / 第三人称 / 自由），以及自由相机操作。
///
/// - `following_controls`：Tab 循环切换三种模式（在 Input 阶段）。
/// - `update_camera_follow`：按当前模式把主相机摆到目标位置（在 Camera 阶段，晚于游戏逻辑）。
/// - `freecam_controls`：自由模式下用鼠标 + WASD/NJ 移动相机。
///
/// 第一人称模式复用 glTF 的 `CAM_DIRECTION` / `SHOT_DIRECTION` 节点契约（见 setup.rs），
/// 且与 systems/projectile.rs 的弹道方向使用**同一套旋转合成**，保证"准星指哪打哪"。

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use std::f32::consts::PI;

use crate::components::{
    CameraMode, Controlled, FollowingType, Infantry, InfantryGimbal, InfantryLaunchOffset,
    InfantryViewOffset, MainCamera,
};
use crate::config::SimulationConfig;
use crate::systems::ControllerState;

/// Tab 循环切换相机模式：Free → Robot → ThirdPerson → Free（Input 阶段）。
pub fn following_controls(mut mode: ResMut<CameraMode>, controller: Res<ControllerState>) {
    // 读取当前控制器状态的切换相机模式开关状态
    if controller.controlled.switch_camera_just_pressed {
        // 对枚举做 match 循环推进；三种模式显式穷尽（无 `_` 兜底，新增变体会编译报错提醒）。
        mode.0 = match mode.0 {
            FollowingType::Free => FollowingType::Robot,  // 如果当前是自由模式，切换到第一人称模式
            FollowingType::Robot => FollowingType::ThirdPerson,  // 如果当前是第一一人称模式，切换到三人称模式
            FollowingType::ThirdPerson => FollowingType::Free,  // 如果当前是三人称模式，切换到自由模式
        };
    }
}

/// 按当前相机模式摆正主相机（Camera 阶段）。
pub fn update_camera_follow(
    
    camera_query: Single<(&mut Transform, &MainCamera), Without<Controlled>>,// `Without<Controlled>`：主相机不挂在任何被操控的车上，避免与车实体混淆。
    infantry: Single<&Transform, (With<Infantry>, With<Controlled>)>, // 从 Bevy 获取被操控的车实体的变换组件，用于计算相机位置 
    gimbal: Single<&Transform, (With<Controlled>, With<InfantryGimbal>)>, // 从 Bevy 获取被操控的车实体的云台实体的变换组件，用于计算相机旋转
    view_offset: Single<&Transform, (With<Controlled>, With<InfantryViewOffset>)>, // 从 Bevy 获取被操控的车实体的 InfantryViewOffset 组件的变换组件，用于计算相机位置
    launch_offset: Single<&Transform, (With<Controlled>, With<InfantryLaunchOffset>)>,// 从 Bevy 获取被操控的车实体的 InfantryLaunchOffset 组件的变换组件，用于计算相机旋转
    mode: Res<CameraMode>, // 从 Bevy 获取当前相机模式组件，用于判断相机模式
) {
    // `into_inner()`：把 Single 解包成里面的元组/引用，避免反复解引用。
    let gimbal_transform = gimbal.into_inner();
    // camera_query 是 Single 类型，所以直接解包查询主相机的变换组件和相机模式组件的引用
    let (mut camera_transform, camera_offset) = camera_query.into_inner();

    /*ROS2 的习惯：
    > ROS2（REP103 机器人标准）：**X 向前，Y 向左，Z 向上**
    > 右手手势：食指 = X (前)，中指 = Y (左)，大拇指 = Z (上)，就是你说的那个手势

    Bevy：
    > Bevy 世界：**X 向右，Y 向上，+Z 指向自己（朝屏幕外），相机看向 -Z**
    > 右手手势：大拇指 = X (右)，食指 = Y (上)，中指 = Z (朝你) */
    match mode.0 {
        FollowingType::Robot => {
            // 第一人称：相机放在 CAM_DIRECTION 节点处，朝向与炮口方向合成一致。
            let view_offset_transform = view_offset.into_inner();
            // 云台世界旋转 ≈ 车世界旋转 × 云台本地旋转
            // （严格说中间还有 VEHICLE 节点那一层；当前资产里它的旋转是单位阵，故可省略）
            let gimbal_world_rotation = infantry.rotation * gimbal_transform.rotation;
            // `view_offset_transform.translation` 是 CAM_DIRECTION 节点在"云台坐标系"下的
            // 局部平移（单位米）；用云台世界旋转把它转到世界系，得到"从车指向相机的世界偏移向量"。
            let view_offset_world = gimbal_world_rotation * view_offset_transform.translation;

            // 相机是顶层实体（没有父级），所以它的 `Transform` 直接就是世界变换，无需再乘父级旋转。
            // 世界位置 = 车世界位置 + CAM_DIRECTION 的世界偏移。
            // 末尾的 `+ Vec3::from([0.0, 0.0, 0.0])` 是加零、当前无任何作用，留作以后微调位置用。
            camera_transform.translation = infantry.translation + view_offset_world + Vec3::from([0.0, 0.0, 0.0]);
            // 朝向 = 云台世界旋转 × 炮口局部旋转 × Rz(90°)。
            // 最后这项是"画面横滚修正"：绕局部 Z（视线轴）转 90°，让渲染出的图像正立。
            // 注意它**不改变视线方向**——NEG_Z 在"绕 Z 轴旋转"下是不变的，所以它与弹道方向无关；
            // 但该值必须与 projectile.rs 里的同一套合成保持一致（见 projectile.rs L91-98），
            // 改了它画面会整体歪掉、视觉端解算随之出错。
            camera_transform.rotation = (gimbal_world_rotation
                * launch_offset.rotation // 炮口（SHOT_DIRECTION）局部旋转
                * Quat::from_euler(EulerRot::ZYX, 0.0, 0.0, PI / 2.0))
            .normalize() // 确保旋转矩阵是正交的，避免旋转矩阵的不稳定性
        }
        FollowingType::ThirdPerson => {
            // 第三人称：在车后上方固定偏移处，回头看向车（`Vec3::Y` 指定世界上方）。
            let base_transform = infantry.into_inner();
            // 相机位置 = 车世界位置 + 固定偏移。
            let offset = base_transform.rotation * camera_offset.follow_offset;
            // 相机位置 = 车世界位置 + 固定偏移。
            camera_transform.translation = base_transform.translation + offset;
            // 相机看向车（`Vec3::Y` 指定世界上方）。
            camera_transform.look_at(base_transform.translation, Vec3::Y);
        }
        FollowingType::Free => {} // 自由模式由 freecam_controls 接管，这里不动
    }
}

/// 自由相机：鼠标看向 + 键盘平移（仅在 Free 模式运行）。
pub fn freecam_controls(
    time: Res<Time>,     // 从 Bevy 获取当前时间，用于计算 dt
    mode: Res<CameraMode>, // 从 Bevy 获取当前相机模式，用于判断是否为自由模式
    config: Res<SimulationConfig>, // 从 Bevy 获取模拟配置，用于获取相机移动速度
    // `MessageReader<T>`：读取消息流（消息 vs 事件的差别见 controller.rs 中 MessageWriter 的说明）。
    mut mouse_motion_events: MessageReader<MouseMotion>, // 从 Bevy 获取鼠标移动事件，用于计算相机旋转
    keyboard: Res<ButtonInput<KeyCode>>, // 从 Bevy 获取键盘输入，用于平移相机
    camera_query: Single<&mut Transform, (With<MainCamera>, Without<Infantry>)>, // 从 Bevy 获取主相机的变换组件，用于更新相机位置
) {
    if mode.0 != FollowingType::Free {
        return;
    }

    // 计算本帧时间间隔（dt），用于平滑移动
    let delta = time.delta_secs();
    // 从 Bevy 获取主相机的变换组件，用于更新相机位置，into_inner获取相机的transform可变引用，避免重复解引用
    let mut camera_transform = camera_query.into_inner();

    // 汇总本帧所有鼠标位移（一帧可能收到多段 MouseMotion）。
    let mut mouse_delta = Vec2::ZERO;
    // 遍历鼠标移动事件，累加鼠标位移，用于计算相机旋转
    for event in mouse_motion_events.read() {
        // delta表示上一帧到当前帧的鼠标位移
        mouse_delta += event.delta;
    }

    if mouse_delta != Vec2::ZERO {
        // 把当前旋转拆成 yaw/pitch/roll（YXZ 顺序），只改 yaw/pitch 再合回去。
        let (yaw, pitch, roll) = camera_transform.rotation.to_euler(EulerRot::YXZ);
        // 计算新的 yaw/pitch 旋转角度
        let new_yaw = yaw - mouse_delta.x * config.camera.mouse_sensitivity;
        // pitch 限制在 ±1.4 rad（约 ±80°），防止视角翻转。
        let new_pitch = (pitch - mouse_delta.y * config.camera.mouse_sensitivity).clamp(-1.4, 1.4);
        // 更新相机旋转（合并新的 yaw/pitch 旋转角度和当前 roll 角度，保持相机朝向不变）
        camera_transform.rotation = Quat::from_euler(EulerRot::YXZ, new_yaw, new_pitch, roll);
    }

    // 单位位移 = 速度(m/s) × dt；方向取相机自身的前/右/上轴（相机朝向由其旋转决定）。
    let speed = config.camera.free_move_speed * delta;
    let forward = camera_transform.forward();
    let right = camera_transform.right();
    let up = camera_transform.up();

    // WASD 前后左右，N/J 升降（键位与 controller.rs 的帮助文本一致）。
    if keyboard.pressed(KeyCode::KeyW) {
        camera_transform.translation += forward * speed;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        camera_transform.translation -= forward * speed;
    }
    if keyboard.pressed(KeyCode::KeyA) {
        camera_transform.translation -= right * speed;
    }
    if keyboard.pressed(KeyCode::KeyD) {
        camera_transform.translation += right * speed;
    }
    if keyboard.pressed(KeyCode::KeyN) {
        // 提升相机位置
        // 添加相机提升功能，并使用时间间隔平滑移动
        camera_transform.translation += up * speed;
    }
    if keyboard.pressed(KeyCode::KeyJ) {
        // 下降相机位置
        camera_transform.translation -= up * speed;
    }
}
