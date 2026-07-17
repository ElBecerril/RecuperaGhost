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
