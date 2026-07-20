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

/// Qué acción está esperando a que el usuario resuelva la advertencia de mismo-disco.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    Scan,
    Recover,
}

struct RecupeGhostApp {
    drives: Vec<DriveInfo>,
    selected_drive: usize,
    manual_path: String,
    cats: [bool; 4], // fotos, videos, audio, documentos
    output_dir: String,

    /// Advertencia de "vas a guardar en el mismo disco que estás recuperando" pendiente de
    /// resolver, con la acción que quedó frenada esperándola.
    pending_warning: Option<(String, PendingAction)>,
    /// El usuario ya aceptó explícitamente el riesgo de mismo-disco en este flujo; no se le
    /// vuelve a preguntar (repetir la advertencia la devalúa y entrena a descartarla).
    same_device_accepted: bool,

    phase: Phase,
    // Trabajo en background y sus resultados.
    source: Option<PathBuf>,
    scan_total: u64,
    scan_rx: Option<Receiver<anyhow::Result<ScanResult>>>,
    scan_result: Option<ScanResult>,
    recovery_rx: Option<Receiver<anyhow::Result<RecoveryResult>>>,
    recovery_result: Option<RecoveryResult>,
    error_msg: String,
    /// Traducción amigable del error, cuando el fallo viene de un `io::Error` reconocible.
    error_hint: Option<&'static str>,
}

