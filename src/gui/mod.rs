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

mod theme;

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
        Box::new(|cc| {
            // Sistema visual (tema claro, escala tipográfica grande, controles anchos). Sin esto
            // la GUI sale con los defaults de egui, que son de una herramienta de programador.
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(RecupeGhostApp::new()))
        }),
    )
}

/// Pasos del asistente previo a la búsqueda.
///
/// Se separó el formulario único en cuatro pantallas a propósito: el público de esta herramienta
/// llega asustado y con poca confianza. Una pantalla con todo junto obliga a decidir tres cosas a
/// la vez sin saber cuántas faltan; de a una, cada pantalla hace UNA pregunta en castellano y la
/// barra de arriba muestra cuánto queda.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Qué disco o imagen revisar.
    Source,
    /// Qué tipos de archivo buscar.
    Types,
    /// Dónde guardar lo recuperado.
    Output,
    /// Repaso de las tres respuestas antes de una espera que puede durar horas.
    Summary,
}

impl Step {
    /// Los pasos en orden, para dibujar la barra de progreso del asistente.
    const ALL: [Step; 4] = [Step::Source, Step::Types, Step::Output, Step::Summary];

    fn label(self) -> &'static str {
        match self {
            Step::Source => "Disco",
            Step::Types => "Tipos",
            Step::Output => "Guardar",
            Step::Summary => "Buscar",
        }
    }

    fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn prev(self) -> Option<Step> {
        Step::ALL.get(self.index().checked_sub(1)?).copied()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Setup(Step),
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
    /// Combinación (origen, destino) EXACTA para la que el usuario ya aceptó el riesgo de
    /// mismo-disco. No alcanza con un booleano: una aceptación puntual no puede apagar la
    /// protección para discos y carpetas que la persona nunca aprobó. Con la pareja guardada,
    /// cambiar cualquiera de las dos puntas vuelve a disparar la advertencia sola.
    same_device_accepted: Option<(PathBuf, PathBuf)>,

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
        let mut app = Self {
            drives,
            selected_drive,
            manual_path: String::new(),
            cats: [true, true, true, true],
            output_dir: default_output_name(),
            pending_warning: None,
            same_device_accepted: None,
            phase: Phase::Setup(Step::Source),
            source: None,
            scan_total: 0,
            scan_rx: None,
            scan_result: None,
            recovery_rx: None,
            recovery_result: None,
            error_msg: String::new(),
            error_hint: None,
        };
        app.apply_demo_state();
        app
    }

    /// Abre la GUI directamente en una pantalla concreta, para poder MIRARLA.
    ///
    /// Existe porque varias pantallas (sobre todo el diálogo de mismo-disco, que es la última
    /// barrera antes de una pérdida de datos irreversible) solo aparecen con un disco real en
    /// determinado estado, y quedaban revisadas por código pero nunca vistas funcionando.
    ///
    /// Solo en builds de **debug**: el binario que se distribuye se compila en release, así que
    /// esto no existe para el usuario final ni agrega superficie. Uso:
    /// `RECUPEGHOST_GUI_DEMO=same_device cargo run --features gui --bin recupe_ghost_gui`
    #[cfg(debug_assertions)]
    fn apply_demo_state(&mut self) {
        match std::env::var("RECUPEGHOST_GUI_DEMO").as_deref() {
            Ok("same_device") => {
                self.pending_warning = Some((
                    "Vas a guardar los archivos recuperados en el MISMO disco del que los estás \
                     recuperando."
                        .to_string(),
                    PendingAction::Scan,
                ));
            }
            // Pasos del asistente: para poder mirarlos sin tener que clickear.
            Ok("types") => self.phase = Phase::Setup(Step::Types),
            Ok("output") => self.phase = Phase::Setup(Step::Output),
            Ok("summary") => self.phase = Phase::Setup(Step::Summary),
            // Escaneo de verdad sobre una imagen de prueba. Un archivo .img NO es un dispositivo
            // físico, así que `same_device_warning` devuelve None y no se cruza el diálogo.
            Ok("scanning") => {
                if let Ok(img) = std::env::var("RECUPEGHOST_GUI_DEMO_IMG") {
                    self.manual_path = img;
                    self.output_dir = std::env::temp_dir()
                        .join("recupeghost_demo")
                        .display()
                        .to_string();
                    self.start_scan();
                }
            }
            Ok("done") => {
                self.recovery_result = Some(RecoveryResult {
                    recovered: 214,
                    truncated: 0,
                    failed: 0,
                    total_bytes: 1_476_395_008,
                    cancelled: false,
                    output_dir: self.output_path(),
                    errors: Vec::new(),
                });
                self.phase = Phase::Done;
            }
            _ => {}
        }
    }

    #[cfg(not(debug_assertions))]
    fn apply_demo_state(&mut self) {}

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
        self.pending_warning = None;
        self.phase = Phase::Error;
    }

    /// Falla a partir de un error real de I/O, guardando además su traducción amigable.
    fn fail_io(&mut self, prefix: &str, e: &anyhow::Error) {
        self.error_msg = format!("{prefix}: {e:#}");
        self.error_hint = util::friendly_error_hint(e);
        self.pending_warning = None;
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
        let out = self.output_path();
        // La aceptación vale solo para la combinación exacta que se aprobó.
        if self.same_device_accepted.as_ref() == Some(&(source.to_path_buf(), out.clone())) {
            return false;
        }
        match crate::ui::same_device_warning(source, &out) {
            Some(warning) => {
                self.pending_warning = Some((warning, action));
                true
            }
            None => false,
        }
    }

    fn start_scan(&mut self) {
        // Con la advertencia en pantalla no se arranca nada: la `Window` de egui flota sobre el
        // panel pero NO lo bloquea por sí sola, así que sin este guard se podía disparar un
        // segundo escaneo concurrente desde abajo (dos hilos pisando los mismos globales de
        // progreso y cancelación del scanner).
        if self.pending_warning.is_some() {
            return;
        }
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
        if self.pending_warning.is_some() {
            return;
        }
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
            let _ = tx.send(recovery::recover_files_quiet(&source, &files, &out));
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

    /// Barra de pasos del asistente. Responde sin que haya que preguntar "¿cuánto falta?", que es
    /// la duda que hace abandonar a alguien que ya está nervioso.
    fn ui_stepper(&self, ui: &mut egui::Ui, current: Step) {
        ui.horizontal(|ui| {
            for (i, step) in Step::ALL.iter().enumerate() {
                if i > 0 {
                    ui.label(egui::RichText::new("—").color(theme::BORDER));
                }
                let done = step.index() < current.index();
                let activo = *step == current;
                // El punto se DIBUJA en vez de escribirse con un carácter: "●" no existe en
                // Atkinson Hyperlegible y salía como un cuadrito de "glifo faltante".
                let color = if done {
                    theme::OK
                } else if activo {
                    theme::BRAND
                } else {
                    theme::NEUTRAL
                };
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(11.0, 11.0), egui::Sense::hover());
                if done || activo {
                    ui.painter().circle_filled(rect.center(), 5.0, color);
                } else {
                    ui.painter().circle_stroke(
                        rect.center(),
                        4.5,
                        egui::Stroke::new(1.5_f32, color),
                    );
                }
                let texto = egui::RichText::new(step.label()).size(13.0);
                ui.label(if activo {
                    texto.color(theme::BRAND).strong()
                } else if done {
                    texto.color(theme::TEXT_WEAK)
                } else {
                    texto.color(theme::NEUTRAL)
                });
            }
        });
    }

    /// Pie de navegación común: "Volver" a la izquierda y la acción principal a la derecha.
    ///
    /// `bloqueo` es el motivo por el que no se puede avanzar, si lo hay. Se muestra **al lado del
    /// botón deshabilitado** en vez de dejar apretar y mandar a una pantalla de error roja: para
    /// alguien no técnico, un ❌ después de un clic se lee como "rompí algo", cuando lo único que
    /// pasó es que falta un dato.
    fn ui_nav(
        &mut self,
        ui: &mut egui::Ui,
        actual: Step,
        etiqueta: &str,
        bloqueo: Option<&str>,
    ) -> bool {
        let mut avanzar = false;
        ui.add_space(16.0);
        ui.separator();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if let Some(anterior) = actual.prev() {
                if ui.button("↩  Volver").clicked() {
                    self.phase = Phase::Setup(anterior);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let habilitado = bloqueo.is_none();
                if ui
                    .add_enabled(habilitado, theme::primary_button(etiqueta))
                    .clicked()
                {
                    avanzar = true;
                }
                if let Some(motivo) = bloqueo {
                    ui.label(egui::RichText::new(motivo).color(theme::WARN).size(13.0));
                }
            });
        });
        avanzar
    }

    /// Atajo para volver a unos resultados que ya costaron horas de escaneo.
    ///
    /// Sin esto, alguien que llega al asistente desde la advertencia de mismo-disco para CORREGIR
    /// la carpeta perdía el escaneo — o sea, elegir la opción segura salía más caro que aceptar el
    /// riesgo. Un diálogo de protección de datos no puede tener los incentivos al revés.
    fn ui_volver_a_resultados(&mut self, ui: &mut egui::Ui) {
        if self.scan_result.is_none() {
            return;
        }
        ui.horizontal(|ui| {
            if ui
                .button("↩  Volver a los resultados del escaneo")
                .clicked()
            {
                self.phase = Phase::Results;
            }
            ui.label(
                egui::RichText::new("(no hace falta buscar de nuevo)")
                    .size(13.0)
                    .color(theme::TEXT_WEAK),
            );
        });
        ui.add_space(6.0);
    }

    fn ui_step_source(&mut self, ui: &mut egui::Ui) {
        self.ui_volver_a_resultados(ui);
        theme::section_title(ui, "¿Dónde estaban tus archivos?");
        ui.label(
            egui::RichText::new("Elegí el disco o la memoria que querés revisar.")
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(10.0);

        if self.drives.is_empty() {
            theme::notice(
                ui,
                theme::WARN,
                theme::WARN_BG,
                "No se detectó ningún disco. Si el programa no se abrió como administrador, \
                 Windows no lo deja verlos. Probá 'Buscar de nuevo', o usá las opciones avanzadas \
                 para abrir un archivo de imagen.",
            );
            ui.add_space(8.0);
        }

        egui::ScrollArea::vertical()
            .id_salt("drives_list")
            .max_height(190.0)
            .show(ui, |ui| {
                for (i, d) in self.drives.iter().enumerate() {
                    let elegido = self.selected_drive == i;
                    if ui
                        .selectable_label(elegido, drive_label(d))
                        .on_hover_text(d.path.clone())
                        .clicked()
                    {
                        self.selected_drive = i;
                    }
                }
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("🔄  Buscar discos de nuevo").clicked() {
                self.drives = drives::list_drives();
                self.selected_drive = self
                    .drives
                    .iter()
                    .position(|d| d.is_removable)
                    .unwrap_or(0)
                    .min(self.drives.len().saturating_sub(1));
            }
        });

        ui.add_space(8.0);
        // La ruta manual se esconde detrás de "opciones avanzadas". Cuando estaba siempre visible
        // PISABA EN SILENCIO al disco elegido en la lista: alguien clickeaba su USB, quedaba texto
        // viejo en el campo, y se escaneaba otra cosa sin ningún aviso.
        egui::CollapsingHeader::new("Opciones avanzadas: usar un archivo de imagen (.img)")
            .id_salt("avanzadas_origen")
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(
                        "Solo si ya tenés una copia del disco en un archivo. Si escribís algo acá, \
                         se usa esto en lugar del disco elegido arriba.",
                    )
                    .size(13.0)
                    .color(theme::TEXT_WEAK),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.manual_path);
                    if ui.button("Buscar archivo…").clicked() {
                        if let Some(f) = rfd::FileDialog::new()
                            .set_title("Elegí el archivo de imagen del disco")
                            .add_filter("Imagen de disco", &["img", "dd", "raw", "iso"])
                            .pick_file()
                        {
                            self.manual_path = f.display().to_string();
                        }
                    }
                });
            });

        // Si el usuario eligió una imagen a mano, se dice explícitamente cuál gana. Nunca en
        // silencio.
        if !self.manual_path.trim().is_empty() {
            ui.add_space(6.0);
            theme::notice(
                ui,
                theme::WARN,
                theme::WARN_BG,
                "Se va a revisar el archivo de imagen que escribiste en opciones avanzadas, no el \
                 disco de la lista. Vaciá ese campo para volver a usar el disco.",
            );
        }

        let bloqueo = if self.resolve_source().is_none() {
            Some("Elegí un disco o un archivo de imagen")
        } else {
            None
        };
        if self.ui_nav(ui, Step::Source, "Continuar", bloqueo) {
            self.phase = Phase::Setup(Step::Types);
        }
    }

    fn ui_step_types(&mut self, ui: &mut egui::Ui) {
        theme::section_title(ui, "¿Qué querés recuperar?");
        ui.label(
            egui::RichText::new("Si no estás seguro, dejá todo marcado.").color(theme::TEXT_WEAK),
        );
        ui.add_space(12.0);

        // Casillas con el ejemplo de extensiones al lado: alguien que busca "las fotos del
        // celular" no tiene por qué saber que eso es un JPG.
        const TIPOS: [(&str, &str); 4] = [
            ("📷  Fotos", "JPG, PNG, HEIC, RAW de cámara…"),
            ("🎬  Videos", "MP4, MOV, AVI, MKV…"),
            ("🎵  Audio", "MP3, WAV, FLAC, M4A…"),
            ("📄  Documentos", "PDF"),
        ];
        for (i, (nombre, ejemplos)) in TIPOS.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.cats[i], *nombre);
                ui.label(
                    egui::RichText::new(*ejemplos)
                        .size(13.0)
                        .color(theme::TEXT_WEAK),
                );
            });
            ui.add_space(6.0);
        }

        let bloqueo = if self.selected_categories().is_empty() {
            Some("Marcá al menos un tipo")
        } else {
            None
        };
        if self.ui_nav(ui, Step::Types, "Continuar", bloqueo) {
            self.phase = Phase::Setup(Step::Output);
        }
    }

    fn ui_step_output(&mut self, ui: &mut egui::Ui) {
        self.ui_volver_a_resultados(ui);
        theme::section_title(ui, "¿Dónde guardamos lo que encontremos?");
        ui.label(
            egui::RichText::new("Se va a crear una carpeta nueva acá:").color(theme::TEXT_WEAK),
        );
        ui.add_space(8.0);

        // La RUTA ABSOLUTA, siempre. El campo mostraba solo "RecupeGhost_20260720_153012", que es
        // relativo al directorio desde donde se abrió el programa: nadie podía saber dónde iba a
        // caer eso. El CLI ya había aprendido esta lección.
        let destino = self.output_path();
        ui.label(
            egui::RichText::new(destino.display().to_string())
                .font(egui::FontId::monospace(14.0))
                .color(theme::TEXT),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("📁  Elegir carpeta…").clicked() {
                // Diálogo nativo del sistema: es el que esta gente ya sabe usar. Tipear rutas a
                // mano era lo peor de los dos mundos en una interfaz gráfica.
                let inicio = destino
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                if let Some(carpeta) = rfd::FileDialog::new()
                    .set_title("Elegí dónde guardar los archivos recuperados")
                    .set_directory(inicio)
                    .pick_folder()
                {
                    self.output_dir = carpeta.display().to_string();
                }
            }
            ui.label(
                egui::RichText::new("o escribila a mano:")
                    .size(13.0)
                    .color(theme::TEXT_WEAK),
            );
            ui.text_edit_singleline(&mut self.output_dir);
        });

        ui.add_space(12.0);
        theme::notice(
            ui,
            theme::WARN,
            theme::WARN_BG,
            "Guardá en un disco DISTINTO del que estás revisando. Si guardás en la misma memoria, \
             podés borrar para siempre lo que estás tratando de recuperar.",
        );

        let vacio = self.output_dir.trim().is_empty();
        let es_dispositivo = !vacio && util::is_physical_device(&self.output_path());
        let bloqueo = if vacio {
            Some("Elegí una carpeta")
        } else if es_dispositivo {
            // Misma protección crítica que el CLI: escribir sobre la ruta de un dispositivo con
            // permisos de administrador sobrescribiría el disco entero.
            Some("Eso es un disco, no una carpeta")
        } else {
            None
        };
        if self.ui_nav(ui, Step::Output, "Continuar", bloqueo) {
            self.phase = Phase::Setup(Step::Summary);
        }
    }

    fn ui_step_summary(&mut self, ui: &mut egui::Ui) {
        theme::section_title(ui, "Todo listo. Revisá antes de empezar:");
        ui.add_space(10.0);

        let origen = match self.resolve_source() {
            Some(s) => self
                .drives
                .iter()
                .find(|d| self.manual_path.trim().is_empty() && std::path::Path::new(&d.path) == s)
                .map(|d| d.display_name.clone())
                .unwrap_or_else(|| s.display().to_string()),
            None => "(sin elegir)".to_string(),
        };
        let tipos: Vec<&str> = [
            (self.cats[0], "fotos"),
            (self.cats[1], "videos"),
            (self.cats[2], "audio"),
            (self.cats[3], "documentos"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect();

        for (rotulo, valor) in [
            ("Buscar en", origen),
            ("Qué buscar", tipos.join(", ")),
            ("Guardar en", self.output_path().display().to_string()),
        ] {
            ui.label(
                egui::RichText::new(rotulo)
                    .size(13.0)
                    .color(theme::TEXT_WEAK),
            );
            ui.label(egui::RichText::new(valor).color(theme::TEXT));
            ui.add_space(8.0);
        }

        ui.add_space(4.0);
        // El aviso de los nombres perdidos va ACÁ, que es la última pantalla que se lee con calma
        // antes de una espera larga. Estaba en gris chico debajo del botón de recuperar: o sea, se
        // leía después de haber clickeado, o nunca. Y se explica que es una propiedad del borrado,
        // no un defecto del programa, para desactivar el "este programa me rompió los nombres".
        theme::notice(
            ui,
            theme::TEXT_WEAK,
            theme::GROUND,
            "La búsqueda puede tardar desde unos minutos hasta más de una hora, según el tamaño \
             del disco. Podés dejar la computadora trabajando.\n\nLos archivos recuperados van a \
             tener nombres nuevos (recovered_0001.jpg). El nombre original se pierde cuando un \
             archivo se borra: es normal y no afecta al contenido.",
        );

        if self.ui_nav(ui, Step::Summary, "🔍  Empezar la búsqueda", None) {
            self.start_scan();
        }
    }

    fn ui_scanning(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        theme::section_title(ui, "Buscando tus archivos…");
        ui.label(
            egui::RichText::new("Esto puede tardar. No desconectes la memoria.")
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(14.0);

        let done = scanner::scan_progress_bytes();
        let frac = if self.scan_total > 0 {
            (done as f32 / self.scan_total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.add(
            egui::ProgressBar::new(frac)
                .show_percentage()
                .desired_height(24.0),
        );
        if self.scan_total > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} de {} revisados",
                    format_size(done.min(self.scan_total)),
                    format_size(self.scan_total)
                ))
                .color(theme::TEXT_WEAK),
            );
        }

        // El contador vivo de hallazgos es el ansiolítico de esta pantalla: durante una espera que
        // puede durar horas es la única señal de que algo bueno está pasando. Sin él, la barra
        // avanzando sola no dice si se está encontrando algo o si el disco está vacío.
        ui.add_space(12.0);
        let encontrados = scanner::scan_progress_files();
        ui.label(
            egui::RichText::new(format!("Encontrados hasta ahora: {encontrados}"))
                .font(egui::FontId::new(17.0, theme::bold_family()))
                .color(if encontrados > 0 {
                    theme::OK
                } else {
                    theme::TEXT
                }),
        );
        ui.label(
            egui::RichText::new("Podés usar la computadora normalmente mientras tanto.")
                .size(13.0)
                .color(theme::TEXT_WEAK),
        );

        ui.add_space(18.0);
        // "Detener y ver lo encontrado", no "Cancelar". Para alguien asustado "cancelar" suena a
        // perder todo, cuando el motor en realidad conserva lo hallado: el texto del botón hace el
        // trabajo de una explicación. Y como la cancelación es cooperativa y tarda, el botón pasa
        // a "Deteniendo…" deshabilitado — si no, parece que el clic no hizo nada y se vuelve a
        // apretar pensando que se colgó.
        let deteniendo = scanner::cancel_requested();
        if deteniendo {
            ui.add_enabled(false, egui::Button::new("Deteniendo…"));
        } else if ui.button("⏹  Detener y ver lo encontrado").clicked() {
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
            theme::notice(
                ui,
                theme::WARN,
                theme::WARN_BG,
                "⏸ Detuviste la búsqueda antes de que terminara. Esto es lo que se encontró hasta \
                 ahí: lo podés recuperar igual, o volver y buscar de nuevo hasta el final.",
            );
            ui.add_space(4.0);
        }
        if had_errors {
            theme::notice(
                ui,
                theme::WARN,
                theme::WARN_BG,
                "⚠ El disco tiene partes que no se pudieron leer. Se buscó en todo lo que sí se \
                 pudo, así que pueden faltar archivos de las zonas dañadas. Si el disco está \
                 fallando, conviene hacer una copia antes de seguir usándolo.",
            );
            ui.add_space(4.0);
        }

        theme::section_title(ui, format!("Se encontraron {count} archivos."));
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
                self.phase = Phase::Setup(Step::Source);
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
                        Integrity::Intact => theme::OK,
                        Integrity::Suspect => theme::WARN,
                        Integrity::Unverifiable => theme::NEUTRAL,
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
                .add(theme::primary_button("💾  Recuperar todo"))
                .clicked()
            {
                self.start_recovery();
            }
            if ui.button("↩ Volver").clicked() {
                self.phase = Phase::Setup(Step::Source);
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
        ui.add_space(20.0);
        theme::section_title(ui, "Guardando tus archivos…");
        ui.label(
            egui::RichText::new("No cierres el programa ni desconectes la memoria.")
                .color(theme::TEXT_WEAK),
        );
        ui.add_space(14.0);

        // Progreso real, no un spinner indefinido. Recuperar miles de archivos también puede
        // tardar, y una animación que gira sin decir nada no distingue "trabajando" de "colgado".
        let total = self
            .scan_result
            .as_ref()
            .map(|r| r.found_files.len() as u64)
            .unwrap_or(0);
        let hechos = recovery::recovery_progress_files();
        let frac = if total > 0 {
            (hechos as f32 / total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.add(
            egui::ProgressBar::new(frac)
                .show_percentage()
                .desired_height(24.0),
        );
        if total > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} de {} archivos  ·  {} guardados",
                    hechos.min(total),
                    total,
                    format_size(recovery::recovery_progress_bytes())
                ))
                .color(theme::TEXT_WEAK),
            );
        }

        ui.add_space(18.0);
        // Se puede detener, y lo ya guardado sirve: son archivos completos en disco, no un estado
        // a medias que haya que descartar.
        let deteniendo = recovery::cancel_requested();
        if deteniendo {
            ui.add_enabled(false, egui::Button::new("Deteniendo…"));
        } else if ui.button("⏹  Detener y quedarme con lo guardado").clicked() {
            recovery::request_cancel();
        }
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
            theme::section_title(
                ui,
                format!(
                    "Se recuperaron {recovered} archivos, y {incomplete} quedaron incompletos."
                ),
            );
            ui.label(
                "Los incompletos también están en la carpeta, por si sirven en parte. Suele pasar \
                 cuando el disco tiene zonas dañadas.",
            );
        } else {
            theme::section_title(
                ui,
                format!("✅ ¡Listo! Se recuperaron {recovered} archivos."),
            );
        }
        ui.add_space(6.0);
        ui.label(summary);
        ui.add_space(10.0);

        // Abrir la carpeta es lo que el usuario quiere hacer a continuación, siempre. Sin esto
        // se le entrega una ruta en texto plano y mucha gente no la encuentra.
        if ui
            .add(theme::primary_button(
                "📂  Abrir la carpeta con mis archivos",
            ))
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
            self.phase = Phase::Setup(Step::Source);
            self.scan_result = None;
            self.recovery_result = None;
        }

        self.ui_apoyo(ui);
    }

    /// Bloque de apoyo al canal. El CLI ya lo tenía en `ui::show_goodbye()` y la GUI no lo tenía
    /// en ninguna parte.
    ///
    /// Va **solo acá**, en la pantalla final: es el único momento en que la herramienta ya cumplió
    /// lo que prometió, así que es el único momento en que pedir algo no le estorba a alguien que
    /// todavía está tratando de rescatar sus fotos. Y no interrumpe: son dos enlaces al pie, no un
    /// diálogo. Un pedido de apoyo nunca puede competir por atención con una advertencia.
    fn ui_apoyo(&mut self, ui: &mut egui::Ui) {
        ui.add_space(18.0);
        ui.separator();
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("👻 ¿Te sirvió RecupeGhost?")
                .font(egui::FontId::new(17.0, theme::bold_family()))
                .color(theme::TEXT),
        );
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(
                "Es gratis y de código abierto. Si te ayudó a recuperar tus archivos, la mejor \
                 forma de apoyar es ver los videos del canal.",
            )
            .color(theme::TEXT_WEAK),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("🎬  YouTube  ·  @el_becerril").clicked() {
                crate::ui::open_url("https://www.youtube.com/@el_becerril");
            }
            if ui.button("📘  Facebook  ·  El Becerril").clicked() {
                crate::ui::open_url("https://www.facebook.com/ElBecerril");
            }
        });
    }

    fn ui_error(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        // Primero la traducción en criollo, cuando la hay: para el público de esta herramienta un
        // "Acceso denegado. (os error 5)" es el final del intento, cuando la solución era abrir el
        // programa como administrador. El texto técnico queda abajo, para quien vaya a pedir ayuda.
        if let Some(hint) = self.error_hint {
            ui.label(egui::RichText::new(hint.trim()).strong());
            ui.add_space(8.0);
            ui.label("Tus archivos siguen donde estaban: esto no borró nada.");
            ui.add_space(10.0);
            ui.collapsing("Detalle técnico (por si pedís ayuda)", |ui| {
                ui.colored_label(theme::DANGER, self.error_msg.clone());
            });
        } else {
            ui.colored_label(theme::DANGER, format!("❌ {}", self.error_msg));
        }
        ui.add_space(12.0);
        if ui.button("↩ Volver").clicked() {
            self.phase = Phase::Setup(Step::Source);
        }
    }
}

