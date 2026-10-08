//! 相机控制与跟随：三种模式（第一人称 Robot / 第三人称 / 自由），以及自由相机操作。
//!
//! - `following_controls`：Tab 循环切换三种模式（在 Input 阶段）。
//! - `update_camera_follow`：按当前模式把主相机摆到目标位置（在 Camera 阶段，晚于游戏逻辑）。
//! - `freecam_controls`：自由模式下用鼠标 + WASD/NJ 移动相机。
//!
//! 第一人称模式复用 glTF 的 `CAM_DIRECTION` / `SHOT_DIRECTION` 节点契约（见 setup.rs），
//! 且与 systems/projectile.rs 的弹道方向使用**同一套旋转合成**，保证"准星指哪打哪"。

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
    if controller.controlled.switch_camera_just_pressed {
        // 对枚举做 match 循环推进；三种模式显式穷尽（无 `_` 兜底，新增变体会编译报错提醒）。
        mode.0 = match mode.0 {
            FollowingType::Free => FollowingType::Robot,
            FollowingType::Robot => FollowingType::ThirdPerson,
            FollowingType::ThirdPerson => FollowingType::Free,
        };
    }
}

/// 按当前相机模式摆正主相机（Camera 阶段）。
pub fn update_camera_follow(
    // `Without<Controlled>`：主相机不挂在任何被操控的车上，避免与车实体混淆。
    camera_query: Single<(&mut Transform, &MainCamera), Without<Controlled>>,
    infantry: Single<&Transform, (With<Infantry>, With<Controlled>)>,
    gimbal: Single<&Transform, (With<Controlled>, With<InfantryGimbal>)>,
    view_offset: Single<&Transform, (With<Controlled>, With<InfantryViewOffset>)>,
    launch_offset: Single<&Transform, (With<Controlled>, With<InfantryLaunchOffset>)>,
    mode: Res<CameraMode>,
) {
    // `into_inner()`：把 Single 解包成里面的元组/引用，避免反复解引用。
    let gimbal_transform = gimbal.into_inner();
    let (mut camera_transform, camera_offset) = camera_query.into_inner();

    match mode.0 {
        FollowingType::Robot => {
            // 第一人称：相机放在 CAM_DIRECTION 节点处，朝向与炮口方向合成一致。
            let view_offset_transform = view_offset.into_inner();
            // 车世界旋转 × 云台本地旋转 = 云台世界旋转。
            let gimbal_world_rotation = infantry.rotation * gimbal_transform.rotation;
            // CAM_DIRECTION 在云台坐标系里的局部偏移，旋转到世界方向。
            let view_offset_world = gimbal_world_rotation * view_offset_transform.translation;

            camera_transform.translation = infantry.translation + view_offset_world;
            // 朝向 = 云台世界旋转 × 炮口局部旋转 × Rz(90°)（Bevy 相机朝 -Z，需要这个修正）。
            camera_transform.rotation = (gimbal_world_rotation
                * launch_offset.rotation
                * Quat::from_euler(EulerRot::ZYX, 0.0, 0.0, PI / 2.0))
            .normalize()
        }
        FollowingType::ThirdPerson => {
            // 第三人称：在车后上方固定偏移处，回头看向车（`Vec3::Y` 指定世界上方）。
            let base_transform = infantry.into_inner();
            let offset = base_transform.rotation * camera_offset.follow_offset;
            camera_transform.translation = base_transform.translation + offset;
            camera_transform.look_at(base_transform.translation, Vec3::Y);
        }
        FollowingType::Free => {} // 自由模式由 freecam_controls 接管，这里不动
    }
}

/// 自由相机：鼠标看向 + 键盘平移（仅在 Free 模式运行）。
pub fn freecam_controls(
    time: Res<Time>,
    mode: Res<CameraMode>,
    config: Res<SimulationConfig>,
    // `MessageReader<T>`：读取消息流（消息 vs 事件的差别见 controller.rs 中 MessageWriter 的说明）。
    mut mouse_motion_events: MessageReader<MouseMotion>,
    keyboard: Res<ButtonInput<KeyCode>>,
    camera_query: Single<&mut Transform, (With<MainCamera>, Without<Infantry>)>,
) {
    if mode.0 != FollowingType::Free {
        return;
    }

    let delta = time.delta_secs();
    let mut camera_transform = camera_query.into_inner();

    // 汇总本帧所有鼠标位移（一帧可能收到多段 MouseMotion）。
    let mut mouse_delta = Vec2::ZERO;
    for event in mouse_motion_events.read() {
        mouse_delta += event.delta;
    }

    if mouse_delta != Vec2::ZERO {
        // 把当前旋转拆成 yaw/pitch/roll（YXZ 顺序），只改 yaw/pitch 再合回去。
        let (yaw, pitch, roll) = camera_transform.rotation.to_euler(EulerRot::YXZ);

        let new_yaw = yaw - mouse_delta.x * config.camera.mouse_sensitivity;
        // pitch 限制在 ±1.4 rad（约 ±80°），防止视角翻转。
        let new_pitch = (pitch - mouse_delta.y * config.camera.mouse_sensitivity).clamp(-1.4, 1.4);

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
        camera_transform.translation += up * speed;
    }
    if keyboard.pressed(KeyCode::KeyJ) {
        camera_transform.translation -= up * speed;
    }
}
