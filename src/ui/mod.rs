use std::path::PathBuf;

use anyhow::Result;
use colored::Colorize;
use dialoguer::{Confirm, Input, MultiSelect, Select};

use crate::drives;
use crate::signatures::FileCategory;

/// Opciones del menú principal
#[derive(Debug)]
pub enum MainMenuChoice {
    Scan,
    About,
    Exit,
}

/// Configuración de escaneo elegida por el usuario
pub struct ScanConfig {
    pub source_path: PathBuf,
    pub output_dir: PathBuf,
    pub categories: Vec<FileCategory>,
}

/// Muestra el menú principal y retorna la opción elegida
pub fn main_menu() -> Result<MainMenuChoice> {
    let options = vec![
        "🔍 Escanear disco/imagen",
        "ℹ️  Acerca de RecupeGhost",
        "🚪 Salir",
    ];

    let selection = Select::new()
        .with_prompt("  👻 ¿Qué deseas hacer?")
        .items(&options)
        .default(0)
        .interact()?;

    match selection {
        0 => Ok(MainMenuChoice::Scan),
        1 => Ok(MainMenuChoice::About),
        _ => Ok(MainMenuChoice::Exit),
    }
}

/// Menú de configuración del escaneo
pub fn scan_menu() -> Result<Option<ScanConfig>> {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "         🔍 CONFIGURAR ESCANEO                  ".bright_white().bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_cyan()
    );
    println!();

    // 1. Seleccionar origen con menú inteligente
    let source_path = match select_source()? {
        Some(path) => path,
        None => return Ok(None),
    };

    // Verificar permisos si es un disco físico
    let src_str = source_path.to_string_lossy();
    let is_physical = src_str.starts_with("\\\\.\\") || src_str.starts_with("/dev/");
    if is_physical && !drives::is_admin() {
        println!();
        println!(
            "{}",
            "  ⚠️  No tienes permisos de Administrador.".bright_yellow()
        );
        println!(
            "{}",
            "     El escaneo de discos físicos requiere permisos elevados.".bright_yellow()
        );
        println!();
        #[cfg(target_os = "windows")]
        println!(
            "{}",
            "  💡 Cierra el programa, haz clic derecho en el .exe".bright_cyan()
        );
        #[cfg(target_os = "windows")]
        println!(
            "{}",
            "     y selecciona \"Ejecutar como administrador\".".bright_cyan()
        );
        #[cfg(not(target_os = "windows"))]
        println!(
            "{}",
            "  💡 Ejecuta el programa con: sudo ./recupe_ghost".bright_cyan()
        );
        println!();

        let retry_options = vec![
            "🔄 Intentar de todas formas",
            "↩️  Volver al menú",
        ];

        let choice = Select::new()
            .with_prompt("  ¿Qué deseas hacer?")
            .items(&retry_options)
            .default(1)
            .interact()?;

        if choice == 1 {
            return Ok(None);
        }
    }

    println!();

    // 2. Seleccionar tipos de archivo
    println!(
        "{}",
        "  Selecciona qué tipos de archivos buscar:".bright_yellow()
    );
    println!(
        "{}",
        "  (Usa ESPACIO para marcar/desmarcar, ENTER para confirmar)".bright_black()
    );
    println!();

    let type_options = vec![
        "📷 Fotos (JPG, PNG, GIF, BMP, WebP, TIFF)",
        "🎬 Videos (MP4, AVI, MKV, MOV, FLV, 3GP)",
        "🎵 Audio (MP3, WAV, FLAC, OGG, AAC, M4A, AMR, WMA, OPUS)",
    ];

    let selected_types = MultiSelect::new()
        .with_prompt("  🎯 Tipos de archivo")
        .items(&type_options)
        .defaults(&[true, true, true])
        .interact()?;

    if selected_types.is_empty() {
        println!(
            "{}",
            "  ❌ Debes seleccionar al menos un tipo de archivo."
                .bright_red()
        );
        return Ok(None);
    }

    let mut categories = Vec::new();
    for idx in &selected_types {
        match idx {
            0 => categories.push(FileCategory::Photo),
            1 => categories.push(FileCategory::Video),
            2 => categories.push(FileCategory::Audio),
            _ => {}
        }
    }

    println!();

    // 3. Directorio de salida
    let default_output = format!(
        "RecupeGhost_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );

    let output: String = Input::new()
        .with_prompt("  📂 Carpeta de salida")
        .default(default_output)
        .interact_text()?;

    let output_dir = PathBuf::from(output.trim());

    println!();

    // 4. Confirmar
    println!("{}", "  ═══ Resumen del escaneo ═══".bright_cyan());
    println!("  📁 Origen:  {}", source_path.display());
    println!("  📂 Salida:  {}", output_dir.display());
    println!(
        "  🎯 Buscar:  {}",
        categories
            .iter()
            .map(|c| format!("{}", c))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!();

    let confirm = Confirm::new()
        .with_prompt("  ¿Iniciar escaneo?")
        .default(true)
        .interact()?;

    if !confirm {
        println!("  ⏹️  Escaneo cancelado.");
        return Ok(None);
    }

    Ok(Some(ScanConfig {
        source_path,
        output_dir,
        categories,
    }))
}

/// Menú inteligente para seleccionar la fuente de escaneo.
/// Presenta 3 opciones: USB/disco externo, archivo de imagen, o ruta manual.
fn select_source() -> Result<Option<PathBuf>> {
    let options = vec![
        "💾 Memoria USB / disco externo",
        "📁 Archivo de imagen (.img, .dd, .raw)",
        "🔧 Escribir ruta manualmente",
    ];

    let selection = Select::new()
        .with_prompt("  ¿Qué deseas escanear?")
        .items(&options)
        .default(0)
        .interact()?;

    match selection {
        0 => select_removable_drive(),
        1 => select_image_file(),
        2 => select_manual_path(),
        _ => Ok(None),
    }
}

/// Detecta y lista discos removibles (USB, externos) para que el usuario elija.
fn select_removable_drive() -> Result<Option<PathBuf>> {
    println!();
    println!(
        "{}",
        "  🔎 Detectando dispositivos...".bright_cyan()
    );

    // Verificar permisos de administrador en Windows
    #[cfg(target_os = "windows")]
    if !drives::is_admin() {
        println!();
        println!(
            "{}",
            "  ⚠️  Para escanear discos físicos, ejecuta como Administrador"
                .bright_yellow()
        );
        println!(
            "{}",
            "     (clic derecho → Ejecutar como administrador)"
                .bright_yellow()
        );
        println!();
        println!(
            "{}",
            "  Intentando detectar dispositivos de todas formas...".bright_black()
        );
    }

    let removable = drives::list_removable();

    if removable.is_empty() {
        println!();
        println!(
            "{}",
            "  😔 No se detectaron memorias USB ni discos externos."
                .bright_yellow()
        );
        println!();

        let fallback_options = vec![
            "📋 Ver TODOS los discos del sistema",
            "📁 Buscar archivo de imagen en su lugar",
            "🔧 Escribir ruta manualmente",
            "↩️  Volver",
        ];

        let fallback = Select::new()
            .with_prompt("  ¿Qué deseas hacer?")
            .items(&fallback_options)
            .default(0)
            .interact()?;

        return match fallback {
            0 => select_all_drives(),
            1 => select_image_file(),
            2 => select_manual_path(),
            _ => Ok(None),
        };
    }

    println!();
    show_drive_list(&removable)
}

/// Muestra todos los discos del sistema (fallback cuando no hay removibles).
fn select_all_drives() -> Result<Option<PathBuf>> {
    let all_drives = drives::list_drives();

    if all_drives.is_empty() {
        println!();
        println!(
            "{}",
            "  ❌ No se pudieron detectar discos en el sistema.".bright_red()
        );
        println!(
            "{}",
            "     Intenta escribir la ruta manualmente.".bright_black()
        );
        println!();
        return select_manual_path();
    }

    println!();
    println!(
        "{}",
        "  ⚠️  Se muestran TODOS los discos. Ten cuidado de no escanear"
            .bright_yellow()
    );
    println!(
        "{}",
        "     el disco del sistema operativo.".bright_yellow()
    );
    println!();

    show_drive_list(&all_drives)
}

/// Muestra una lista de discos para que el usuario seleccione uno.
fn show_drive_list(drive_list: &[drives::DriveInfo]) -> Result<Option<PathBuf>> {
    let mut display_items: Vec<String> = drive_list
        .iter()
        .map(|d| format!("  {}", d.display_name))
        .collect();
    display_items.push("  ↩️  Volver".to_string());

    let selection = Select::new()
        .with_prompt("  Selecciona el dispositivo")
        .items(&display_items)
        .default(0)
        .interact()?;

    if selection >= drive_list.len() {
        return Ok(None);
    }

    let selected = &drive_list[selection];
    println!(
        "  ✅ Seleccionado: {}",
        selected.display_name.bright_green()
    );

    Ok(Some(PathBuf::from(&selected.path)))
}

/// Busca archivos de imagen de disco (.img, .dd, .raw) en el directorio actual
/// y permite al usuario seleccionar uno o escribir la ruta manualmente.
fn select_image_file() -> Result<Option<PathBuf>> {
    println!();
    println!(
        "{}",
        "  🔎 Buscando archivos de imagen en el directorio actual..."
            .bright_cyan()
    );

    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut image_files: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&current_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext_lower = ext.to_lowercase();
                    if ext_lower == "img" || ext_lower == "dd" || ext_lower == "raw" || ext_lower == "iso" {
                        image_files.push(path);
                    }
                }
            }
        }
    }

    if image_files.is_empty() {
        println!();
        println!(
            "{}",
            "  😔 No se encontraron archivos de imagen (.img, .dd, .raw) en el directorio actual."
                .bright_yellow()
        );
        println!();

        let source: String = Input::new()
            .with_prompt("  📁 Escribe la ruta del archivo de imagen")
            .interact_text()?;

        let path = PathBuf::from(source.trim());
        if !path.exists() {
            println!(
                "{}",
                "  ❌ La ruta especificada no existe. Verifica e intenta de nuevo."
                    .bright_red()
            );
            return Ok(None);
        }

        return Ok(Some(path));
    }

    println!();

    let mut display_items: Vec<String> = image_files
        .iter()
        .map(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            format!("  📄 {} ({})", name, format_file_size(size))
        })
        .collect();
    display_items.push("  ✏️  Escribir ruta manualmente".to_string());
    display_items.push("  ↩️  Volver".to_string());

    let selection = Select::new()
        .with_prompt("  Selecciona el archivo de imagen")
        .items(&display_items)
        .default(0)
        .interact()?;

    if selection < image_files.len() {
        let selected = &image_files[selection];
        println!(
            "  ✅ Seleccionado: {}",
            selected.display().to_string().bright_green()
        );
        return Ok(Some(selected.clone()));
    }

    if selection == image_files.len() {
        // Escribir ruta manualmente
        let source: String = Input::new()
            .with_prompt("  📁 Ruta del archivo de imagen")
            .interact_text()?;

        let path = PathBuf::from(source.trim());
        if !path.exists() {
            println!(
                "{}",
                "  ❌ La ruta especificada no existe. Verifica e intenta de nuevo."
                    .bright_red()
            );
            return Ok(None);
        }

        return Ok(Some(path));
    }

    // Volver
    Ok(None)
}

