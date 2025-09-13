use crate::build_meta;

use crate::build::{BuildJob, BuildResult, BuildWorker};
use crate::fs_watcher::WatchWorker;
use crate::preview::PreviewHandle;
use crate::project::{AttachedScript, CompData, ProjectState, SceneDoc};
use crossbeam::channel::{Receiver, Sender, unbounded};
use eframe::egui;
use eframe::egui::{ComboBox, DragValue, Rgba};
use egui::color_picker::Alpha;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Schema {
    scripts: Vec<ScriptMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScriptMeta {
    name: String,
    rust_symbol: String,
    params: Vec<ParamMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ParamMeta {
    key: String,
    label: String,
    ty: ParamType,
    default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
enum ParamType {
    Bool,
    I64,
    F64,
    String,
    Vec3,
    ColorRgba,
}

pub struct EditorApp {
    project: Option<ProjectState>,
    build_tx: Sender<BuildJob>,
    build_rx: Receiver<BuildResult>,
    watcher: Option<WatchWorker>,
    last_log: String,
    selected_entity: Option<usize>,

    // --- runner state ---
    run_child: Option<Child>,
    run_rx: Option<Receiver<String>>,
    run_log: Vec<String>,

    // Push-based wakeups
    egui_ctx: egui::Context,
    preview: Option<(PreviewHandle, Sender<SceneDoc>)>,

    // --- viewport (2D top-down preview) ---
    view_offset: egui::Vec2, // world-space pan (in "meters")
    view_zoom: f32,          // screen pixels per world unit
    //
    script_schema: Option<Schema>,
    schema_mtime: Option<std::time::SystemTime>,
}

impl EditorApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (build_tx, build_rx) = BuildWorker::start();
        Self {
            project: None,
            build_tx,
            build_rx,
            watcher: None,
            last_log: String::new(),
            selected_entity: None,

            run_child: None,
            run_rx: None,
            run_log: Vec::new(),

            egui_ctx: cc.egui_ctx.clone(),
            preview: None,

            view_offset: egui::vec2(0.0, 0.0),
            view_zoom: 40.0,
            script_schema: None,
            schema_mtime: None,
        }
    }
    fn draw_scripts_section(
        ui: &mut egui::Ui,
        ent: &mut crate::project::EntityDoc,
        schema: Option<&Schema>,
    ) {
        ui.separator();
        ui.collapsing("Scripts", |ui| {
            let scripts_vec = &mut ent.scripts;

            if let Some(schema) = schema {
                ui.horizontal(|ui| {
                    static mut PICK: usize = 0;
                    let mut pick = unsafe { PICK };

                    let names: Vec<&str> = schema.scripts.iter().map(|s| s.name.as_str()).collect();

                    egui::ComboBox::from_label("Add script")
                        .selected_text(names.get(pick).copied().unwrap_or("<select>"))
                        .show_ui(ui, |ui| {
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut pick, i, *n);
                            }
                        });

                    if ui.button("Add").clicked() {
                        if let Some(sel) = names.get(pick) {
                            let already = scripts_vec.iter().any(|a| a.name == *sel);
                            if !already {
                                scripts_vec.push(AttachedScript {
                                    name: (*sel).to_string(),
                                    params: Default::default(),
                                });
                            }
                        }
                        unsafe {
                            PICK = pick;
                        }
                    }
                });
            } else {
                ui.small("No schema loaded yet.");
            }

            ui.separator();

            // current attachments
            let mut to_remove: Option<usize> = None;
            for (i, a) in scripts_vec.iter_mut().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("• {}", a.name));
                        if ui.button("Remove").clicked() {
                            to_remove = Some(i);
                        }
                    });

                    // Params UI can be added next iteration using schema lookup.
                    ui.small("Params UI TBD.");
                });
                ui.add_space(4.0);
            }
            if let Some(i) = to_remove {
                scripts_vec.remove(i);
            }
        });
    }

    fn load_script_schema_from(&mut self, root: &std::path::Path) {
        use std::fs;

        let path = root.join("design/.schema.ron");
        match fs::read_to_string(&path) {
            Ok(txt) => match ron::from_str::<Schema>(&txt) {
                Ok(schema) => {
                    self.schema_mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
                    let count = schema.scripts.len();
                    self.script_schema = Some(schema);
                    self.last_log = format!("Loaded script schema ({} scripts).", count);
                }
                Err(e) => {
                    self.script_schema = None;
                    self.last_log = format!("Failed to parse .schema.ron: {e}");
                }
            },
            Err(e) => {
                self.script_schema = None;
                self.last_log = format!("No .schema.ron yet (run exporter): {e}");
            }
        }

        self.egui_ctx.request_repaint();
    }

    // button to open/ensure preview (reserved for future Bevy offscreen):
    fn ensure_preview(&mut self) {
        if self.preview.is_none() {
            let (tx, rx) = unbounded::<SceneDoc>();
            let handle = PreviewHandle::start(rx);
            self.preview = Some((handle, tx));
        }
    }

    fn open_project(&mut self, path: PathBuf) {
        match ProjectState::open(&path) {
            Ok(proj) => {
                // Initial check
                let _ = self.build_tx.send(BuildJob::Check {
                    root: proj.root.clone(),
                });
                self.egui_ctx.request_repaint();

                // Watcher -> build loop
                let (evt_tx, evt_rx) = unbounded();
                self.watcher = Some(WatchWorker::start(proj.root.clone(), evt_tx));

                let build_tx = self.build_tx.clone();
                let root = proj.root.clone(); // avoid partially moving proj
                let egui_ctx = self.egui_ctx.clone();
                std::thread::spawn(move || {
                    while let Ok(_evt) = evt_rx.recv() {
                        let _ = build_tx.send(BuildJob::Check { root: root.clone() });
                        egui_ctx.request_repaint(); // wake UI when FS events arrive
                    }
                });

                // Set the project
                self.project = Some(proj);

                // ⬅️ Borrow ends; now take a plain PathBuf and call the &mut self method.
                let root_for_schema = self.project.as_ref().unwrap().root.clone();
                self.load_script_schema_from(&root_for_schema);
            }
            Err(e) => {
                self.last_log = format!("Failed to open project: {e:?}");
                self.egui_ctx.request_repaint();
            }
        }
    }

    fn ui_menubar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New Project…").clicked() {
                    // wire later
                    ui.close();
                }
                if ui.button("Open Project…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.open_project(path);
                    }
                    ui.close();
                }
                if ui
                    .add_enabled(self.project.is_some(), egui::Button::new("Save Scene"))
                    .clicked()
                {
                    if let Some(p) = &mut self.project {
                        match p.save_design() {
                            Ok(_) => self.last_log = "scene saved".into(),
                            Err(e) => self.last_log = format!("save failed: {e:#}"),
                        }
                    }
                    self.egui_ctx.request_repaint();
                    ui.close();
                }
                if ui
                    .add_enabled(
                        self.project.is_some() && self.run_child.is_none(),
                        egui::Button::new("Run"),
                    )
                    .clicked()
                {
                    self.start_run();
                    ui.close();
                }
                if ui
                    .add_enabled(self.run_child.is_some(), egui::Button::new("Stop"))
                    .clicked()
                {
                    self.stop_run();
                    ui.close();
                }
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }

    // ---------- runner helpers ----------

    fn start_run(&mut self) {
        let Some(p) = &self.project else {
            self.last_log = "no project open".into();
            self.egui_ctx.request_repaint();
            return;
        };
        if self.run_child.is_some() {
            self.last_log = "runner already active".into();
            self.egui_ctx.request_repaint();
            return;
        }

        let mut cmd = Command::new("cargo");
        cmd.arg("run")
            .current_dir(&p.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let (tx, rx) = unbounded::<String>();

                if let Some(out) = stdout {
                    let tx_out = tx.clone();
                    let egui_ctx = self.egui_ctx.clone();
                    std::thread::spawn(move || {
                        let reader = BufReader::new(out);
                        for line in reader.lines().flatten() {
                            let _ = tx_out.send(format!("[out] {line}"));
                            egui_ctx.request_repaint(); // wake per line
                        }
                    });
                }
                if let Some(err) = stderr {
                    let tx_err = tx.clone();
                    let egui_ctx = self.egui_ctx.clone();
                    std::thread::spawn(move || {
                        let reader = BufReader::new(err);
                        for line in reader.lines().flatten() {
                            let _ = tx_err.send(format!("[err] {line}"));
                            egui_ctx.request_repaint(); // wake per line
                        }
                    });
                }

                self.run_child = Some(child);
                self.run_rx = Some(rx);
                self.run_log.clear();
                self.last_log = "runner started".into();
                self.egui_ctx.request_repaint();
            }
            Err(e) => {
                self.last_log = format!("failed to start runner: {e}");
                self.egui_ctx.request_repaint();
            }
        }
    }

    fn stop_run(&mut self) {
        if let Some(mut child) = self.run_child.take() {
            let _ = child.kill();
            let _ = child.wait();
            self.last_log = "runner stopped".into();
        }
        self.run_rx = None;
        self.egui_ctx.request_repaint();
    }

    fn pump_run_log(&mut self) {
        if let Some(rx) = &self.run_rx {
            while let Ok(line) = rx.try_recv() {
                self.run_log.push(line);
                if self.run_log.len() > 5000 {
                    let drain = self.run_log.len() - 5000;
                    self.run_log.drain(0..drain);
                }
            }
        }
    }
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(p) = &mut self.project {
            // Hot-reload scene file if changed
            p.reload_design_if_changed();

            // Hot-reload .schema.ron if changed
            use std::fs;
            let root = p.root.clone(); // take a copy while we have &mut p
            if let Ok(mt) = fs::metadata(root.join("design/.schema.ron")).and_then(|m| m.modified())
            {
                let changed = match self.schema_mtime {
                    None => true,
                    Some(old) => mt > old,
                };
                if changed {
                    self.load_script_schema_from(&root);
                }
            }
        }

        // drain build results
        while let Ok(msg) = self.build_rx.try_recv() {
            match msg {
                BuildResult::Ok { duration_ms } => {
                    self.last_log = format!("cargo check: OK in {duration_ms} ms");
                }
                BuildResult::Err {
                    duration_ms,
                    diagnostics,
                } => {
                    self.last_log = format!("cargo check: ERR in {duration_ms} ms");
                    if let Some(p) = &mut self.project {
                        p.last_diagnostics = diagnostics;
                    }
                }
            }
        }

        // drain runner output
        self.pump_run_log();

        egui::TopBottomPanel::top("menubar").show(ctx, |ui| self.ui_menubar(ui));

        egui::SidePanel::left("hierarchy")
            .resizable(true)
            .default_width(240.0)
            .show(ctx, |ui| {
                ui.heading("Hierarchy");

                match &self.project {
                    Some(p) => {
                        if let Some(scene) = &p.design_scene {
                            ui.label(format!("{} entities", scene.entities.len()));
                            ui.separator();
                            for (i, ent) in scene.entities.iter().enumerate() {
                                let selected = self.selected_entity == Some(i);
                                if ui.selectable_label(selected, &ent.id).clicked() {
                                    self.selected_entity = Some(i);
                                }
                            }
                        } else {
                            ui.label("No scene loaded yet.");
                            ui.small("Put design/initial.scene.ron in the project.");
                        }
                    }
                    None => {
                        ui.label("Open a project.");
                    }
                }
            });

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(360.0)
            .show(ctx, |ui| {
                ui.heading("Inspector");

                if let Some(p) = &mut self.project {
                    
                    if let (Some(scene), Some(sel)) = (&mut p.design_scene, self.selected_entity) {
                        let mut want_save = false;

                        {
                            // ── begin short borrow of the selected entity
                            let ent = scene
                                .entities
                                .get_mut(sel)
                                .expect("selected index valid while drawing");

                            ui.monospace(format!("Entity: {}", ent.id));
                            ui.separator();

                            for comp in &mut ent.components {
                                ui.collapsing(&comp.type_id, |ui| match comp.type_id.as_str() {
                                    "Transform"  => draw_transform(ui, &mut comp.data),
                                    "Mesh3d"     => draw_mesh3d(ui, &mut comp.data),
                                    "Material3d" => draw_material3d(ui, &mut comp.data),
                                    "PointLight" => draw_point_light(ui, &mut comp.data),
                                    "Camera3d"   => { ui.label("No editable fields"); }
                                    _            => { ui.label("Unsupported component"); }
                                });
                            }

                            ui.separator();
                            // just set a flag; do NOT call save while `ent` is borrowed
                            if ui.button("Save scene").clicked() {
                                want_save = true;
                            }

                            // scripts UI also needs &mut ent, so keep it inside this scope
                            Self::draw_scripts_section(ui, ent, self.script_schema.as_ref());
                        } // ── entity borrow ends here

                        // Now it's safe to call methods that borrow `p` mutably.
                        if want_save {
                            match p.save_design() {
                                Ok(_)  => self.last_log = "scene saved".into(),
                                Err(e) => self.last_log = format!("save failed: {e:#}"),
                            }
                            self.egui_ctx.request_repaint();
                        }
                    }

                    ui.separator();
                    if ui.button("Run cargo check").clicked() {
                        let _ = self.build_tx.send(BuildJob::Check {
                            root: p.root.clone(),
                        });
                        self.egui_ctx.request_repaint();
                    }
                    ui.separator();
                    ui.monospace(&self.last_log);
                    ui.separator();
                    ui.collapsing("Diagnostics", |ui| {
                        for d in &p.last_diagnostics {
                            ui.label(format!(
                                "{}:{}:{} {}",
                                d.file.display(),
                                d.line,
                                d.col,
                                d.msg
                            ));
                        }
                    });

                    ui.separator();
                    ui.collapsing("Scripts (schema)", |ui| {
                        match &self.script_schema {
                            Some(s) if !s.scripts.is_empty() => {
                                for sm in &s.scripts {
                                    ui.label(format!("• {}  ({})", sm.name, sm.rust_symbol));
                                }
                            }
                            _ => {
                                ui.small("No scripts available. Build the game with `--features bandana_export` and run the exporter bin to create design/.schema.ron");
                            }
                        }
                    });

                   // Stage the root we want to export from
                    let mut want_export: Option<std::path::PathBuf> = None;

                    if ui.button("Export meta").clicked() {
                        if let Some(p) = &self.project {
                            want_export = Some(p.root.clone());
                        }
                    }

                    // Run export after the borrow of `p` has ended
                    if let Some(root) = want_export {
                        match build_meta::export_schema(&root, &[]) {
                            Ok(res) => {
                                // show logs in your console
                                if !res.stdout.is_empty() {
                                    for line in res.stdout.lines() {
                                        self.run_log.push(format!("[export/stdout] {line}"));
                                    }
                                }
                                if !res.stderr.is_empty() {
                                    for line in res.stderr.lines() {
                                        self.run_log.push(format!("[export/stderr] {line}"));
                                    }
                                }

                                // keep console bounded like elsewhere
                                if self.run_log.len() > 5000 {
                                    let drain = self.run_log.len() - 5000;
                                    self.run_log.drain(0..drain);
                                }

                                if res.success() {
                                    self.last_log = "Exported script schema.".into();
                                    // hot-reload the schema file into the editor
                                    self.load_script_schema_from(&root);
                                } else {
                                    self.last_log =
                                        format!("Export failed (exit {}). See console.", res.status);
                                }
                            }
                            Err(e) => {
                                self.last_log = format!("Failed to run exporter: {e}");
                            }
                        }
                        self.egui_ctx.request_repaint();
                    }


                } else {
                    ui.label("Open a project to inspect.");
                }
            });

        // --- Console / Logs bottom panel (ALWAYS VISIBLE) ---
        egui::TopBottomPanel::bottom("console")
            .resizable(true)
            .default_height(160.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Console");
                    ui.separator();
                    if let Some(p) = &self.project {
                        if ui.button("Run cargo check").clicked() {
                            let _ = self.build_tx.send(BuildJob::Check {
                                root: p.root.clone(),
                            });
                        }
                    }
                    ui.separator();
                    ui.label(&self.last_log);
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        if self.run_log.is_empty() {
                            ui.label("Runner output will be shown here.");
                        } else {
                            for line in &self.run_log {
                                ui.monospace(line);
                            }
                        }
                    });
            });

        // --- Main viewport (scene preview) ---
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Viewport");
            ui.separator();

            // Run controls
            ui.horizontal(|ui| {
                let running = self.run_child.is_some();
                if ui
                    .add_enabled(
                        !running && self.project.is_some(),
                        egui::Button::new("Run project"),
                    )
                    .clicked()
                {
                    self.start_run();
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop_run();
                }
                ui.label(if running {
                    "Status: running"
                } else {
                    "Status: idle"
                });
            });

            ui.separator();

            // Scene preview
            if let Some(p) = &self.project {
                if let Some(scene) = &p.design_scene {
                    draw_scene_preview(ui, scene, &mut self.view_offset, &mut self.view_zoom);
                } else {
                    ui.label("No scene loaded yet (design/initial.scene.ron).");
                }
            } else {
                ui.label("No project open.");
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // don't leave a rogue process chewing ammo
        self.stop_run();
    }
}

