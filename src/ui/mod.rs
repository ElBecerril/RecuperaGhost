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
    let is_physical = crate::util::is_physical_device(&source_path);
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

    // Advertencia best-effort: no recuperar sobre el mismo disco que se escanea.
    if let Some(warning) = same_device_warning(&source_path, &output_dir) {
        println!("{}", warning.bright_yellow());
        println!();
    }

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

/// Verificación best-effort (no bloqueante): advierte si el directorio de salida
/// parece estar en el mismo dispositivo físico que se va a escanear. No pretende
/// detectar todos los casos, solo el escenario típico de herramienta portable
/// (ej: correr el .exe desde el mismo USB que se quiere recuperar).
/// Normaliza el path de un dispositivo de partición al path del disco completo
/// que lo contiene, para poder compararlo contra `drives::list_drives()` (que
/// solo enumera discos completos, nunca particiones individuales).
///
/// Patrones reconocidos:
/// - Linux: `/dev/sdXN` -> `/dev/sdX` (ej. `/dev/sdb1` -> `/dev/sdb`)
/// - Linux: `/dev/nvmeXnYpZ` -> `/dev/nvmeXnY` (ej. `/dev/nvme0n1p2` -> `/dev/nvme0n1`)
/// - macOS: `/dev/diskNsM` -> `/dev/diskN` (ej. `/dev/disk2s1` -> `/dev/disk2`)
///
/// Si el path no matchea ninguno de estos patrones de partición (ej. ya es un
/// disco completo, o es una ruta de Windows tipo `\\.\PhysicalDriveN`), se
/// devuelve tal cual sin modificar.
fn normalize_to_whole_disk(path: &str) -> String {
    // Caso NVMe: /dev/nvme<N>n<M>p<P> -> /dev/nvme<N>n<M>
    if let Some(rest) = path.strip_prefix("/dev/nvme") {
        // rest debe tener forma "<N>n<M>p<P>"; buscamos la 'p' que precede al
        // sufijo numérico de partición.
        if let Some(p_idx) = rest.rfind('p') {
            let (before_p, after_p) = rest.split_at(p_idx);
            let partition_suffix = &after_p[1..]; // sin la 'p'
            if !before_p.is_empty()
                && before_p.contains('n')
                && !partition_suffix.is_empty()
                && partition_suffix.chars().all(|c| c.is_ascii_digit())
            {
                return format!("/dev/nvme{}", before_p);
            }
        }
        return path.to_string();
    }

    // Caso macOS: /dev/disk<N>s<M> -> /dev/disk<N>
    if let Some(rest) = path.strip_prefix("/dev/disk") {
        if let Some(s_idx) = rest.find('s') {
            let (before_s, after_s) = rest.split_at(s_idx);
            let partition_suffix = &after_s[1..]; // sin la 's'
            if !before_s.is_empty()
                && before_s.chars().all(|c| c.is_ascii_digit())
                && !partition_suffix.is_empty()
                && partition_suffix.chars().all(|c| c.is_ascii_digit())
            {
                return format!("/dev/disk{}", before_s);
            }
        }
        return path.to_string();
    }

    // Caso genérico Linux (sd*, vd*, hd*, xvd*, etc.): /dev/<letras><digitos> -> /dev/<letras>
    if let Some(rest) = path.strip_prefix("/dev/") {
        let letters_end = rest.find(|c: char| c.is_ascii_digit());
        if let Some(idx) = letters_end {
            let (letters, digits) = rest.split_at(idx);
            if !letters.is_empty()
                && letters.chars().all(|c| c.is_ascii_alphabetic())
                && !digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit())
            {
                return format!("/dev/{}", letters);
            }
        }
    }

    path.to_string()
}

pub fn same_device_warning(source_path: &std::path::Path, output_dir: &std::path::Path) -> Option<String> {
    let src_str = source_path.to_string_lossy();
    if !crate::util::is_physical_device(source_path) {
        return None;
    }

    let normalized_src = normalize_to_whole_disk(&src_str);

    // Buscar el punto de montaje del dispositivo origen entre los discos detectados.
    // Primero se intenta con el path tal cual (caso disco completo o
    // \\.\PhysicalDriveN de Windows), y si no matchea se reintenta con el
    // identificador de disco completo normalizado (caso partición, ej.
    // /dev/sdb1 -> /dev/sdb, /dev/nvme0n1p2 -> /dev/nvme0n1, /dev/disk2s1 -> /dev/disk2).
    let drives = drives::list_drives();
    let mount = drives
        .iter()
        .find(|d| d.path == src_str)
        .or_else(|| drives.iter().find(|d| d.path == normalized_src))
        .and_then(|d| d.letter.clone())?;

    if mount.trim().is_empty() {
        return None;
    }

    let mount_path = PathBuf::from(&mount);

    // Resolver el directorio de salida a una ruta absoluta lo mejor posible
    // sin requerir que exista todavía.
    let candidate = if output_dir.exists() {
        output_dir.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(output_dir),
            Err(_) => output_dir.to_path_buf(),
        }
    };

    let candidate_str = candidate.to_string_lossy().replace('\\', "/");
    let mount_str = mount_path.to_string_lossy().replace('\\', "/");
    let mount_prefix = mount_str.trim_end_matches('/');

    let mut same = candidate_str == mount_str
        || candidate_str.starts_with(&format!("{}/", mount_prefix))
        || candidate_str == mount_prefix;

    // Comprobación adicional en Unix vía device id (best-effort, si es posible).
    #[cfg(unix)]
    if !same {
        use std::os::unix::fs::MetadataExt;

        fn dev_id_of(mut path: std::path::PathBuf) -> Option<u64> {
            loop {
                if let Ok(meta) = std::fs::metadata(&path) {
                    return Some(meta.dev());
                }
                if !path.pop() {
                    return None;
                }
            }
        }

        if let (Some(out_dev), Some(mount_dev)) =
            (dev_id_of(candidate.clone()), dev_id_of(mount_path.clone()))
        {
            same = out_dev == mount_dev;
        }
    }

    if same {
        Some(format!(
            "  ⚠️  El directorio de salida parece estar en el mismo dispositivo que estás escaneando ({}).\n     Recuperar archivos ahí puede sobrescribir los sectores borrados que intentas rescatar.",
            mount
        ))
    } else {
        None
    }
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
            format!("  📄 {} ({})", name, crate::util::format_size(size))
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
        "  Licencia: GPL-3.0                             ".bright_green(),
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
