use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SceneDoc {
    pub entities: Vec<EntityDoc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EntityDoc {
    pub id: String,
    pub components: Vec<ComponentDoc>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ComponentDoc {
    pub type_id: String,
    // Typed payload used by both editor and runtime
    #[serde(default)]
    pub data: CompData,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct CompData {
    // Transform
    #[serde(default)]
    pub translation: Option<(f32, f32, f32)>,
    #[serde(default)]
    pub look_at: Option<(f32, f32, f32)>,
    #[serde(default)]
    pub rot_x_deg: Option<f32>,

    // Mesh3d
    #[serde(default)]
    pub shape: Option<String>, // "Circle" | "Cuboid"
    #[serde(default)]
    pub radius: Option<f32>, // Circle
    #[serde(default)]
    pub x: Option<f32>, // Cuboid dims
    #[serde(default)]
    pub y: Option<f32>,
    #[serde(default)]
    pub z: Option<f32>,

    // Material3d
    #[serde(default)]
    pub color: Option<(f32, f32, f32, f32)>,

    // PointLight
    #[serde(default)]
    pub shadows_enabled: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectConfig {
    pub name: String,
    pub entry: String,        // e.g., "src/main.rs"
    pub bevy_version: String, // stored as text; you’ll drive cargo add externally
}

#[derive(Debug)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

#[derive(Debug)]
pub struct ProjectState {
    pub root: PathBuf,
    pub config: ProjectConfig,
    pub last_diagnostics: Vec<Diagnostic>,

    pub design_scene: Option<SceneDoc>,
    design_path: Option<PathBuf>,
    design_mtime: Option<SystemTime>,
}

impl ProjectState {
    pub fn open(dir: &Path) -> Result<Self> {
        let root = dir.to_path_buf();

        let cfg_path = root.join("project.ron");
        let cfg_text = fs::read_to_string(&cfg_path)
            .with_context(|| format!("reading {}", cfg_path.display()))?;
        let config: ProjectConfig =
            ron::from_str(&cfg_text).with_context(|| "parsing project.ron")?;

        let design_path = root.join("design/initial.scene.ron");
        let (design_scene, design_mtime) = if design_path.exists() {
            let txt = fs::read_to_string(&design_path)
                .with_context(|| format!("reading {}", design_path.display()))?;
            let scene: SceneDoc =
                ron::from_str(&txt).with_context(|| "parsing design/initial.scene.ron")?;
            let mt = fs::metadata(&design_path)?.modified().ok();
            (Some(scene), mt)
        } else {
            (None, None)
        };

        Ok(Self {
            root,
            config,
            last_diagnostics: Vec::new(),
            design_scene,
            design_path: if design_path.exists() {
                Some(design_path)
            } else {
                None
            },
            design_mtime,
        })
    }

    pub fn save_design(&mut self) -> anyhow::Result<()> {
        let Some(path) = &self.design_path else {
            anyhow::bail!("no design file");
        };
        let Some(scene) = &self.design_scene else {
            anyhow::bail!("no scene in memory");
        };

        // pretty RON for humans
        let pretty = ron::ser::PrettyConfig::new()
            .struct_names(true)
            .compact_arrays(false)
            .indentor("  ");

        let text = ron::ser::to_string_pretty(scene, pretty)?;
        fs::write(path, text)?;
        // bump mtime so our watcher doesn’t thrash
        self.design_mtime = fs::metadata(path).ok().and_then(|m| m.modified().ok());
        Ok(())
    }

    pub fn reload_design_if_changed(&mut self) {
        let Some(p) = &self.design_path else {
            return;
        };
        let Ok(md) = fs::metadata(p) else {
            return;
        };
        let Ok(mt) = md.modified() else {
            return;
        };
        if self.design_mtime.map(|t| mt > t).unwrap_or(true) {
            if let Ok(txt) = fs::read_to_string(p) {
                if let Ok(scene) = ron::from_str::<SceneDoc>(&txt) {
                    self.design_scene = Some(scene);
                    self.design_mtime = Some(mt);
                }
            }
        }
    }
}