// ================== Typed inspectors ==================

fn draw_transform(ui: &mut egui::Ui, d: &mut CompData) {
    // Translation only (rotation & look_at removed for now)
    ui.vertical(|ui| {
        ui.label("translation");
        let mut t = d.translation.unwrap_or((0.0, 0.0, 0.0));
        ui.horizontal(|ui| {
            ui.add(DragValue::new(&mut t.0).speed(0.1).prefix("x "));
            ui.add(DragValue::new(&mut t.1).speed(0.1).prefix("y "));
            ui.add(DragValue::new(&mut t.2).speed(0.1).prefix("z "));
        });
        d.translation = Some(t);
    });
}

fn draw_mesh3d(ui: &mut egui::Ui, d: &mut CompData) {
    let mut shape = d.shape.clone().unwrap_or_else(|| "Cuboid".into());
    ComboBox::from_label("shape")
        .selected_text(&shape)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut shape, "Circle".into(), "Circle");
            ui.selectable_value(&mut shape, "Cuboid".into(), "Cuboid");
        });
    d.shape = Some(shape.clone());

    match shape.as_str() {
        "Circle" => {
            let mut r = d.radius.unwrap_or(1.0);
            ui.add(DragValue::new(&mut r).speed(0.1).prefix("radius "));
            d.radius = Some(r);
            // clear cuboid dims so we don't serialize junk
            d.x = None;
            d.y = None;
            d.z = None;
        }
        _ => {
            let mut x = d.x.unwrap_or(1.0);
            let mut y = d.y.unwrap_or(1.0);
            let mut z = d.z.unwrap_or(1.0);
            ui.horizontal(|ui| {
                ui.add(DragValue::new(&mut x).speed(0.1).prefix("x "));
                ui.add(DragValue::new(&mut y).speed(0.1).prefix("y "));
                ui.add(DragValue::new(&mut z).speed(0.1).prefix("z "));
            });
            d.x = Some(x);
            d.y = Some(y);
            d.z = Some(z);
            d.radius = None;
        }
    }
}