/// Permite escribir una ruta manualmente (comportamiento original).
fn select_manual_path() -> Result<Option<PathBuf>> {
    println!();
    println!(
        "{}",
        "  Ingresa la ruta del disco, partición o archivo de imagen:".bright_yellow()
    );
    println!(
        "{}",
        "  Ejemplos: /dev/sdb1, \\\\.\\PhysicalDrive1, disco.img".bright_black()
    );
    println!();

    let source: String = Input::new()
        .with_prompt("  📁 Ruta de origen")
        .interact_text()?;

    let source_path = PathBuf::from(source.trim());

    // Verificar que existe (solo si no es un dispositivo raw)
    if !source_path.to_string_lossy().starts_with("\\\\.\\")
        && !source_path.to_string_lossy().starts_with("/dev/")
        && !source_path.exists()
    {
        println!(
            "{}",
            "  ❌ La ruta especificada no existe. Verifica e intenta de nuevo."
                .bright_red()
        );
        return Ok(None);
    }

    Ok(Some(source_path))
}

/// Formatea bytes a una cadena legible para archivos.
fn format_file_size(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1_024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Pregunta si el usuario quiere recuperar los archivos encontrados
pub fn ask_recover() -> Result<bool> {
    println!();
    let confirm = Confirm::new()
        .with_prompt("  💾 ¿Deseas recuperar los archivos encontrados?")
        .default(true)
        .interact()?;
    Ok(confirm)
}

/// Muestra la pantalla "Acerca de"
pub fn show_about() {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "         ℹ️  ACERCA DE RECUPEGHOST               ".bright_white().bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  RecupeGhost es una herramienta portable de    ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  recuperación de archivos multimedia borrados.  ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Utiliza la técnica de 'file carving' para     ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  buscar firmas (magic bytes) de archivos       ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  directamente en el disco o imagen raw.        ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Soporta: JPG, PNG, GIF, BMP, WebP, TIFF,     ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  MP4, AVI, MKV, MOV, MP3, WAV, FLAC, OGG,     ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  AAC, M4A, AMR, WMA, OPUS y más.               ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Creado por: El_Becerril                       ".bright_green(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Licencia: MIT                                 ".bright_green(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_cyan()
    );
    println!();
}

/// Muestra un mensaje de despedida con opciones de apoyo
pub fn show_goodbye() {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  👻 ¿Te sirvió RecupeGhost?                    "
            .bright_white()
            .bold(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  RecupeGhost es 100% gratis y open source.     "
            .bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Si te ayudó a recuperar tus archivos,         "
            .bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  apóyanos viendo nuestros videos.              "
            .bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "                                                ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_cyan()
    );
    println!();

    let options = vec![
        "🎬 YouTube  (@el_becerril)",
        "📘 Facebook (El Becerril)",
        "🚪 Cerrar",
    ];

    let selection = Select::new()
        .with_prompt("  👻 ¿Quieres apoyar?")
        .items(&options)
        .default(2)
        .interact();

    match selection {
        Ok(0) => {
            let _ = open_url("https://www.youtube.com/@el_becerril");
        }
        Ok(1) => {
            let _ = open_url("https://www.facebook.com/ElBecerril");
        }
        _ => {}
    }

    println!();
    println!(
        "{}",
        "  👻 ¡Hasta la próxima! RecupeGhost siempre estará aquí".bright_cyan()
    );
    println!(
        "{}",
        "     para rescatar tus archivos perdidos...".bright_cyan()
    );
    println!();
}

/// Abre una URL en el navegador predeterminado
fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}
