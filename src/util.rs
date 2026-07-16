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
