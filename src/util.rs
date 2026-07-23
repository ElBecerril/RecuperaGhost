//! Utilidades compartidas entre módulos (formateo, detección de dispositivos, etc.)

/// Mensajes con personalidad para las esperas largas (escaneo, recuperación, clonado). Voz del
/// fantasma detective de RecupeGhost, en español mexicano. La GUI los rota en vivo (ver
/// [`wait_message_at`]) y el CLI los usa en los preámbulos. El primero es el mensaje ancla del
/// café que venía del CLI.
///
/// OJO (landmine de emojis): solo emojis BASE (☕ 👻 💤), sin selector de variación U+FE0F. La
/// fuente de la GUI (Atkinson) no trae FE0F y egui dibujaría un cuadrito de "glifo faltante". Los
/// que sí lo llevan (⚠️, ⏹️) NO van acá.
pub const WAIT_MESSAGES: &[&str] = &[
    "☕ Estos escaneos son bastante tardados, así que te recomendamos ir por un café o echarte un sueñito en lo que nosotros chambeamos. 👻💤",
    "Voy revisando pedacito por pedacito. Tranquilo, no me apuro con tus recuerdos. 👻",
    "Mientras más grande el disco, más me tardo. Aprovecha y estira las piernas.",
    "Puedes usar la compu normalmente, yo sigo trabajando aquí atrás.",
    "No apagues ni desconectes la memoria mientras trabajo, porfa. 👻",
    "Buscando entre lo que creías perdido… para esto nací. Bueno, morí. 👻",
];

/// Mensaje de espera para `secs` segundos transcurridos, rotando cada 8 s. Determinista, sin RNG:
/// la GUI lo llama cada frame con su reloj y el texto avanza solo. Seguro: `WAIT_MESSAGES` nunca
/// está vacío, así que el módulo siempre devuelve un índice válido.
pub fn wait_message_at(secs: f64) -> &'static str {
    const ROTATE_SECS: f64 = 8.0;
    let secs = if secs.is_finite() && secs >= 0.0 {
        secs
    } else {
        0.0
    };
    let idx = ((secs / ROTATE_SECS) as usize) % WAIT_MESSAGES.len();
    WAIT_MESSAGES[idx]
}

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
///
/// Es una barrera de protección de datos: con permisos elevados, un `File::create` sobre una de
/// estas rutas abriría el disco entero en escritura. Por eso reconoce también las formas
/// alternativas que Windows acepta para el MISMO objeto de dispositivo — `//./PhysicalDrive0`
/// (barras normales, que Win32 normaliza a `\\.\`) y el prefijo verbatim `\\?\` — que antes se
/// colaban por los gates. Ante la duda se prefiere el falso positivo (rechazar una carpeta rara)
/// al falso negativo (dejar pasar un dispositivo), porque el invariante nº1 es no destruir datos.
pub fn is_physical_device(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    // Linux/macOS: dispositivos crudos bajo /dev/ (con barras normales, sin normalizar).
    if s.starts_with("/dev/") {
        return true;
    }
    // Windows: el namespace de dispositivos es `\\.\` (device) y `\\?\` (verbatim, que también
    // alcanza dispositivos). Win32 trata `/` y `\` como equivalentes, así que `//./` y `//?/`
    // abren lo mismo; se unifican las barras antes de comparar para no dejar pasar la variante
    // con `/`.
    let win = s.replace('/', "\\");
    win.starts_with("\\\\.\\") || win.starts_with("\\\\?\\")
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
                    "  🔒 No tienes permisos suficientes para acceder a ese disco o archivo. \
Si es un disco físico, ejecuta el programa como Administrador (Windows) o con sudo (Linux/macOS).",
                ),
                std::io::ErrorKind::NotFound => Some(
                    "  🔍 No se encontró la ruta indicada. Verifica que el disco/USB siga conectado \
y que la ruta esté bien escrita.",
                ),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted => Some(
                    "  ⏱️  El dispositivo tardó demasiado en responder. Puede estar desconectado \
o dañado — prueba reconectarlo.",
                ),
                _ => None,
            };
        }
    }
    None
}

/// Evita que un proceso hijo de consola abra una ventana negra visible.
///
/// Hace falta desde que la GUI se compila con `windows_subsystem = "windows"`: un proceso de
/// subsistema gráfico que lanza un hijo de subsistema consola hace que Windows le asigne una
/// consola NUEVA y visible. Antes no se notaba porque el `.exe` de consola le prestaba la suya.
///
/// Sin esto, al abrir la GUI parpadean dos rectángulos negros (`powershell` para listar discos),
/// se repiten en cada "Buscar discos de nuevo", y otra vez dentro de `same_device_warning` — o
/// sea, justo en el instante previo a la advertencia crítica de pérdida de datos. Es exactamente
/// el efecto que ocultar la consola venía a eliminar.
///
/// En Linux y macOS es un no-op.
pub fn sin_ventana(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW` de la API de Windows.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_is_physical_device_detects_raw_devices() {
        // Linux / macOS.
        assert!(is_physical_device(Path::new("/dev/sda")));
        assert!(is_physical_device(Path::new("/dev/nvme0n1")));
        assert!(is_physical_device(Path::new("/dev/disk2")));
        // Windows, forma canónica.
        assert!(is_physical_device(Path::new("\\\\.\\PhysicalDrive0")));
        assert!(is_physical_device(Path::new("\\\\.\\E:")));
    }

    #[test]
    fn test_is_physical_device_detects_windows_alt_prefixes() {
        // Regresión (auditoría pre-beta): Windows abre el MISMO dispositivo con barras normales
        // (`//./`, que Win32 normaliza a `\\.\`) y con el prefijo verbatim `\\?\`. Antes se
        // colaban por los gates de "el destino no puede ser un disco".
        assert!(
            is_physical_device(Path::new("//./PhysicalDrive0")),
            "//./ es equivalente a \\\\.\\"
        );
        assert!(
            is_physical_device(Path::new("\\\\?\\PhysicalDrive0")),
            "el prefijo verbatim también alcanza dispositivos"
        );
        assert!(is_physical_device(Path::new("//?/PhysicalDrive0")));
    }

    #[test]
    fn test_is_physical_device_false_for_normal_folders() {
        assert!(!is_physical_device(Path::new("/home/usuario/Recuperados")));
        assert!(!is_physical_device(Path::new("D:\\Recuperados")));
        assert!(!is_physical_device(Path::new(
            "RecupeGhost_20260101_000000"
        )));
        assert!(!is_physical_device(Path::new("copia.img")));
        // No empieza con el prefijo: una carpeta que casualmente contiene "dev" no es un
        // dispositivo.
        assert!(!is_physical_device(Path::new("/home/dev/fotos")));
    }
}