impl RecupeGhostApp {
    fn new() -> Self {
        let drives = drives::list_drives();
        // Preseleccionar el primer disco EXTRAÍBLE. El índice 0 suele ser el disco del sistema,
        // y el caso central de la herramienta es "formateé el USB / la tarjeta de la cámara":
        // arrancar apuntando al disco de la PC invita a escanear el equivocado.
        let selected_drive = drives.iter().position(|d| d.is_removable).unwrap_or(0);
        Self {
            drives,
            selected_drive,
            manual_path: String::new(),
            cats: [true, true, true, true],
            output_dir: default_output_name(),
            pending_warning: None,
            same_device_accepted: false,
            phase: Phase::Setup,
            source: None,
            scan_total: 0,
            scan_rx: None,
            scan_result: None,
            recovery_rx: None,
            recovery_result: None,
            error_msg: String::new(),
            error_hint: None,
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
        self.error_hint = None;
        self.phase = Phase::Error;
    }

    /// Falla a partir de un error real de I/O, guardando además su traducción amigable.
    fn fail_io(&mut self, prefix: &str, e: &anyhow::Error) {
        self.error_msg = format!("{prefix}: {e:#}");
        self.error_hint = util::friendly_error_hint(e);
        self.phase = Phase::Error;
    }

    /// Carpeta de salida ya resuelta a ruta absoluta.
    fn output_path(&self) -> PathBuf {
        util::to_absolute_output(PathBuf::from(self.output_dir.trim()))
    }

    /// Protección de datos crítica: frena la acción si el destino cae en el MISMO disco físico
    /// que se está recuperando. Escribir ahí puede pisar los sectores libres donde viven los
    /// archivos borrados — o sea, destruir justo lo que se vino a rescatar.
    ///
    /// Devuelve `true` si hay que frenar y esperar la decisión del usuario. El CLI ya hacía esto
    /// en sus tres flujos; la GUI no lo hacía en ninguno, y su única defensa era un cartel fijo
    /// que no verificaba nada.
    fn blocked_by_same_device(&mut self, source: &std::path::Path, action: PendingAction) -> bool {
        if self.same_device_accepted {
            return false;
        }
        match crate::ui::same_device_warning(source, &self.output_path()) {
            Some(warning) => {
                self.pending_warning = Some((warning, action));
                true
            }
            None => false,
        }
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
        if self.output_dir.trim().is_empty() {
            return self.fail(
                "Elegí una carpeta donde guardar lo recuperado. Tiene que estar en un disco \
                 distinto del que estás recuperando.",
            );
        }
        // Misma protección crítica que el CLI: la carpeta de salida no puede ser un dispositivo.
        let out = self.output_path();
        if util::is_physical_device(&out) {
            return self.fail(
                "La carpeta de salida no puede ser un disco/dispositivo. Elegí una carpeta normal.",
            );
        }
        // Y no puede estar en el mismo disco que se está recuperando. Se pregunta acá, donde el
        // error todavía es gratis de corregir.
        if self.blocked_by_same_device(&source, PendingAction::Scan) {
            return;
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
        // Segunda barrera, en el momento en que de verdad se escribe. El escaneo pudo haber
        // durado horas: acá se vuelve a chequear por si el escenario cambió (por ejemplo, se
        // desmontó y remontó un disco entre medio).
        if self.blocked_by_same_device(&source, PendingAction::Recover) {
            return;
        }
        let out = self.output_path();
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
                        self.fail_io("No se pudo escanear", &e);
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
                        self.fail_io("No se pudo recuperar", &e);
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
        let (count, cancelled, had_errors, rows): (usize, bool, bool, Vec<(Integrity, String)>) =
            match &self.scan_result {
                Some(res) => (
                    res.found_files.len(),
                    res.cancelled,
                    res.had_errors,
                    res.found_files
                        .iter()
                        .take(500)
                        .map(|f| (f.integrity(), f.friendly_summary()))
                        .collect(),
                ),
                None => return,
            };

        ui.add_space(6.0);

        // Un resultado parcial NUNCA se presenta como completo. Sin estos avisos, alguien que
        // canceló al 2%, o cuyo disco tenía media superficie ilegible, concluye "mis archivos no
        // están" — y para mucha gente esa es la decisión de abandonar su único intento.
        if cancelled {
            ui.colored_label(
                egui::Color32::from_rgb(0xC8, 0x8A, 0x00),
                "⏸ Detuviste la búsqueda antes de que terminara. Esto es lo que se encontró hasta \
                 ahí: lo podés recuperar igual, o volver y buscar de nuevo hasta el final.",
            );
            ui.add_space(4.0);
        }
        if had_errors {
            ui.colored_label(
                egui::Color32::from_rgb(0xC8, 0x8A, 0x00),
                "⚠ El disco tiene partes que no se pudieron leer. Se buscó en todo lo que sí se \
                 pudo, así que pueden faltar archivos de las zonas dañadas. Si el disco está \
                 fallando, conviene hacer una copia antes de seguir usándolo.",
            );
            ui.add_space(4.0);
        }

        ui.strong(format!("Se encontraron {count} archivos."));
        if count == 0 {
            if cancelled {
                ui.label(
                    "Detuviste la búsqueda antes de que apareciera nada. Si la dejás correr \
                     completa, es probable que aparezcan archivos.",
                );
            } else {
                ui.label(
                    "No se encontró nada. Puede ser que ya se haya escrito otra cosa encima, que \
                     el tipo de archivo no esté marcado, o que sea otro el disco.",
                );
            }
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
        let (summary, recovered, incomplete) = match self.recovery_result.as_ref() {
            Some(r) => (r.summary(), r.recovered, r.truncated + r.failed),
            None => (String::new(), 0, 0),
        };
        let out = self.output_path();

        ui.add_space(12.0);
        // El titular no puede decir "¡Listo!" a secas si algo quedó a medias: eso es mentir por
        // omisión justo cuando el usuario decide si sigue buscando o da el tema por cerrado.
        if incomplete > 0 {
            ui.strong(format!(
                "Se recuperaron {recovered} archivos, y {incomplete} quedaron incompletos."
            ));
            ui.label(
                "Los incompletos también están en la carpeta, por si sirven en parte. Suele pasar \
                 cuando el disco tiene zonas dañadas.",
            );
        } else {
            ui.strong(format!("✅ ¡Listo! Se recuperaron {recovered} archivos."));
        }
        ui.add_space(6.0);
        ui.label(summary);
        ui.add_space(10.0);

        // Abrir la carpeta es lo que el usuario quiere hacer a continuación, siempre. Sin esto
        // se le entrega una ruta en texto plano y mucha gente no la encuentra.
        if ui
            .button(
                egui::RichText::new("📂 Abrir la carpeta con mis archivos")
                    .size(16.0)
                    .strong(),
            )
            .clicked()
        {
            open_folder(&out);
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Los archivos tienen nombres nuevos (recovered_0001.jpg y similares), separados \
                 en carpetas por tipo. El nombre original se pierde cuando un archivo se borra: \
                 es normal y no afecta al contenido.",
            )
            .size(13.0),
        );
        ui.add_space(12.0);
        if ui.button("↩ Volver al inicio").clicked() {
            self.phase = Phase::Setup;
            self.scan_result = None;
            self.recovery_result = None;
        }
    }

    fn ui_error(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        // Primero la traducción en criollo, cuando la hay: para el público de esta herramienta un
        // "Acceso denegado. (os error 5)" es el final del intento, cuando la solución era abrir el
        // programa como administrador. El texto técnico queda abajo, para quien vaya a pedir ayuda.
        if let Some(hint) = self.error_hint {
            ui.label(egui::RichText::new(hint.trim()).size(16.0).strong());
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Tus archivos siguen donde estaban: esto no borró nada.")
                    .size(14.0),
            );
            ui.add_space(10.0);
            ui.collapsing("Detalle técnico (por si pedís ayuda)", |ui| {
                ui.colored_label(egui::Color32::from_rgb(220, 90, 90), self.error_msg.clone());
            });
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(220, 90, 90),
                format!("❌ {}", self.error_msg),
            );
        }
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
        self.ui_same_device_dialog(ctx);
    }
}

