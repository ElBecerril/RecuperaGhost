mod banner;
mod drives;
mod recovery;
mod scanner;
mod signatures;
mod ui;
mod updater;
mod util;

use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use signatures::{signatures_for_categories, FileCategory};
use ui::{MainMenuChoice, ScanConfig};

/// RecupeGhost - El Detective de Archivos Perdidos
///
/// Recupera fotos, videos, audios y documentos borrados desde discos o imágenes raw.
/// Sin argumentos entra en modo interactivo.
#[derive(Parser)]
#[command(name = "RecupeGhost", version = banner::VERSION)]
struct CliArgs {
    /// Ruta del disco o imagen de origen (ej: disco.img, /dev/sdb1)
    source: Option<String>,

    /// Buscar fotos (JPG, PNG, GIF, BMP, WebP, TIFF)
    #[arg(long)]
    fotos: bool,

    /// Buscar videos (MP4, AVI, MKV, MOV, FLV, 3GP)
    #[arg(long)]
    videos: bool,

    /// Buscar audio (MP3, WAV, FLAC, OGG, AAC, M4A, AMR, WMA, OPUS)
    #[arg(long)]
    audio: bool,

    /// Buscar documentos (PDF)
    #[arg(long)]
    documentos: bool,

    /// Directorio de salida para archivos recuperados
    #[arg(short = 'o', long = "output")]
    output: Option<String>,

    /// No buscar actualizaciones ni mostrar prompts interactivos relacionados
    /// (útil para scripts/cron; se activa automáticamente sin TTY o en modo batch)
    #[arg(long = "no-update")]
    no_update: bool,
}

impl CliArgs {
    /// Convierte los argumentos CLI en un ScanConfig para modo batch.
    /// Recibe `source` ya extraído y validado por el call site (nunca hace unwrap interno).
    fn into_scan_config(self, source: String) -> ScanConfig {
        let source_path = PathBuf::from(source);

        let categories = if !self.fotos && !self.videos && !self.audio && !self.documentos {
            // Ningún flag = buscar todo
            vec![
                FileCategory::Photo,
                FileCategory::Video,
                FileCategory::Audio,
                FileCategory::Document,
            ]
        } else {
            let mut cats = Vec::new();
            if self.fotos {
                cats.push(FileCategory::Photo);
            }
            if self.videos {
                cats.push(FileCategory::Video);
            }
            if self.audio {
                cats.push(FileCategory::Audio);
            }
            if self.documentos {
                cats.push(FileCategory::Document);
            }
            cats
        };

        let output_dir = match self.output {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(format!(
                "RecupeGhost_{}",
                chrono::Local::now().format("%Y%m%d_%H%M%S")
            )),
        };
        // Misma resolución a ruta absoluta que el flujo interactivo, para que el resumen y el
        // mensaje final de ubicación muestren la ruta completa también en modo batch.
        let output_dir = util::to_absolute_output(output_dir);

        ScanConfig {
            source_path,
            output_dir,
            categories,
        }
    }

    /// Valida que no se pasen flags sin source.
    fn validate(&self) {
        let has_flags =
            self.fotos || self.videos || self.audio || self.documentos || self.output.is_some();
        if self.source.is_none() && has_flags {
            eprintln!(
                "{}",
                "  ❌ Error: debes especificar una ruta de origen cuando usas --fotos, --videos, --audio, --documentos u -o."
                    .bright_red()
            );
            eprintln!(
                "{}",
                "     Uso: recupe_ghost <SOURCE> [--fotos] [--videos] [--audio] [--documentos] [-o <OUTPUT>]"
                    .bright_yellow()
            );
            process::exit(1);
        }
    }
}