impl eframe::App for RecupeGhostApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background(ctx);

        // Con la advertencia de mismo-disco en pantalla, TODO lo de atrás queda deshabilitado.
        // `egui::Window` es una ventana flotante, no un modal (`egui::Modal` recién existe desde
        // 0.31): sin esto el usuario puede seguir tocando los controles de abajo, y una decisión
        // de protección de datos no puede quedar dando vueltas mientras el estado que la motivó
        // cambia atrás.
        let blocked = self.pending_warning.is_some();
        egui::CentralPanel::default().show(ctx, |ui| {
            if blocked {
                ui.disable();
            }
            ui.add_space(6.0);
            ui.heading("👻 RecupeGhost");
            ui.label("Recuperá fotos, videos, audios y documentos borrados.");
            ui.separator();
            match self.phase {
                Phase::Setup(step) => {
                    self.ui_stepper(ui, step);
                    ui.add_space(10.0);
                    match step {
                        Step::Source => self.ui_step_source(ui),
                        Step::Types => self.ui_step_types(ui),
                        Step::Output => self.ui_step_output(ui),
                        Step::Summary => self.ui_step_summary(ui),
                    }
                }
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
                ui.label(warning.trim());
                ui.add_space(6.0);
                ui.label(
                    "Si guardás en el mismo disco del que estás recuperando, lo que se escriba \
                     puede tapar para siempre los archivos que estás buscando. No tiene vuelta \
                     atrás.",
                );
                ui.add_space(12.0);
                // La opción segura es la única con peso visual. La riesgosa queda como texto
                // chico y apagado: sigue estando a un clic, pero hay que ir a buscarla.
                if ui
                    .add(theme::primary_button("Elegir otra carpeta"))
                    .clicked()
                {
                    choice = Some(false);
                }
                ui.add_space(4.0);
                if ui
                    .button(
                        egui::RichText::new("Entiendo el riesgo y quiero seguir igual")
                            .size(13.0)
                            .color(theme::TEXT_WEAK),
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
                if let Some(src) = self.resolve_source() {
                    self.same_device_accepted = Some((src, self.output_path()));
                }
                match action {
                    PendingAction::Scan => self.start_scan(),
                    PendingAction::Recover => self.start_recovery(),
                }
            }
            Some(false) => {
                // Volver siempre a donde se elige la carpeta: es el único lugar donde el usuario
                // puede corregir el destino. La pantalla de resultados no tiene ese campo, así
                // que dejarlo ahí sería un callejón sin salida con la advertencia ya descartada.
                // Si venía de recuperar, `scan_result` se conserva y `ui_setup` ofrece volver a
                // los resultados: corregir la carpeta no cuesta re-escanear.
                self.pending_warning = None;
                self.phase = Phase::Setup(Step::Output);
            }
            None => {}
        }
    }
}

/// Etiqueta de un disco para la lista de elección.
///
/// `display_name` ya viene armado por `drives` con lo que le sirve al usuario (en Windows arranca
/// con la letra de unidad: `D: - Kingston DataTraveler (14.5 GB)`). Acá solo se le agrega la marca
/// de extraíble, que es el dato que de verdad ayuda a reconocer "este es mi USB". La ruta cruda
/// del dispositivo queda en el tooltip: no le dice nada al público objetivo y encima asusta.
fn drive_label(d: &DriveInfo) -> String {
    if d.is_removable {
        format!("{}  ·  Extraíble", d.display_name)
    } else {
        d.display_name.clone()
    }
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