impl RecupeGhostApp {
    /// Diálogo de la advertencia de mismo-disco. Es la última barrera antes de una pérdida de
    /// datos irreversible, así que está diseñado para que NO se pueda descartar de apuro:
    ///
    /// - La opción segura es la primaria y la que queda a mano.
    /// - La opción riesgosa **describe el riesgo en su propia etiqueta**: nadie la puede apretar
    ///   sin haber leído qué está aceptando. Un "Sí"/"Continuar" genérico no da esa garantía.
    /// - No hay "no volver a mostrar", justamente para no entrenar el reflejo de descartarla; y
    ///   como solo aparece cuando `same_device_warning` dispara de verdad, sigue siendo rara.
    ///
    /// egui 0.29 todavía no tiene `Modal` (llegó en 0.31), así que se arma con una `Window` fija
    /// y no colapsable.
    fn ui_same_device_dialog(&mut self, ctx: &egui::Context) {
        let Some((warning, action)) = self.pending_warning.clone() else {
            return;
        };

        let mut choice: Option<bool> = None; // Some(true) = seguir igual, Some(false) = corregir
        egui::Window::new("Un momento — esto es importante")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(warning.trim()).size(15.0));
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(
                        "Si guardás en el mismo disco del que estás recuperando, lo que se escriba \
                         puede tapar para siempre los archivos que estás buscando. No tiene vuelta \
                         atrás.",
                    )
                    .size(15.0),
                );
                ui.add_space(12.0);
                if ui
                    .button(
                        egui::RichText::new("Elegir otra carpeta")
                            .size(16.0)
                            .strong(),
                    )
                    .clicked()
                {
                    choice = Some(false);
                }
                ui.add_space(4.0);
                if ui
                    .button(
                        egui::RichText::new("Entiendo el riesgo y quiero seguir igual").size(13.0),
                    )
                    .clicked()
                {
                    choice = Some(true);
                }
                ui.add_space(4.0);
            });

        match choice {
            Some(true) => {
                self.pending_warning = None;
                self.same_device_accepted = true;
                match action {
                    PendingAction::Scan => self.start_scan(),
                    PendingAction::Recover => self.start_recovery(),
                }
            }
            Some(false) => {
                // Volver siempre a donde se elige la carpeta: es el único lugar donde el usuario
                // puede corregir el destino. La pantalla de resultados no tiene ese campo, así
                // que dejarlo ahí sería un callejón sin salida con la advertencia ya descartada.
                self.pending_warning = None;
                self.phase = Phase::Setup;
            }
            None => {}
        }
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

/// Abre la carpeta de resultados en el explorador de archivos del sistema. Best-effort: si el
/// comando no está o falla, no pasa nada (la ruta igual se muestra en pantalla).
fn open_folder(path: &std::path::Path) {
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";

    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

fn default_output_name() -> String {
    format!(
        "RecupeGhost_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    )
}