fn draw_material3d(ui: &mut egui::Ui, d: &mut CompData) {
    let (mut r, mut g, mut b, mut a) = d.color.unwrap_or((1.0, 1.0, 1.0, 1.0));
    let mut rgba = Rgba::from_rgba_premultiplied(r, g, b, a);
    egui::color_picker::color_edit_button_rgba(ui, &mut rgba, Alpha::Opaque);
    r = rgba.r();
    g = rgba.g();
    b = rgba.b();
    a = rgba.a();
    d.color = Some((r, g, b, a));
}

fn draw_point_light(ui: &mut egui::Ui, d: &mut CompData) {
    let mut sh = d.shadows_enabled.unwrap_or(false);
    ui.checkbox(&mut sh, "shadows_enabled");
    d.shadows_enabled = Some(sh);
}

// ================== 2D top-down preview (egui painter) ==================

#[derive(Clone, Copy)]
struct DrawCmd {
    kind: DrawKind,
    pos: egui::Vec2,      // world xz
    size: egui::Vec2,     // world size (for circle: x = radius, y = radius)
    color: egui::Color32, // sRGBA
    height_y: f32,
}

#[derive(Clone, Copy)]
enum DrawKind {
    Circle,
    Rect,
}

fn gather_draw_cmds(scene: &crate::project::SceneDoc) -> Vec<DrawCmd> {
    use egui::Color32;
    let mut cmds = Vec::new();

    for ent in &scene.entities {
        let mut pos_xz = (0.0f32, 0.0f32);
        let mut pos_y = 0.0f32; // <-- NEW

        let mut color = Color32::from_rgba_premultiplied(200, 200, 200, 255);
        let mut shape: Option<&str> = None;
        let mut radius: Option<f32> = None;
        let mut cuboid_xz: Option<(f32, f32)> = None;

        for comp in &ent.components {
            match comp.type_id.as_str() {
                "Transform" => {
                    if let Some((x, y, z)) = comp.data.translation {
                        pos_xz = (x, z);
                        pos_y = y; // <-- NEW
                    }
                }
                "Material3d" => {
                    if let Some((r, g, b, a)) = comp.data.color {
                        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                        color = Color32::from_rgba_premultiplied(
                            to_u8(r),
                            to_u8(g),
                            to_u8(b),
                            to_u8(a),
                        );
                    }
                }
                "Mesh3d" => {
                    if let Some(s) = comp.data.shape.as_deref() {
                        shape = Some(s);
                        match s {
                            "Circle" => radius = comp.data.radius.or(Some(1.0)),
                            "Cuboid" => {
                                let x = comp.data.x.unwrap_or(1.0);
                                let z = comp.data.z.unwrap_or(1.0);
                                cuboid_xz = Some((x, z));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        match shape {
            Some("Circle") => {
                let r = radius.unwrap_or(1.0);
                cmds.push(DrawCmd {
                    kind: DrawKind::Circle,
                    pos: egui::vec2(pos_xz.0, pos_xz.1),
                    size: egui::vec2(r, r),
                    color,
                    height_y: pos_y,
                });
            }
            Some("Cuboid") => {
                let (x, z) = cuboid_xz.unwrap_or((1.0, 1.0));
                cmds.push(DrawCmd {
                    kind: DrawKind::Rect,
                    pos: egui::vec2(pos_xz.0, pos_xz.1),
                    size: egui::vec2(x, z),
                    color,
                    height_y: pos_y,
                });
            }
            _ => {}
        }
    }

    cmds
}

fn draw_scene_preview(
    ui: &mut egui::Ui,
    scene: &crate::project::SceneDoc,
    view_offset: &mut egui::Vec2,
    view_zoom: &mut f32,
) {
    use std::cmp::Ordering;

    // Panel area
    let avail = ui.available_size();
    let (response, painter) = ui.allocate_painter(avail, egui::Sense::click_and_drag());

    // Mouse wheel zoom:
    if response.hovered() {
        if let Some(scroll) = ui.input(|i| i.smooth_scroll_delta.y).into() {
            // zoom around mouse
            let zoom_factor = (1.0 + scroll * -0.001).clamp(0.5, 4.0);
            let old_zoom = *view_zoom;
            let new_zoom = (old_zoom * zoom_factor).clamp(10.0, 400.0);

            // keep world point under cursor stable
            let mouse_pos = ui.input(|i| i.pointer.hover_pos());
            if let Some(mp) = mouse_pos {
                let world_before = screen_to_world(mp, response.rect, *view_offset, old_zoom);
                *view_zoom = new_zoom;
                let world_after = screen_to_world(mp, response.rect, *view_offset, new_zoom);
                *view_offset += world_after - world_before;
            } else {
                *view_zoom = new_zoom;
            }
        }
    }

    // Drag to pan:
    if response.dragged() {
        let drag = response.drag_delta();
        // convert screen drag to world delta
        *view_offset -= drag / *view_zoom;
    }

    // Background
    painter.rect_filled(response.rect, 0.0, ui.visuals().extreme_bg_color);

    // Draw grid (every 1.0 world unit)
    draw_grid(
        &painter,
        response.rect,
        *view_offset,
        *view_zoom,
        ui.visuals().weak_text_color(),
    );

    // Gather draw commands from scene
    let mut cmds = gather_draw_cmds(scene);

    // 🔹 Depth sort: lower Y first, higher Y last (so higher objects draw on top)
    cmds.sort_by(|a, b| {
        a.height_y
            .partial_cmp(&b.height_y)
            .unwrap_or(Ordering::Equal)
    });

    // Draw each
    for cmd in cmds {
        match cmd.kind {
            DrawKind::Circle => {
                let center = world_to_screen(cmd.pos, response.rect, *view_offset, *view_zoom);
                let r_px = cmd.size.x * *view_zoom;
                painter.circle_filled(center, r_px, cmd.color);
                painter.circle_stroke(
                    center,
                    r_px,
                    egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.fg_stroke.color),
                );
            }
            DrawKind::Rect => {
                // Rect centered at pos with size.x by size.y (world)
                let half = cmd.size * 0.5;
                let p0 = world_to_screen(cmd.pos - half, response.rect, *view_offset, *view_zoom);
                let p1 = world_to_screen(cmd.pos + half, response.rect, *view_offset, *view_zoom);
                let rect = egui::Rect::from_two_pos(p0, p1);
                painter.rect_filled(rect, 2.0, cmd.color);
                painter.rect_stroke(
                    rect,
                    2.0,
                    egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.fg_stroke.color),
                    egui::StrokeKind::Inside,
                );
            }
        }
    }
}

fn world_to_screen(
    world_xz: egui::Vec2,
    rect: egui::Rect,
    offset_world: egui::Vec2,
    zoom: f32,
) -> egui::Pos2 {
    // origin is centered; +x right, +z down (screen y grows downward)
    let centered = (world_xz - offset_world) * zoom;
    let screen_center = rect.center();
    egui::pos2(screen_center.x + centered.x, screen_center.y + centered.y)
}

fn screen_to_world(
    pos: egui::Pos2,
    rect: egui::Rect,
    offset_world: egui::Vec2,
    zoom: f32,
) -> egui::Vec2 {
    let screen_center = rect.center();
    let v = egui::vec2(pos.x - screen_center.x, pos.y - screen_center.y) / zoom;
    v + offset_world
}

fn draw_grid(
    painter: &egui::Painter,
    rect: egui::Rect,
    offset_world: egui::Vec2,
    zoom: f32,
    color: egui::Color32,
) {
    // grid every 1 world unit; show about ~50 lines max
    let spacing_px = zoom;
    if spacing_px < 8.0 {
        return; // too dense, skip
    }

    let center = rect.center();
    // how many lines to each side
    let half_w = rect.width() * 0.5 / spacing_px;
    let half_h = rect.height() * 0.5 / spacing_px;

    let ox = offset_world.x.fract(); // fractional part to align grid smoothly
    let oz = offset_world.y.fract();

    let x0_idx = (-half_w.floor() as i32) - 2;
    let x1_idx = (half_w.ceil() as i32) + 2;
    let z0_idx = (-half_h.floor() as i32) - 2;
    let z1_idx = (half_h.ceil() as i32) + 2;

    let thin = egui::Stroke::new(1.0, color.linear_multiply(0.25));
    let bold = egui::Stroke::new(1.5, color.linear_multiply(0.6));

    for ix in x0_idx..=x1_idx {
        let x = (ix as f32 - ox) * spacing_px;
        let sx = center.x + x;
        let stroke = if ix == 0 { bold } else { thin };
        painter.line_segment(
            [egui::pos2(sx, rect.top()), egui::pos2(sx, rect.bottom())],
            stroke,
        );
    }
    for iz in z0_idx..=z1_idx {
        let y = (iz as f32 - oz) * spacing_px;
        let sy = center.y + y;
        let stroke = if iz == 0 { bold } else { thin };
        painter.line_segment(
            [egui::pos2(rect.left(), sy), egui::pos2(rect.right(), sy)],
            stroke,
        );
    }
}
