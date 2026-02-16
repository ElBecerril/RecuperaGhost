use std::path::PathBuf;

use anyhow::Result;
use colored::Colorize;
use dialoguer::{Confirm, Input, MultiSelect, Select};

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

    // 1. Seleccionar origen
    println!(
        "{}",
        "  Ingresa la ruta del archivo de imagen de disco (.img, .dd, .raw)".bright_yellow()
    );
    println!(
        "{}",
        "  o la ruta de un disco/partición para escanear:".bright_yellow()
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
        "  ¿Te sirvió Salva Godínez?                     "
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
        "  Esta herramienta es 100% gratis. Si quieres   "
            .bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  apoyar su desarrollo, ver mis videos me        "
            .bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  ayuda muchísimo.                               "
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
