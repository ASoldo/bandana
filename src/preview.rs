use bevy::prelude::*;
use crossbeam::channel::{Receiver, TryRecvError};
use std::thread;

use crate::project::{CompData, SceneDoc}; // your types

#[derive(Component)]
struct PreviewTag; // mark spawned scene entities so we can clear/rebuild

#[derive(Resource)]
struct SceneRx(Receiver<SceneDoc>);

pub struct PreviewHandle {
    tx_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _thread: thread::JoinHandle<()>,
}

impl PreviewHandle {
    /// Spawn a Bevy window in a background thread and return a handle.
    pub fn start(scene_rx: Receiver<SceneDoc>) -> Self {
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_clone = alive.clone();

        let th = thread::spawn(move || {
            let mut app = App::new();

            app.add_plugins(DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "Preview".into(),
                    resolution: (960., 540.).into(),
                    present_mode: bevy::window::PresentMode::AutoVsync,
                    ..default()
                }),
                ..default()
            }));

            app.insert_resource(SceneRx(scene_rx));

            // basic scene that matches your game defaults
            app.add_systems(Startup, setup)
                .add_systems(Update, (apply_scene_updates,));

            app.run();
            // When the window closes, the app exits; thread ends.
            let _ = alive_clone;
        });

        Self {
            tx_alive: alive,
            _thread: th,
        }
    }
}

fn setup(mut commands: Commands) {
    // light + camera live outside PreviewTag so we don't wipe them
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(-2.5, 4.5, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        PointLight {
            shadows_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0),
    ));
}

/// Poll the channel; if there’s a new SceneDoc, rebuild PreviewTag entities.
fn apply_scene_updates(
    mut commands: Commands,
    rx: Res<SceneRx>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    query_existing: Query<Entity, With<PreviewTag>>,
) {
    let doc = match rx.0.try_recv() {
        Ok(d) => d,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => return,
    };

    // clear old content
    for e in &query_existing {
        commands.entity(e).despawn_recursive();
    }

    // rebuild from SceneDoc (same logic as your game loader)
    for ent in doc.entities {
        let mut transform = Transform::default();
        let mut want_mesh: Option<Mesh3d> = None;
        let mut want_mat: Option<MeshMaterial3d<StandardMaterial>> = None;

        for c in ent.components {
            match c.type_id.as_str() {
                "Transform" => {
                    if let Some((x, y, z)) = c.data.translation {
                        transform.translation = Vec3::new(x, y, z);
                    }
                    if let Some(deg) = c.data.rot_x_deg {
                        transform.rotate_x(deg.to_radians());
                    }
                    if let Some((x, y, z)) = c.data.look_at {
                        transform.look_at(Vec3::new(x, y, z), Vec3::Y);
                    }
                }
                "Mesh3d" => match c.data.shape.as_deref() {
                    Some("Circle") => {
                        let r = c.data.radius.unwrap_or(1.0);
                        want_mesh = Some(Mesh3d(meshes.add(Circle::new(r))));
                    }
                    Some("Cuboid") => {
                        let x = c.data.x.unwrap_or(1.0);
                        let y = c.data.y.unwrap_or(1.0);
                        let z = c.data.z.unwrap_or(1.0);
                        want_mesh = Some(Mesh3d(meshes.add(Cuboid::new(x, y, z))));
                    }
                    _ => {}
                },
                "Material3d" => {
                    let (r, g, b, a) = c.data.color.unwrap_or((1.0, 1.0, 1.0, 1.0));
                    want_mat = Some(MeshMaterial3d(
                        materials.add(Color::linear_rgba(r, g, b, a)),
                    ));
                }
                _ => {}
            }
        }

        let id = commands.spawn((PreviewTag, transform)).id();
        let mut ec = commands.entity(id);
        if let Some(m) = want_mesh {
            ec.insert(m);
        }
        if let Some(mat) = want_mat {
            ec.insert(mat);
        }
    }
}
