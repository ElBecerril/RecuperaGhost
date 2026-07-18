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
    Clone,
    About,
    Exit,
}

/// Configuración de escaneo elegida por el usuario
pub struct ScanConfig {
    pub source_path: PathBuf,
    pub output_dir: PathBuf,
    pub categories: Vec<FileCategory>,
}

/// Configuración de un clonado a imagen elegida por el usuario
pub struct CloneConfig {
    pub source_path: PathBuf,
    pub output_path: PathBuf,
}

/// Muestra el menú principal y retorna la opción elegida
pub fn main_menu() -> Result<MainMenuChoice> {
    let options = vec![
        "🔍 Escanear disco/imagen",
        "📀 Clonar un disco que está fallando (copiarlo a una imagen primero)",
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
        1 => Ok(MainMenuChoice::Clone),
        2 => Ok(MainMenuChoice::About),
        _ => Ok(MainMenuChoice::Exit),
    }
}

/// Menú de configuración del escaneo (el usuario elige el origen).
pub fn scan_menu() -> Result<Option<ScanConfig>> {
    scan_menu_with_source(None)
}

/// Igual que `scan_menu`, pero permite entrar con un origen ya elegido (`preselected`), para el
/// flujo "clonar y después escanear la imagen recién creada" sin volver a pedir el origen.
pub fn scan_menu_with_source(preselected: Option<PathBuf>) -> Result<Option<ScanConfig>> {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "         🔍 CONFIGURAR ESCANEO                  "
            .bright_white()
            .bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝".bright_cyan()
    );
    println!();
    println!(
        "{}",
        "  En todos los menús: usá las flechas ↑↓ para moverte y ENTER para elegir.".bright_black()
    );
    println!();

    // 1. Seleccionar origen con menú inteligente (o usar el pre-seleccionado, ej. una imagen
    //    recién clonada).
    let source_path = match preselected {
        Some(path) => {
            println!("  📁 Origen: {}", path.display());
            println!();
            path
        }
        None => {
            println!(
                "{}",
                "  ¿De dónde querés recuperar archivos?".bright_yellow()
            );
            println!(
                "{}",
                "  · Memoria USB / disco externo: para una memoria, tarjeta SD o disco que conectaste aparte."
                    .bright_black()
            );
            println!(
                "{}",
                "  · Disco interno: para el disco de tu PC (fotos borradas del disco principal)."
                    .bright_black()
            );
            println!(
                "{}",
                "  · Archivo de imagen: para un archivo .img/.dd/.raw que ya tenés (uso avanzado)."
                    .bright_black()
            );
            println!();
            match select_source()? {
                Some(path) => path,
                None => return Ok(None),
            }
        }
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

        let retry_options = vec!["🔄 Intentar de todas formas", "↩️  Volver al menú"];

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
        "📷 Fotos (JPG, PNG, GIF, BMP, WebP, TIFF, HEIC, CR2)",
        "🎬 Videos (MP4, AVI, MKV, MOV, FLV, 3GP)",
        "🎵 Audio (MP3, WAV, FLAC, OGG, AAC, M4A, AMR, WMA, OPUS)",
        "📄 Documentos (PDF)",
    ];

    let selected_types = MultiSelect::new()
        .with_prompt("  🎯 Tipos de archivo")
        .items(&type_options)
        .defaults(&[true, true, true, true])
        .interact()?;

    if selected_types.is_empty() {
        println!(
            "{}",
            "  ❌ Debes seleccionar al menos un tipo de archivo.".bright_red()
        );
        return Ok(None);
    }

    let mut categories = Vec::new();
    for idx in &selected_types {
        match idx {
            0 => categories.push(FileCategory::Photo),
            1 => categories.push(FileCategory::Video),
            2 => categories.push(FileCategory::Audio),
            3 => categories.push(FileCategory::Document),
            _ => {}
        }
    }

    println!();

    // 3. Directorio de salida. Para un usuario no técnico, un nombre de carpeta suelto
    //    (RecupeGhost_20260718_...) no dice nada: no se entiende qué es, dónde va a quedar, ni
    //    que se puede cambiar. Explicamos las tres cosas antes de pedir el dato.
    println!(
        "{}",
        "  ¿Dónde querés guardar los archivos recuperados?".bright_yellow()
    );
    let default_name = format!(
        "RecupeGhost_{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    // Mostrar la ruta absoluta donde caería la carpeta por defecto, así el usuario ve el lugar
    // real (ej. C:\Users\...\Downloads\RecupeGhost_...) y no solo un nombre.
    let default_abs = crate::util::to_absolute_output(PathBuf::from(&default_name));
    println!(
        "{}",
        format!(
            "  · Si dejás el nombre sugerido y presionás ENTER, se crea acá:\n      {}",
            default_abs.display()
        )
        .bright_black()
    );
    println!(
        "{}",
        "  · O escribí una ruta completa a otro disco/USB (ej. en Windows: D:\\Recuperados)."
            .bright_black()
    );
    println!(
        "{}",
        "  ⚠️  Guardá en un disco DISTINTO al que estás recuperando, nunca en el mismo."
            .bright_yellow()
    );
    println!();

    let output: String = Input::new()
        .with_prompt("  📂 Carpeta donde guardar")
        .default(default_name)
        .interact_text()?;

    // Resolver a ruta absoluta ahora (en vez de dejarla relativa): ver `to_absolute_output`.
    let output_dir = crate::util::to_absolute_output(PathBuf::from(output.trim()));

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

/// Menú para clonar un disco (que puede estar fallando) a un archivo de imagen.
/// Devuelve el origen y el archivo `.img` de destino elegidos, o `None` si se cancela.
pub fn clone_menu() -> Result<Option<CloneConfig>> {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "        📀 CLONAR DISCO A IMAGEN                "
            .bright_white()
            .bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝".bright_cyan()
    );
    println!();
    println!(
        "{}",
        "  Si tu disco o memoria está fallando, lo más seguro es copiarlo entero a un"
            .bright_black()
    );
    println!(
        "{}",
        "  archivo de imagen ANTES de buscar nada, y después escanear esa copia. Así no"
            .bright_black()
    );
    println!(
        "{}",
        "  estresás el disco enfermo (cada lectura extra puede acelerar su muerte).".bright_black()
    );
    println!();
    println!(
        "{}",
        "  Los sectores que no se puedan leer se saltan: la copia sigue con lo demás."
            .bright_black()
    );
    println!();

    // 1. Elegir el disco/origen a clonar (mismo menú inteligente que el escaneo).
    println!("{}", "  ¿Qué disco querés clonar?".bright_yellow());
    println!();
    let source_path = match select_source()? {
        Some(path) => path,
        None => return Ok(None),
    };

    // Permisos: clonar un disco físico requiere admin igual que escanearlo.
    let is_physical = crate::util::is_physical_device(&source_path);
    if is_physical && !drives::is_admin() {
        println!();
        println!(
            "{}",
            "  ⚠️  No tienes permisos de Administrador.".bright_yellow()
        );
        println!(
            "{}",
            "     Clonar un disco físico requiere permisos elevados.".bright_yellow()
        );
        #[cfg(target_os = "windows")]
        println!(
            "{}",
            "  💡 Cerrá el programa y volvé a abrirlo como Administrador.".bright_cyan()
        );
        #[cfg(not(target_os = "windows"))]
        println!(
            "{}",
            "  💡 Ejecutá el programa con: sudo ./recupe_ghost".bright_cyan()
        );
        println!();
        let retry = vec!["🔄 Intentar de todas formas", "↩️  Volver al menú"];
        let choice = Select::new()
            .with_prompt("  ¿Qué deseas hacer?")
            .items(&retry)
            .default(1)
            .interact()?;
        if choice == 1 {
            return Ok(None);
        }
    }

    // 2. Tamaño esperado de la imagen (best-effort: si no se puede leer, seguimos igual).
    println!();
    match crate::scanner::device_or_file_size(&source_path) {
        Ok(size) => {
            println!(
                "  📏 La imagen va a ocupar aproximadamente {} (el tamaño del disco).",
                crate::util::format_size(size).bright_white()
            );
            println!(
                "{}",
                "     Asegurate de tener ese espacio libre en el destino.".bright_black()
            );
        }
        Err(_) => {
            println!(
                "{}",
                "  ℹ️  No pudimos calcular el tamaño del disco (¿permisos?). La imagen va a ser"
                    .bright_black()
            );
            println!(
                "{}",
                "     del tamaño completo del disco; asegurate de tener espacio de sobra."
                    .bright_black()
            );
        }
    }
    println!();

    // 3. Archivo .img de destino.
    println!(
        "{}",
        "  ¿Dónde guardo la imagen? (elegí un disco DISTINTO al que estás clonando)"
            .bright_yellow()
    );
    let default_output = format!(
        "RecupeGhost_imagen_{}.img",
        chrono::Local::now().format("%Y%m%d_%H%M%S")
    );
    let output: String = Input::new()
        .with_prompt("  📀 Archivo de imagen de salida")
        .default(default_output)
        .interact_text()?;
    let mut output_path = PathBuf::from(output.trim());
    // Asegurar extensión .img si el usuario no puso ninguna extensión reconocible.
    if output_path.extension().is_none() {
        output_path.set_extension("img");
    }
    let output_path = crate::util::to_absolute_output(output_path);

    println!();

    // 4. Resumen + advertencia crítica de mismo-disco (clonar sobre el propio disco de origen
    //    lo sobrescribiría y perdería justo lo que se intenta rescatar).
    println!("{}", "  ═══ Resumen del clonado ═══".bright_cyan());
    println!("  📁 Disco a clonar: {}", source_path.display());
    println!("  📀 Imagen destino: {}", output_path.display());
    println!();

    if let Some(warning) = same_device_warning(&source_path, &output_path) {
        println!("{}", warning.bright_yellow());
        println!();
    }

    let confirm = Confirm::new()
        .with_prompt("  ¿Iniciar el clonado?")
        .default(true)
        .interact()?;
    if !confirm {
        println!("  ⏹️  Clonado cancelado.");
        return Ok(None);
    }

    Ok(Some(CloneConfig {
        source_path,
        output_path,
    }))
}