/// Habilita los códigos ANSI de color en la consola de Windows.
/// Sin esto, los colores se muestran como texto crudo (ej. ←[96m).
#[cfg(windows)]
fn enable_windows_ansi() {
    extern "system" {
        fn GetStdHandle(handle: u32) -> isize;
        fn GetConsoleMode(console: isize, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: isize, mode: u32) -> i32;
    }

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    enable_windows_ansi();

    // Handler de Ctrl+C (una sola vez, multiplataforma vía `ctrlc`). Si hay un escaneo en
    // curso, lo cancela de forma cooperativa (para conservando lo encontrado); si no, cierra el
    // programa con el código estándar 130 (Ctrl+C), que es lo que el usuario espera cuando está
    // en un menú. El closure corre en un thread propio del crate `ctrlc`, no en el contexto
    // async de la señal, así que `process::exit` acá es seguro.
    let _ = ctrlc::set_handler(|| {
        if scanner::is_scan_in_progress() {
            scanner::request_cancel();
        } else {
            process::exit(130);
        }
    });

    // CliArgs::parse() debe correr ANTES que cualquier llamada de red o prompt
    // interactivo: así --help/--version responden de inmediato sin tocar la red.
    let args = CliArgs::parse();
    args.validate();

    // Modo batch (con source) o sin TTY (script/cron/redirección) => sin red ni prompts.
    let is_tty = console::Term::stdout().is_term();
    let is_batch = args.source.is_some();
    let skip_update_check = args.no_update || is_batch || !is_tty;

    updater::cleanup_old_binary();
    if !skip_update_check {
        updater::check_for_updates();
    }

    if let Some(source) = args.source.clone() {
        // ── Modo batch ──
        banner::show_banner();

        let config = args.into_scan_config(source);

        // Validar que la ruta existe (excepto dispositivos raw)
        let src = config.source_path.to_string_lossy();
        if !util::is_physical_device(&config.source_path) && !config.source_path.exists() {
            eprintln!(
                "{}",
                format!("  ❌ La ruta '{}' no existe.", src).bright_red()
            );
            if is_tty {
                wait_for_keypress();
            }
            process::exit(1);
        }

        // Advertencia best-effort: no recuperar sobre el mismo disco que se escanea.
        if let Some(warning) = ui::same_device_warning(&config.source_path, &config.output_dir) {
            eprintln!("{}", warning.bright_yellow());
            if is_tty {
                let continuar = dialoguer::Confirm::new()
                    .with_prompt("  ¿Continuar de todas formas?")
                    .default(false)
                    .interact()
                    .unwrap_or(false);
                if !continuar {
                    println!("  ⏹️  Escaneo cancelado.");
                    wait_for_keypress();
                    return Ok(());
                }
            } else {
                eprintln!(
                    "{}",
                    "  ⚠️  Continuando en modo no interactivo (sin confirmación).".bright_yellow()
                );
            }
        }

        println!("{}", "  ═══ Modo batch ═══".bright_cyan());
        println!("  📁 Origen:  {}", config.source_path.display());
        println!("  📂 Salida:  {}", config.output_dir.display());
        println!(
            "  🎯 Buscar:  {}",
            config
                .categories
                .iter()
                .map(|c| format!("{}", c))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!();

        if let Err(e) = run_scan(config, true) {
            eprintln!("{}", format!("  ❌ Error: {}", e).bright_red());
            if let Some(hint) = friendly_error_hint(&e) {
                eprintln!("{}", hint.bright_yellow());
            }
            let mut source = e.source();
            while let Some(cause) = source {
                eprintln!("{}", format!("     Causa: {}", cause).bright_red());
                source = cause.source();
            }
            eprintln!();
            eprintln!(
                "{}",
                "  💡 Si estás escaneando un disco físico, ejecuta como Administrador."
                    .bright_yellow()
            );
        }
        // Un cron/script sin TTY nunca debe quedar colgado esperando ENTER.
        if is_tty {
            wait_for_keypress();
        }
    } else {
        // Sin source y sin TTY de ENTRADA (script/cron que olvidó pasar argumentos,
        // wrapper que invoca mal el binario, etc.): no entrar al menú interactivo, que
        // se quedaría colgado esperando input de dialoguer que nunca va a llegar.
        // Ojo: se chequea stdin específicamente (lo que dialoguer realmente lee), no
        // `is_tty` (que es de stdout, usado arriba para decisiones cosméticas como
        // saltar el update-check). Si se usara `is_tty` acá, un uso legítimo como
        // `recupe_ghost | tee log.txt` (stdin sigue siendo un TTY real; solo stdout
        // está redirigido) abortaría en vez de mostrar el menú interactivo.
        let stdin_is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        if !stdin_is_tty {
            eprintln!(
                "{}",
                "  ❌ Error: no se especificó un origen y no hay una terminal interactiva disponible (ejecución sin TTY)."
                    .bright_red()
            );
            eprintln!(
                "{}",
                "     Especifica un origen: recupe_ghost <SOURCE> [--fotos] [--videos] [--audio] [--documentos] [-o <OUTPUT>]"
                    .bright_yellow()
            );
            process::exit(1);
        }

        // ── Modo interactivo (comportamiento original) ──
        banner::show_banner();

        loop {
            match ui::main_menu()? {
                MainMenuChoice::Scan => {
                    if let Some(config) = ui::scan_menu()? {
                        if let Err(e) = run_scan(config, false) {
                            eprintln!();
                            eprintln!(
                                "{}",
                                format!("  ❌ Error durante el escaneo: {}", e).bright_red()
                            );
                            if let Some(hint) = friendly_error_hint(&e) {
                                eprintln!("{}", hint.bright_yellow());
                            }
                            // Mostrar causa raíz si existe
                            let mut source = e.source();
                            while let Some(cause) = source {
                                eprintln!("{}", format!("     Causa: {}", cause).bright_red());
                                source = cause.source();
                            }
                            eprintln!();
                            eprintln!(
                                "{}",
                                "  💡 Si estás escaneando un disco físico, ejecuta como Administrador."
                                    .bright_yellow()
                            );
                            eprintln!();
                        }
                    }
                }
                MainMenuChoice::About => {
                    ui::show_about();
                }
                MainMenuChoice::Exit => {
                    ui::show_goodbye();
                    break;
                }
            }
        }
    }

    Ok(())
}

/// Busca un `std::io::Error` en la cadena de causas del error y devuelve una traducción
/// amigable en español para los casos más comunes con los que alguien sin conocimiento técnico
/// puede toparse (permisos, dispositivo desconectado). El mensaje técnico original se sigue
/// mostrando abajo como "Causa:" para quien lo necesite; esto es un resumen en criollo antes.
fn friendly_error_hint(e: &anyhow::Error) -> Option<&'static str> {
    for cause in e.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>() {
            return match io_err.kind() {
                std::io::ErrorKind::PermissionDenied => Some(
                    "  🔒 No tenés permisos suficientes para acceder a ese disco o archivo. \
Si es un disco físico, ejecutá el programa como Administrador (Windows) o con sudo (Linux/macOS).",
                ),
                std::io::ErrorKind::NotFound => Some(
                    "  🔍 No se encontró la ruta indicada. Verificá que el disco/USB siga conectado \
y que la ruta esté bien escrita.",
                ),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted => Some(
                    "  ⏱️  El dispositivo tardó demasiado en responder. Puede estar desconectado \
o dañado — probá reconectarlo.",
                ),
                _ => None,
            };
        }
    }
    None
}

