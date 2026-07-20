//! Interfaz gráfica (egui/eframe) de RecupeGhost.
//!
//! Es un frente más sobre el mismo motor que usa el CLI: detecta discos con `drives`, escanea con
//! `scanner::scan_source_quiet` (sin salida por terminal), y recupera con `recovery`. El escaneo y
//! la recuperación corren en un hilo aparte para no congelar la ventana; el avance se lee de
//! `scanner::scan_progress_bytes()` y el resultado llega por un canal.
//!
//! Fase 1 (este archivo): asistente origen → tipos → destino → escaneo con progreso → resultados
//! con marcas de integridad → recuperar. Se irá puliendo (look, manifiesto de admin, etc.).

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;

use crate::drives::{self, DriveInfo};
use crate::recovery::{self, RecoveryResult};
use crate::scanner::{self, Integrity, ScanResult};
use crate::signatures::{signatures_for_categories, FileCategory};
use crate::util::{self, format_size};

/// Abre la ventana de la GUI. Bloquea hasta que el usuario la cierra.
pub fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 640.0])
            .with_min_inner_size([560.0, 480.0])
            .with_title("RecupeGhost"),
        ..Default::default()
    };
    eframe::run_native(
        "RecupeGhost",
        options,
        Box::new(|_cc| Ok(Box::new(RecupeGhostApp::new()))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Setup,
    Scanning,
    Results,
    Recovering,
    Done,
    Error,
}

struct RecupeGhostApp {
    drives: Vec<DriveInfo>,
    selected_drive: usize,
    manual_path: String,
    cats: [bool; 4], // fotos, videos, audio, documentos
    output_dir: String,

    phase: Phase,
    // Trabajo en background y sus resultados.
    source: Option<PathBuf>,
    scan_total: u64,
    scan_rx: Option<Receiver<anyhow::Result<ScanResult>>>,
    scan_result: Option<ScanResult>,
    recovery_rx: Option<Receiver<anyhow::Result<RecoveryResult>>>,
    recovery_result: Option<RecoveryResult>,
    error_msg: String,
}

impl RecupeGhostApp {
    fn new() -> Self {
        Self {
            drives: drives::list_drives(),
            selected_drive: 0,
            manual_path: String::new(),
            cats: [true, true, true, true],
            output_dir: default_output_name(),
            phase: Phase::Setup,
            source: None,
            scan_total: 0,
            scan_rx: None,
            scan_result: None,
            recovery_rx: None,
            recovery_result: None,
            error_msg: String::new(),
        }
    }

    fn selected_categories(&self) -> Vec<FileCategory> {
        let mut c = Vec::new();
        if self.cats[0] {
            c.push(FileCategory::Photo);
        }
        if self.cats[1] {
            c.push(FileCategory::Video);
        }
        if self.cats[2] {
            c.push(FileCategory::Audio);
        }
        if self.cats[3] {
            c.push(FileCategory::Document);
        }
        c
    }

    /// Origen elegido: la ruta manual si el usuario escribió algo; si no, el disco seleccionado.
    fn resolve_source(&self) -> Option<PathBuf> {
        let manual = self.manual_path.trim();
        if !manual.is_empty() {
            return Some(PathBuf::from(manual));
        }
        self.drives
            .get(self.selected_drive)
            .map(|d| PathBuf::from(&d.path))
    }

    fn fail(&mut self, msg: impl Into<String>) {
        self.error_msg = msg.into();
        self.phase = Phase::Error;
    }

    fn start_scan(&mut self) {
        let source = match self.resolve_source() {
            Some(s) => s,
            None => return self.fail("Elegí un disco o escribí una ruta de imagen."),
        };
        let cats = self.selected_categories();
        if cats.is_empty() {
            return self.fail("Elegí al menos un tipo de archivo para buscar.");
        }
        // Misma protección crítica que el CLI: la carpeta de salida no puede ser un dispositivo.
        let out = util::to_absolute_output(PathBuf::from(self.output_dir.trim()));
        if util::is_physical_device(&out) {
            return self.fail(
                "La carpeta de salida no puede ser un disco/dispositivo. Elegí una carpeta normal.",
            );
        }

        let sigs = signatures_for_categories(&cats);
        self.scan_total = scanner::device_or_file_size(&source).unwrap_or(0);
        self.source = Some(source.clone());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(scanner::scan_source_quiet(&source, &sigs));
        });
        self.scan_rx = Some(rx);
        self.scan_result = None;
        self.phase = Phase::Scanning;
    }

    fn start_recovery(&mut self) {
        let (source, files) = match (&self.source, &self.scan_result) {
            (Some(s), Some(r)) => (s.clone(), r.found_files.clone()),
            _ => return,
        };
        let out = util::to_absolute_output(PathBuf::from(self.output_dir.trim()));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(recovery::recover_files(&source, &files, &out));
        });
        self.recovery_rx = Some(rx);
        self.phase = Phase::Recovering;
    }

    /// Revisa los canales de background y avanza de fase cuando llega un resultado.
    fn poll_background(&mut self, ctx: &egui::Context) {
        match self.phase {
            Phase::Scanning => {
                // `as_ref().map(try_recv)` suelta el borrow de `self.scan_rx` antes de mutar self.
                let msg = self.scan_rx.as_ref().map(|rx| rx.try_recv());
                match msg {
                    Some(Ok(Ok(r))) => {
                        self.scan_result = Some(r);
                        self.scan_rx = None;
                        self.phase = Phase::Results;
                    }
                    Some(Ok(Err(e))) => {
                        self.scan_rx = None;
                        self.fail(format!("No se pudo escanear: {e:#}"));
                    }
                    Some(Err(TryRecvError::Empty)) => ctx.request_repaint(),
                    Some(Err(TryRecvError::Disconnected)) => {
                        self.scan_rx = None;
                        self.fail("El escaneo terminó inesperadamente.");
                    }
                    None => {}
                }
            }
            Phase::Recovering => {
                let msg = self.recovery_rx.as_ref().map(|rx| rx.try_recv());
                match msg {
                    Some(Ok(Ok(r))) => {
                        self.recovery_result = Some(r);
                        self.recovery_rx = None;
                        self.phase = Phase::Done;
                    }
                    Some(Ok(Err(e))) => {
                        self.recovery_rx = None;
                        self.fail(format!("No se pudo recuperar: {e:#}"));
                    }
                    Some(Err(TryRecvError::Empty)) => ctx.request_repaint(),
                    Some(Err(TryRecvError::Disconnected)) => {
                        self.recovery_rx = None;
                        self.fail("La recuperación terminó inesperadamente.");
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    // ── Pantallas ──

    fn ui_setup(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.strong("1. ¿Qué disco o imagen querés recuperar?");
        ui.horizontal(|ui| {
            if ui.button("🔄 Actualizar discos").clicked() {
                self.drives = drives::list_drives();
                if self.selected_drive >= self.drives.len() {
                    self.selected_drive = 0;
                }
            }
            ui.label(egui::RichText::new("(elegí uno de la lista)").weak());
        });
        if self.drives.is_empty() {
            ui.label(
                egui::RichText::new(
                    "No se detectaron discos. Probá 'Actualizar' o usá una ruta manual abajo.",
                )
                .weak(),
            );
        }
        egui::ScrollArea::vertical()
            .id_salt("drives_list")
            .max_height(120.0)
            .show(ui, |ui| {
                for (i, d) in self.drives.iter().enumerate() {
                    if ui
                        .selectable_label(self.selected_drive == i, drive_label(d))
                        .clicked()
                    {
                        self.selected_drive = i;
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.label("o ruta manual:");
            ui.text_edit_singleline(&mut self.manual_path);
        });
        ui.label(
            egui::RichText::new(
                "Dejá la ruta manual vacía para usar el disco de arriba, o poné un archivo de imagen (.img/.dd/.raw).",
            )
            .weak(),
        );

        ui.add_space(12.0);
        ui.strong("2. ¿Qué buscar?");
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cats[0], "📷 Fotos");
            ui.checkbox(&mut self.cats[1], "🎬 Videos");
            ui.checkbox(&mut self.cats[2], "🎵 Audio");
            ui.checkbox(&mut self.cats[3], "📄 Documentos");
        });

        ui.add_space(12.0);
        ui.strong("3. ¿Dónde guardo lo recuperado?");
        ui.text_edit_singleline(&mut self.output_dir);
        ui.label(
            egui::RichText::new("⚠ Guardá en un disco DISTINTO al que estás recuperando.")
                .color(egui::Color32::from_rgb(230, 180, 60)),
        );

        ui.add_space(18.0);
        if ui
            .add(egui::Button::new(
                egui::RichText::new("🔍  Escanear").size(18.0),
            ))
            .clicked()
        {
            self.start_scan();
        }
    }

    fn ui_scanning(&mut self, ui: &mut egui::Ui) {
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Escaneando… esto puede tardar según el tamaño del disco.");
        });
        ui.add_space(8.0);
        let done = scanner::scan_progress_bytes();
        let frac = if self.scan_total > 0 {
            (done as f32 / self.scan_total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.add(egui::ProgressBar::new(frac).show_percentage());
        if self.scan_total > 0 {
            ui.label(format!(
                "{} de {}",
                format_size(done.min(self.scan_total)),
                format_size(self.scan_total)
            ));
        }
        ui.add_space(12.0);
        if ui.button("⏹ Cancelar").clicked() {
            scanner::request_cancel();
        }
        ui.ctx().request_repaint();
    }

    fn ui_results(&mut self, ui: &mut egui::Ui) {
        // Extraer lo mostrable sin retener el borrow de `self.scan_result`.
        let (count, rows): (usize, Vec<(Integrity, String)>) = match &self.scan_result {
            Some(res) => (
                res.found_files.len(),
                res.found_files
                    .iter()
                    .take(500)
                    .map(|f| (f.integrity(), f.friendly_summary()))
                    .collect(),
            ),
            None => return,
        };

        ui.add_space(6.0);
        ui.strong(format!("Se encontraron {count} archivos."));
        if count == 0 {
            ui.label("No se encontró nada. Probá con otra imagen o tipos distintos.");
            if ui.button("↩ Volver").clicked() {
                self.phase = Phase::Setup;
                self.scan_result = None;
            }
            return;
        }
        ui.label("✅ íntegro    ⚠ posiblemente dañado    (sin marca) = no verificable");
        ui.add_space(4.0);
        egui::ScrollArea::vertical()
            .id_salt("results_list")
            .max_height(320.0)
            .show(ui, |ui| {
                for (integ, text) in &rows {
                    let color = match integ {
                        Integrity::Intact => egui::Color32::from_rgb(90, 200, 120),
                        Integrity::Suspect => egui::Color32::from_rgb(230, 180, 60),
                        Integrity::Unverifiable => egui::Color32::GRAY,
                    };
                    ui.colored_label(color, text);
                }
                if count > rows.len() {
                    ui.label(format!("… y {} archivos más", count - rows.len()));
                }
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("💾  Recuperar todo").size(16.0),
                ))
                .clicked()
            {
                self.start_recovery();
            }
            if ui.button("↩ Volver").clicked() {
                self.phase = Phase::Setup;
                self.scan_result = None;
            }
        });
        ui.label(
            egui::RichText::new(
                "Los archivos se guardan con nombres nuevos (recovered_NNNN); no conservan el nombre original.",
            )
            .weak(),
        );
    }

    fn ui_recovering(&mut self, ui: &mut egui::Ui) {
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Recuperando y guardando los archivos…");
        });
        ui.ctx().request_repaint();
    }

    fn ui_done(&mut self, ui: &mut egui::Ui) {
        let summary = self
            .recovery_result
            .as_ref()
            .map(|r| r.summary())
            .unwrap_or_default();
        ui.add_space(12.0);
        ui.strong("✅ ¡Listo! Recuperación completada.");
        ui.add_space(6.0);
        ui.label(summary);
        ui.add_space(12.0);
        if ui.button("↩ Volver al inicio").clicked() {
            self.phase = Phase::Setup;
            self.scan_result = None;
            self.recovery_result = None;
        }
    }

    fn ui_error(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.colored_label(
            egui::Color32::from_rgb(220, 90, 90),
            format!("❌ {}", self.error_msg),
        );
        ui.add_space(12.0);
        if ui.button("↩ Volver").clicked() {
            self.phase = Phase::Setup;
        }
    }
}

impl eframe::App for RecupeGhostApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("👻 RecupeGhost");
            ui.label("Recuperá fotos, videos, audios y documentos borrados.");
            ui.separator();
            match self.phase {
                Phase::Setup => self.ui_setup(ui),
                Phase::Scanning => self.ui_scanning(ui),
                Phase::Results => self.ui_results(ui),
                Phase::Recovering => self.ui_recovering(ui),
                Phase::Done => self.ui_done(ui),
                Phase::Error => self.ui_error(ui),
            }
        });
    }
}

fn drive_label(d: &DriveInfo) -> String {
    format!(
        "{}  ·  {}  ({})",
        d.display_name,
        d.path,
        format_size(d.size_bytes)
    )
}

fn default_output_name() -> String {
    format!(
        "RecupeGhost_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    )
}
