//! Utilidades compartidas entre módulos (formateo, detección de dispositivos, etc.)

/// Formatea un tamaño en bytes de forma legible (ej: `14.5 GB`, `488.3 KB`, `120 B`).
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * KB;
    const GB: f64 = 1024.0 * MB;
    const TB: f64 = 1024.0 * GB;

    let b = bytes as f64;
    if b >= TB {
        format!("{:.1} TB", b / TB)
    } else if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Determina si una ruta apunta a un dispositivo físico crudo
/// (ej: `\\.\PhysicalDrive1` en Windows, `/dev/sdb` en Linux/macOS).
pub fn is_physical_device(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.starts_with("\\\\.\\") || s.starts_with("/dev/")
}

/// Resuelve una carpeta de salida a ruta absoluta (relativa al directorio de trabajo actual)
/// sin requerir que exista todavía. Alguien sin conocimiento técnico no sabe "relativo a qué"
/// es una carpeta como `RecupeGhost_20260217_143022`; con la ruta completa mostrada en el
/// resumen y al terminar puede encontrarla a simple vista en el explorador de archivos. Se usa
/// en el flujo interactivo y en el batch para que ambos muestren lo mismo. Si ya es absoluta se
/// devuelve tal cual; si no se puede determinar el directorio actual, se devuelve sin cambios.
pub fn to_absolute_output(dir: std::path::PathBuf) -> std::path::PathBuf {
    if dir.is_absolute() {
        dir
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&dir))
            .unwrap_or(dir)
    }
}

/// Busca un `std::io::Error` en la cadena de causas del error y devuelve una traducción
/// amigable en español para los casos más comunes con los que alguien sin conocimiento técnico
/// puede toparse (permisos, dispositivo desconectado). El mensaje técnico original se sigue
/// mostrando abajo como "Causa:" para quien lo necesite; esto es un resumen en criollo antes.
///
/// Vive en `util` (y no en el binario del CLI) porque la GUI necesita lo mismo: sin esto, su
/// pantalla de error mostraba el texto crudo del sistema operativo — para el público de esta
/// herramienta, un "Acceso denegado. (os error 5)" es el final del intento, cuando la solución
/// era abrir el programa como administrador. Los textos arrancan con sangría para el CLI; quien
/// los muestre en otro contexto que use `trim_start()`.
pub fn friendly_error_hint(e: &anyhow::Error) -> Option<&'static str> {
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
