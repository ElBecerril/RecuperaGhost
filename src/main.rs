mod banner;
mod drives;
mod recovery;
mod scanner;
mod signatures;
mod ui;
mod updater;

use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;

use signatures::{signatures_for_categories, FileCategory};
use ui::{MainMenuChoice, ScanConfig};

/// RecupeGhost - El Detective de Archivos Perdidos
///
/// Recupera fotos, videos y audios borrados desde discos o imágenes raw.
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

    /// Directorio de salida para archivos recuperados
    #[arg(short = 'o', long = "output")]
    output: Option<String>,
}

impl CliArgs {
    /// Convierte los argumentos CLI en un ScanConfig para modo batch.
    fn into_scan_config(self) -> ScanConfig {
        let source_path = PathBuf::from(self.source.unwrap());

        let categories = if !self.fotos && !self.videos && !self.audio {
            // Ningún flag = buscar todo
            vec![FileCategory::Photo, FileCategory::Video, FileCategory::Audio]
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
            cats
        };

        let output_dir = match self.output {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(format!(
                "RecupeGhost_{}",
                chrono::Local::now().format("%Y%m%d_%H%M%S")
            )),
        };

        ScanConfig {
            source_path,
            output_dir,
            categories,
        }
    }

    /// Valida que no se pasen flags sin source.
    fn validate(&self) {
        let has_flags = self.fotos || self.videos || self.audio || self.output.is_some();
        if self.source.is_none() && has_flags {
            eprintln!(
                "{}",
                "  ❌ Error: debes especificar una ruta de origen cuando usas --fotos, --videos, --audio u -o."
                    .bright_red()
            );
            eprintln!(
                "{}",
                "     Uso: recupe_ghost <SOURCE> [--fotos] [--videos] [--audio] [-o <OUTPUT>]"
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

    updater::cleanup_old_binary();
    updater::check_for_updates();

    let args = CliArgs::parse();
    args.validate();

    if args.source.is_some() {
        // ── Modo batch ──
        banner::show_banner();

        let config = args.into_scan_config();

        // Validar que la ruta existe (excepto dispositivos raw)
        let src = config.source_path.to_string_lossy();
        if !src.starts_with("\\\\.\\") && !src.starts_with("/dev/") && !config.source_path.exists()
        {
            eprintln!(
                "{}",
                format!("  ❌ La ruta '{}' no existe.", src).bright_red()
            );
            wait_for_keypress();
            process::exit(1);
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
            eprintln!(
                "{}",
                format!("  ❌ Error: {}", e).bright_red()
            );
            let mut source = e.source();
            while let Some(cause) = source {
                eprintln!(
                    "{}",
                    format!("     Causa: {}", cause).bright_red()
                );
                source = cause.source();
            }
            eprintln!();
            eprintln!(
                "{}",
                "  💡 Si estás escaneando un disco físico, ejecuta como Administrador."
                    .bright_yellow()
            );
        }
        wait_for_keypress();
    } else {
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
                            // Mostrar causa raíz si existe
                            let mut source = e.source();
                            while let Some(cause) = source {
                                eprintln!(
                                    "{}",
                                    format!("     Causa: {}", cause).bright_red()
                                );
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

/// Espera a que el usuario presione ENTER antes de cerrar.
/// Útil cuando se ejecuta con doble clic en Windows.
fn wait_for_keypress() {
    println!(
        "{}",
        "  Presiona ENTER para cerrar...".bright_black()
    );
    let _ = std::io::stdin().read_line(&mut String::new());
}

fn run_scan(config: ScanConfig, batch: bool) -> Result<()> {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_cyan()
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
        "  ╚══════════════════════════════════════════════╝"
            .bright_cyan()
    );
    println!();

    // Obtener firmas según las categorías seleccionadas
    let sigs = signatures_for_categories(&config.categories);

    println!(
        "  🔎 Buscando {} tipos de archivo...",
        sigs.len()
    );
    println!();

    // Ejecutar escaneo
    let result = scanner::scan_source(&config.source_path, &sigs)?;

    // Mostrar resumen
    println!("{}", result.summary().bright_green());
    println!();

    if result.found_files.is_empty() {
        println!(
            "{}",
            "  😔 No se encontraron archivos multimedia."
                .bright_yellow()
        );
        println!(
            "{}",
            "     Intenta con otra imagen de disco o categorías diferentes."
                .bright_black()
        );
        println!();
        return Ok(());
    }

    // Mostrar lista de archivos encontrados
    println!(
        "{}",
        "  ═══ Archivos encontrados ═══".bright_cyan()
    );
    for (i, found) in result.found_files.iter().enumerate() {
        if i < 20 {
            println!("  {}", found);
        } else if i == 20 {
            println!(
                "  ... y {} archivos más",
                result.found_files.len() - 20
            );
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
            "  ╔══════════════════════════════════════════════╗"
                .bright_green()
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
            "  ╚══════════════════════════════════════════════╝"
                .bright_green()
        );
        println!();

        let recovery_result = recovery::recover_files(
            &config.source_path,
            &result.found_files,
            &config.output_dir,
        )?;

        println!();
        println!(
            "{}",
            "  ╔══════════════════════════════════════════════╗"
                .bright_green()
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
            "  ╠══════════════════════════════════════════════╣"
                .bright_green()
        );
        println!("{}", recovery_result.summary());
        println!(
            "{}",
            "  ╚══════════════════════════════════════════════╝"
                .bright_green()
        );
        println!();
    }

    Ok(())
}