/// Espera a que el usuario presione ENTER antes de cerrar.
/// Útil cuando se ejecuta con doble clic en Windows.
fn wait_for_keypress() {
    println!("{}", "  Presiona ENTER para cerrar...".bright_black());
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn run_scan(config: ScanConfig, batch: bool) -> Result<()> {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "         👻 ESCANEANDO...                        "
            .bright_white()
            .bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝".bright_cyan()
    );
    println!();

    // Obtener firmas según las categorías seleccionadas
    let sigs = signatures_for_categories(&config.categories);

    println!("  🔎 Buscando {} tipos de archivo...", sigs.len());
    println!();

    // Ejecutar escaneo
    let result = scanner::scan_source(&config.source_path, &sigs)?;

    // Mostrar resumen
    println!("{}", result.summary().bright_green());
    println!();

    if result.found_files.is_empty() {
        if result.cancelled {
            println!(
                "{}",
                "  ⏹️  Cancelaste antes de que se encontrara ningún archivo.".bright_yellow()
            );
            println!(
                "{}",
                "     Podés volver a escanear cuando quieras.".bright_black()
            );
        } else {
            println!(
                "{}",
                "  😔 No se encontraron archivos multimedia.".bright_yellow()
            );
            println!(
                "{}",
                "     Intenta con otra imagen de disco o categorías diferentes.".bright_black()
            );
        }
        println!();
        return Ok(());
    }

    // Mostrar lista de archivos encontrados
    println!("{}", "  ═══ Archivos encontrados ═══".bright_cyan());
    println!(
        "{}",
        "  (se guardan con este nombre; no conservan el nombre ni la carpeta original)"
            .bright_black()
    );
    for (i, found) in result.found_files.iter().enumerate() {
        if i < 20 {
            println!("  {}", found.friendly_summary());
        } else if i == 20 {
            println!("  ... y {} archivos más", result.found_files.len() - 20);
            break;
        }
    }
    println!();

    // En modo batch: recuperar directamente. En interactivo: preguntar.
    let should_recover = if batch { true } else { ui::ask_recover()? };

    if should_recover {
        println!();
        println!(
            "{}",
            "  ╔══════════════════════════════════════════════╗".bright_green()
        );
        println!(
            "{}{}{}",
            "  ║".bright_green(),
            "         💾 RECUPERANDO ARCHIVOS...              "
                .bright_white()
                .bold(),
            "║".bright_green()
        );
        println!(
            "{}",
            "  ╚══════════════════════════════════════════════╝".bright_green()
        );
        println!();

        let recovery_result =
            recovery::recover_files(&config.source_path, &result.found_files, &config.output_dir)?;

        println!();
        println!(
            "{}",
            "  ╔══════════════════════════════════════════════╗".bright_green()
        );
        println!(
            "{}{}{}",
            "  ║".bright_green(),
            "         ✅ RECUPERACIÓN COMPLETADA              "
                .bright_white()
                .bold(),
            "║".bright_green()
        );
        println!(
            "{}",
            "  ╠══════════════════════════════════════════════╣".bright_green()
        );
        println!("{}", recovery_result.summary());
        println!(
            "{}",
            "  ╚══════════════════════════════════════════════╝".bright_green()
        );
        println!();
    }

    Ok(())
}