/// Pregunta si se quiere escanear ahora la imagen recién clonada.
pub fn ask_scan_cloned_image(image: &std::path::Path) -> Result<bool> {
    println!();
    Ok(Confirm::new()
        .with_prompt(format!(
            "  🔍 ¿Escanear ahora la imagen recién creada ({})?",
            image.display()
        ))
        .default(true)
        .interact()?)
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
/// - Linux: `/dev/mmcblkNpM` -> `/dev/mmcblkN` (ej. `/dev/mmcblk0p1` -> `/dev/mmcblk0`, tarjetas SD/eMMC)
/// - Linux: cualquier `/dev/<prefijo><N>p<M>` donde el nombre termina en
///   dígitos y la 'p' está precedida por otro dígito (ej. `/dev/nbd0p1`)
/// - macOS: `/dev/diskNsM` -> `/dev/diskN` (ej. `/dev/disk2s1` -> `/dev/disk2`)
///
/// Si el path no matchea ninguno de estos patrones de partición (ej. ya es un
/// disco completo, o es una ruta de Windows tipo `\\.\PhysicalDriveN`), se
/// devuelve tal cual sin modificar.
fn normalize_to_whole_disk(path: &str) -> String {
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

    if let Some(rest) = path.strip_prefix("/dev/") {
        // Caso genérico "con número de controlador" (bug #3: antes solo se
        // reconocía "nvme" hardcodeado, dejando afuera mmcblk/nbd/loop/etc.):
        // si el nombre termina en dígitos y justo antes de esos dígitos hay
        // una 'p' precedida a su vez por otro dígito, el sufijo "pN" es el
        // separador estándar de partición (ej. nvme0n1p2, mmcblk0p1, nbd0p1)
        // y se corta ahí, quedándonos con el disco completo.
        let digits_start = rest.rfind(|c: char| !c.is_ascii_digit()).map(|i| i + 1);
        if let Some(digits_start) = digits_start {
            if digits_start < rest.len() {
                // rest[..digits_start] termina en 'p', y el char anterior a
                // esa 'p' es un dígito (ej. "nvme0n1" + "p" + "2").
                if let Some(before_p) = rest[..digits_start].strip_suffix('p') {
                    if before_p
                        .chars()
                        .last()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        return format!("/dev/{}", before_p);
                    }
                }
            }
        }

        // Caso simple sd*, vd*, hd*, xvd* (letras + dígitos, sin 'p'):
        // /dev/<letras><digitos> -> /dev/<letras> (ej. /dev/sdb1 -> /dev/sdb).
        //
        // OJO (bug dormido corregido): esto SOLO aplica a las familias cuyo
        // disco completo es puramente alfabético (sda, vdb, hdc, xvda) y cuya
        // partición añade un dígito. Familias como mmcblk0, nbd0, loop0, sr0,
        // md0 tienen el número como parte del NOMBRE del disco completo (sus
        // particiones usan el sufijo `pN`, ya cubierto por la rama de arriba, o
        // directamente no tienen particiones). Recortarles el dígito devolvía
        // un dispositivo inexistente (/dev/mmcblk, /dev/loop) y rompía la
        // comparación. Por eso restringimos a un whitelist de prefijos
        // conocidos: si el nombre no matchea, se devuelve tal cual y
        // same_device_warning cae en su fail-safe (advertir), nunca en un falso
        // negativo silencioso.
        let letters_end = rest.find(|c: char| c.is_ascii_digit());
        if let Some(idx) = letters_end {
            let (letters, digits) = rest.split_at(idx);
            let is_alpha_whole_disk_family = ["sd", "hd", "vd", "xvd"]
                .iter()
                .any(|prefix| letters.starts_with(prefix));
            if is_alpha_whole_disk_family
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

pub fn same_device_warning(
    source_path: &std::path::Path,
    output_dir: &std::path::Path,
) -> Option<String> {
    let src_str = source_path.to_string_lossy();
    if !crate::util::is_physical_device(source_path) {
        return None;
    }

    let normalized_src = normalize_to_whole_disk(&src_str);

    // Buscar el disco origen entre los discos detectados. Primero se intenta
    // con el path tal cual (caso disco completo o \\.\PhysicalDriveN de
    // Windows), y si no matchea se reintenta con el identificador de disco
    // completo normalizado (caso partición, ej. /dev/sdb1 -> /dev/sdb,
    // /dev/nvme0n1p2 -> /dev/nvme0n1, /dev/mmcblk0p1 -> /dev/mmcblk0,
    // /dev/disk2s1 -> /dev/disk2).
    //
    // Bug #5: la comparación de rutas de Windows (`\\.\PhysicalDrive1` vs
    // `\\.\physicaldrive1`) debe ser case-insensitive.
    let drives = drives::list_drives();
    let matched_drive = drives
        .iter()
        .find(|d| d.path.eq_ignore_ascii_case(&src_str))
        .or_else(|| {
            drives
                .iter()
                .find(|d| d.path.eq_ignore_ascii_case(&normalized_src))
        });

    // Bug #1/#2: usar TODOS los mountpoints/letras del disco origen, no solo
    // el primero (`all_mounts`), para no perder de vista particiones
    // adicionales montadas del mismo disco físico.
    let mounts: Vec<String> = matched_drive
        .map(|d| {
            d.all_mounts
                .iter()
                .filter(|m| !m.trim().is_empty())
                .cloned()
                .collect()
        })
        .unwrap_or_default();

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

    // Bug #4: si no pudimos determinar ningún mountpoint/letra del disco de
    // origen (ya sea porque el disco no aparece en `list_drives()` o porque
    // no tiene ninguna partición montada detectada), no hay forma segura de
    // probar que el destino está en un disco distinto. En vez de saltear la
    // advertencia (`?` anterior devolvía `None` de inmediato), preferimos un
    // falso positivo ocasional a un falso negativo que corrompa la
    // recuperación en curso.
    if mounts.is_empty() {
        return Some(format!(
            "  ⚠️  No pudimos confirmar que la carpeta de salida esté en un disco distinto al que estás escaneando ({}).\n     Por las dudas, fijate que la carpeta de salida NO esté en ese mismo disco/USB — si no, podrías perder para siempre justo lo que estás tratando de recuperar.",
            src_str
        ));
    }

    for mount in &mounts {
        let mount_path = PathBuf::from(mount);
        let mount_str = mount_path.to_string_lossy().replace('\\', "/");
        let mount_prefix = mount_str.trim_end_matches('/');

        // Bug #6: cuando `mount == "/"`, `mount_prefix` queda vacío tras el
        // `trim_end_matches('/')`, y `starts_with("{}/", mount_prefix)`
        // degenera en `starts_with("/")`, que matchea CUALQUIER ruta
        // absoluta (falso positivo con discos montados en otro punto). Solo
        // aplicamos el chequeo de prefijo cuando `mount_prefix` no es vacío;
        // el caso `mount == "/"` sigue cubierto por la igualdad exacta y por
        // la comprobación de device id de más abajo.
        let mut same = candidate_str == mount_str
            || candidate_str == mount_prefix
            || (!mount_prefix.is_empty()
                && candidate_str.starts_with(&format!("{}/", mount_prefix)));

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
            return Some(format!(
                "  ⚠️  La carpeta de salida está en el mismo disco que estás escaneando ({}).\n     Si guardás ahí, podrías perder para siempre justo los archivos que estás tratando de recuperar. Elegí otra carpeta, idealmente en otro disco.",
                mount
            ));
        }
    }

    None
}

/// Menú inteligente para seleccionar la fuente de escaneo.
/// Presenta 3 opciones: USB/disco externo, archivo de imagen, o ruta manual.
fn select_source() -> Result<Option<PathBuf>> {
    let options = vec![
        "💾 Memoria USB / disco externo",
        "💽 Disco interno / ver todos los discos del sistema",
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
        1 => select_all_drives(),
        2 => select_image_file(),
        3 => select_manual_path(),
        _ => Ok(None),
    }
}

/// Detecta y lista discos removibles (USB, externos) para que el usuario elija.
fn select_removable_drive() -> Result<Option<PathBuf>> {
    println!();
    println!("{}", "  🔎 Detectando dispositivos...".bright_cyan());

    // Verificar permisos de administrador en Windows
    #[cfg(target_os = "windows")]
    if !drives::is_admin() {
        println!();
        println!(
            "{}",
            "  ⚠️  Para escanear discos físicos, ejecuta como Administrador".bright_yellow()
        );
        println!(
            "{}",
            "     (clic derecho → Ejecutar como administrador)".bright_yellow()
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
            "  😔 No se detectaron memorias USB ni discos externos.".bright_yellow()
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

/// Muestra todos los discos del sistema (incluye discos internos), tanto por
/// selección directa desde `select_source` como de fallback cuando no se
/// detectan dispositivos removibles.
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
        "  ⚠️  Se muestran TODOS los discos, incluido el de tu sistema (Windows/tu PC)."
            .bright_yellow()
    );
    println!(
        "{}",
        "     Podés recuperar del disco de tu PC sin problema — lo importante es GUARDAR"
            .bright_yellow()
    );
    println!(
        "{}",
        "     los archivos recuperados en OTRO disco o USB, nunca en el mismo que escaneás."
            .bright_yellow()
    );
    println!();

    show_drive_list(&all_drives)
}

/// Heurística best-effort para marcar en la lista cuál disco es probablemente el del sistema
/// operativo (Windows: letra C:; Linux/macOS: montado en la raíz "/"). Se marca NO para
/// prohibir escanearlo (recuperar del disco de la PC es un caso de uso legítimo y común), sino
/// para recordarle a quien no tiene conocimiento técnico que la carpeta de salida debe ir en
/// OTRO disco — escribir la recuperación sobre el mismo disco que se escanea es lo peligroso,
/// no leerlo (ver `same_device_warning`).
fn is_likely_system_disk(d: &drives::DriveInfo) -> bool {
    d.all_mounts
        .iter()
        .any(|m| m.eq_ignore_ascii_case("C:") || m == "/")
}

/// Muestra una lista de discos para que el usuario seleccione uno.
fn show_drive_list(drive_list: &[drives::DriveInfo]) -> Result<Option<PathBuf>> {
    let mut display_items: Vec<String> = drive_list
        .iter()
        .map(|d| {
            if is_likely_system_disk(d) {
                format!(
                    "  {}  💻 (disco de tu PC — guardá lo recuperado en OTRO disco/USB)",
                    d.display_name
                )
            } else {
                format!("  {}", d.display_name)
            }
        })
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
        "  🔎 Buscando archivos de imagen en el directorio actual...".bright_cyan()
    );

    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut image_files: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&current_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    let ext_lower = ext.to_lowercase();
                    if ext_lower == "img"
                        || ext_lower == "dd"
                        || ext_lower == "raw"
                        || ext_lower == "iso"
                    {
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

        let no_auto_options = vec!["✏️  Escribir ruta manualmente", "↩️  Volver"];
        let choice = Select::new()
            .with_prompt("  ¿Qué deseas hacer?")
            .items(&no_auto_options)
            .default(0)
            .interact()?;

        if choice == 1 {
            return Ok(None);
        }

        return prompt_path_or_cancel("  📁 Ruta del archivo de imagen", false);
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
        return prompt_path_or_cancel("  📁 Ruta del archivo de imagen", false);
    }

    // Volver
    Ok(None)
}

/// Pide una ruta por teclado; dejar el campo vacío y presionar Enter cancela
/// (devuelve `None` en vez de tratar la cadena vacía como una ruta inválida).
/// `allow_raw_device`: si es true, no valida `exists()` para rutas de
/// dispositivo crudo (`\\.\...`, `/dev/...`), que no son archivos regulares.
fn prompt_path_or_cancel(prompt: &str, allow_raw_device: bool) -> Result<Option<PathBuf>> {
    println!(
        "{}",
        "  (dejá el campo vacío y presioná Enter para volver)".bright_black()
    );

    let source: String = Input::new()
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;

    let trimmed = source.trim();
    if trimmed.is_empty() {
        println!("  ↩️  Cancelado.");
        return Ok(None);
    }

    let path = PathBuf::from(trimmed);
    let is_raw_device =
        allow_raw_device && (trimmed.starts_with("\\\\.\\") || trimmed.starts_with("/dev/"));

    if !is_raw_device && !path.exists() {
        println!(
            "{}",
            "  ❌ La ruta especificada no existe. Verifica e intenta de nuevo.".bright_red()
        );
        return Ok(None);
    }

    Ok(Some(path))
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

    prompt_path_or_cancel("  📁 Ruta de origen", true)
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
        "  ╔══════════════════════════════════════════════╗".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "         ℹ️  ACERCA DE RECUPEGHOST               "
            .bright_white()
            .bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣".bright_cyan()
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
        "  RecupeGhost busca fotos, videos y audios      ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  borrados que ya no ves en tu PC o USB,        ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  leyendo el disco directamente.                ".white(),
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
        "  Cómo se usa (3 pasos):                        ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  1. Elegí el disco/USB a revisar               ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  2. Elegí qué tipo de archivo buscar           ".white(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  3. Confirmá, esperá, y recuperalos            ".white(),
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
        "  Ojo: los archivos recuperados NO              ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  conservan su nombre original                  ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  (se llaman recovered_0001.ext, etc.)          ".bright_yellow(),
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
        "  Soporta: JPG, PNG, GIF, BMP, WebP, TIFF,     ".bright_green(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  MP4, AVI, MKV, MOV, MP3, WAV, FLAC, OGG,     ".bright_green(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  AAC, M4A, AMR, WMA, OPUS, PDF y más.         ".bright_green(),
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
        "  ╚══════════════════════════════════════════════╝".bright_cyan()
    );
    println!();
}

/// Muestra un mensaje de despedida con opciones de apoyo
pub fn show_goodbye() {
    println!();
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗".bright_cyan()
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
        "  RecupeGhost es 100% gratis y open source.     ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  Si te ayudó a recuperar tus archivos,         ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "  apóyanos viendo nuestros videos.              ".bright_yellow(),
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
        "  ╚══════════════════════════════════════════════╝".bright_cyan()
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
            open_url("https://www.youtube.com/@el_becerril");
        }
        Ok(1) => {
            open_url("https://www.facebook.com/ElBecerril");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn drive_with_mounts(mounts: &[&str]) -> drives::DriveInfo {
        drives::DriveInfo {
            path: "irrelevante".to_string(),
            display_name: "irrelevante".to_string(),
            letter: mounts.first().map(|m| m.to_string()),
            all_mounts: mounts.iter().map(|m| m.to_string()).collect(),
            size_bytes: 0,
            is_removable: false,
        }
    }

    #[test]
    fn test_is_likely_system_disk_windows_c() {
        assert!(is_likely_system_disk(&drive_with_mounts(&["C:"])));
        // Case-insensitive: WMI a veces devuelve la letra en minuscula.
        assert!(is_likely_system_disk(&drive_with_mounts(&["c:"])));
    }

    #[test]
    fn test_is_likely_system_disk_unix_root() {
        assert!(is_likely_system_disk(&drive_with_mounts(&["/"])));
    }

    #[test]
    fn test_is_likely_system_disk_false_for_data_drives() {
        assert!(!is_likely_system_disk(&drive_with_mounts(&["D:"])));
        assert!(!is_likely_system_disk(&drive_with_mounts(&["/home"])));
        assert!(!is_likely_system_disk(&drive_with_mounts(&["/media/usb"])));
        assert!(!is_likely_system_disk(&drive_with_mounts(&[])));
    }

    #[test]
    fn test_is_likely_system_disk_checks_all_mounts_not_just_first() {
        // Un disco con /home como primer mount y / como segundo sigue siendo el disco de
        // sistema (bug #1/#2 de la sesion anterior: antes solo se guardaba el primer mount).
        assert!(is_likely_system_disk(&drive_with_mounts(&["/home", "/"])));
    }

    // --- normalize_to_whole_disk ---

    #[test]
    fn test_normalize_sd_family_partitions_to_whole_disk() {
        // Familias con disco completo puramente alfabético: la partición añade
        // un dígito, que se recorta para quedarnos con el disco.
        assert_eq!(normalize_to_whole_disk("/dev/sdb1"), "/dev/sdb");
        assert_eq!(normalize_to_whole_disk("/dev/sda2"), "/dev/sda");
        assert_eq!(normalize_to_whole_disk("/dev/hda1"), "/dev/hda");
        assert_eq!(normalize_to_whole_disk("/dev/vdb3"), "/dev/vdb");
        assert_eq!(normalize_to_whole_disk("/dev/xvda1"), "/dev/xvda");
    }

    #[test]
    fn test_normalize_sd_family_whole_disk_unchanged() {
        // Un disco completo (sin número de partición) se devuelve tal cual.
        assert_eq!(normalize_to_whole_disk("/dev/sda"), "/dev/sda");
        assert_eq!(normalize_to_whole_disk("/dev/sdb"), "/dev/sdb");
    }

    #[test]
    fn test_normalize_pn_separator_families() {
        // Familias que separan la partición con `pN` (el número forma parte del
        // nombre del disco completo): se corta en la 'p'.
        assert_eq!(normalize_to_whole_disk("/dev/nvme0n1p2"), "/dev/nvme0n1");
        assert_eq!(normalize_to_whole_disk("/dev/mmcblk0p1"), "/dev/mmcblk0");
        assert_eq!(normalize_to_whole_disk("/dev/nbd0p1"), "/dev/nbd0");
    }

    #[test]
    fn test_normalize_digit_terminal_whole_disks_unchanged() {
        // BUG DORMIDO CORREGIDO: estos discos completos terminan en dígito y NO
        // deben normalizarse (antes devolvían /dev/mmcblk, /dev/nbd, etc., que
        // no existen). Sus particiones usan el sufijo `pN`, no un dígito pelado.
        assert_eq!(normalize_to_whole_disk("/dev/mmcblk0"), "/dev/mmcblk0");
        assert_eq!(normalize_to_whole_disk("/dev/nbd0"), "/dev/nbd0");
        assert_eq!(normalize_to_whole_disk("/dev/loop0"), "/dev/loop0");
        assert_eq!(normalize_to_whole_disk("/dev/sr0"), "/dev/sr0");
        assert_eq!(normalize_to_whole_disk("/dev/md0"), "/dev/md0");
        // nvme0n1 es el disco completo; sin sufijo pN no se toca.
        assert_eq!(normalize_to_whole_disk("/dev/nvme0n1"), "/dev/nvme0n1");
    }

    #[test]
    fn test_normalize_macos_disk_partitions() {
        assert_eq!(normalize_to_whole_disk("/dev/disk2s1"), "/dev/disk2");
        assert_eq!(normalize_to_whole_disk("/dev/disk0s2"), "/dev/disk0");
        // Disco completo de macOS sin partición: sin cambios.
        assert_eq!(normalize_to_whole_disk("/dev/disk2"), "/dev/disk2");
    }

    #[test]
    fn test_normalize_non_partition_paths_unchanged() {
        // Rutas de Windows y rutas que no son de dispositivo se devuelven tal cual.
        assert_eq!(
            normalize_to_whole_disk("\\\\.\\PhysicalDrive0"),
            "\\\\.\\PhysicalDrive0"
        );
        assert_eq!(
            normalize_to_whole_disk("/home/usuario/imagen.dd"),
            "/home/usuario/imagen.dd"
        );
    }

    // --- same_device_warning ---

    #[test]
    fn test_same_device_warning_none_for_non_physical_source() {
        // Si el origen no es un dispositivo físico (ej. un archivo de imagen),
        // no hay riesgo de escribir sobre el disco escaneado → sin advertencia.
        use std::path::Path;
        assert!(same_device_warning(
            Path::new("/home/usuario/imagen.dd"),
            Path::new("/home/usuario/recuperados")
        )
        .is_none());
    }
}
