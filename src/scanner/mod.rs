use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use crate::signatures::{FileCategory, FileSignature};
use crate::util::format_size;

// ─── Cancelación cooperativa del escaneo (Ctrl+C) ───
//
// `SCAN_IN_PROGRESS` lo setea `scan_source_impl` mientras hay un escaneo corriendo, para que el
// handler de Ctrl+C (instalado una vez en `main`) distinga "cancelar el escaneo en curso" de
// "cerrar el programa" (comportamiento normal de Ctrl+C cuando el usuario está en un menú).
//
// `SCAN_CANCEL_REQUESTED` lo setea el handler; el loop de lectura de `scan_segment` lo chequea
// una vez por bloque (1 MB) y, si está seteado, corta conservando todo lo encontrado hasta ese
// punto — exactamente el mismo comportamiento "parar y conservar lo parcial" que ya se usa para
// sectores dañados. Es cancelación COOPERATIVA: no interrumpe un `read()` ya bloqueado en el
// kernel (ej. un dispositivo que se cayó), solo evita empezar el siguiente bloque.
static SCAN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static SCAN_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

// Bytes leídos del origen en el escaneo en curso. Es la MISMA cuenta que alimenta la barra de
// progreso de la terminal, expuesta como global para que la GUI (que corre el escaneo en un hilo
// aparte y no tiene la ProgressBar de indicatif) pueda leer el avance y dibujar su propia barra.
//
// OJO: este global es un ESPEJO de solo lectura para la UI, NO la fuente de verdad. La cuenta
// real vive en un `Arc<AtomicU64>` por escaneo (ver `ScanProgress`), porque un global se corrompe
// si dos escaneos corren a la vez en el mismo proceso (pasa en los tests, que corren en paralelo)
// y `ScanResult::bytes_scanned` tiene que ser el dato de SU escaneo, no el del vecino. Y por la
// misma razón nada puede depender de este global para TERMINAR (ver el comentario del monitor).
static SCAN_PROGRESS_BYTES: AtomicU64 = AtomicU64::new(0);

// Archivos encontrados hasta ahora en el escaneo en curso. Mismo rol y mismas advertencias que
// `SCAN_PROGRESS_BYTES`: espejo para que la GUI muestre "Encontrados hasta ahora: N" en vivo,
// mientras la cuenta de verdad va por el contador por escaneo.
static SCAN_PROGRESS_FILES: AtomicU64 = AtomicU64::new(0);

/// True si hay un escaneo corriendo ahora mismo. Lo usa el handler de Ctrl+C para decidir entre
/// cancelar el escaneo o dejar que el programa termine normalmente.
pub fn is_scan_in_progress() -> bool {
    SCAN_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Bytes del origen ya escaneados en el escaneo en curso (0 si no hay ninguno). Lo usa la GUI para
/// dibujar su barra de progreso mientras el escaneo corre en un hilo de fondo.
pub fn scan_progress_bytes() -> u64 {
    SCAN_PROGRESS_BYTES.load(Ordering::Relaxed)
}

/// Archivos encontrados hasta ahora en el escaneo en curso (0 si no hay ninguno). Lo usa la GUI
/// para mostrar "Encontrados hasta ahora: N" mientras el escaneo corre en un hilo de fondo, así el
/// usuario ve que algo está apareciendo y no solo una barra avanzando.
///
/// Es un valor EN VIVO y aproximado: cuenta los hallazgos a medida que aparecen, antes del segundo
/// pase de footers y del dedup final. Al terminar el escaneo queda igualado al total exacto, pero
/// el número definitivo es `ScanResult::found_files.len()`.
pub fn scan_progress_files() -> u64 {
    SCAN_PROGRESS_FILES.load(Ordering::Relaxed)
}

/// Pide cancelar el escaneo en curso (lo llama el handler de Ctrl+C). El escaneo se detiene en
/// el próximo bloque y devuelve lo encontrado hasta el momento con `ScanResult::cancelled`.
pub fn request_cancel() {
    SCAN_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Si ya se pidió cancelar el escaneo en curso.
///
/// La GUI la usa para pasar el botón a "Deteniendo…": la cancelación es cooperativa y puede tardar
/// (el bloque en curso se termina de leer), así que sin esta señal el botón parece no haber hecho
/// nada y la persona lo aprieta de nuevo creyendo que se colgó.
pub fn cancel_requested() -> bool {
    is_cancel_requested()
}

fn is_cancel_requested() -> bool {
    SCAN_CANCEL_REQUESTED.load(Ordering::SeqCst)
}

/// Archivo encontrado durante el escaneo
#[derive(Debug, Clone)]
pub struct FoundFile {
    pub signature: FileSignature,
    pub offset: u64,
    pub size: u64,
    pub index: usize,
    /// true si el tamaño real se determinó encontrando el footer (o el campo de tamaño del
    /// header, para formatos como BMP); false si se quedó en `max_size` a falta de footer.
    /// Usado por `refine_footers` (A2) para saber a qué archivos vale la pena reintentarles
    /// una búsqueda de footer más profunda, fuera del buffer/chunk original.
    pub footer_found: bool,
    /// True cuando NO se pudo determinar el final del archivo por una limitación externa —error de
    /// I/O, cancelación, tamaño mayor al máximo de la firma, o una caja que declara "hasta el fin
    /// del archivo"— y no porque se haya comprobado que el archivo no cierra.
    ///
    /// La diferencia decide si el archivo se guarda: "confirmé que esto no cierra" es un probable
    /// falso positivo (dudoso, no se guarda por defecto), pero "no pude saberlo" es exactamente lo
    /// que pasa con un archivo REAL en un disco que está fallando — y ese tiene que seguir
    /// guardándose, como antes. Sin esta distinción, un MP3 al que le faltaban 200 bytes al final
    /// pasaba de recuperarse a perderse entero (lo encontró una revisión adversarial).
    pub end_unknown: bool,
}

impl std::fmt::Display for FoundFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} | {} | offset: 0x{:X} | {}",
            self.signature.category,
            self.signature,
            self.offset,
            format_size(self.size)
        )
    }
}

impl FoundFile {
    /// Nombre de archivo con el que se va a guardar al recuperarlo (ver
    /// `recovery::recover_files`, que usa este mismo formato para nombrar el archivo real en
    /// disco). Centralizado acá para que la lista de "archivos encontrados" que ve el usuario
    /// (antes de recuperar) muestre el mismo nombre que después va a encontrar en la carpeta
    /// de salida, en vez de un offset hexadecimal sin usar `unwrap()`/expect() que no le sirve
    /// de nada a alguien sin conocimiento técnico.
    pub fn recovered_filename(&self) -> String {
        format!("recovered_{:04}.{}", self.index, self.signature.extension)
    }

    /// Estado de integridad best-effort del archivo carveado (ver `Integrity`). Se apoya en la
    /// señal que el escaneo ya calculó (`footer_found`) más las capacidades de la firma; NO hace
    /// I/O extra. La idea es avisarle al usuario cuáles resultados son confiables y cuáles pueden
    /// estar dañados/incompletos, SIN ocultar ninguno (puede recuperarlos todos igual).
    pub fn integrity(&self) -> Integrity {
        // Un formato tiene "final detectable" si define un footer, si codifica su tamaño en el
        // header (ej. BMP), o si es un OOXML/ZIP (el fin se saca del EOCD, ver `zip_ooxml_size`). En
        // esos casos `footer_found` nos dice si dimos con el final real.
        let end_detectable = self.signature.footer.is_some()
            || self.signature.size_from_header.is_some()
            || signature_is_zip_ooxml(&self.signature)
            // Audio por frames (MP3/AAC): el final sale de recorrer la cadena de frames. Que cuente
            // como "final detectable" es lo que hace que un candidato cuya cadena NO cerró salga
            // marcado "posiblemente dañado" (y por lo tanto no se guarde por defecto) en vez de
            // "no verificable", que sí se guardaba.
            || self.signature.stream_end().is_some();
        if end_detectable {
            // `end_unknown` gana sobre `footer_found`: puede saberse hasta dónde llega un archivo y
            // aun así no poder afirmar que esté completo (al que le falta el final, por ejemplo).
            // Afirmar "íntegro" ahí sería mentirle al usuario sobre un archivo al que le falta un
            // pedazo.
            if self.footer_found && !self.end_unknown {
                Integrity::Intact
            } else if self.end_unknown {
                // No se pudo determinar el final por una limitación externa (I/O, cancelación,
                // tamaño). No se afirma ni se niega: se guarda igual, como antes de que estos
                // formatos tuvieran cálculo de tamaño.
                Integrity::Unverifiable
            } else {
                // El formato TIENE un final detectable pero no lo encontramos: el archivo quedó
                // truncado a max_size. Probable falso positivo o archivo incompleto/dañado.
                Integrity::Suspect
            }
        } else {
            // Sin footer ni tamaño en header: no hay forma barata de verificar el final. No lo
            // afirmamos ni lo negamos.
            Integrity::Unverifiable
        }
    }

    /// Línea de resumen pensada para el usuario final (sin offsets ni jerga técnica): muestra
    /// una marca de integridad, el nombre con el que va a quedar guardado y el tamaño.
    pub fn friendly_summary(&self) -> String {
        let (mark, suffix) = match self.integrity() {
            Integrity::Intact => ("✅ ", ""),
            Integrity::Suspect => ("⚠️  ", "  (posiblemente dañado)"),
            Integrity::Unverifiable => ("   ", ""),
        };
        format!(
            "{}{} {} — {}{}",
            mark,
            self.signature.category,
            self.recovered_filename(),
            format_size(self.size),
            suffix
        )
    }
}

/// Estado de integridad best-effort de un archivo recuperado por carving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// Se encontró el final real del archivo (footer o tamaño del header): estructuralmente
    /// completo, alta confianza.
    Intact,
    /// El formato tiene un final detectable pero no se encontró: quedó truncado a `max_size`
    /// (probable falso positivo, o archivo real incompleto/dañado).
    Suspect,
    /// El formato no tiene una forma barata de verificar el final; no lo podemos afirmar ni negar.
    Unverifiable,
}

impl Integrity {
    /// Orden de presentación: primero los íntegros, después los no verificables, al final los
    /// dudosos (para que el usuario vea arriba lo más confiable y abajo lo que puede estar mal).
    pub fn display_rank(self) -> u8 {
        match self {
            Integrity::Intact => 0,
            Integrity::Unverifiable => 1,
            Integrity::Suspect => 2,
        }
    }
}

/// Resultado completo del escaneo
pub struct ScanResult {
    pub found_files: Vec<FoundFile>,
    pub bytes_scanned: u64,
    pub photos_count: usize,
    pub videos_count: usize,
    pub audios_count: usize,
    pub documents_count: usize,
    /// (B1) true si el escaneo encontró errores de I/O leyendo el origen (sectores dañados,
    /// dispositivo que se cayó a media lectura, etc.) y por lo tanto el resultado puede estar
    /// incompleto — pero `found_files` igual contiene todo lo que se logró encontrar en las
    /// partes del origen que sí se pudieron leer, en vez de haberse descartado por completo.
    pub had_errors: bool,
    /// true si el usuario canceló el escaneo con Ctrl+C antes de terminar. Igual que con
    /// `had_errors`, `found_files` conserva todo lo hallado hasta el momento de cancelar.
    pub cancelled: bool,
}

impl ScanResult {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "📊 Resumen: {} archivos encontrados\n   📷 Fotos: {}  |  🎬 Videos: {}  |  🎵 Audios: {}  |  📄 Documentos: {}\n   💾 Bytes escaneados: {}",
            self.found_files.len(),
            self.photos_count,
            self.videos_count,
            self.audios_count,
            self.documents_count,
            format_size(self.bytes_scanned),
        );
        if self.cancelled {
            s.push_str(
                "\n   ⏹️  Cancelaste el escaneo antes de terminar — abajo está solo lo que se\n       alcanzó a encontrar hasta ese punto. Puedes recuperarlo igual.",
            );
        }
        if self.had_errors {
            s.push_str(
                "\n   ⚠️  El escaneo tuvo errores de I/O leyendo el origen (sectores dañados u\n       otro fallo de lectura) — el resultado es parcial: puede faltar contenido de\n       las zonas que no se pudieron leer.",
            );
        }
        s
    }
}

/// Tamaño de un origen (disco físico o archivo) abriéndolo, reusando la misma lógica que el
/// escaneo (IOCTL en discos físicos de Windows, `seek(End)` en el resto). Lo usa el módulo de
/// clonado para saber cuántos bytes copiar.
pub fn device_or_file_size(source_path: &Path) -> Result<u64> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;
    get_source_size(&mut file, source_path)
}

/// Obtiene el tamaño de la fuente (archivo o disco físico).
/// En discos físicos de Windows, `seek(End)` no funciona; se usa `IOCTL_DISK_GET_LENGTH_INFO`.
fn get_source_size(file: &mut File, source_path: &Path) -> Result<u64> {
    // Discos físicos crudos de Windows (\\.\PhysicalDriveN): NO confiar en seek(End) como
    // definitivo. El propio caso de uso de este fallback es que seek puede "tener éxito" y
    // devolver un tamaño > 0 pero incorrecto, derivado de metadata de partición en vez del
    // tamaño real del medio — por eso antes NO alcanzaba con solo probar el IOCTL cuando
    // seek daba 0; hay que preferir SIEMPRE el IOCTL en este caso y usar seek solo si el
    // IOCTL falla.
    #[cfg(target_os = "windows")]
    {
        let src = source_path.to_string_lossy();
        if src.starts_with("\\\\.\\") {
            if let Ok(size) = get_disk_size_windows(file) {
                return Ok(size);
            }
            // IOCTL falló: fallback a seek(End) como último recurso.
            if let Some(size) = file.seek(SeekFrom::End(0)).ok().filter(|&s| s > 0) {
                return Ok(size);
            }
            anyhow::bail!(
                "No se pudo determinar el tamaño del disco físico (IOCTL_DISK_GET_LENGTH_INFO y seek fallaron)"
            );
        }
    }

    // Camino normal: archivos regulares, o no-Windows (seek(End) es confiable ahí).
    let seek_result = file.seek(SeekFrom::End(0)).ok();
    if let Some(size) = seek_result {
        if size > 0 {
            return Ok(size);
        }
    }

    let _ = source_path; // evitar warning en no-windows
    if seek_result == Some(0) {
        // B2: distinguir un origen vacío (0 bytes) de un fallo genérico al determinar el
        // tamaño — antes ambos casos caían en el mismo mensaje genérico. El comentario viejo
        // ("leer hasta EOF contando bytes") no reflejaba código real: nunca se implementó ese
        // último intento, así que se elimina en vez de dejarlo como promesa falsa.
        anyhow::bail!("El origen está vacío, no hay nada que escanear");
    }
    anyhow::bail!("No se pudo determinar el tamaño del origen")
}

/// Obtiene el tamaño de un disco físico en Windows usando IOCTL_DISK_GET_LENGTH_INFO.
#[cfg(target_os = "windows")]
fn get_disk_size_windows(file: &mut File) -> Result<u64> {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn DeviceIoControl(
            hDevice: isize,
            dwIoControlCode: u32,
            lpInBuffer: *const u8,
            nInBufferSize: u32,
            lpOutBuffer: *mut u8,
            nOutBufferSize: u32,
            lpBytesReturned: *mut u32,
            lpOverlapped: *mut u8,
        ) -> i32;
    }

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007405C;

    let handle = file.as_raw_handle() as isize;
    let mut disk_length: u64 = 0;
    let mut bytes_returned: u32 = 0;

    let result = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            &mut disk_length as *mut u64 as *mut u8,
            std::mem::size_of::<u64>() as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };

    if result != 0 && disk_length > 0 {
        Ok(disk_length)
    } else {
        anyhow::bail!("IOCTL_DISK_GET_LENGTH_INFO falló")
    }
}

/// Tamaño del buffer de lectura (1 MB)
const BUFFER_SIZE: usize = 1024 * 1024;

/// Segmento de datos asignado a un hilo de escaneo.
/// Cada segmento tiene una zona de lectura [start, end) que incluye overlap
/// y una zona exclusiva [claim_start, claim_end) donde solo este hilo reporta hallazgos.
struct Segment {
    start: u64,       // Inicio de lectura (incluye overlap anterior)
    end: u64,         // Fin de lectura (incluye overlap posterior)
    claim_start: u64, // Inicio de zona exclusiva de este hilo
    claim_end: u64,   // Fin de zona exclusiva de este hilo
}

/// Divide el archivo en segmentos para escaneo paralelo.
/// Las zonas exclusivas (claim) cubren todo el archivo sin gaps ni solapamiento.
/// Las zonas de lectura se extienden con overlap para detectar firmas en fronteras.
fn calculate_segments(file_size: u64, num_threads: usize, overlap_size: u64) -> Vec<Segment> {
    debug_assert!(num_threads >= 1, "num_threads debe ser >= 1");
    debug_assert!(
        num_threads == 1 || file_size >= 512 * num_threads as u64,
        "file_size ({}) demasiado pequeño para {} hilos (mínimo {})",
        file_size,
        num_threads,
        512 * num_threads as u64
    );

    let align = 512u64;
    let chunk_size = file_size / num_threads as u64;

    let mut segments = Vec::with_capacity(num_threads);
    for i in 0..num_threads {
        let claim_start = if i == 0 {
            0
        } else {
            (i as u64 * chunk_size / align) * align
        };
        let claim_end = if i == num_threads - 1 {
            file_size
        } else {
            ((i as u64 + 1) * chunk_size / align) * align
        };

        let start = claim_start.saturating_sub(overlap_size);
        let end = std::cmp::min(claim_end + overlap_size, file_size);

        segments.push(Segment {
            start,
            end,
            claim_start,
            claim_end,
        });
    }
    segments
}

/// Determina cuántos hilos usar para el escaneo.
/// - Dispositivos físicos: siempre 1 (I/O secuencial es óptimo)
/// - Archivos: min(CPU cores, 8, file_size / 16MB), mínimo 1
fn select_thread_count(source_path: &Path, file_size: u64) -> usize {
    if crate::util::is_physical_device(source_path) {
        return 1;
    }

    const MIN_SIZE_PER_THREAD: u64 = 16 * 1024 * 1024; // 16 MB
    if file_size < MIN_SIZE_PER_THREAD {
        return 1;
    }

    let cpu_cores = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let by_size = (file_size / MIN_SIZE_PER_THREAD) as usize;

    std::cmp::max(1, std::cmp::min(cpu_cores, std::cmp::min(8, by_size)))
}

/// Precalcula el máximo alcance de verificación de firmas.
/// Se usa para determinar el overlap necesario entre chunks y entre segmentos.
fn max_signature_reach(signatures: &[FileSignature]) -> usize {
    signatures
        .iter()
        .map(|s| {
            let header_end = s.header_offset + s.header.len();
            let extra_end = s
                .extra_check
                .map(|(bytes, offset)| offset + bytes.len())
                .unwrap_or(0);
            let validator_end = s.validator.map(|(_, needed)| needed).unwrap_or(0);
            let size_field_end = s
                .size_from_header
                .map(|(offset, len)| offset + len)
                .unwrap_or(0);
            std::cmp::max(
                std::cmp::max(header_end, extra_end),
                std::cmp::max(validator_end, size_field_end),
            )
        })
        .max()
        .unwrap_or(16)
}

/// Resultado de escanear un segmento: los archivos hallados, y si hubo algún error de I/O
/// leyendo el origen (disco con sectores dañados, etc.) que impidió leer una parte del
/// segmento — en cuyo caso `found_files` igual contiene todo lo que se logró encontrar ANTES
/// (y, si se pudo saltar el bloque dañado, DESPUÉS) del error.
struct SegmentResult {
    found_files: Vec<FoundFile>,
    had_errors: bool,
}

/// Contadores de avance de UN escaneo, que `scan_segment` va alimentando.
///
/// `bytes` y `files` son propios del escaneo (un `Arc<AtomicU64>` que comparten sus workers) y son
/// la FUENTE DE VERDAD: de ahí sale `ScanResult::bytes_scanned`. `mirror_*` son los globales que
/// lee la UI, y se actualizan como copia para que la GUI pueda dibujar el avance sin tener acceso
/// a los contadores internos.
///
/// La separación existe porque el global se pisa entre escaneos concurrentes del mismo proceso
/// (los tests corren en paralelo): leer `bytes_scanned` del global daba un número del escaneo de
/// al lado. Se pasa por parámetro —igual que el flag de cancelación— para que un test pueda usar
/// sus propios contadores sin tocar el estado global compartido.
struct ScanProgress<'a> {
    bytes: &'a AtomicU64,
    files: &'a AtomicU64,
    mirror_bytes: Option<&'a AtomicU64>,
    mirror_files: Option<&'a AtomicU64>,
    /// Sin salida por terminal. Viaja acá y no como un argumento suelto porque `scan_segment` ya
    /// tiene la lista de argumentos al límite, y porque es por escaneo: nada de globales.
    ///
    /// Que los avisos de error de I/O lo respeten es CRÍTICO: la GUI se compila con
    /// `windows_subsystem = "windows"`, o sea sin consola. Ahí un `eprintln!` no tiene a dónde ir
    /// y, si el handle existe pero la escritura falla, `std` paniquea. Y justo esos avisos salen
    /// en el escenario CENTRAL de la herramienta: un disco que está fallando.
    quiet: bool,
}

impl ScanProgress<'_> {
    /// Suma bytes leídos y devuelve el total del escaneo (el que va a la barra de progreso).
    fn add_bytes(&self, n: u64) -> u64 {
        let total = self.bytes.fetch_add(n, Ordering::Relaxed) + n;
        if let Some(m) = self.mirror_bytes {
            // `fetch_max`, no `store`: con varios hilos, dos pueden invertir el orden entre su
            // `fetch_add` y su escritura al espejo, y el espejo RETROCEDERÍA. Una barra de
            // progreso que va para atrás, a alguien no técnico y asustado, se le lee como que el
            // programa se rompió.
            m.fetch_max(total, Ordering::Relaxed);
        }
        total
    }

    /// Suma archivos recién encontrados (para el contador en vivo de la GUI).
    fn add_files(&self, n: u64) {
        let total = self.files.fetch_add(n, Ordering::Relaxed) + n;
        if let Some(m) = self.mirror_files {
            // Mismo motivo que en `add_bytes`: el contador de "encontrados hasta ahora" nunca
            // puede bajar delante del usuario.
            m.fetch_max(total, Ordering::Relaxed);
        }
    }
}

/// Cuántos de estos hallazgos caen en la zona exclusiva del segmento. Solo esos van a sobrevivir
/// al filtro final, así que son los únicos que se cuentan en vivo: contar también los de la zona
/// de overlap haría que el contador de la GUI se pasara del total real y después "bajara".
fn count_claimed(files: &[FoundFile], segment: &Segment) -> u64 {
    files
        .iter()
        .filter(|f| f.offset >= segment.claim_start && f.offset < segment.claim_end)
        .count() as u64
}

/// Escanea un segmento del archivo buscando firmas multimedia.
/// Cada hilo abre su propio File handle y escanea secuencialmente dentro del segmento.
/// Solo retiene resultados con offset en [claim_start, claim_end).
///
/// `cancel`: flag de cancelación cooperativa (Ctrl+C). Se chequea una vez por bloque; si está
/// seteado, el escaneo del segmento corta conservando lo encontrado hasta ese punto. Se recibe
/// por parámetro (en producción es `&SCAN_CANCEL_REQUESTED`, el flag global que setea el handler
/// de Ctrl+C) en vez de leer el global directamente, para que los tests puedan pasar su propio
/// flag sin interferir con el estado global compartido entre tests que corren en paralelo.
///
/// Limitación conocida (M7, no resuelta aquí): la cancelación es COOPERATIVA — no interrumpe un
/// `file.read()` ya bloqueado en el kernel. Si el dispositivo de origen deja de responder (ej.
/// un USB que se cae a media lectura), ese read puede bloquear indefinidamente y el chequeo de
/// `cancel` no llega a ejecutarse hasta el siguiente bloque. Cancelar un escaneo que progresa
/// (el caso común) sí funciona; interrumpir un dispositivo colgado requeriría timeouts de I/O.
///
/// (B1) A propósito esta función NUNCA devuelve `Err`: un solo sector dañado (I/O error) en
/// cualquier punto del origen es el escenario CENTRAL de uso de esta herramienta (discos
/// fallando), y antes un solo error acá se propagaba con `?` hacia el caller, descartando en
/// el camino de 1 hilo TODO lo encontrado hasta ese punto, y en el camino multi-hilo el
/// resultado de los OTROS hilos que sí terminaron bien. Ahora los errores de lectura del
/// origen se tratan como "saltar y seguir" en vez de "abortar todo", y se reportan vía
/// `SegmentResult::had_errors` en vez de con `Result::Err`.
#[allow(clippy::too_many_arguments)]
fn scan_segment(
    source_path: &Path,
    segment: &Segment,
    signatures: &[FileSignature],
    source_size: u64,
    max_header_len: usize,
    progress: &ScanProgress<'_>,
    inline_pb: Option<&ProgressBar>,
    cancel: &AtomicBool,
) -> SegmentResult {
    let mut file = match File::open(source_path) {
        Ok(f) => f,
        Err(e) => {
            if !progress.quiet {
                eprintln!(
                    "  ⚠️  No se pudo abrir {} para escanear [0x{:X}, 0x{:X}): {} — este segmento se omite",
                    source_path.display(),
                    segment.start,
                    segment.end,
                    e
                );
            }
            return SegmentResult {
                found_files: Vec::new(),
                had_errors: true,
            };
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(segment.start)) {
        if !progress.quiet {
            eprintln!(
                "  ⚠️  No se pudo posicionar en 0x{:X}: {} — este segmento se omite",
                segment.start, e
            );
        }
        return SegmentResult {
            found_files: Vec::new(),
            had_errors: true,
        };
    }

    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut overlap: Vec<u8> = Vec::new();
    let mut found_files: Vec<FoundFile> = Vec::new();
    let mut position = segment.start;
    let mut file_index: usize = 0;
    let mut had_errors = false;
    // Claves (offset absoluto, extensión) ya registradas en este segmento, para deduplicar
    // hits repetidos por overlap en O(1) en vez de un scan O(n) de found_files (ver C2).
    let mut seen: HashSet<(u64, &'static str)> = HashSet::new();

    loop {
        if position >= segment.end {
            break;
        }

        // Cancelación cooperativa: si el usuario apretó Ctrl+C, cortar acá conservando todo lo
        // encontrado hasta este bloque (mismo patrón "parar y conservar" que los errores de
        // I/O de más abajo). Se chequea una vez por bloque de 1 MB → respuesta en ~decenas de ms.
        if cancel.load(Ordering::SeqCst) {
            break;
        }

        let max_to_read = std::cmp::min(BUFFER_SIZE as u64, segment.end - position) as usize;

        let bytes_read = match file.read(&mut buffer[..max_to_read]) {
            Ok(n) => n,
            Err(e) => {
                // (B1) No propagar: un sector dañado no debe tirar lo ya encontrado. Se
                // intenta saltar este bloque (avanzar `position` y reposicionar el file
                // handle después de él) y seguir escaneando el resto del segmento. El
                // `overlap` de antes del error ya no es válido (hay un hueco sin leer), así
                // que se descarta para no combinar bytes no contiguos.
                if !progress.quiet {
                    eprintln!(
                        "  ⚠️  Error de I/O leyendo en offset 0x{:X}: {} — saltando este bloque y continuando",
                        position, e
                    );
                }
                had_errors = true;
                overlap.clear();
                let next_position = position + max_to_read as u64;
                let total = progress.add_bytes(max_to_read as u64);
                if let Some(pb) = inline_pb {
                    pb.set_position(total);
                }
                if next_position >= segment.end {
                    break;
                }
                match file.seek(SeekFrom::Start(next_position)) {
                    Ok(_) => {
                        position = next_position;
                        continue;
                    }
                    Err(seek_err) => {
                        if !progress.quiet {
                            eprintln!(
                                "  ⚠️  No se pudo reposicionar tras error de I/O: {} — abandonando el resto de este segmento",
                                seek_err
                            );
                        }
                        break;
                    }
                }
            }
        };
        if bytes_read == 0 {
            break;
        }

        // Buscar firmas: con overlap del chunk anterior si existe, o solo el buffer actual.
        // Se anota cuántos hallazgos había antes para poder sumar al contador en vivo solo los
        // nuevos (el vector es acumulativo dentro del segmento).
        let found_before = found_files.len();
        if !overlap.is_empty() {
            let mut combined = overlap.clone();
            combined.extend_from_slice(&buffer[..bytes_read]);
            check_signatures_in_buffer(
                &combined,
                position - overlap.len() as u64,
                signatures,
                &mut found_files,
                &mut file_index,
                source_size,
                &mut seen,
            );
            overlap.clear();
        } else {
            check_signatures_in_buffer(
                &buffer[..bytes_read],
                position,
                signatures,
                &mut found_files,
                &mut file_index,
                source_size,
                &mut seen,
            );
        }

        let newly_claimed = count_claimed(&found_files[found_before..], segment);
        if newly_claimed > 0 {
            progress.add_files(newly_claimed);
        }

        // Guardar overlap para el siguiente chunk (siempre, incluso con reads parciales)
        if bytes_read >= max_header_len {
            overlap = buffer[bytes_read - max_header_len..bytes_read].to_vec();
        } else if bytes_read > 0 {
            overlap = buffer[..bytes_read].to_vec();
        }

        position += bytes_read as u64;
        let total = progress.add_bytes(bytes_read as u64);
        if let Some(pb) = inline_pb {
            pb.set_position(total);
        }
    }

    // Filtrar: solo retener archivos en la zona exclusiva de este segmento
    found_files.retain(|f| f.offset >= segment.claim_start && f.offset < segment.claim_end);

    SegmentResult {
        found_files,
        had_errors,
    }
}

/// Escanea un archivo/dispositivo buscando firmas de archivos multimedia
pub fn scan_source(source_path: &Path, signatures: &[FileSignature]) -> Result<ScanResult> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;
    let file_size = get_source_size(&mut file, source_path)
        .with_context(|| "No se pudo obtener el tamaño del origen")?;
    drop(file);

    let num_threads = select_thread_count(source_path, file_size);
    scan_source_impl(source_path, signatures, file_size, num_threads, false)
}

/// Igual que `scan_source`, pero SIN salida por terminal (ni `println!` ni barra `indicatif`).
/// Pensada para la GUI: un binario de subsistema gráfico en Windows no tiene consola, así que un
/// `println!` paniquearía. El avance se sigue por `scan_progress_bytes()` (y el total con
/// `device_or_file_size`), y la cancelación por `request_cancel()`, igual que en el CLI.
pub fn scan_source_quiet(source_path: &Path, signatures: &[FileSignature]) -> Result<ScanResult> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;
    let file_size = get_source_size(&mut file, source_path)
        .with_context(|| "No se pudo obtener el tamaño del origen")?;
    drop(file);

    let num_threads = select_thread_count(source_path, file_size);
    scan_source_impl(source_path, signatures, file_size, num_threads, true)
}

/// Variante interna para testing: permite forzar un número específico de hilos.
#[cfg(test)]
fn scan_source_with_threads(
    source_path: &Path,
    signatures: &[FileSignature],
    forced_threads: usize,
) -> Result<ScanResult> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;
    let file_size = get_source_size(&mut file, source_path)
        .with_context(|| "No se pudo obtener el tamaño del origen")?;
    drop(file);

    scan_source_impl(
        source_path,
        signatures,
        file_size,
        forced_threads.max(1),
        false,
    )
}

/// Preámbulo de terminal del escaneo (encabezado, tamaño, estimación de tiempo y aviso de Ctrl+C).
/// Extraído para poder saltearlo por completo en modo GUI (`quiet`), donde no hay consola.
fn print_scan_preamble(source_path: &Path, file_size: u64, num_threads: usize) {
    println!("  🔎 Escaneando: {}", source_path.display());
    println!("  📏 Tamaño: {}", format_size(file_size));

    // Estimar tiempo con velocidad ajustada por hilos
    let is_device = crate::util::is_physical_device(source_path);
    let speed: u64 = if is_device { 40 } else { 150 };
    let effective_speed = if num_threads > 1 {
        speed * std::cmp::min(num_threads as u64, 4)
    } else {
        speed
    };
    let estimated_secs = file_size / (effective_speed * 1024 * 1024);

    if estimated_secs > 30 {
        let mins = estimated_secs / 60;
        let secs = estimated_secs % 60;
        println!("  ⏱️  Tiempo estimado: ~{} min {} seg", mins, secs);
        println!();
        println!(
            "{}",
            "  ☕ Estos escaneos son bastante tardados, así que te".bright_yellow()
        );
        println!(
            "{}",
            "     recomendamos ir por un café o echarte un sueñito".bright_yellow()
        );
        println!(
            "{}",
            "     en lo que nosotros chambeamos. 👻💤".bright_yellow()
        );
    } else if estimated_secs > 5 {
        let mins = estimated_secs / 60;
        let secs = estimated_secs % 60;
        if mins > 0 {
            println!("  ⏱️  Tiempo estimado: ~{} min {} seg", mins, secs);
        } else {
            println!("  ⏱️  Tiempo estimado: ~{} seg", secs);
        }
    }

    if num_threads > 1 {
        println!("  🧵 Usando {} hilos de escaneo", num_threads);
    }
    println!(
        "{}",
        "  ⏹️  Puedes cancelar en cualquier momento con Ctrl+C (se guarda lo encontrado hasta ahí)."
            .bright_black()
    );
    println!();
}

/// Implementación central del escaneo: orquesta single-thread o multi-thread.
/// `quiet`: si es true, no imprime nada por terminal ni usa la barra `indicatif` (modo GUI); el
/// avance se expone igual por `SCAN_PROGRESS_BYTES`.
fn scan_source_impl(
    source_path: &Path,
    signatures: &[FileSignature],
    file_size: u64,
    num_threads: usize,
    quiet: bool,
) -> Result<ScanResult> {
    // Marcar el escaneo como "en progreso" para que el handler de Ctrl+C cancele en vez de
    // cerrar el programa, y limpiar cualquier cancelación pendiente de un escaneo anterior (en
    // modo interactivo se puede escanear, cancelar, volver al menú y reescanear). El guard con
    // `Drop` garantiza que `SCAN_IN_PROGRESS` se limpie al salir de la función pase lo que pase.
    struct ScanGuard;
    impl Drop for ScanGuard {
        fn drop(&mut self) {
            SCAN_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
    // El orden importa: `SCAN_IN_PROGRESS` se levanta AL FINAL, cuando el resto del estado ya
    // quedó limpio. La GUI usa ese flag para saber cuándo puede empezar a mostrar el progreso y a
    // ofrecer el botón de detener; si se levantara primero, alcanzaría a dibujar los contadores
    // del escaneo ANTERIOR (barra al 100%) y un "Detener" cuyo pedido el reset de acá pisaría.
    SCAN_CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    SCAN_PROGRESS_BYTES.store(0, Ordering::Relaxed);
    SCAN_PROGRESS_FILES.store(0, Ordering::Relaxed);
    SCAN_IN_PROGRESS.store(true, Ordering::SeqCst);
    let _scan_guard = ScanGuard;

    // Contadores propios de ESTE escaneo (fuente de verdad; los globales son solo el espejo que
    // lee la GUI). Son `Arc` porque en multi-hilo los comparten todos los workers y el monitor.
    let progress_bytes = Arc::new(AtomicU64::new(0));
    let progress_files = Arc::new(AtomicU64::new(0));

    if !quiet {
        print_scan_preamble(source_path, file_size, num_threads);
    }

    // En modo GUI (`quiet`) la barra es oculta (sus métodos son no-ops); el avance real se sigue
    // por `SCAN_PROGRESS_BYTES`. En CLI es la barra visible de siempre.
    let pb = if quiet {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(file_size);
        pb.set_style(
            ProgressStyle::with_template(
                "  👻 [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%)",
            )
            .unwrap()
            .progress_chars("█▓▒░  "),
        );
        pb
    };

    let max_header_len = max_signature_reach(signatures);

    let (mut found_files, bytes_scanned_actual, had_errors) = if num_threads <= 1 {
        // ── Fast path: 1 hilo, sin overhead de threads ──
        let segment = Segment {
            start: 0,
            end: file_size,
            claim_start: 0,
            claim_end: file_size,
        };
        // (B1) scan_segment ya no propaga errores de I/O con `?` — un sector dañado en
        // cualquier punto del origen ya no descarta todo lo encontrado antes de llegar a él.
        let result = scan_segment(
            source_path,
            &segment,
            signatures,
            file_size,
            max_header_len,
            &ScanProgress {
                bytes: &progress_bytes,
                files: &progress_files,
                mirror_bytes: Some(&SCAN_PROGRESS_BYTES),
                mirror_files: Some(&SCAN_PROGRESS_FILES),
                quiet,
            },
            Some(&pb),
            &SCAN_CANCEL_REQUESTED,
        );
        if result.had_errors && !quiet {
            eprintln!(
                "  ⚠️  El escaneo tuvo errores de I/O leyendo el origen; el resultado es parcial."
            );
        }
        // B3: reportar lo realmente leído, no file_size fijo — un EOF prematuro (bytes_read
        // == 0 antes de llegar a segment.end) corta el escaneo antes de tiempo. Se lee del
        // contador PROPIO, no del global, que otro escaneo concurrente puede haber reseteado.
        let scanned = progress_bytes.load(Ordering::Relaxed);
        (result.found_files, scanned, result.had_errors)
    } else {
        // ── Multi-hilo ──
        let segments = calculate_segments(file_size, num_threads, max_header_len as u64);

        // Hilo dedicado de progreso: lee `SCAN_PROGRESS_BYTES` cada 100ms y actualiza la
        // ProgressBar (en modo GUI la barra es oculta y esto es no-op; el avance lo lee la GUI del
        // mismo global).
        //
        // El monitor termina por un flag PROPIO de este escaneo, no por el contador de bytes.
        // Antes salía con `if pos >= file_size`, y eso era un cuelgue infinito esperando: el
        // contador es un GLOBAL, así que cualquier otro escaneo que arranque en el mismo proceso
        // lo resetea a 0 (`SCAN_PROGRESS_BYTES.store(0)`). Si ese reset caía después del
        // `store(file_size)` que señalaba el fin y antes de que el monitor lo leyera (ventana de
        // 100ms), el monitor giraba para siempre y `monitor_handle.join()` no volvía nunca. Se
        // manifestó como el job de macOS del CI colgado 6 h en
        // `test_signature_at_segment_boundary` (los tests corren en paralelo en un mismo proceso).
        // La terminación de un hilo no debe depender de un contador compartido y mutable.
        // A propósito el monitor sigue LEYENDO el global (solo para dibujar): así el test de
        // regresión `test_multithread_scan_terminates_despite_concurrent_progress_resets`, que
        // martilla ese global con escaneos concurrentes, conserva su capacidad de atrapar el bug
        // si alguien vuelve a atar la salida del loop al contador.
        // El flag va detrás de un guard con `Drop` para que también se levante si este hilo se
        // va por un panic (por ejemplo, si el OS no puede crear un worker). Sin el guard, en la
        // GUI —donde el escaneo corre en un hilo y un panic no mata el proceso— quedaría un hilo
        // monitor girando en background por cada escaneo que falle así. Es la misma clase de bug
        // que se acaba de arreglar: que la terminación de un hilo dependa de que otro llegue a
        // una línea.
        struct MonitorGuard(Arc<AtomicBool>);
        impl Drop for MonitorGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let monitor_done = Arc::new(AtomicBool::new(false));
        let monitor_flag = monitor_done.clone();
        // OJO: tiene que ser un binding CON NOMBRE. Si esto se "simplifica" a
        // `let _ = MonitorGuard(...)`, el guard se dropea en el acto, el flag queda en true antes
        // de que el monitor entre al loop, y la barra de progreso se congela en 0 durante todo el
        // escaneo — en un disco grande, horas de pantalla muerta. Ni los tests ni clippy avisan.
        let _monitor_guard = MonitorGuard(monitor_done.clone());
        let pb_monitor = pb.clone();
        let monitor_handle = std::thread::spawn(move || {
            while !monitor_flag.load(Ordering::Acquire) {
                let pos = SCAN_PROGRESS_BYTES.load(Ordering::Relaxed);
                pb_monitor.set_position(std::cmp::min(pos, file_size));
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        // Los workers también van detrás de un guard, y este es el que importa de verdad: son
        // los hilos que LEEN EL DISCO. Si el orquestador se va por un panic a mitad del spawn
        // (el OS puede rechazar la creación de un hilo por límite de procesos o memoria), los
        // workers ya creados quedan detached y siguen leyendo. En el CLI da igual porque el
        // proceso muere, pero en la GUI el escaneo corre en un hilo y el panic no mata nada:
        // quedaban hilos huérfanos machacando un disco que puede estar muriéndose, con el handle
        // de `\\.\PhysicalDriveN` abierto, y sumando al contador global de progreso — así que el
        // escaneo SIGUIENTE mostraba una barra que avanzaba más rápido que la realidad.
        struct WorkersGuard(Vec<std::thread::JoinHandle<SegmentResult>>);
        impl Drop for WorkersGuard {
            fn drop(&mut self) {
                if self.0.is_empty() {
                    return; // Camino feliz: los handles ya se consumieron al recolectar.
                }
                // Se pide cancelación y se ESPERA de verdad: soltar sin joinear no serviría,
                // porque el escaneo siguiente limpia el flag y los huérfanos nunca lo verían.
                // La cancelación es cooperativa y se chequea por bloque de 1 MB, así que salen
                // rápido. El flag queda en true, pero el próximo escaneo lo resetea al arrancar.
                SCAN_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
                for h in self.0.drain(..) {
                    let _ = h.join();
                }
            }
        }

        let source_buf = source_path.to_path_buf();
        let sigs_arc: Arc<Vec<FileSignature>> = Arc::new(signatures.to_vec());

        let mut workers = WorkersGuard(Vec::with_capacity(segments.len()));
        for segment in segments {
            let path = source_buf.clone();
            let sigs = sigs_arc.clone();
            // Los contadores propios del escaneo sí hay que clonarlos (Arc) para moverlos al
            // worker; `&SCAN_CANCEL_REQUESTED` y los espejos globales son referencias 'static.
            let bytes = progress_bytes.clone();
            let files = progress_files.clone();
            workers.0.push(std::thread::spawn(move || {
                scan_segment(
                    &path,
                    &segment,
                    &sigs,
                    file_size,
                    max_header_len,
                    &ScanProgress {
                        bytes: &bytes,
                        files: &files,
                        mirror_bytes: Some(&SCAN_PROGRESS_BYTES),
                        mirror_files: Some(&SCAN_PROGRESS_FILES),
                        quiet,
                    },
                    None,
                    &SCAN_CANCEL_REQUESTED,
                )
            }));
        }
        // Se sacan del guard para recolectarlos; a partir de acá el guard queda vacío y su Drop
        // es un no-op.
        let handles: Vec<_> = workers.0.drain(..).collect();

        // Recolectar resultados de todos los hilos. (B1) A propósito NO se aborta ni se
        // descarta nada si un hilo tuvo errores de I/O o incluso panicó: los demás hilos que
        // sí terminaron bien conservan sus resultados. Antes, un solo `Err` de cualquier hilo
        // hacía `return Err(e)` acá y tiraba a la basura `all_files` completo, incluyendo lo
        // que habían encontrado los OTROS hilos exitosos — exactamente el escenario central
        // de uso de la herramienta (un sector malo en algún punto del disco).
        let mut all_files: Vec<FoundFile> = Vec::new();
        let mut multi_had_errors = false;
        for handle in handles {
            match handle.join() {
                Ok(result) => {
                    all_files.extend(result.found_files);
                    multi_had_errors |= result.had_errors;
                }
                Err(_) => {
                    // El hilo panicó: perdemos SUS resultados (no hay forma de recuperarlos
                    // de un panic), pero los demás hilos ya recolectados en `all_files` se
                    // conservan igual.
                    if !quiet {
                        eprintln!("  ⚠️  Un hilo de escaneo falló inesperadamente (panic); se conservan los resultados de los demás hilos.");
                    }
                    multi_had_errors = true;
                }
            }
        }
        if multi_had_errors && !quiet {
            eprintln!("  ⚠️  El escaneo tuvo errores en uno o más hilos; el resultado es parcial.");
        }

        // B3: reportar lo realmente leído, no `file_size` fijo (un EOF prematuro corta antes).
        // Del contador propio: el global lo pisa cualquier otro escaneo del mismo proceso.
        // Se acota a `file_size` porque en multi-hilo los segmentos se solapan (overlap para
        // detectar firmas en la frontera) y esos bytes se leen dos veces: sin el tope, el resumen
        // le decía al usuario que se escanearon más bytes de los que tiene el origen.
        let scanned = std::cmp::min(progress_bytes.load(Ordering::Relaxed), file_size);

        // Siempre señalar al monitor que termine. Ya no hace falta falsear el contador a
        // `file_size` para lograrlo: el flag es propio de este escaneo y nadie más lo toca.
        monitor_done.store(true, Ordering::Release);
        let _ = monitor_handle.join();

        // Sort por offset, dedup defensivo, re-indexar
        all_files.sort_by_key(|f| f.offset);
        all_files.dedup_by(|a, b| {
            a.offset == b.offset && a.signature.extension == b.signature.extension
        });
        for (i, f) in all_files.iter_mut().enumerate() {
            f.index = i + 1;
        }

        (all_files, scanned, multi_had_errors)
    };

    // A2: segundo pase de footer, en un solo hilo, para archivos cuyo footer no apareció dentro
    // del buffer/chunk original — ver `refine_footers`. Es CANCELABLE: en un disco que está
    // fallando este pase puede releer cientos de MB (hasta `max_size` por cada candidato sin
    // footer), que es EXACTAMENTE lo que el usuario pidió dejar de hacer al apretar "Detener" /
    // Ctrl+C. Se le pasa el flag para que corte por candidato y por chunk.
    refine_footers(source_path, &mut found_files, &SCAN_CANCEL_REQUESTED);

    // Mismo problema que el footer, otra familia: el audio por frames (MP3/AAC) casi nunca cierra su
    // cadena dentro del buffer de 1 MB, porque una canción típica pesa varios MB. Sin este pase, la
    // música REAL quedaba marcada "posiblemente dañada" y por lo tanto no se guardaba por defecto.
    // También es cancelable, por el mismo motivo que `refine_footers`.
    refine_audio_streams(source_path, &mut found_files, &SCAN_CANCEL_REQUESTED);

    // Misma idea para los contenedores ISOBMFF (MP4/HEIC/3GP/M4A): sus cajas traen el largo, así que
    // el tamaño real se resuelve saltando de caja en caja. Sin esto, un `ftyp` que aparece por
    // casualidad en datos binarios se carveaba hasta max_size — 2 GB por candidato en MP4.
    refine_isobmff_sizes(source_path, &mut found_files, &SCAN_CANCEL_REQUESTED);

    // Supresión de solapamiento: quitar los archivos que caen ENTEROS dentro de otro cuyo final se
    // detectó de verdad (footer/EOCD). Son contenido embebido —miniaturas, iconos, imágenes dentro
    // de un PDF o un Word— que el carving ve como archivos sueltos y que, sin esto, llenan la salida
    // de basura y hacen que la suma de tamaños supere de lejos al disco. Va DESPUÉS de refine_footers
    // para que los tamaños de los contenedores ya estén afinados.
    suppress_contained(&mut found_files);

    // Capturar la cancelación DESPUÉS del refinamiento: si el usuario cortó durante ese pase,
    // `cancelled` debe reflejarlo. `found_files` ya contiene solo lo hallado antes de cortar; el
    // refinamiento no agrega archivos, solo ajusta el tamaño de los que ya estaban.
    let cancelled = is_cancel_requested();

    // Igualar el contador en vivo al total exacto (el conteo de a bloques es previo al dedup del
    // camino multi-hilo). Así el último "Encontrados: N" que muestra la GUI coincide con la lista
    // que el usuario ve después, en vez de quedar uno o dos por encima sin explicación.
    progress_files.store(found_files.len() as u64, Ordering::Relaxed);
    SCAN_PROGRESS_FILES.store(found_files.len() as u64, Ordering::Relaxed);

    if cancelled {
        pb.finish_with_message("⏹️  Escaneo cancelado");
    } else {
        pb.finish_with_message("✅ Escaneo completado");
    }
    if !quiet {
        println!();
    }

    let photos_count = found_files
        .iter()
        .filter(|f| f.signature.category == FileCategory::Photo)
        .count();
    let videos_count = found_files
        .iter()
        .filter(|f| f.signature.category == FileCategory::Video)
        .count();
    let audios_count = found_files
        .iter()
        .filter(|f| f.signature.category == FileCategory::Audio)
        .count();
    let documents_count = found_files
        .iter()
        .filter(|f| f.signature.category == FileCategory::Document)
        .count();

    Ok(ScanResult {
        found_files,
        bytes_scanned: bytes_scanned_actual,
        photos_count,
        videos_count,
        audios_count,
        documents_count,
        had_errors,
        cancelled,
    })
}

/// Header local de una entrada ZIP (`PK\x03\x04`). Los documentos OOXML (docx/xlsx/pptx) son ZIPs y
/// necesitan un cálculo de tamaño propio: su tamaño NO está en el header inicial (como BMP) NI se
/// puede sacar con un footer fijo. Dos motivos: (1) el header `PK\x03\x04` se repite en CADA entrada
/// interna del zip, así que la lógica de footer anidado nunca cerraría; (2) el registro de fin
/// (EOCD, `PK\x05\x06`) lleva 18+ bytes variables detrás, que un footer fijo no capturaría.
const ZIP_LOCAL_FILE_HEADER: &[u8] = &[0x50, 0x4B, 0x03, 0x04];

/// True si la firma es un documento OOXML (ZIP): usa el cálculo de tamaño `zip_ooxml_size` en vez
/// de footer / size_from_header. Solo las firmas docx/xlsx/pptx tienen este header.
fn signature_is_zip_ooxml(sig: &FileSignature) -> bool {
    sig.header == ZIP_LOCAL_FILE_HEADER
}

/// Tamaño exacto de un archivo ZIP/OOXML que empieza en `start`, delegando en
/// `signatures::zip_local_file_end` (que ubica y valida el EOCD). Se acota la vista del buffer a
/// `search_limit` para no gastar en buscar más allá de `max_size`, y para que un archivo cuyo EOCD
/// caiga más allá devuelva `None` (→ se cae a max_size, como un footer no hallado).
fn zip_ooxml_size(buf: &[u8], start: usize, search_limit: usize) -> Option<usize> {
    let end_scan = search_limit.min(buf.len());
    if start >= end_scan {
        return None;
    }
    crate::signatures::zip_local_file_end(&buf[start..end_scan])
}

/// Elimina de `found_files` los archivos que caen ENTERAMENTE dentro de otro cuyo final se detectó
/// de verdad (footer o EOCD hallado → `footer_found`). Son contenido embebido —una miniatura JPEG
/// dentro de una foto, una imagen dentro de un PDF o un Word, un icono dentro de otro archivo— que
/// el carving detecta como archivo suelto y que, sin esto, llena la salida de "basura" y hace que la
/// suma de tamaños supere de lejos al tamaño del disco (varios carves pisándose entre sí).
///
/// Clave de seguridad: solo se suprime lo contenido en un contenedor CONFIABLE (`footer_found`). Un
/// falso positivo carveado a `max_size` (que englobaría archivos reales que vengan después) tiene
/// `footer_found=false`, así que NO cuenta como contenedor y NUNCA borra a los reales de adentro
/// —esos los filtra después el criterio de integridad al recuperar—.
/// True si el FINAL de esta firma se detecta por una validación ESTRUCTURAL (no por matchear un
/// footer de pocos bytes). Solo estos pueden suprimir archivos que tienen su propio final —ver
/// `suppress_contained`.
fn es_contenedor_fuerte(sig: &FileSignature) -> bool {
    signature_is_zip_ooxml(sig)          // EOCD del ZIP/OOXML
        || sig.is_isobmff()              // cadena de cajas MP4/HEIC/3GP/M4A
        || sig.audio_stream().is_some()  // cadena de frames MP3/AAC
        || sig.size_from_header.is_some() // tamaño en el header (BMP)
}

fn suppress_contained(found_files: &mut Vec<FoundFile>) {
    if found_files.len() < 2 {
        return;
    }
    // Índices ordenados por offset asc y, a igual offset, tamaño desc (el contenedor antes que lo
    // contenido). No se reordena `found_files` en sí para no cambiar el orden que ve el resto.
    let mut order: Vec<usize> = (0..found_files.len()).collect();
    order.sort_by(|&a, &b| {
        found_files[a]
            .offset
            .cmp(&found_files[b].offset)
            .then(found_files[b].size.cmp(&found_files[a].size))
    });

    let mut drop_flags = vec![false; found_files.len()];
    // Se distinguen dos clases de contenedor porque su final es de fiar en grados MUY distintos:
    //
    // - FUERTE: el final salió de una validación ESTRUCTURAL — el EOCD de un ZIP/OOXML, la cadena de
    //   cajas de un ISOBMFF, la cadena de frames de un audio, o el tamaño en el header (BMP). Estos
    //   casi no dan falsos positivos, así que suprimen TODO lo que engloban (incluidos archivos con
    //   su propio footer: una miniatura real dentro de un video, una imagen dentro de un ZIP).
    //
    // - DÉBIL: el final salió de matchear un footer de pocos bytes (JPEG `FF D9`, GIF `00 3B`), que
    //   aparece por azar ~16 veces por MB en datos cualquiera. Un contenedor así NO puede suprimir a
    //   un archivo que tiene su PROPIO final detectado — porque casi siempre significa que ESE
    //   contenedor agarró un footer espurio dentro de los datos del archivo de al lado. Caso medido:
    //   una foto truncada se tragaba las DOS fotos reales que venían detrás. Sí suprime lo que no
    //   tiene final propio (frames internos, miniaturas carveadas a max_size).
    let mut strong_end: u64 = 0;
    let mut any_end: u64 = 0;
    for &i in &order {
        let f = &found_files[i];
        let f_end = f.offset.saturating_add(f.size);
        let contenido_fuerte = f_end <= strong_end;
        let contenido_debil = f_end <= any_end && !f.footer_found;
        if contenido_fuerte || contenido_debil {
            drop_flags[i] = true;
            continue;
        }
        if f.footer_found {
            any_end = any_end.max(f_end);
            if es_contenedor_fuerte(&f.signature) {
                strong_end = strong_end.max(f_end);
            }
        }
    }

    let mut i = 0;
    found_files.retain(|_| {
        let keep = !drop_flags[i];
        i += 1;
        keep
    });
}

/// Busca firmas dentro de un buffer.
/// El tamaño se determina buscando el footer DENTRO del buffer (sin seeks extra al disco).
/// Esto hace el escaneo puramente secuencial y rápido incluso en USBs.
fn check_signatures_in_buffer(
    buf: &[u8],
    base_offset: u64,
    signatures: &[FileSignature],
    found_files: &mut Vec<FoundFile>,
    file_index: &mut usize,
    source_size: u64,
    seen: &mut HashSet<(u64, &'static str)>,
) {
    for i in 0..buf.len() {
        for sig in signatures {
            let check_pos = i + sig.header_offset;
            let end_pos = check_pos + sig.header.len();

            if end_pos > buf.len() {
                continue;
            }

            if &buf[check_pos..end_pos] == sig.header {
                // Verificar extra_check si existe (desambigua RIFF, OggS, etc.)
                if let Some((extra_bytes, extra_offset)) = &sig.extra_check {
                    let extra_pos = i + extra_offset;
                    let extra_end = extra_pos + extra_bytes.len();
                    if extra_end > buf.len() || &buf[extra_pos..extra_end] != *extra_bytes {
                        continue;
                    }
                }

                // Verificar validator bit-level si existe (ver C2: MP3 Sync / AAC ADTS).
                // `needed_len` es el mínimo de bytes indispensable para el chequeo básico de
                // bits reservados; se le pasa al validador TODO lo que queda del buffer (no
                // solo `needed_len`) para que pueda además hacer frame chaining (calcular el
                // largo del frame y verificar un segundo syncword más adelante) cuando haya
                // suficientes datos — si no los hay, el validador decide aceptar sin ese
                // chequeo extra en vez de rechazar por falta de datos (ver C2 fix v2).
                if let Some((validator_fn, needed_len)) = sig.validator {
                    if check_pos + needed_len > buf.len() || !validator_fn(&buf[check_pos..]) {
                        continue;
                    }
                }

                let absolute_offset = base_offset + i as u64;

                // Verificar que no está ya registrado (evitar duplicados del overlap).
                // HashSet en vez de scan lineal de found_files: O(1) en vez de O(n) por hit,
                // crítico ahora que los falsos positivos de header corto ya no explotan pero
                // el volumen de hits legítimos + overlaps sigue pudiendo ser alto (ver C2).
                if !seen.insert((absolute_offset, sig.extension)) {
                    continue;
                }

                let max_possible = std::cmp::min(
                    sig.max_size as u64,
                    source_size.saturating_sub(absolute_offset),
                );

                // Determinar tamaño: EOCD del zip (OOXML), campo de tamaño en el header (BMP),
                // footer, o max_size.
                let (size, footer_found) = if signature_is_zip_ooxml(sig) {
                    // OOXML/ZIP: tamaño exacto parseando el EOCD (ver `zip_ooxml_size`). Se acota la
                    // búsqueda al mismo `max_possible` que el resto (un EOCD más allá de max_size no
                    // serviría). Si no se encuentra/valida, se cae a max_size como un footer no
                    // hallado (footer_found=false → el archivo sale marcado "posiblemente dañado").
                    let limit = i.saturating_add(max_possible as usize);
                    match zip_ooxml_size(buf, i, limit) {
                        Some(sz) if sz as u64 <= max_possible && sz > 0 => (sz as u64, true),
                        _ => (max_possible, false),
                    }
                } else if let Some(stream_end) = sig.stream_end() {
                    // Audio por frames (MP3, AAC): no hay footer ni tamaño en el header, así que el
                    // final sale de recorrer la cadena de frames. Sin esto, TODO candidato —real o
                    // falso— se carveaba hasta `max_size`: 382 MB de origen daban 13 GB de salida, y
                    // los audios de verdad quedaban con decenas de MB de relleno pegado atrás.
                    let limit = i.saturating_add(max_possible as usize).min(buf.len());
                    // ¿Lo que se le pasa al walker llega hasta el final del origen? Es la diferencia
                    // entre "el audio termina justo acá" (final limpio) y "se acabó el buffer y el
                    // archivo quizá sigue". Sin esto, un MP3 que ocupa todo el origen —una tarjeta
                    // llena de música— quedaba marcado "posiblemente dañado" y no se guardaba.
                    let at_source_end = base_offset.saturating_add(limit as u64) >= source_size;
                    match stream_end(&buf[i..limit], at_source_end) {
                        Some(sz) if sz as u64 <= max_possible && sz > 0 => (sz as u64, true),
                        // La cadena no cerró dentro de lo disponible: se cae a max_size igual que un
                        // footer no hallado (footer_found=false → sale "posiblemente dañado").
                        _ => (max_possible, false),
                    }
                } else if let Some((sf_offset, sf_len)) = sig.size_from_header {
                    let sf_start = i + sf_offset;
                    let sf_end = sf_start + sf_len;
                    if sf_end <= buf.len() {
                        let mut val: u64 = 0;
                        for (idx, b) in buf[sf_start..sf_end].iter().enumerate() {
                            val |= (*b as u64) << (8 * idx);
                        }
                        if val > 0 && val <= max_possible {
                            (val, true)
                        } else {
                            (max_possible, false)
                        }
                    } else {
                        (max_possible, false)
                    }
                } else if let Some(footer) = sig.footer {
                    // Buscar el footer que realmente cierra ESTE header, tolerando anidamiento
                    // del mismo formato (ej. thumbnail EXIF embebido: un JPEG completo SOI..EOI
                    // dentro del segmento APP1, antes del EOI real de la foto) sin por eso
                    // absorber un segundo archivo distinto que caiga en el mismo buffer (ver
                    // A1 fix v2 — combina la búsqueda "última ocurrencia" que resuelve
                    // thumbnails con la acotación que evita englobar el siguiente archivo).
                    let search_start = check_pos + sig.header.len();
                    // Acotar cuánto buscar: un footer más allá de max_size bytes del header no
                    // serviría de nada (se descartaría de todas formas a favor de max_possible
                    // más abajo), así que no vale la pena que find_footer_nested siga leyendo
                    // más allá — esto evita el blowup O(buffer_size^2) descrito en B/M2.
                    let search_limit =
                        std::cmp::min(buf.len(), i.saturating_add(max_possible as usize));
                    if let Some(pos) =
                        find_footer_nested(buf, sig.header, footer, search_start, search_limit)
                    {
                        // Invariante requerido para que el cálculo de abajo no underflowee:
                        // toda firma con footer debe tener header.len() >= footer.len(). Hoy
                        // se cumple para todas las firmas en signatures/mod.rs, pero nada lo
                        // fuerza en tiempo de compilación — si una firma nueva lo rompe, este
                        // assert lo va a detectar en debug/test builds en vez de producir
                        // silenciosamente un tamaño de archivo absurdo en release (ver B4).
                        debug_assert!(
                            sig.header.len() >= footer.len(),
                            "Firma '{}': footer.len() ({}) > header.len() ({}) rompe el cálculo de found_size",
                            sig.name,
                            footer.len(),
                            sig.header.len()
                        );
                        let found_size = (pos - i) as u64 + footer.len() as u64;
                        if found_size <= max_possible {
                            (found_size, true)
                        } else {
                            (max_possible, false)
                        }
                    } else {
                        // Footer no está en este buffer → usar max_size por ahora; se
                        // reintenta con más alcance en `refine_footers` (ver A2).
                        (max_possible, false)
                    }
                } else {
                    (max_possible, false)
                };

                if size > 512 {
                    // Ignorar archivos menores a 512 bytes (probablemente falsos positivos)
                    *file_index += 1;
                    found_files.push(FoundFile {
                        signature: sig.clone(),
                        offset: absolute_offset,
                        size,
                        index: *file_index,
                        footer_found,
                        // El pase de refinamiento lo ajusta si corresponde.
                        end_unknown: false,
                    });
                }
            }
        }
    }
}

/// Busca el footer que cierra el header ya encontrado (en `header_start`, implícito por
/// `start` = fin del header), tolerando anidamiento del MISMO formato entre medio — ej. un
/// thumbnail EXIF embebido en JPEG: un SOI+EOI (FFD8...FFD9) completo dentro del segmento
/// APP1, antes del EOI real de la foto (ver A1 fix v2).
///
/// Algoritmo: profundidad de anidamiento, empezando en 1 (el header ya encontrado por el
/// caller, anterior a `start`). Escaneando hacia adelante desde `start`: cada nueva ocurrencia
/// del MISMO `header` suma 1 (se asume un archivo embebido tipo thumbnail) y cada ocurrencia
/// de `footer` resta 1; el footer que hace bajar la profundidad a 0 es el que cierra este
/// archivo. Esto resuelve dos problemas con una sola pasada:
/// - JPEG con thumbnail EXIF: el EOI del thumbnail interno no baja la profundidad a 0 (sigue
///   en 1, por el SOI del thumbnail que la subió a 2 antes), así que se sigue buscando hasta
///   el EOI real.
/// - Dos archivos del mismo formato en el mismo buffer: si no hay anidamiento real, el primer
///   footer encontrado SÍ baja la profundidad a 0 de inmediato, así que no se sigue buscando
///   más allá y no se engloba el siguiente archivo.
fn find_footer_nested(
    buf: &[u8],
    header: &[u8],
    footer: &[u8],
    start: usize,
    search_limit: usize,
) -> Option<usize> {
    // depth arranca en 1 (el header ya encontrado por el caller); `start` también sirve como
    // `skip_before` para `scan_nesting` porque no queremos contar nada antes de él.
    //
    // Optimización de rendimiento (B/M2): la versión anterior siempre escaneaba `buf` completo
    // desde el índice 0, sin importar dónde cayó el header. En datos de alta entropía donde un
    // footer corto (ej. GIF, 2 bytes) aparece por azar cada ~64KB, eso hacía O(buffer_size^2)
    // por chunk. El fix tiene dos partes:
    // - Empezar en `scan_from` (≈ `start`, con un pequeño margen hacia atrás) en vez de 0: no
    //   hace falta escanear antes de `start` porque `scan_nesting` ya ignora (via
    //   `skip_before`) cualquier match que TERMINE antes de `start`; el margen (el mayor entre
    //   header.len() y footer.len(), menos 1) solo cubre el caso borde de un match que
    //   "straddlea" la frontera (empieza antes de `start` pero termina después).
    // - Acotar el final de la búsqueda a `search_limit` (calculado por el caller a partir de
    //   `max_size` de la firma): un footer más allá de `max_size` bytes del header no serviría
    //   de nada igual (`check_signatures_in_buffer` lo descartaría a favor de `max_possible`),
    //   así que no vale la pena seguir buscando más allá.
    let margin = std::cmp::max(header.len(), footer.len()).saturating_sub(1);
    let scan_from = start.saturating_sub(margin);
    scan_nesting(buf, header, footer, 1, start, scan_from, search_limit).1
}

/// Motor compartido de conteo de anidamiento usado por `find_footer_nested` (pasada en buffer,
/// A1 fix v2) y por `find_footer_sequential` (segundo pase A2, cross-chunk). Escanea `buf`
/// completo desde el índice 0 buscando `header` y `footer`, ajustando `depth` como se describe
/// en `find_footer_nested`, pero ignorando matches que caen ENTERAMENTE antes de `skip_before`
/// — usado por `find_footer_sequential` para no re-contar, en cada chunk, bytes de overlap que
/// ya fueron contados en la iteración anterior. Retorna la profundidad resultante y, si llegó a
/// 0, la posición (índice en `buf`) del footer que cerró el archivo.
///
/// `scan_from`/`scan_to` acotan el rango de `buf` efectivamente recorrido (ver B/M2 en
/// `find_footer_nested`): `find_footer_sequential` sigue pasando el chunk completo (0..len,
/// ya acotado a `chunk_size` por el caller, no hay blowup ahí), mientras que
/// `find_footer_nested` acota a la vecindad del header recién encontrado para evitar el costo
/// O(buffer_size) por cada header en un buffer de 1 MB.
fn scan_nesting(
    buf: &[u8],
    header: &[u8],
    footer: &[u8],
    mut depth: i32,
    skip_before: usize,
    scan_from: usize,
    scan_to: usize,
) -> (i32, Option<usize>) {
    if footer.is_empty() {
        return (depth, None);
    }
    let scan_to = std::cmp::min(scan_to, buf.len());
    let mut i = scan_from;
    while i < scan_to {
        if i + footer.len() <= buf.len() && &buf[i..i + footer.len()] == footer {
            if i + footer.len() > skip_before {
                depth -= 1;
                if depth == 0 {
                    return (depth, Some(i));
                }
            }
            i += footer.len();
            continue;
        }
        if !header.is_empty()
            && i + header.len() <= buf.len()
            && &buf[i..i + header.len()] == header
        {
            if i + header.len() > skip_before {
                depth += 1;
            }
            i += header.len();
            continue;
        }
        i += 1;
    }
    (depth, None)
}

/// Segundo pase de footer (A2): para archivos cuyo tamaño quedó en `max_size` porque el
/// footer no apareció dentro del buffer/chunk original (típicamente 1 MB), reabre la fuente
/// y busca el footer secuencialmente, en chunks de 4 MB, desde el offset del header hasta
/// `max_size` bytes después. Corre en un solo hilo, después de juntar los resultados de todos
/// los workers, para no complicar el paralelismo y porque el volumen de candidatos aquí ya es
/// pequeño (solo los que no encontraron footer). Esto también hace el resultado determinista
/// entre 1 hilo y N hilos: antes, un archivo cuyo header caía cerca del final de un chunk se
/// carveaba a max_size de forma distinta según dónde cayeran las fronteras de segmento/chunk.
/// Qué se pudo concluir sobre el final de un archivo en el pase de refinamiento.
///
/// Las variantes existen porque mezclarlas tuvo consecuencias graves y MEDIDAS. En particular, "no
/// pude saberlo" no puede significar a la vez "se rompió el disco" y "se me acabó el presupuesto
/// mirando algo que probablemente es basura": lo primero hay que guardarlo, lo segundo no. Cuando
/// estuvieron juntos, 57 MB de origen produjeron 619 GB de salida.
enum EndResult {
    /// Se determinó el tamaño real y el archivo cierra bien. Sirve como contenedor confiable.
    Size(u64),
    /// Se sabe hasta dónde llega —el dato sale de recorrer contenido REAL, no de suponer— pero no
    /// que esté completo: al archivo le falta el final. Es el archivo real cortado (imagen truncada,
    /// último sector pisado). Sirve como contenedor confiable, porque su extensión es real.
    SizeUnverified(u64),
    /// Se sabe que llega AL MENOS hasta ahí, sin poder confirmarlo: se agotó el tamaño máximo de la
    /// firma, o una caja dice "hasta el fin del archivo" y no hay forma de saber dónde es.
    ///
    /// Se guarda (puede ser un audiolibro de más de 50 MB, o el video de una cámara que graba a
    /// streaming), pero NO cuenta como contenedor: dar por bueno un tamaño supuesto y suprimir con
    /// él fue el peor bug de la sesión — un MP4 cortado borraba los 20 archivos que venían después,
    /// sin ningún aviso.
    SizeGuess(u64),
    /// Se comprobó que la cadena/las cajas NO describen un archivo válido: probable falso positivo.
    Rejected,
    /// No se pudo leer (sector dañado) o el usuario canceló. Un archivo REAL en un disco que está
    /// fallando cae acá, así que se guarda igual, sin afirmar nada de su final.
    Unreadable,
}

/// Segundo pase para el audio por frames (MP3/AAC): sigue la cadena de frames LEYENDO DEL DISCO,
/// para los candidatos cuya cadena no cerró dentro del buffer de 1 MB del escaneo.
///
/// Sin esto la corrección de tamaño servía solo para audios chicos: una canción de 4 MB nunca cierra
/// su cadena en un buffer de 1 MB, así que quedaba marcada "posiblemente dañada" y —desde el recorte
/// de basura— no se guardaba por defecto. O sea: el arreglo de los falsos positivos se habría
/// llevado puesta la música de verdad. Se detectó probando con MP3 reales, no con `cargo test`.
///
/// Es CANCELABLE por candidato y por chunk, igual que `refine_footers`: en un disco que está
/// fallando, este pase puede releer bastante y es justo lo que el usuario pidió dejar de hacer al
/// apretar "Detener".
fn refine_audio_streams(source_path: &Path, found_files: &mut [FoundFile], cancel: &AtomicBool) {
    const AUDIO_CHUNK: usize = 1024 * 1024;

    // Se recorre por OFFSET creciente para poder saltar los candidatos que caen dentro de un
    // archivo ya resuelto. Cada frame de un MP3 empieza con el mismo syncword, así que el escaneo
    // registra un candidato POR FRAME (~2400 por MB): sin este salto, cada uno vuelve a recorrer el
    // tema entero desde su offset hasta el final, lo que hace el pase cuadrático (medido: 16 MB de
    // música contigua tardaban ~44 s, y una tarjeta de 4 GB se iba a más de una hora con la barra
    // congelada al 100%). Los saltados quedan sin refinar y `suppress_contained` los descarta
    // después, porque caen enteros dentro de un contenedor confiable.
    //
    // NO se reordena `found_files`: el orden fija el nombre `recovered_NNNN`. Se ordena una lista de
    // índices.
    let mut orden: Vec<usize> = (0..found_files.len()).collect();
    orden.sort_by_key(|&i| found_files[i].offset);
    let mut resuelto_hasta: u64 = 0;

    for i in orden {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let f = &found_files[i];
        // Solo el AUDIO de este pase mueve `resuelto_hasta`. Dejar que lo moviera cualquier archivo
        // con footer fue un bug: un JPEG al que le pisaron el final agarra como footer un `FF D9`
        // que aparece DENTRO del audio siguiente, y entonces a esa canción se le recortaba el
        // principio — medido, 97 KB perdidos: la etiqueta con título, artista y carátula, más los
        // primeros segundos.
        let Some(kind) = f.signature.audio_stream() else {
            continue;
        };
        // Ya tiene tamaño real (la cadena cerró dentro del buffer): nada que refinar.
        if f.footer_found {
            resuelto_hasta = resuelto_hasta.max(f.offset.saturating_add(f.size));
            continue;
        }
        if f.offset < resuelto_hasta {
            // Está dentro de un audio ya resuelto: es uno de sus frames, no un archivo. Se le acota
            // el tamaño al del archivo que lo contiene —no puede seguir más allá— para que
            // `suppress_contained` lo reconozca como contenido y lo descarte. Sin esto conservaban
            // el tamaño "hasta el final del origen", sobresalían del contenedor y sobrevivían: una
            // sola canción cortada dejaba 39 archivos basura.
            let dentro = resuelto_hasta.saturating_sub(f.offset);
            let i_mut = i;
            found_files[i_mut].size = found_files[i_mut].size.min(dentro);
            continue;
        }
        let (offset, max_size) = (f.offset, f.signature.max_size as u64);
        let resultado =
            walk_audio_stream_on_disk(source_path, offset, max_size, kind, AUDIO_CHUNK, cancel);
        let f = &mut found_files[i];
        match resultado {
            EndResult::Size(size) => {
                f.size = size;
                f.footer_found = true;
                resuelto_hasta = offset.saturating_add(size);
            }
            // Extensión REAL aunque incompleta: se guarda marcado no verificable, y puede contener
            // a otros (sus propios frames internos).
            EndResult::SizeUnverified(size) => {
                f.size = size;
                f.footer_found = true;
                f.end_unknown = true;
                resuelto_hasta = offset.saturating_add(size);
            }
            // Tamaño SUPUESTO (audiolibro más largo que el máximo de la firma): se guarda, pero sin
            // `footer_found`, así que nunca suprime a nadie. Igual avanza `resuelto_hasta`: sus
            // frames internos no son archivos y no vale la pena recorrerlos uno por uno.
            EndResult::SizeGuess(size) => {
                f.size = size;
                f.end_unknown = true;
                resuelto_hasta = offset.saturating_add(size);
            }
            EndResult::Unreadable => f.end_unknown = true,
            EndResult::Rejected => {}
        }
    }
}

/// Segundo pase para los contenedores ISOBMFF (MP4, HEIC, 3GP, M4A): recorre las cajas del archivo
/// LEYENDO DEL DISCO, para los que no cerraron dentro del buffer de 1 MB del escaneo.
///
/// Es mucho más barato que el pase de audio: las cajas traen su largo, así que se salta de una a la
/// siguiente con `seek` en vez de leer el contenido. Un video de 1 GB se resuelve con un puñado de
/// lecturas de 16 bytes.
fn refine_isobmff_sizes(source_path: &Path, found_files: &mut [FoundFile], cancel: &AtomicBool) {
    let source_size = {
        let Ok(mut file) = File::open(source_path) else {
            return;
        };
        match get_source_size(&mut file, source_path) {
            Ok(s) => s,
            Err(_) => return,
        }
    };

    for f in found_files.iter_mut() {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if f.footer_found || !f.signature.is_isobmff() {
            continue;
        }
        match walk_isobmff_on_disk(
            source_path,
            f.offset,
            f.signature.max_size as u64,
            source_size,
            cancel,
        ) {
            EndResult::Size(size) => {
                f.size = size;
                f.footer_found = true;
            }
            EndResult::SizeUnverified(size) => {
                f.size = size;
                f.footer_found = true;
                f.end_unknown = true;
            }
            // Tamaño SUPUESTO: se guarda sin `footer_found`, así que no suprime a nadie.
            EndResult::SizeGuess(size) => {
                f.size = size;
                f.end_unknown = true;
            }
            EndResult::Unreadable => f.end_unknown = true,
            EndResult::Rejected => {}
        }
    }
}

/// Recorre las cajas de un ISOBMFF saltando de una a la siguiente, y devuelve el tamaño real del
/// archivo. `None` si no se puede afirmar dónde termina.
///
/// Aplica las mismas reglas que el recorrido en memoria (`signatures::walk_isobmff_boxes`), que
/// salieron de bugs reales: la primera caja tiene que ser `ftyp`; al toparse con otro `ftyp` se
/// para (ahí empieza el archivo siguiente, y sin esto los videos contiguos de una tarjeta de cámara
/// se fusionaban en uno); hay que haber visto el índice (`moov`/`moof`/`meta`) para afirmar que el
/// archivo sirve; y `size == 0` significa "hasta el fin del archivo", no "acá no hay caja".
fn walk_isobmff_on_disk(
    source_path: &Path,
    offset: u64,
    max_size: u64,
    source_size: u64,
    cancel: &AtomicBool,
) -> EndResult {
    use crate::signatures::{
        is_isobmff_index_box_bytes, isobmff_box_len_at, isobmff_box_type_at_bytes, BoxLen,
        ISOBMFF_MAX_BOXES, ISOBMFF_MAX_BOXES_SIN_INDICE, ISOBMFF_MIN_BOXES,
    };

    let Ok(mut file) = File::open(source_path) else {
        return EndResult::Unreadable;
    };
    // 16 bytes de header de caja + un sector para el desfase de alineación.
    let mut header = [0u8; 2 * SECTOR as usize];
    let mut pos: u64 = 0;
    let mut boxes: usize = 0;
    let mut has_index = false;

    // Cierra el recorrido: solo se afirma un tamaño si además del encadenado se vio el índice. Si
    // no, es un candidato que NO describe un archivo utilizable: rechazado.
    let terminar = |boxes: usize, has_index: bool, pos: u64| -> EndResult {
        if boxes >= ISOBMFF_MIN_BOXES && has_index && pos > 0 {
            EndResult::Size(pos)
        } else {
            EndResult::Rejected
        }
    };
    // Se agotó el presupuesto (tamaño máximo o tope de cajas) o la caja dice "hasta el fin del
    // archivo": se supone la extensión, sin afirmarla y sin que sirva de contenedor. Y si NUNCA se
    // vio el índice (`moov`/`moof`/`meta`), esto directamente no parece un archivo: se rechaza — si
    // no, un `ftyp` cualquiera que se quede sin datos (un JPEG2000 empieza con `ftypjp2 `) se
    // guardaba carveado hasta el tope de la firma.
    let suponer = |has_index: bool, hasta: u64| -> EndResult {
        if has_index && hasta > 0 {
            EndResult::SizeGuess(hasta)
        } else {
            EndResult::Rejected
        }
    };

    loop {
        // Cancelar y los topes de tamaño NO son un rechazo: no se pudo saber.
        if cancel.load(Ordering::SeqCst) {
            return EndResult::Unreadable;
        }
        if pos > max_size {
            return suponer(has_index, max_size);
        }
        // Mientras no se haya visto el índice, el tope es bajo: acota lo que cuesta un candidato
        // falso. Una vez visto, el tope es alto — con uno bajo, un MP4 FRAGMENTADO (un par
        // `moof`+`mdat` por fragmento) se pasaba a los 68 segundos de video y se carveaba entero
        // hasta el tope de la firma.
        let tope = if has_index {
            ISOBMFF_MAX_BOXES
        } else {
            ISOBMFF_MAX_BOXES_SIN_INDICE
        };
        if boxes >= tope {
            return suponer(has_index, pos.min(max_size));
        }
        let Some(absolute) = offset.checked_add(pos) else {
            return EndResult::Unreadable;
        };
        if absolute >= source_size {
            return if absolute == source_size {
                terminar(boxes, has_index, pos)
            } else {
                suponer(has_index, pos.min(max_size))
            };
        }

        let Some((skew, n)) = read_aligned(&mut file, absolute, ISOBMFF_HEADER_READ, &mut header)
        else {
            // Error de I/O (sector dañado): no se pudo LEER. Se conserva el candidato tal cual;
            // en un disco que está fallando suele ser un archivo real.
            return EndResult::Unreadable;
        };
        let vista = &header[skew..skew + n];

        let Some(tipo) = isobmff_box_type_at_bytes(vista) else {
            // Lo que sigue no es una caja: ahí termina el archivo.
            return terminar(boxes, has_index, pos);
        };
        if boxes == 0 && &tipo != b"ftyp" {
            // El candidato no empieza en el inicio de un archivo: rechazado.
            return EndResult::Rejected;
        }
        if boxes > 0 && &tipo == b"ftyp" {
            // Empieza el archivo siguiente: este termina acá.
            return terminar(boxes, has_index, pos);
        }
        match isobmff_box_len_at(vista, boxes == 0) {
            BoxLen::Bytes(len) => {
                if boxes == 0 && len > crate::signatures::ISOBMFF_MAX_FTYP {
                    // Un `ftyp` enorme no describe un archivo (ver ISOBMFF_MAX_FTYP).
                    return EndResult::Rejected;
                }
                let Some(next) = pos.checked_add(len) else {
                    return EndResult::Unreadable;
                };
                match offset.checked_add(next) {
                    // La última caja declara más de lo que hay: el archivo está cortado. Su
                    // extensión NO se conoce (llega "hasta donde alcanza el origen"), así que es un
                    // tamaño SUPUESTO y no puede actuar como contenedor. Tratarlo como extensión
                    // real fue el peor bug de la sesión: un video cortado cerca del principio del
                    // disco borraba los 20 archivos que venían después.
                    Some(fin) if fin > source_size => {
                        let hasta = source_size.saturating_sub(offset);
                        return suponer(has_index, hasta.min(max_size));
                    }
                    None => return EndResult::Unreadable,
                    _ => {}
                }
                // Se marca DESPUÉS de validar el largo: 4 bytes de basura que digan `moov` con un
                // largo imposible no pueden hacer pasar a un candidato por archivo con índice.
                has_index |= is_isobmff_index_box_bytes(&tipo);
                pos = next;
                boxes += 1;
            }
            // `size == 0` = "esta caja llega hasta el fin del archivo". En una imagen de disco no
            // hay forma de saber dónde termina de verdad (lo que sigue puede ser el archivo
            // siguiente), así que no se afirma nada: se recupera igual, sin mentir que está íntegro.
            // "Hasta el fin del archivo": en una imagen de disco no hay forma de saber dónde es.
            BoxLen::ToEndOfFile => {
                let hasta = source_size.saturating_sub(offset);
                return suponer(has_index, hasta.min(max_size));
            }
            BoxLen::Invalid => return terminar(boxes, has_index, pos),
        }
    }
}

/// Recorre la cadena de frames de un audio leyendo secuencialmente desde `offset` y concluye sobre
/// su final: el tamaño real, un rechazo (no era audio), o que no se pudo saber.
///
/// Lee de a `chunk_size` bytes y arrastra el recorrido entre chunks: `consumed` marca hasta dónde
/// llegó la cadena confirmada, y el chunk siguiente se lee desde ahí, así que ningún frame queda
/// partido entre dos lecturas.
fn walk_audio_stream_on_disk(
    source_path: &Path,
    offset: u64,
    max_size: u64,
    kind: crate::signatures::AudioStream,
    chunk_size: usize,
    cancel: &AtomicBool,
) -> EndResult {
    use crate::signatures::{id3v2_tag_size, walk_audio_frames, ChainStop};

    let Ok(mut file) = File::open(source_path) else {
        return EndResult::Unreadable;
    };

    // Un sector extra: la lectura alineada arranca antes del offset pedido.
    let mut buf = vec![0u8; chunk_size + SECTOR as usize];
    let mut consumed: u64 = 0;
    let mut frames: usize = 0;
    let mut sample_rate: Option<u32> = None;
    // La etiqueta ID3 inicial no es audio: se saltea antes de empezar a encadenar frames.
    let mut tag: Option<u64> = None;

    // Cierra el recorrido con lo acumulado: si la cadena fue lo bastante larga, es el tamaño real;
    // si no, el candidato no era audio.
    let terminar = |frames: usize, consumed: u64| -> EndResult {
        if frames >= crate::signatures::AUDIO_MIN_CHAIN_FRAMES && consumed > 0 {
            EndResult::Size(consumed)
        } else {
            EndResult::Rejected
        }
    };

    loop {
        // Cancelar no es un rechazo: lo que se alcanzó a ver sigue valiendo.
        if cancel.load(Ordering::SeqCst) {
            return if frames >= crate::signatures::AUDIO_MIN_CHAIN_FRAMES && consumed > 0 {
                EndResult::SizeGuess(consumed)
            } else {
                EndResult::Unreadable
            };
        }
        if consumed >= max_size {
            // Se agotó el tamaño máximo de la firma. Si hay una cadena larga de por medio, es un
            // audio MÁS LARGO que ese máximo (un audiolibro, un set de DJ): se supone la extensión
            // hasta el tope y se guarda, sin afirmarla. Si no se encontró NI UN frame, esto no era
            // audio: rechazo. Sin esa distinción, un "ID3" falso en datos binarios —cuyo tamaño de
            // etiqueta basura salta de una más allá del máximo— se guardaba como un MP3 de 50 MB
            // (medido: 15 de esos, 712 MB de basura sobre 382 MB de origen), y un audiolibro de
            // 57 MB producía 12 380 archivos de 50 MB cada uno: 619 GB de salida.
            return if frames >= crate::signatures::AUDIO_MIN_CHAIN_FRAMES {
                EndResult::SizeGuess(max_size)
            } else {
                EndResult::Rejected
            };
        }

        let want = std::cmp::min(chunk_size as u64, max_size - consumed) as usize;
        let Some((skew, n)) = read_aligned(&mut file, offset + consumed, want, &mut buf) else {
            // Error de I/O (sector dañado): no se pudo LEER. Es exactamente el caso del disco que
            // está fallando, donde el archivo suele ser real, así que se conserva.
            return EndResult::Unreadable;
        };
        let data = &buf[skew..skew + n];
        // Se leyó menos de lo pedido => se llegó al final del origen.
        let at_source_end = n < want;

        let start = match tag {
            Some(_) => 0,
            None => {
                let t = id3v2_tag_size(data).unwrap_or(0);
                // Una etiqueta que sola se pasa del tamaño máximo del formato no es una etiqueta:
                // es basura leída como si lo fuera.
                if t as u64 >= max_size {
                    return EndResult::Rejected;
                }
                tag = Some(t as u64);
                if t >= data.len() {
                    // La etiqueta sola llena el chunk (posible con carátulas grandes): saltarla y
                    // seguir en la vuelta siguiente.
                    consumed += t as u64;
                    continue;
                }
                t
            }
        };

        let chain = walk_audio_frames(&data[start..], kind, usize::MAX, sample_rate);
        frames += chain.frames;
        sample_rate = chain.sample_rate;
        consumed += (start + chain.bytes) as u64;

        match chain.stop {
            // La cadena terminó de verdad: lo que sigue no es audio.
            ChainStop::BadData => return terminar(frames, consumed),
            ChainStop::NoMoreData | ChainStop::Truncated => {
                if at_source_end {
                    return if chain.stop == ChainStop::Truncated {
                        // El archivo se corta a mitad de un frame justo donde termina el origen: es
                        // un archivo REAL al que le falta un pedazo (imagen truncada, último sector
                        // pisado). Se conoce hasta dónde llega, así que se usa ese tamaño, pero sin
                        // afirmar que está entero.
                        if frames >= crate::signatures::AUDIO_MIN_CHAIN_FRAMES && consumed > 0 {
                            EndResult::SizeUnverified(consumed)
                        } else {
                            EndResult::Rejected
                        }
                    } else {
                        terminar(frames, consumed)
                    };
                }
                if chain.bytes == 0 && start == 0 {
                    // No se avanzó y quedan datos por leer: sin esta guarda el bucle no terminaría.
                    // Si ya se venía siguiendo una cadena larga, esto NO es un rechazo — pasa cuando
                    // lo que queda del presupuesto es más chico que un frame, o sea en un audio más
                    // largo que el máximo de su firma. Tratarlo como rechazo perdía el archivo
                    // entero. (Un frame no entra en un chunk de 1 MB solo si algo está muy mal: el
                    // máximo real son 1441 bytes en MP3 y 8191 en AAC.)
                    return if frames >= crate::signatures::AUDIO_MIN_CHAIN_FRAMES && consumed > 0 {
                        EndResult::SizeGuess(consumed)
                    } else {
                        EndResult::Rejected
                    };
                }
            }
            // Tope de frames: imposible acá, se recorre con `usize::MAX`.
            ChainStop::Capped => return EndResult::Unreadable,
        }
    }
}

/// Tamaño de sector al que hay que alinear las lecturas de un disco físico crudo.
const SECTOR: u64 = 512;
/// Bytes que se leen para inspeccionar el header de una caja ISOBMFF (8 normales, 16 si el tamaño
/// viene en 64 bits).
const ISOBMFF_HEADER_READ: usize = 16;

/// Lee `want` bytes lógicos desde `offset`, ALINEANDO la lectura física a 512 bytes.
///
/// Los dispositivos físicos crudos de Windows (`\\.\PhysicalDriveN`) rechazan con
/// `ERROR_INVALID_PARAMETER` cualquier `ReadFile` cuyo offset o tamaño no sea múltiplo de 512.
/// Los dos pases de refinamiento leen en fronteras de frame o de caja —posiciones arbitrarias por
/// definición—, así que sin esto TODAS sus lecturas fallaban en el escenario más común del público:
/// meter el USB y escanear el disco directo, sin clonar a `.img` primero. Como cada fallo deja al
/// candidato sin poder cerrar su cadena, el resultado era "0 archivos" en música, video y fotos de
/// iPhone. En un archivo normal alinear no cuesta nada (se lee un poco de más y se descarta), así
/// que se hace siempre en vez de preguntar qué es el origen.
///
/// `buf` tiene que tener lugar para `want` + un sector. Devuelve `(inicio_util, bytes_utiles)`.
fn read_aligned(
    file: &mut File,
    offset: u64,
    want: usize,
    buf: &mut [u8],
) -> Option<(usize, usize)> {
    let sector = SECTOR as usize;
    let aligned = offset & !(SECTOR - 1);
    let skew = (offset - aligned) as usize;
    // El largo también tiene que ser múltiplo de sector, así que se redondea hacia ARRIBA... y al
    // acotarlo al buffer se redondea hacia ABAJO. Sin esa segunda parte había un bug fino y real:
    // con un buffer de 528 bytes (16 + un sector) y un offset cuyo resto cayera entre 497 y 511, el
    // recorte dejaba una lectura de 528 bytes — no múltiplo de 512 —, o sea justo la lectura
    // desalineada que esta función existe para evitar. En Linux no se nota (ahí funciona igual); en
    // un disco físico de Windows fallaba, y el archivo quedaba sin dimensionar.
    let physical = (skew + want)
        .div_ceil(sector)
        .saturating_mul(sector)
        .min(buf.len() / sector * sector);
    if physical <= skew {
        return Some((skew, 0));
    }

    file.seek(SeekFrom::Start(aligned)).ok()?;
    let leidos = read_up_to(file, &mut buf[..physical])?;
    // Lo que se pidió de verdad empieza pasado el desfase, y nunca es más que `want`.
    let utiles = leidos.saturating_sub(skew).min(want);
    Some((skew, utiles))
}

/// Lee hasta llenar `buf`, tolerando lecturas cortas. Devuelve cuántos bytes se leyeron, o `None`
/// ante un error de lectura (un sector dañado: no se puede afirmar el tamaño, se deja el candidato
/// como está en vez de inventar uno).
fn read_up_to(file: &mut File, buf: &mut [u8]) -> Option<usize> {
    let mut total = 0;
    while total < buf.len() {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(total)
}

fn refine_footers(source_path: &Path, found_files: &mut [FoundFile], cancel: &AtomicBool) {
    const REFINE_CHUNK: usize = 4 * 1024 * 1024;

    for f in found_files.iter_mut() {
        // Cancelación cooperativa: si el usuario ya pidió parar, no se sigue releyendo el disco
        // (que puede estar muriendo) para refinar los candidatos que faltan. Se corta acá y se
        // deja lo que se alcanzó a refinar; los que queden sin footer conservan su tamaño a
        // `max_size` (marcados como "posiblemente dañado" en la integridad, que es lo correcto).
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if f.footer_found {
            continue;
        }
        let Some(footer) = f.signature.footer else {
            continue;
        };

        let header_end =
            f.offset + f.signature.header_offset as u64 + f.signature.header.len() as u64;
        let search_end = f.offset + f.signature.max_size as u64;

        if let Some(new_size) = find_footer_sequential(
            source_path,
            f.offset,
            header_end,
            search_end,
            f.signature.header,
            footer,
            REFINE_CHUNK,
            cancel,
        ) {
            f.size = new_size;
            f.footer_found = true;
        }
    }
}

/// Busca `footer` leyendo secuencialmente desde `search_start` hasta `search_end` (exclusivo),
/// en chunks de `chunk_size` bytes con solapamiento de `max(header.len(), footer.len()) - 1`
/// entre chunks para no perder coincidencias en la frontera. Igual que `find_footer_nested`,
/// trackea profundidad de anidamiento del mismo `header` para no cortar en el footer de un
/// thumbnail embebido (ver A1 fix v2), manteniendo la profundidad entre chunks y evitando
/// recontar matches ya vistos en el overlap del chunk anterior. Retorna el tamaño del archivo
/// (relativo a `header_offset`) si lo encuentra.
// Los 8 parámetros son intencionales (offsets, patrones y flag de cancelación de la búsqueda de
// footer); agruparlos en un struct no aportaría claridad y sí ruido.
#[allow(clippy::too_many_arguments)]
fn find_footer_sequential(
    source_path: &Path,
    header_offset: u64,
    search_start: u64,
    search_end: u64,
    header: &[u8],
    footer: &[u8],
    chunk_size: usize,
    cancel: &AtomicBool,
) -> Option<u64> {
    if search_start >= search_end {
        return None;
    }

    let mut file = File::open(source_path).ok()?;

    let mut pos = search_start;
    let mut overlap: Vec<u8> = Vec::new();
    let mut depth: i32 = 1;
    // Buffer con un sector de más: las lecturas se alinean a 512 (ver `read_aligned`), así que la
    // lectura física arranca un poco antes del offset pedido.
    let mut buf = vec![0u8; chunk_size + SECTOR as usize];

    while pos < search_end {
        // Cancelación cooperativa: se chequea una vez por chunk (4 MB) para no seguir leyendo un
        // disco que está fallando después de que el usuario apretó "Detener".
        if cancel.load(Ordering::SeqCst) {
            return None;
        }
        let to_read = std::cmp::min(chunk_size as u64, search_end - pos) as usize;
        // ALINEADO a 512: este pase busca el footer de JPEG/PNG/PDF/AVI/ZIP leyendo desde offsets
        // arbitrarios, y los discos físicos crudos de Windows rechazan esas lecturas (comprobado
        // sobre un `\\.\PhysicalDrive` real: offset 0 y 512 OK, offset 3 y 1000 fallan). Sin esto,
        // en el escenario más común del público —meter el USB y escanear el disco directo— ninguna
        // foto ni PDF cuyo final caiga fuera del buffer de 1 MB llegaba a cerrarse.
        let (skew, bytes_read) = read_aligned(&mut file, pos, to_read, &mut buf)?;
        if bytes_read == 0 {
            break;
        }
        let buf = &buf[skew..skew + bytes_read];

        let combined_start = pos - overlap.len() as u64;
        let skip_before = overlap.len(); // bytes ya contados en la iteración anterior
        let mut combined = overlap.clone();
        combined.extend_from_slice(buf);

        let combined_len = combined.len();
        let (new_depth, footer_pos) = scan_nesting(
            &combined,
            header,
            footer,
            depth,
            skip_before,
            0,
            combined_len,
        );
        depth = new_depth;
        if let Some(rel_pos) = footer_pos {
            let abs_pos = combined_start + rel_pos as u64;
            return Some((abs_pos + footer.len() as u64).saturating_sub(header_offset));
        }

        let keep = std::cmp::max(header.len(), footer.len()).saturating_sub(1);
        overlap = if buf.len() >= keep {
            buf[buf.len() - keep..].to_vec()
        } else {
            buf.to_vec()
        };
        pos += bytes_read as u64;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::{all_signatures, signatures_for_categories, FileCategory};
    use std::io::Write;

    /// Contadores de progreso LOCALES al test (sin espejo a los globales), para poder llamar a
    /// `scan_segment` sin pisar el estado que comparten los tests que corren en paralelo — misma
    /// razón por la que el flag de cancelación también se pasa por parámetro.
    fn local_progress<'a>(bytes: &'a AtomicU64, files: &'a AtomicU64) -> ScanProgress<'a> {
        ScanProgress {
            bytes,
            files,
            mirror_bytes: None,
            mirror_files: None,
            // Los tests no deben ensuciar la salida de `cargo test`.
            quiet: true,
        }
    }

    fn found_with(sig: crate::signatures::FileSignature, footer_found: bool) -> FoundFile {
        FoundFile {
            signature: sig,
            offset: 0,
            size: 1000,
            index: 1,
            footer_found,
            end_unknown: false,
        }
    }

    #[test]
    fn test_suppress_contained_drops_embedded_but_not_inside_false_positive() {
        let sig = all_signatures()
            .into_iter()
            .find(|s| s.extension == "jpg")
            .unwrap();
        let ff = |offset: u64, size: u64, footer_found: bool, index: usize| FoundFile {
            signature: sig.clone(),
            offset,
            size,
            index,
            footer_found,
            end_unknown: false,
        };

        // Dentro de un contenedor FUERTE (final estructural: EOCD, ISOBMFF, audio, tamaño en header)
        // → se suprime lo de adentro aunque tenga su propio footer; lo de afuera se conserva.
        let zip = all_signatures()
            .into_iter()
            .find(|s| s.extension == "docx")
            .unwrap();
        let fz = |offset: u64, size: u64, footer_found: bool, index: usize| FoundFile {
            signature: zip.clone(),
            offset,
            size,
            index,
            footer_found,
            end_unknown: false,
        };
        let mut v = vec![
            fz(0, 1000, true, 1),   // contenedor ZIP/OOXML (final estructural = fuerte)
            ff(100, 200, true, 2),  // imagen embebida (dentro), con su propio footer
            ff(5000, 300, true, 3), // archivo aparte (no contenido)
        ];
        suppress_contained(&mut v);
        let offsets: Vec<u64> = v.iter().map(|f| f.offset).collect();
        assert_eq!(
            offsets,
            vec![0, 5000],
            "un contenedor fuerte suprime lo embebido, incluso con footer propio"
        );

        // REGRESIÓN (ronda 3): un contenedor DÉBIL (footer de 2 bytes, JPEG) NO puede suprimir a un
        // archivo con su propio final. Una foto truncada agarraba un `FF D9` espurio dentro de los
        // datos de la de al lado y se tragaba las fotos reales que venían detrás.
        let mut v_debil = vec![
            ff(0, 100_000, true, 1),    // "foto" con footer espurio lejano (débil)
            ff(10_000, 5_000, true, 2), // FOTO REAL adentro, con su propio footer
            ff(50_000, 5_000, true, 3), // otra FOTO REAL adentro
        ];
        suppress_contained(&mut v_debil);
        assert_eq!(
            v_debil.len(),
            3,
            "un contenedor de footer débil no puede borrar fotos reales que englobó"
        );

        // Dentro de un FALSO POSITIVO carveado a max_size (footer_found=false) → NO se suprime al
        // archivo real de adentro (si no, un carve gigante borraría reales).
        let mut v2 = vec![
            ff(0, 100_000, false, 1), // falso positivo a max_size (no es contenedor confiable)
            ff(500, 800, true, 2),    // archivo REAL adentro
        ];
        suppress_contained(&mut v2);
        assert_eq!(
            v2.len(),
            2,
            "un falso positivo no debe suprimir a los reales de adentro"
        );
    }

    /// Arma un archivo ZIP válido (entradas STORED, sin comprimir) con su directorio central y EOCD
    /// correctos, para probar el carver de OOXML. El CRC va en 0 a propósito: el carver no lo valida
    /// (solo lee nombres y los campos estructurales del EOCD), y así el helper queda mínimo.
    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn u16le(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn u32le(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        let mut out = Vec::new();
        let mut local_offsets = Vec::new();
        for (name, data) in entries {
            local_offsets.push(out.len() as u32);
            u32le(&mut out, 0x0403_4b50); // header local
            u16le(&mut out, 20); // versión necesaria
            u16le(&mut out, 0); // flags
            u16le(&mut out, 0); // método = stored
            u16le(&mut out, 0); // hora
            u16le(&mut out, 0); // fecha
            u32le(&mut out, 0); // crc32
            u32le(&mut out, data.len() as u32); // tamaño comprimido
            u32le(&mut out, data.len() as u32); // tamaño sin comprimir
            u16le(&mut out, name.len() as u16); // largo del nombre
            u16le(&mut out, 0); // largo del campo extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }
        let cd_offset = out.len() as u32;
        for (i, (name, data)) in entries.iter().enumerate() {
            u32le(&mut out, 0x0201_4b50); // header del directorio central
            u16le(&mut out, 20); // versión que lo creó
            u16le(&mut out, 20); // versión necesaria
            u16le(&mut out, 0); // flags
            u16le(&mut out, 0); // método
            u16le(&mut out, 0); // hora
            u16le(&mut out, 0); // fecha
            u32le(&mut out, 0); // crc32
            u32le(&mut out, data.len() as u32);
            u32le(&mut out, data.len() as u32);
            u16le(&mut out, name.len() as u16);
            u16le(&mut out, 0); // extra
            u16le(&mut out, 0); // comentario
            u16le(&mut out, 0); // disco de inicio
            u16le(&mut out, 0); // attrs internos
            u32le(&mut out, 0); // attrs externos
            u32le(&mut out, local_offsets[i]); // offset del header local
            out.extend_from_slice(name.as_bytes());
        }
        let cd_size = out.len() as u32 - cd_offset;
        u32le(&mut out, 0x0605_4b50); // EOCD
        u16le(&mut out, 0); // disco
        u16le(&mut out, 0); // disco del CD
        u16le(&mut out, entries.len() as u16); // entradas en este disco
        u16le(&mut out, entries.len() as u16); // entradas totales
        u32le(&mut out, cd_size);
        u32le(&mut out, cd_offset);
        u16le(&mut out, 0); // largo del comentario
        out
    }

    /// Un docx/xlsx/pptx mínimo pero VÁLIDO: primera entrada `first` (para simular el orden de MS
    /// Office con `[Content_Types].xml` o el de LibreOffice con `_rels/.rels`), y la parte principal
    /// `main` con relleno para superar el filtro de 512 bytes.
    fn build_ooxml(first: &str, main: &str) -> Vec<u8> {
        let pad = vec![b'x'; 700];
        build_zip(&[
            (first, b"<Types/>"),
            ("[Content_Types].xml", b"<Types/>"),
            (main, &pad),
        ])
    }

    fn scan_bytes_as_docs(data: &[u8]) -> Vec<FoundFile> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();
        let sigs = signatures_for_categories(&[FileCategory::Document]);
        scan_source(file.path(), &sigs).unwrap().found_files
    }

    #[test]
    fn test_ooxml_docx_xlsx_pptx_detected_sized_and_not_cross_matched() {
        let docx = build_ooxml("[Content_Types].xml", "word/document.xml");
        let xlsx = build_ooxml("[Content_Types].xml", "xl/workbook.xml");
        let pptx = build_ooxml("[Content_Types].xml", "ppt/presentation.xml");
        // Tres Office SEGUIDOS (con relleno no-PK entre medio): el caso que destapó el falso
        // positivo cruzado (un docx "encontraba" el xl/workbook.xml del xlsx de al lado).
        let mut img = Vec::new();
        let mut offsets = Vec::new();
        for f in [&docx, &xlsx, &pptx] {
            img.extend_from_slice(&[0xAA; 4096]);
            offsets.push(img.len() as u64);
            img.extend_from_slice(f);
        }
        img.extend_from_slice(&[0xAA; 4096]);

        let found = scan_bytes_as_docs(&img);
        assert_eq!(found.len(), 3, "esperaba exactamente 3 (sin duplicados)");
        for (i, (ext, data)) in [("docx", &docx), ("xlsx", &xlsx), ("pptx", &pptx)]
            .iter()
            .enumerate()
        {
            let f = found
                .iter()
                .find(|f| f.offset == offsets[i])
                .unwrap_or_else(|| panic!("no se detectó nada en el offset del {ext}"));
            assert_eq!(f.signature.extension, *ext, "tipo mal clasificado");
            assert_eq!(
                f.size,
                data.len() as u64,
                "tamaño (EOCD) incorrecto para {ext}"
            );
            assert!(
                f.footer_found,
                "{ext}: debería tener el fin detectado (EOCD)"
            );
            assert_eq!(
                f.integrity(),
                Integrity::Intact,
                "{ext}: debería ser íntegro"
            );
        }
    }

    #[test]
    fn test_ooxml_libreoffice_style_rels_first_detected() {
        // LibreOffice abre el zip con `_rels/.rels`, no con `[Content_Types].xml` (verificado con un
        // .docx real). El anclaje por EOCD tiene que detectarlo igual.
        let docx = build_ooxml("_rels/.rels", "word/document.xml");
        let mut img = vec![0xAA; 4096];
        img.extend_from_slice(&docx);
        img.extend_from_slice(&[0xAA; 4096]);
        let found = scan_bytes_as_docs(&img);
        assert_eq!(found.len(), 1, "un solo docx");
        assert_eq!(found[0].signature.extension, "docx");
        assert_eq!(found[0].size, docx.len() as u64);
    }

    #[test]
    fn test_ooxml_libreoffice_calc_workbook_rels_first_detected() {
        // REGRESIÓN (ronda 3): LibreOffice CALC abre el zip con `xl/_rels/workbook.xml.rels`, no con
        // `[Content_Types].xml` ni `_rels/.rels`. El filtro de nombres exactos perdía ENTERO ese
        // xlsx —ni como dañado—, una feature recién lanzada. Verificado con un xlsx real de Calc.
        let xlsx = build_ooxml("xl/_rels/workbook.xml.rels", "xl/workbook.xml");
        let mut img = vec![0xAA; 4096];
        img.extend_from_slice(&xlsx);
        img.extend_from_slice(&[0xAA; 4096]);
        let found = scan_bytes_as_docs(&img);
        assert_eq!(found.len(), 1, "un solo xlsx");
        assert_eq!(found[0].signature.extension, "xlsx");
        assert_eq!(found[0].size, xlsx.len() as u64);
    }

    #[test]
    fn test_generic_zip_not_detected_as_office() {
        // Un .zip común (sin las partes de OOXML), de más de 512 bytes para descartar que lo filtre
        // el umbral de tamaño: se rechaza por el validator, no por chico.
        let readme = vec![b'r'; 600];
        let zip = build_zip(&[("readme.txt", &readme), ("data/blob.bin", &[0u8; 300])]);
        assert!(zip.len() > 512);
        let mut img = vec![0xAA; 4096];
        img.extend_from_slice(&zip);
        img.extend_from_slice(&[0xAA; 4096]);
        let found = scan_bytes_as_docs(&img);
        assert!(
            found.is_empty(),
            "un zip común NO debe carvearse como Office, encontró: {:?}",
            found
                .iter()
                .map(|f| f.signature.extension)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_integrity_footer_format_found_is_intact() {
        // JPEG tiene footer (FFD9). Si se encontró el footer, es íntegro.
        let jpeg = all_signatures()
            .into_iter()
            .find(|s| s.extension == "jpg")
            .expect("firma jpg");
        assert!(jpeg.footer.is_some());
        assert_eq!(found_with(jpeg, true).integrity(), Integrity::Intact);
    }

    #[test]
    fn test_integrity_footer_format_not_found_is_suspect() {
        // JPEG con footer NO encontrado (truncado a max_size) → posiblemente dañado.
        let jpeg = all_signatures()
            .into_iter()
            .find(|s| s.extension == "jpg")
            .expect("firma jpg");
        assert_eq!(found_with(jpeg, false).integrity(), Integrity::Suspect);
    }

    #[test]
    fn test_integrity_size_from_header_format_is_intact() {
        // BMP determina su tamaño por el header (size_from_header), sin footer; footer_found=true
        // cuando ese tamaño se leyó bien → íntegro.
        let bmp = all_signatures()
            .into_iter()
            .find(|s| s.extension == "bmp")
            .expect("firma bmp");
        assert!(bmp.footer.is_none() && bmp.size_from_header.is_some());
        assert_eq!(found_with(bmp, true).integrity(), Integrity::Intact);
    }

    #[test]
    fn test_integrity_no_end_marker_format_is_unverifiable() {
        // Un formato sin footer ni tamaño en header no se puede verificar → no verificable.
        let no_end = all_signatures()
            .into_iter()
            .find(|s| s.footer.is_none() && s.size_from_header.is_none())
            .expect("alguna firma sin footer ni size_from_header");
        assert_eq!(
            found_with(no_end, false).integrity(),
            Integrity::Unverifiable
        );
    }

    #[test]
    fn test_integrity_display_rank_orders_intact_first_suspect_last() {
        assert!(Integrity::Intact.display_rank() < Integrity::Unverifiable.display_rank());
        assert!(Integrity::Unverifiable.display_rank() < Integrity::Suspect.display_rank());
    }

    /// Crea un archivo temporal con firmas multimedia embebidas para testing
    fn create_test_image() -> (tempfile::NamedTempFile, Vec<(&'static str, u64)>) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut data = vec![0u8; 512 * 1024]; // 512 KB

        let mut expected: Vec<(&str, u64)> = Vec::new();

        // 1. JPEG (FFD8FF ... FFD9)
        let pos = 1024usize;
        data[pos..pos + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2048 {
            data[pos + i] = ((i * 7) % 256) as u8;
        }
        data[pos + 2048..pos + 2050].copy_from_slice(&[0xFF, 0xD9]);
        expected.push(("jpg", pos as u64));

        // 2. PNG (89504E47...)
        let pos = 8192;
        data[pos..pos + 8].copy_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);
        for i in 8..3000 {
            data[pos + i] = ((i * 13) % 256) as u8;
        }
        data[pos + 3000..pos + 3008]
            .copy_from_slice(&[0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]);
        expected.push(("png", pos as u64));

        // 3. WebP (RIFF....WEBP) - NO debe confundirse con AVI/WAV
        let pos = 16384;
        data[pos..pos + 4].copy_from_slice(b"RIFF");
        data[pos + 4..pos + 8].copy_from_slice(&1500u32.to_le_bytes());
        data[pos + 8..pos + 12].copy_from_slice(b"WEBP");
        for i in 12..1512 {
            data[pos + i] = ((i * 3) % 256) as u8;
        }
        expected.push(("webp", pos as u64));

        // 4. AVI (RIFF....AVI ) - NO debe confundirse con WebP/WAV
        let pos = 24576;
        data[pos..pos + 4].copy_from_slice(b"RIFF");
        data[pos + 4..pos + 8].copy_from_slice(&2000u32.to_le_bytes());
        data[pos + 8..pos + 12].copy_from_slice(b"AVI ");
        for i in 12..2012 {
            data[pos + i] = ((i * 11) % 256) as u8;
        }
        expected.push(("avi", pos as u64));

        // 5. WAV (RIFF....WAVE) - NO debe confundirse con WebP/AVI
        let pos = 32768;
        data[pos..pos + 4].copy_from_slice(b"RIFF");
        data[pos + 4..pos + 8].copy_from_slice(&1000u32.to_le_bytes());
        data[pos + 8..pos + 12].copy_from_slice(b"WAVE");
        for i in 12..1012 {
            data[pos + i] = ((i * 17) % 256) as u8;
        }
        expected.push(("wav", pos as u64));

        // 6. MP3 con ID3
        let pos = 40960;
        data[pos..pos + 3].copy_from_slice(&[0x49, 0x44, 0x33]);
        for i in 3..800 {
            data[pos + i] = ((i * 23) % 256) as u8;
        }
        expected.push(("mp3", pos as u64));

        // 7. OGG Vorbis - NO debe confundirse con OPUS
        let pos = 49152;
        data[pos..pos + 4].copy_from_slice(b"OggS");
        data[pos + 4] = 0; // version
        data[pos + 5] = 0x02; // header type
        data[pos + 26] = 1; // 1 segment
        data[pos + 27] = 30; // segment length
        data[pos + 28..pos + 35].copy_from_slice(&[0x01, 0x76, 0x6F, 0x72, 0x62, 0x69, 0x73]);
        for i in 35..800 {
            data[pos + i] = ((i * 29) % 256) as u8;
        }
        expected.push(("ogg", pos as u64));

        // 8. OPUS - NO debe confundirse con OGG Vorbis
        let pos = 57344;
        data[pos..pos + 4].copy_from_slice(b"OggS");
        data[pos + 4] = 0;
        data[pos + 5] = 0x02;
        data[pos + 26] = 1;
        data[pos + 27] = 19;
        data[pos + 28..pos + 36].copy_from_slice(&[0x4F, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64]);
        for i in 36..800 {
            data[pos + i] = ((i * 31) % 256) as u8;
        }
        expected.push(("opus", pos as u64));

        // 9. GIF
        let pos = 65536;
        data[pos..pos + 6].copy_from_slice(b"GIF89a");
        for i in 6..1500 {
            data[pos + i] = ((i * 37) % 256) as u8;
        }
        data[pos + 1500..pos + 1502].copy_from_slice(&[0x00, 0x3B]);
        expected.push(("gif", pos as u64));

        // 10. FLAC
        let pos = 73728;
        data[pos..pos + 4].copy_from_slice(&[0x66, 0x4C, 0x61, 0x43]);
        for i in 4..900 {
            data[pos + i] = ((i * 41) % 256) as u8;
        }
        expected.push(("flac", pos as u64));

        file.write_all(&data).unwrap();
        file.flush().unwrap();

        (file, expected)
    }

    #[test]
    fn test_scan_detects_all_signatures() {
        let (file, expected) = create_test_image();
        let all_categories = vec![
            FileCategory::Photo,
            FileCategory::Video,
            FileCategory::Audio,
        ];
        let sigs = signatures_for_categories(&all_categories);

        let result = scan_source(file.path(), &sigs).unwrap();

        println!("\n=== Archivos encontrados ===");
        for f in &result.found_files {
            println!(
                "  {} @ 0x{:X} ({})",
                f.signature.extension, f.offset, f.signature.name
            );
        }

        // Verificar que cada firma esperada fue encontrada
        for (ext, offset) in &expected {
            let found = result
                .found_files
                .iter()
                .any(|f| f.signature.extension == *ext && f.offset == *offset);
            assert!(found, "No se encontro {} en offset 0x{:X}", ext, offset);
        }

        println!(
            "\nTodas las {} firmas detectadas correctamente.",
            expected.len()
        );
    }

    #[test]
    fn test_riff_disambiguation() {
        let (file, _) = create_test_image();
        let all_categories = vec![
            FileCategory::Photo,
            FileCategory::Video,
            FileCategory::Audio,
        ];
        let sigs = signatures_for_categories(&all_categories);

        let result = scan_source(file.path(), &sigs).unwrap();

        // En offset 16384 (WebP) NO debe haber AVI ni WAV
        let webp_offset = 16384u64;
        let at_webp: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == webp_offset)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_webp, vec!["webp"], "Offset WebP tiene: {:?}", at_webp);

        // En offset 24576 (AVI) NO debe haber WebP ni WAV
        let avi_offset = 24576u64;
        let at_avi: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == avi_offset)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_avi, vec!["avi"], "Offset AVI tiene: {:?}", at_avi);

        // En offset 32768 (WAV) NO debe haber WebP ni AVI
        let wav_offset = 32768u64;
        let at_wav: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == wav_offset)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_wav, vec!["wav"], "Offset WAV tiene: {:?}", at_wav);

        println!("\nDesambiguacion RIFF correcta: WebP, AVI y WAV detectados sin confusion.");
    }

    #[test]
    fn test_ogg_opus_disambiguation() {
        let (file, _) = create_test_image();
        let all_categories = vec![FileCategory::Audio];
        let sigs = signatures_for_categories(&all_categories);

        let result = scan_source(file.path(), &sigs).unwrap();

        // En offset 49152 solo debe haber OGG, no OPUS
        let ogg_offset = 49152u64;
        let at_ogg: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == ogg_offset)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_ogg, vec!["ogg"], "Offset OGG tiene: {:?}", at_ogg);

        // En offset 57344 solo debe haber OPUS, no OGG
        let opus_offset = 57344u64;
        let at_opus: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == opus_offset)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_opus, vec!["opus"], "Offset OPUS tiene: {:?}", at_opus);

        println!("\nDesambiguacion OGG/OPUS correcta.");
    }

    #[test]
    fn test_jpeg_footer_detection() {
        let (file, _) = create_test_image();
        let sigs = signatures_for_categories(&[FileCategory::Photo]);

        let result = scan_source(file.path(), &sigs).unwrap();

        let jpeg = result
            .found_files
            .iter()
            .find(|f| f.signature.extension == "jpg")
            .expect("JPEG no encontrado");

        // El footer FFD9 esta a 2050 bytes del inicio del JPEG
        assert_eq!(
            jpeg.size, 2050,
            "Tamano JPEG deberia ser 2050, es {}",
            jpeg.size
        );
        println!(
            "\nFooter JPEG detectado correctamente: {} bytes.",
            jpeg.size
        );
    }

    #[test]
    fn test_recovery() {
        let (file, _) = create_test_image();
        let all_categories = vec![
            FileCategory::Photo,
            FileCategory::Video,
            FileCategory::Audio,
        ];
        let sigs = signatures_for_categories(&all_categories);

        let result = scan_source(file.path(), &sigs).unwrap();

        let output_dir = tempfile::tempdir().unwrap();
        let recovery =
            crate::recovery::recover_files(file.path(), &result.found_files, output_dir.path())
                .unwrap();

        assert_eq!(
            recovery.failed, 0,
            "Hubo {} fallos de recuperacion",
            recovery.failed
        );
        assert!(recovery.recovered > 0, "No se recupero ningun archivo");
        assert!(recovery.total_bytes > 0, "0 bytes recuperados");

        // Verificar que se crearon las subcarpetas
        assert!(output_dir.path().join("fotos").exists());
        assert!(output_dir.path().join("videos").exists());
        assert!(output_dir.path().join("audios").exists());

        println!(
            "\nRecuperacion exitosa: {} archivos, {} bytes.",
            recovery.recovered, recovery.total_bytes
        );
    }

    // ═══════════════════════ TESTS MULTI-HILO ═══════════════════════

    #[test]
    fn test_segment_calculation() {
        let overlap = 36u64;

        // Probar con thread counts pares e impares, y tamaños alineados y no-alineados
        let cases: Vec<(u64, &[usize])> = vec![
            (100 * 1024 * 1024, &[2, 3, 4, 5, 7, 8]), // 100 MB exacto
            (100 * 1024 * 1024 + 1, &[2, 3, 5, 7]),   // 100 MB + 1 byte
            (17 * 1024 * 1024 + 12345, &[2, 3]),      // ~17 MB no alineado
        ];

        for (file_size, thread_counts) in &cases {
            for &num_threads in *thread_counts {
                let segments = calculate_segments(*file_size, num_threads, overlap);
                assert_eq!(segments.len(), num_threads);

                // Las zonas claim cubren todo el archivo sin gaps
                assert_eq!(
                    segments[0].claim_start, 0,
                    "file_size={}, threads={}: primer claim no empieza en 0",
                    file_size, num_threads
                );
                assert_eq!(
                    segments[num_threads - 1].claim_end,
                    *file_size,
                    "file_size={}, threads={}: ultimo segmento no llega a file_size",
                    file_size,
                    num_threads
                );
                for i in 1..num_threads {
                    assert_eq!(
                        segments[i].claim_start,
                        segments[i - 1].claim_end,
                        "file_size={}, threads={}: gap entre segmento {} y {}",
                        file_size,
                        num_threads,
                        i - 1,
                        i
                    );
                }

                // Las zonas de lectura incluyen overlap
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        assert!(
                            seg.start <= seg.claim_start,
                            "file_size={}, threads={}: segmento {} start {} > claim_start {}",
                            file_size,
                            num_threads,
                            i,
                            seg.start,
                            seg.claim_start
                        );
                    }
                    assert!(
                        seg.end >= seg.claim_end,
                        "file_size={}, threads={}: segmento {} end {} < claim_end {}",
                        file_size,
                        num_threads,
                        i,
                        seg.end,
                        seg.claim_end
                    );
                }

                // No hay zonas claim vacías
                for (i, seg) in segments.iter().take(num_threads).enumerate() {
                    assert!(
                        seg.claim_start < seg.claim_end,
                        "file_size={}, threads={}: zona claim vacia en segmento {}",
                        file_size,
                        num_threads,
                        i
                    );
                }
            }
        }
    }

    #[test]
    fn test_thread_count_selection() {
        use std::path::PathBuf;

        // Dispositivos físicos → siempre 1
        assert_eq!(
            select_thread_count(&PathBuf::from("\\\\.\\PhysicalDrive0"), 1_000_000_000),
            1
        );
        assert_eq!(
            select_thread_count(&PathBuf::from("/dev/sda"), 1_000_000_000),
            1
        );
        assert_eq!(
            select_thread_count(&PathBuf::from("/dev/nvme0n1p2"), 500_000_000_000),
            1
        );

        // Archivo pequeño (< 16 MB) → siempre 1
        assert_eq!(
            select_thread_count(&PathBuf::from("small.img"), 10 * 1024 * 1024),
            1
        );
        assert_eq!(
            select_thread_count(&PathBuf::from("small.img"), 15 * 1024 * 1024),
            1
        );

        // Archivo grande → depende de cores disponibles
        let cpu_cores = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        let count = select_thread_count(&PathBuf::from("large.img"), 1_000_000_000);

        if cpu_cores > 1 {
            assert!(
                count > 1,
                "Esperaba >1 hilo para 1GB en maquina multi-core, obtuve {}",
                count
            );
        }
        assert!(count <= 8, "Esperaba <=8 hilos, obtuve {}", count);
        assert!(
            count <= cpu_cores,
            "No debe exceder cores disponibles: {} > {}",
            count,
            cpu_cores
        );
        assert!(count >= 1, "Siempre al menos 1 hilo");

        // Archivo de exactamente 16 MB → 1 hilo (by_size = 16MB/16MB = 1)
        let count_16 = select_thread_count(&PathBuf::from("medium.img"), 16 * 1024 * 1024);
        assert_eq!(count_16, 1, "16MB exacto deberia dar 1 hilo (by_size=1)");

        // Archivo de 32 MB → max 2 hilos (by_size = 32/16 = 2)
        let count_32 = select_thread_count(&PathBuf::from("medium.img"), 32 * 1024 * 1024);
        assert!(
            count_32 <= 2,
            "32MB no deberia dar mas de 2 hilos, obtuve {}",
            count_32
        );
    }

    #[test]
    fn test_multithreaded_scan_consistency() {
        // Usar la imagen de test con TODAS las categorías (incluye RIFF/OggS disambiguation)
        let (file, expected) = create_test_image();
        let all_categories = vec![
            FileCategory::Photo,
            FileCategory::Video,
            FileCategory::Audio,
        ];
        let sigs = signatures_for_categories(&all_categories);

        // Referencia: resultado single-threaded
        let result_1 = scan_source_with_threads(file.path(), &sigs, 1).unwrap();

        // Probar con thread counts pares e impares
        for num_threads in [2, 3, 4, 7] {
            let result_n = scan_source_with_threads(file.path(), &sigs, num_threads).unwrap();

            // Mismo número de archivos
            assert_eq!(
                result_1.found_files.len(),
                result_n.found_files.len(),
                "1 hilo encontró {}, {} hilos encontraron {}",
                result_1.found_files.len(),
                num_threads,
                result_n.found_files.len()
            );

            // Comparar campo por campo: offset, extension y size
            for (f1, fn_) in result_1.found_files.iter().zip(result_n.found_files.iter()) {
                assert_eq!(
                    f1.offset, fn_.offset,
                    "Offset difiere con {} hilos: 0x{:X} vs 0x{:X}",
                    num_threads, f1.offset, fn_.offset
                );
                assert_eq!(
                    f1.signature.extension, fn_.signature.extension,
                    "Extension difiere en offset 0x{:X} con {} hilos: {} vs {}",
                    f1.offset, num_threads, f1.signature.extension, fn_.signature.extension
                );
                assert_eq!(
                    f1.size, fn_.size,
                    "Size difiere en offset 0x{:X} ({}) con {} hilos: {} vs {}",
                    f1.offset, f1.signature.extension, num_threads, f1.size, fn_.size
                );
            }

            // Verificar que todas las firmas esperadas están presentes
            for (ext, offset) in &expected {
                assert!(
                    result_n
                        .found_files
                        .iter()
                        .any(|f| f.signature.extension == *ext && f.offset == *offset),
                    "No se encontró {} en 0x{:X} con {} hilos",
                    ext,
                    offset,
                    num_threads
                );
            }

            // Conteos por categoría deben coincidir
            assert_eq!(
                result_1.photos_count, result_n.photos_count,
                "photos_count difiere con {} hilos",
                num_threads
            );
            assert_eq!(
                result_1.videos_count, result_n.videos_count,
                "videos_count difiere con {} hilos",
                num_threads
            );
            assert_eq!(
                result_1.audios_count, result_n.audios_count,
                "audios_count difiere con {} hilos",
                num_threads
            );
            assert_eq!(
                result_1.documents_count, result_n.documents_count,
                "documents_count difiere con {} hilos",
                num_threads
            );
        }
    }

    #[test]
    fn test_signature_at_segment_boundary() {
        let file_size = 20 * 1024 * 1024usize;
        let mut data = vec![0u8; file_size];

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let overlap = max_signature_reach(&sigs) as u64;

        // Calcular dónde estaría la frontera para 2 hilos
        let segments = calculate_segments(file_size as u64, 2, overlap);
        let boundary = segments[0].claim_end as usize;

        // JPEG exactamente en la frontera (claim_start del segmento 1)
        if boundary + 2050 < file_size {
            data[boundary..boundary + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
            for i in 3..2048 {
                data[boundary + i] = ((i * 7) % 256) as u8;
            }
            data[boundary + 2048..boundary + 2050].copy_from_slice(&[0xFF, 0xD9]);
        }

        // JPEG bien antes de la frontera (en segmento 0)
        let before = 1024usize;
        data[before..before + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2048 {
            data[before + i] = ((i * 13) % 256) as u8;
        }
        data[before + 2048..before + 2050].copy_from_slice(&[0xFF, 0xD9]);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let result = scan_source_with_threads(file.path(), &sigs, 2).unwrap();

        let found_at_boundary = result
            .found_files
            .iter()
            .any(|f| f.offset == boundary as u64);
        let found_before = result.found_files.iter().any(|f| f.offset == before as u64);

        assert!(
            found_at_boundary,
            "Firma en frontera 0x{:X} no encontrada",
            boundary
        );
        assert!(
            found_before,
            "Firma antes de frontera 0x{:X} no encontrada",
            before
        );

        println!(
            "\nFirma en frontera de segmento 0x{:X} detectada correctamente.",
            boundary
        );
    }

    /// Regresión: un escaneo multi-hilo tiene que terminar aunque OTRO escaneo del mismo proceso
    /// esté reseteando el contador global de progreso.
    ///
    /// El hilo monitor salía del loop con `if pos >= file_size`, leyendo el global
    /// `SCAN_PROGRESS_BYTES`. Como cualquier escaneo que arranca lo pone en 0, un reset que caía
    /// en la ventana equivocada dejaba al monitor girando para siempre y a `join()` colgado. En el
    /// CI de macOS esto colgó el job 6 h en `test_signature_at_segment_boundary` (los tests corren
    /// en paralelo en un mismo proceso). Acá se fuerza el escenario a propósito: se martilla el
    /// global con escaneos concurrentes mientras corre el multi-hilo, y se exige que termine.
    #[test]
    fn test_multithread_scan_terminates_despite_concurrent_progress_resets() {
        use std::sync::mpsc;

        // Origen grande: fuerza el camino multi-hilo, el único que tiene hilo monitor.
        let mut big = tempfile::NamedTempFile::new().unwrap();
        big.write_all(&vec![0u8; 20 * 1024 * 1024]).unwrap();
        big.flush().unwrap();

        // Origen chico: cada escaneo sobre él resetea SCAN_PROGRESS_BYTES a 0.
        let mut small = tempfile::NamedTempFile::new().unwrap();
        small.write_all(&vec![0u8; 64 * 1024]).unwrap();
        small.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);

        let stop = Arc::new(AtomicBool::new(false));
        let hammer_stop = stop.clone();
        let small_path = small.path().to_path_buf();
        let hammer_sigs = sigs.clone();
        let hammer = std::thread::spawn(move || {
            while !hammer_stop.load(Ordering::Relaxed) {
                let _ = scan_source_quiet(&small_path, &hammer_sigs);
            }
        });

        let (tx, rx) = mpsc::channel();
        let big_path = big.path().to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(scan_source_with_threads(&big_path, &sigs, 2).is_ok());
        });

        let finished = rx.recv_timeout(std::time::Duration::from_secs(60));

        // Frenar el martilleo ANTES de cualquier assert: si el assert falla y paniquea, no puede
        // quedar un hilo girando para el resto de la suite.
        stop.store(true, Ordering::Relaxed);
        hammer.join().unwrap();

        // A propósito no se hace join del hilo del escaneo: si el bug está vivo, ese hilo está
        // colgado y el join colgaría el test en vez de hacerlo fallar.
        assert!(
            finished.is_ok(),
            "El escaneo multi-hilo no terminó en 60s: el hilo monitor quedó girando porque su \
             condición de salida depende del contador global de progreso."
        );
    }

    // ═══════════════ Tests de regresión: fix v2 de A1 (footer JPEG) y C2 (validators) ═══

    #[test]
    fn test_jpeg_with_embedded_exif_thumbnail_recovers_full_size() {
        let mut data = vec![0u8; 200 * 1024];
        let pos = 1000usize;
        // Header real
        data[pos..pos + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        // Un thumbnail EXIF embebido: SOI...EOI completo poco después del header real
        let thumb = pos + 50;
        data[thumb..thumb + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..200 {
            data[thumb + i] = ((i * 3) % 256) as u8;
        }
        data[thumb + 200..thumb + 202].copy_from_slice(&[0xFF, 0xD9]); // EOI del thumbnail
                                                                       // Resto de datos de la foto real, más largo, con el EOI real mucho más lejos.
                                                                       // `i` se usa como índice Y como valor del byte, así que el range-loop es intencional.
        #[allow(clippy::needless_range_loop)]
        for i in (thumb + 202)..pos + 100_000 {
            data[i] = ((i * 5) % 256) as u8;
        }
        let real_footer = pos + 100_000;
        data[real_footer..real_footer + 2].copy_from_slice(&[0xFF, 0xD9]); // EOI real

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let result = scan_source(file.path(), &sigs).unwrap();
        let jpeg = result
            .found_files
            .iter()
            .find(|f| f.signature.extension == "jpg" && f.offset == pos as u64)
            .expect("JPEG no encontrado");

        println!("size = {}, expected = {}", jpeg.size, real_footer + 2 - pos);
        assert_eq!(jpeg.size as usize, real_footer + 2 - pos);
    }

    #[test]
    fn test_two_jpegs_in_same_buffer_do_not_englobe() {
        let mut data = vec![0u8; 20 * 1024];
        let pos1 = 100usize;
        data[pos1..pos1 + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2000 {
            data[pos1 + i] = ((i * 7) % 256) as u8;
        }
        data[pos1 + 2000..pos1 + 2002].copy_from_slice(&[0xFF, 0xD9]);

        let pos2 = pos1 + 2100;
        data[pos2..pos2 + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2000 {
            data[pos2 + i] = ((i * 11) % 256) as u8;
        }
        data[pos2 + 2000..pos2 + 2002].copy_from_slice(&[0xFF, 0xD9]);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let result = scan_source(file.path(), &sigs).unwrap();
        let jpeg1 = result
            .found_files
            .iter()
            .find(|f| f.offset == pos1 as u64)
            .unwrap();
        println!("jpeg1 size = {} expected 2002", jpeg1.size);
        assert_eq!(jpeg1.size, 2002, "jpeg1 englobó al segundo archivo");
    }

    #[test]
    fn test_find_footer_sequential_is_cancelable() {
        // Regresión (auditoría pre-beta): el segundo pase de footer debe cortar ante la
        // cancelación en vez de seguir releyendo un disco que puede estar fallando. Con el flag en
        // true no devuelve nada aunque el footer esté presente; sin cancelar sí lo encuentra.
        let mut data = vec![b'A', b'B']; // header
        data.extend(vec![0x11u8; 4000]);
        data.extend_from_slice(b"ZZ"); // footer
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let end = data.len() as u64;
        let cancelled = AtomicBool::new(true);
        assert!(
            find_footer_sequential(
                file.path(),
                0,
                2,
                end,
                b"AB",
                b"ZZ",
                4 * 1024 * 1024,
                &cancelled
            )
            .is_none(),
            "con cancel=true no debe buscar ni encontrar el footer"
        );

        let not_cancelled = AtomicBool::new(false);
        assert!(
            find_footer_sequential(
                file.path(),
                0,
                2,
                end,
                b"AB",
                b"ZZ",
                4 * 1024 * 1024,
                &not_cancelled
            )
            .is_some(),
            "sin cancelar debe encontrar el footer presente"
        );
    }

    #[test]
    fn test_mp3_aac_frame_chaining_rejects_most_random_data() {
        let sigs = signatures_for_categories(&[FileCategory::Audio]);
        let mp3_sig = sigs.iter().find(|s| s.name == "MP3 (Sync)").unwrap();
        let aac_sig = sigs.iter().find(|s| s.name == "AAC").unwrap();

        // xorshift64 simple, sin dependencias externas, solo para este test de verificación.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let trials = 200_000usize;
        let mut mp3_pass = 0usize;
        let mut aac_pass = 0usize;
        // Suficientemente grande para cubrir el frame_len máximo posible (MP3 320kbps/32kHz
        // ~1441 bytes; AAC frame_length es un campo de 13 bits, hasta 8191 bytes) y así
        // ejercitar de verdad el chequeo de frame chaining, no solo el camino de "no hay
        // suficiente buffer, aceptar".
        let mut buf = [0u8; 8300];
        for _ in 0..trials {
            for chunk in buf.chunks_mut(8) {
                let v = next().to_le_bytes();
                chunk.copy_from_slice(&v[..chunk.len()]);
            }
            buf[0] = 0xFF;
            buf[1] = 0xFB;
            if let Some((f, _)) = mp3_sig.validator {
                if f(&buf) {
                    mp3_pass += 1;
                }
            }
            buf[1] = 0xF1;
            if let Some((f, _)) = aac_sig.validator {
                if f(&buf) {
                    aac_pass += 1;
                }
            }
        }
        let mp3_pct = mp3_pass as f64 / trials as f64 * 100.0;
        let aac_pct = aac_pass as f64 / trials as f64 * 100.0;
        println!(
            "mp3 pass rate = {:.4}%  aac pass rate = {:.4}%",
            mp3_pct, aac_pct
        );

        // Antes del frame chaining (solo bits reservados) pasaba ~60-65% de datos aleatorios;
        // con frame chaining debe caer a un porcentaje marginal (umbral generoso para no ser
        // frágil ante variaciones del PRNG determinístico usado arriba).
        assert!(
            mp3_pct < 5.0,
            "MP3 sync validator deja pasar demasiados falsos positivos: {:.4}%",
            mp3_pct
        );
        assert!(
            aac_pct < 5.0,
            "AAC ADTS validator deja pasar demasiados falsos positivos: {:.4}%",
            aac_pct
        );
    }

    #[test]
    fn test_tiff_big_endian_detection() {
        // Archivo propio (no create_test_image) porque TIFF no tiene footer ni
        // size_from_header: se carvea hasta max_size o fin de fuente, así que se aisla en un
        // buffer chico para no interferir con los offsets de otras firmas.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut data = vec![0u8; 4096];
        let pos = 512usize;
        // Motorola byte order: "MM" + 0x002A
        data[pos..pos + 4].copy_from_slice(&[0x4D, 0x4D, 0x00, 0x2A]);
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let tiff_be = result
            .found_files
            .iter()
            .find(|f| f.offset == pos as u64)
            .expect("TIFF big-endian no encontrado");
        assert_eq!(tiff_be.signature.extension, "tiff");
        assert_eq!(tiff_be.signature.name, "TIFF (big-endian)");

        println!(
            "\nTIFF big-endian (MM*) detectado correctamente en offset 0x{:X}.",
            pos
        );
    }

    #[test]
    fn test_heic_mp4_disambiguation() {
        // HEIC y MP4/M4V comparten la misma caja contenedora ftyp (ISOBMFF); solo el
        // major_brand (4 bytes tras "ftyp") los distingue. Verifica que un archivo HEIC no se
        // detecte tambien como MP4, y viceversa.
        let mut data = vec![0u8; 4096];

        // HEIC: box size (4) + "ftyp" + "heic" (major_brand)
        let heic_pos = 256usize;
        data[heic_pos + 4..heic_pos + 8].copy_from_slice(b"ftyp");
        data[heic_pos + 8..heic_pos + 12].copy_from_slice(b"heic");

        // MP4: box size (4) + "ftyp" + "isom" (major_brand, no HEIC)
        let mp4_pos = 1024usize;
        data[mp4_pos + 4..mp4_pos + 8].copy_from_slice(b"ftyp");
        data[mp4_pos + 8..mp4_pos + 12].copy_from_slice(b"isom");

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo, FileCategory::Video]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let at_heic: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == heic_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_heic, vec!["heic"], "Offset HEIC tiene: {:?}", at_heic);

        let at_mp4: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == mp4_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_mp4, vec!["mp4"], "Offset MP4 tiene: {:?}", at_mp4);

        println!("\nDesambiguacion HEIC/MP4 (ftyp) correcta.");
    }

    #[test]
    fn test_cr2_tiff_disambiguation() {
        // CR2 (Canon RAW) es TIFF little-endian con un marcador propio "CR\x02\x00" en offset
        // 8. Verifica que un CR2 no se detecte tambien como TIFF generico, y viceversa.
        let mut data = vec![0u8; 4096];

        // CR2: "II*\0" + puntero IFD0 (4 bytes, cualquier valor) + "CR\x02\x00"
        let cr2_pos = 256usize;
        data[cr2_pos..cr2_pos + 4].copy_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
        data[cr2_pos + 8..cr2_pos + 12].copy_from_slice(b"CR\x02\x00");

        // TIFF generico: "II*\0" + puntero IFD0 + datos que NO son el marcador CR2
        let tiff_pos = 1024usize;
        data[tiff_pos..tiff_pos + 4].copy_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
        data[tiff_pos + 8..tiff_pos + 12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let at_cr2: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == cr2_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_cr2, vec!["cr2"], "Offset CR2 tiene: {:?}", at_cr2);

        let at_tiff: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == tiff_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_tiff, vec!["tiff"], "Offset TIFF tiene: {:?}", at_tiff);

        println!("\nDesambiguacion CR2/TIFF correcta.");
    }

    #[test]
    fn test_pdf_header_footer_detection() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut data = vec![0u8; 4096];
        let pos = 512usize;
        data[pos..pos + 5].copy_from_slice(b"%PDF-");
        for i in 5..800 {
            data[pos + i] = ((i * 19) % 256) as u8;
        }
        data[pos + 800..pos + 805].copy_from_slice(b"%%EOF");
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Document]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let pdf = result
            .found_files
            .iter()
            .find(|f| f.offset == pos as u64)
            .expect("PDF no encontrado");
        assert_eq!(pdf.signature.extension, "pdf");
        assert_eq!(pdf.size, 805, "Tamano PDF deberia ser 805, es {}", pdf.size);

        println!("\nPDF detectado correctamente con header %PDF- y footer %%EOF.");
    }

    #[test]
    fn test_3gp_m4a_not_duplicated_as_mp4() {
        // 3GP y M4A comparten la misma caja ftyp que "MP4/M4V" (mismo header, mismo offset).
        // Antes del fix, un .3gp o .m4a real se detectaba DOS veces: una vez bajo su propia
        // firma y otra, redundante, bajo "MP4/M4V". Verifica que ya no pase.
        let mut data = vec![0u8; 4096];

        // 3GP: box size (4) + "ftyp" + "3gp4" (major_brand, digito de version variable)
        let gp3_pos = 256usize;
        data[gp3_pos + 4..gp3_pos + 8].copy_from_slice(b"ftyp");
        data[gp3_pos + 8..gp3_pos + 12].copy_from_slice(b"3gp4");

        // M4A: box size (4) + "ftyp" + "M4A " (major_brand exacto, con espacio final)
        let m4a_pos = 1024usize;
        data[m4a_pos + 4..m4a_pos + 8].copy_from_slice(b"ftyp");
        data[m4a_pos + 8..m4a_pos + 12].copy_from_slice(b"M4A ");

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Video, FileCategory::Audio]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let at_3gp: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == gp3_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_3gp, vec!["3gp"], "Offset 3GP tiene: {:?}", at_3gp);

        let at_m4a: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == m4a_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_m4a, vec!["m4a"], "Offset M4A tiene: {:?}", at_m4a);

        println!("\n3GP y M4A ya no se duplican bajo MP4/M4V.");
    }

    #[test]
    fn test_heic_hevm_hevs_brands_detected_as_heic_not_mp4() {
        // hevm/hevs son los brands HEIC/HEIF reales para secuencias HEVC multiview/escalables
        // (ISO/IEC 23008-12). La sesion anterior habia tipeado "hejc"/"hejs" por error, lo que
        // hacia que estos brands cayeran (incorrectamente) en la deteccion generica MP4/M4V en
        // vez de HEIC.
        let mut data = vec![0u8; 4096];

        let hevm_pos = 256usize;
        data[hevm_pos + 4..hevm_pos + 8].copy_from_slice(b"ftyp");
        data[hevm_pos + 8..hevm_pos + 12].copy_from_slice(b"hevm");

        let hevs_pos = 1024usize;
        data[hevs_pos + 4..hevs_pos + 8].copy_from_slice(b"ftyp");
        data[hevs_pos + 8..hevs_pos + 12].copy_from_slice(b"hevs");

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo, FileCategory::Video]);
        let result = scan_source(file.path(), &sigs).unwrap();

        let at_hevm: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == hevm_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_hevm, vec!["heic"], "Offset hevm tiene: {:?}", at_hevm);

        let at_hevs: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == hevs_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(at_hevs, vec!["heic"], "Offset hevs tiene: {:?}", at_hevs);

        println!("\nBrands hevm/hevs detectados correctamente como HEIC, no MP4.");
    }

    /// Arma un BMP estructuralmente valido (BITMAPFILEHEADER + BITMAPINFOHEADER de 40 bytes,
    /// sin comprimir) del tamano que indican las dimensiones.
    fn make_bmp(width: i32, height: i32, bpp: u16) -> Vec<u8> {
        let row = (((width as i64 * bpp as i64 + 31) / 32) * 4) as usize;
        let pixel_offset = 54u32;
        let file_size = pixel_offset as usize + row * height as usize;
        let mut b = vec![0u8; file_size];
        b[0..2].copy_from_slice(b"BM");
        b[2..6].copy_from_slice(&(file_size as u32).to_le_bytes());
        // 6..10 = reservados, en cero
        b[10..14].copy_from_slice(&pixel_offset.to_le_bytes());
        b[14..18].copy_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        b[18..22].copy_from_slice(&width.to_le_bytes());
        b[22..26].copy_from_slice(&height.to_le_bytes());
        b[26..28].copy_from_slice(&1u16.to_le_bytes()); // planos: siempre 1
        b[28..30].copy_from_slice(&bpp.to_le_bytes());
        // 30..34 = compresion 0 (sin comprimir)
        b
    }

    #[test]
    fn test_bmp_header_validator() {
        // BMP tenia header de solo 2 bytes ("BM") sin validador, y ademas usa
        // size_from_header: en un falso positivo, leia 4 bytes de basura como tamano total del
        // archivo sin ninguna validacion. Verifica que un BITMAPFILEHEADER coherente SI se
        // detecte, y que "BM" con campos incoherentes (tipico de datos aleatorios) NO se
        // detecte.
        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let bmp_sig = sigs.iter().find(|s| s.name == "BMP").unwrap();
        let (validator_fn, _needed) = bmp_sig.validator.expect("BMP deberia tener validator");

        // Caso valido: un BMP de 10x10 a 24 bits, con BITMAPFILEHEADER + BITMAPINFOHEADER
        // completos. El fixture viejo era solo el BITMAPFILEHEADER con el encabezado DIB en CEROS
        // —algo que ningun BMP real es— y pasaba porque el validador no miraba mas alla de los
        // primeros 14 bytes.
        let valid = make_bmp(10, 10, 24);
        assert!(
            validator_fn(&valid),
            "BMP valido deberia pasar el validador"
        );

        // Caso invalido: bfOffBits mayor que bfSize (estructuralmente imposible en un BMP real).
        let mut bad_offset = vec![0u8; 100];
        bad_offset[0] = 0x42;
        bad_offset[1] = 0x4D;
        bad_offset[2..6].copy_from_slice(&100u32.to_le_bytes());
        bad_offset[10..14].copy_from_slice(&500u32.to_le_bytes());
        assert!(
            !validator_fn(&bad_offset),
            "bfOffBits > bfSize deberia rechazarse"
        );

        // Caso invalido: bfSize absurdamente grande (mayor al max_size de la firma).
        let mut bad_size = vec![0u8; 100];
        bad_size[0] = 0x42;
        bad_size[1] = 0x4D;
        bad_size[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        bad_size[10..14].copy_from_slice(&54u32.to_le_bytes());
        assert!(
            !validator_fn(&bad_size),
            "bfSize absurdo deberia rechazarse"
        );

        // Fin a fin: un BMP valido embebido en un buffer se detecta via scan_source, y datos
        // aleatorios con "BM" al inicio pero campos incoherentes no. bfSize se declara en
        // 1000 (no 200): el scanner descarta cualquier archivo detectado de menos de 512
        // bytes por heuristica anti-falsos-positivos (preexistente, ver "size > 512" en
        // check_signatures_in_buffer) — un bfSize menor a ese umbral haria que el test fallara
        // por esa heuristica no relacionada, no por el validador BMP en si.
        let mut data = vec![0u8; 4096];
        let bmp_pos = 512usize;
        // 20x20 a 24 bits = 1254 bytes: por encima del umbral de 512.
        let bmp = make_bmp(20, 20, 24);
        assert!(bmp.len() > 512, "el fixture debe superar el umbral de 512");
        data[bmp_pos..bmp_pos + bmp.len()].copy_from_slice(&bmp);

        // xorshift64 simple, determinístico, solo para este test (mismo patron que
        // test_mp3_aac_frame_chaining_rejects_most_random_data).
        let mut state: u64 = 0xD1B54A32D192ED03;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let random_bmp_pos = 2048usize;
        for i in 0..64 {
            data[random_bmp_pos + i] = (next() & 0xFF) as u8;
        }
        data[random_bmp_pos] = 0x42;
        data[random_bmp_pos + 1] = 0x4D;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let result = scan_source(file.path(), &sigs).unwrap();

        let at_valid_bmp: Vec<&str> = result
            .found_files
            .iter()
            .filter(|f| f.offset == bmp_pos as u64)
            .map(|f| f.signature.extension)
            .collect();
        assert_eq!(
            at_valid_bmp,
            vec!["bmp"],
            "Offset BMP valido tiene: {:?}",
            at_valid_bmp
        );

        let at_random_bmp = result
            .found_files
            .iter()
            .any(|f| f.offset == random_bmp_pos as u64 && f.signature.extension == "bmp");
        assert!(
            !at_random_bmp,
            "'BM' con campos aleatorios/incoherentes no deberia detectarse como BMP"
        );

        println!("\nValidador BMP acepta headers coherentes y rechaza campos incoherentes.");

        // Casos que la version vieja daba por buenos (y que llenaban la salida de basura: 6 BMP
        // falsos, 148 MB, medidos sobre binarios del sistema).
        let mut dib_basura = make_bmp(10, 10, 24);
        dib_basura[14..18].copy_from_slice(&37u32.to_le_bytes());
        assert!(
            !validator_fn(&dib_basura),
            "el encabezado DIB solo mide unos pocos valores definidos, 37 no es uno"
        );

        let mut planos_raros = make_bmp(10, 10, 24);
        planos_raros[26..28].copy_from_slice(&7u16.to_le_bytes());
        assert!(
            !validator_fn(&planos_raros),
            "los planos son SIEMPRE 1 en BMP"
        );

        let mut bpp_raro = make_bmp(10, 10, 24);
        bpp_raro[28..30].copy_from_slice(&13u16.to_le_bytes());
        assert!(!validator_fn(&bpp_raro), "13 bits por pixel no existe");

        let mut reservados = make_bmp(10, 10, 24);
        reservados[6..10].copy_from_slice(&12345u32.to_le_bytes());
        assert!(
            !validator_fn(&reservados),
            "los campos reservados valen 0 en cualquier BMP real"
        );

        // Tamano declarado que no alcanza para los pixeles que dicen las dimensiones: un BMP de
        // 1000x1000 a 24 bits necesita 3 MB, no 400 bytes.
        let mut incoherente = make_bmp(10, 10, 24);
        incoherente[18..22].copy_from_slice(&1000i32.to_le_bytes());
        incoherente[22..26].copy_from_slice(&1000i32.to_le_bytes());
        assert!(
            !validator_fn(&incoherente),
            "el tamano declarado tiene que alcanzar para los pixeles"
        );

        // Y el alto negativo (fila superior primero) es LEGAL: no se puede rechazar.
        let mut arriba_abajo = make_bmp(10, 10, 24);
        arriba_abajo[22..26].copy_from_slice(&(-10i32).to_le_bytes());
        assert!(
            validator_fn(&arriba_abajo),
            "un BMP con alto negativo es valido (fila superior primero)"
        );
    }

    #[test]
    fn test_scan_cancellation_stops_before_reading() {
        // Un flag de cancelación ya seteado al arrancar debe hacer que scan_segment corte en la
        // primera iteración del loop (el chequeo está ANTES del read), sin encontrar nada. Con
        // el flag en false, el mismo escaneo sí encuentra el JPEG. Usa AtomicBool locales, no el
        // global SCAN_CANCEL_REQUESTED, para no interferir con otros tests que corren en paralelo.
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let mut data = vec![0u8; 4096];
        let pos = 512usize;
        data[pos..pos + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2048 {
            data[pos + i] = ((i * 7) % 256) as u8;
        }
        data[pos + 2048..pos + 2050].copy_from_slice(&[0xFF, 0xD9]);
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let max_header_len = max_signature_reach(&sigs);
        let segment = Segment {
            start: 0,
            end: 4096,
            claim_start: 0,
            claim_end: 4096,
        };

        // Cancelado de entrada → no lee, no encuentra nada.
        let progress = AtomicU64::new(0);
        let files_found = AtomicU64::new(0);
        let cancel_on = AtomicBool::new(true);
        let cancelled = scan_segment(
            file.path(),
            &segment,
            &sigs,
            4096,
            max_header_len,
            &local_progress(&progress, &files_found),
            None,
            &cancel_on,
        );
        assert!(
            cancelled.found_files.is_empty(),
            "con cancelación activa scan_segment no debería encontrar archivos, encontró {}",
            cancelled.found_files.len()
        );
        assert_eq!(
            progress.load(Ordering::SeqCst),
            0,
            "con cancelación activa no debería haber leído ningún byte"
        );

        // Sin cancelar → sí encuentra el JPEG.
        let progress2 = AtomicU64::new(0);
        let files_found2 = AtomicU64::new(0);
        let cancel_off = AtomicBool::new(false);
        let normal = scan_segment(
            file.path(),
            &segment,
            &sigs,
            4096,
            max_header_len,
            &local_progress(&progress2, &files_found2),
            None,
            &cancel_off,
        );
        assert!(
            normal
                .found_files
                .iter()
                .any(|f| f.signature.extension == "jpg"),
            "sin cancelar, scan_segment debería encontrar el JPEG"
        );

        println!(
            "\nCancelación cooperativa corta el escaneo antes de leer y conserva el flujo normal."
        );
    }

    // ═══════════════ Contador en vivo de encontrados + bytes_scanned por escaneo ═══════════

    /// Escribe un JPEG válido (SOI..EOI) de ~2 KB en `data` a partir de `pos`.
    /// A propósito bien por encima de 512 bytes: `check_signatures_in_buffer` descarta todo lo
    /// más chico que eso (anti-falsos-positivos) y el "archivo válido" no se detectaría.
    fn write_jpeg(data: &mut [u8], pos: usize) {
        data[pos..pos + 3].copy_from_slice(&[0xFF, 0xD8, 0xFF]);
        for i in 3..2048 {
            data[pos + i] = ((i * 17) % 256) as u8;
        }
        data[pos + 2048..pos + 2050].copy_from_slice(&[0xFF, 0xD9]);
    }

    /// El contador de "encontrados" se actualiza EN VIVO, bloque a bloque, y cuenta exactamente
    /// los hallazgos que van a sobrevivir (los de la zona exclusiva del segmento). Se usa
    /// `scan_segment` directo con contadores locales para que no dependa de timing.
    #[test]
    fn test_found_counter_counts_findings_in_segment() {
        let size = 3 * 1024 * 1024usize;
        let mut data = vec![0u8; size];
        // Uno por bloque de 1 MB: obliga a que el conteo pase por varias iteraciones del loop.
        write_jpeg(&mut data, 4096);
        write_jpeg(&mut data, 1_500_000);
        write_jpeg(&mut data, 2_500_000);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let max_header_len = max_signature_reach(&sigs);
        let cancel = AtomicBool::new(false);

        // Segmento que reclama todo el origen (camino de 1 hilo).
        let bytes = AtomicU64::new(0);
        let files = AtomicU64::new(0);
        let whole = Segment {
            start: 0,
            end: size as u64,
            claim_start: 0,
            claim_end: size as u64,
        };
        let result = scan_segment(
            file.path(),
            &whole,
            &sigs,
            size as u64,
            max_header_len,
            &local_progress(&bytes, &files),
            None,
            &cancel,
        );
        assert!(
            result.found_files.len() >= 3,
            "deberían detectarse los 3 JPEG, se detectaron {}",
            result.found_files.len()
        );
        assert_eq!(
            files.load(Ordering::Relaxed),
            result.found_files.len() as u64,
            "el contador en vivo debe coincidir con lo que el segmento reporta"
        );
        assert_eq!(bytes.load(Ordering::Relaxed), size as u64);

        // Segmento que LEE todo pero solo reclama la segunda mitad: el JPEG de 4096 se ve por el
        // overlap pero no es suyo, así que no debe sumar al contador (si no, la GUI mostraría un
        // total más alto que la lista final).
        let bytes2 = AtomicU64::new(0);
        let files2 = AtomicU64::new(0);
        let claim_start = 1024 * 1024u64;
        let partial = Segment {
            start: 0,
            end: size as u64,
            claim_start,
            claim_end: size as u64,
        };
        let result2 = scan_segment(
            file.path(),
            &partial,
            &sigs,
            size as u64,
            max_header_len,
            &local_progress(&bytes2, &files2),
            None,
            &cancel,
        );
        assert!(
            result2.found_files.iter().all(|f| f.offset >= claim_start),
            "el segmento no debería reportar hallazgos fuera de su zona exclusiva"
        );
        assert_eq!(
            files2.load(Ordering::Relaxed),
            result2.found_files.len() as u64,
            "el contador en vivo no debe incluir hallazgos de la zona de overlap"
        );
    }

    /// El contador global que lee la GUI (`scan_progress_files`) sube MIENTRAS el escaneo corre,
    /// no solo al final — que es justamente lo que la GUI necesita para mostrar
    /// "Encontrados hasta ahora: N".
    #[test]
    fn test_scan_progress_files_visible_during_scan() {
        // Se muestrean contadores LOCALES, no el global `SCAN_PROGRESS_FILES`.
        //
        // La versión anterior de este test muestreaba el global y era un FALSO VERDE: otros tests
        // corriendo en paralelo escanean y escriben ese mismo global, así que le inyectaban
        // muestras intermedias y el assert se satisfacía con el ruido del vecino. Se comprobó que
        // pasaba en verde incluso borrando por completo el conteo en vivo (y que fallaba con
        // `--test-threads=1`). Es la misma familia de bug que el cuelgue de macOS: atar una
        // aserción a un global mutable compartido entre tests paralelos.
        let size = 24 * 1024 * 1024usize;
        let mut data = vec![0u8; size];
        let mut pos = 4096usize;
        let mut written = 0u64;
        while pos + 4096 < size {
            write_jpeg(&mut data, pos);
            pos += 64 * 1024;
            written += 1;
        }

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let path = file.path().to_path_buf();
        let segment = Segment {
            start: 0,
            end: size as u64,
            claim_start: 0,
            claim_end: size as u64,
        };
        let max_header_len = sigs.iter().map(|s| s.header.len()).max().unwrap_or(0);

        let bytes = Arc::new(AtomicU64::new(0));
        let files = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));

        let (b, f, c) = (bytes.clone(), files.clone(), cancel.clone());
        let scan = std::thread::spawn(move || {
            scan_segment(
                &path,
                &segment,
                &sigs,
                size as u64,
                max_header_len,
                &local_progress(&b, &f),
                None,
                &c,
            )
        });

        // Muestras tomadas mientras el segmento se escanea. Un contador que solo se escribiera al
        // terminar no podría producir NINGUNA muestra estrictamente entre 0 y el total.
        let mut muestras = Vec::new();
        while !scan.is_finished() {
            muestras.push(files.load(Ordering::Relaxed));
            std::thread::yield_now();
        }
        let result = scan.join().unwrap();
        let total = result.found_files.len() as u64;

        assert!(
            total >= written,
            "deberían encontrarse al menos los {written} JPEG escritos, se encontraron {total}"
        );
        assert!(
            muestras.iter().any(|&n| n > 0 && n < total),
            "el contador de encontrados nunca mostró un valor intermedio (total {total}, muestras \
             distintas vistas: {:?}): no se está actualizando en vivo, sino de una sola vez al final",
            {
                let mut u: Vec<u64> = muestras.clone();
                u.sort_unstable();
                u.dedup();
                u
            }
        );
    }

    /// `bytes_scanned` tiene que salir del contador PROPIO del escaneo, no del global que la GUI
    /// lee: si sale del global, otro escaneo concurrente del mismo proceso (los tests corren en
    /// paralelo) lo resetea a 0 en medio y el número reportado es el del vecino. Este test martilla
    /// el global con escaneos de otro tamaño y exige el valor exacto.
    #[test]
    fn test_bytes_scanned_is_per_scan_not_global() {
        // 15 MB: por debajo del umbral de multi-hilo (16 MB), así este escaneo va por el camino de
        // 1 hilo, y dura lo suficiente como para que varios escaneos ajenos arranquen mientras
        // corre (cada arranque pone el global en 0).
        let size = 15 * 1024 * 1024usize;
        let mut data = vec![0u8; size];
        write_jpeg(&mut data, 4096);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        file.flush().unwrap();

        // 20 MB: por encima del umbral, para cubrir también el camino multi-hilo.
        let big_size = 20 * 1024 * 1024usize;
        let mut big_data = vec![0u8; big_size];
        write_jpeg(&mut big_data, 4096);
        let mut big = tempfile::NamedTempFile::new().unwrap();
        big.write_all(&big_data).unwrap();
        big.flush().unwrap();

        // Origen de OTRO tamaño para el martilleo: si `bytes_scanned` se contaminara, el valor
        // observado sería distinto del tamaño real del origen escaneado.
        let mut other = tempfile::NamedTempFile::new().unwrap();
        other.write_all(&vec![0u8; 700 * 1024]).unwrap();
        other.flush().unwrap();

        let sigs = signatures_for_categories(&[FileCategory::Photo]);
        let stop = Arc::new(AtomicBool::new(false));
        let hammer_stop = stop.clone();
        let hammer_path = other.path().to_path_buf();
        let hammer_sigs = sigs.clone();
        let hammer = std::thread::spawn(move || {
            while !hammer_stop.load(Ordering::Relaxed) {
                let _ = scan_source_quiet(&hammer_path, &hammer_sigs);
            }
        });

        let mut observed = Vec::new();
        for _ in 0..3 {
            observed.push((
                scan_source_quiet(file.path(), &sigs).unwrap().bytes_scanned,
                size as u64,
            ));
            observed.push((
                scan_source_with_threads(big.path(), &sigs, 4)
                    .unwrap()
                    .bytes_scanned,
                big_size as u64,
            ));
        }

        // Frenar el martilleo ANTES de los asserts: un panic no debe dejar un hilo girando para
        // el resto de la suite.
        stop.store(true, Ordering::Relaxed);
        hammer.join().unwrap();

        for (got, expected) in observed {
            assert_eq!(
                got, expected,
                "bytes_scanned se corrompió con escaneos concurrentes"
            );
        }
    }

    // ── Carving de audio por cadena de frames (MP3 / AAC) ──
    //
    // Contexto: MP3 y AAC se detectan por un syncword de ~12 bits y NO tienen footer. Antes se
    // encadenaban 2 frames para validar y, sin final detectable, cada candidato se carveaba hasta
    // `max_size`. Medido sobre 382 MB de binarios del sistema: 286 archivos y 13.5 GB de salida —
    // y sobre 5 MP3/AAC REALES, 2479 archivos de los que NINGUNO coincidía con los originales.

    /// Un frame MPEG1 Layer III de 417 bytes (128 kbps, 44100 Hz, sin padding), relleno con datos
    /// que no son un syncword.
    fn mpeg_frame() -> Vec<u8> {
        let mut f = vec![0xFF, 0xFB, 0x90, 0x00];
        f.resize(417, 0x5A);
        f
    }

    /// Un frame ADTS (AAC) de 200 bytes: profile 1, sample rate index 4, `frame_length` = 200
    /// codificado en los 13 bits que reparten los bytes 3-5.
    fn adts_frame() -> Vec<u8> {
        let mut f = vec![0xFF, 0xF1, 0x50, 0x00, 200 >> 3, 0x00, 0x00];
        f.resize(200, 0x5A);
        f
    }

    fn repeat(frame: &[u8], n: usize) -> Vec<u8> {
        frame.repeat(n)
    }

    fn scan_bytes(data: &[u8], sigs: &[crate::signatures::FileSignature]) -> Vec<FoundFile> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();
        file.flush().unwrap();
        scan_source(file.path(), sigs).unwrap().found_files
    }

    fn audio_sigs() -> Vec<crate::signatures::FileSignature> {
        signatures_for_categories(&[FileCategory::Audio])
    }

    /// Un MP3 real rodeado de datos que no son audio se recupera con su tamaño EXACTO, y no se
    /// carvea hasta `max_size`. Es lo que hacía que 382 MB de origen produjeran 13.5 GB de salida.
    #[test]
    fn test_mp3_se_carvea_con_su_tamano_real_no_hasta_max_size() {
        let mp3 = repeat(&mpeg_frame(), 40);
        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&mp3);
        data.extend_from_slice(&[0x00; 4096]);

        let found = scan_bytes(&data, &audio_sigs());
        let f = found
            .iter()
            .find(|f| f.offset == offset as u64)
            .expect("no se detectó el MP3");

        assert_eq!(f.size, mp3.len() as u64, "el tamaño debe ser el real");
        assert!(f.footer_found, "el final se detectó de verdad");
        assert_eq!(f.integrity(), Integrity::Intact);
    }

    /// Cada frame de un MP3 empieza con el mismo syncword, así que el carving ve un "archivo nuevo"
    /// en CADA frame: un MP3 de 40 frames daba 40 detecciones. Con el tamaño real, el archivo de
    /// verdad se vuelve un contenedor confiable y `suppress_contained` se lleva a los de adentro.
    #[test]
    fn test_los_frames_internos_de_un_mp3_no_salen_como_archivos_sueltos() {
        let mp3 = repeat(&mpeg_frame(), 40);
        let mut data = vec![0u8; 4096];
        data.extend_from_slice(&mp3);
        data.extend_from_slice(&[0x00; 4096]);

        let mp3s = scan_bytes(&data, &audio_sigs()).len();
        assert_eq!(
            mp3s, 1,
            "debe quedar UN archivo, no uno por frame (dieron {mp3s})"
        );
    }

    /// Un AAC real, mismo trato: tamaño exacto y una sola detección.
    #[test]
    fn test_aac_se_carvea_con_su_tamano_real() {
        let aac = repeat(&adts_frame(), 60);
        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&aac);
        data.extend_from_slice(&[0x00; 4096]);

        let found = scan_bytes(&data, &audio_sigs());
        assert_eq!(found.len(), 1, "una sola detección, no una por frame");
        assert_eq!(found[0].offset, offset as u64);
        assert_eq!(found[0].size, aac.len() as u64);
        assert_eq!(found[0].integrity(), Integrity::Intact);
    }

    /// Un syncword suelto en datos que no son audio (el caso que llenaba la salida de basura) ya no
    /// pasa la validación: encadenar 12 frames exige acertar 12 largos calculados y mantener el
    /// sample rate, cosa que los datos binarios no hacen.
    #[test]
    fn test_un_syncword_suelto_en_datos_binarios_no_es_un_audio() {
        // Dos frames válidos —lo que la versión vieja daba por bueno— y después basura.
        let mut data = vec![0u8; 2048];
        data.extend_from_slice(&repeat(&mpeg_frame(), 2));
        for i in 0..8192 {
            data.push(((i * 31 + 7) % 251) as u8);
        }

        let found = scan_bytes(&data, &audio_sigs());
        assert!(
            found.is_empty(),
            "una cadena de 2 frames no alcanza para declarar un MP3 (se detectaron {})",
            found.len()
        );
    }

    /// Un MP3 que ocupa TODO el origen (una tarjeta llena de música) termina justo donde terminan
    /// los datos. Ese final es limpio y debe contar como tal: si se tratara como "me quedé sin
    /// buffer", el archivo saldría marcado "posiblemente dañado" y NO se guardaría por defecto.
    /// Falla real detectada probando con un MP3 de verdad, no con datos sintéticos.
    #[test]
    fn test_un_mp3_que_ocupa_todo_el_origen_no_queda_marcado_danado() {
        let mp3 = repeat(&mpeg_frame(), 40);

        let found = scan_bytes(&mp3, &audio_sigs());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].offset, 0);
        assert_eq!(found[0].size, mp3.len() as u64);
        assert_eq!(
            found[0].integrity(),
            Integrity::Intact,
            "el final del origen ES el final del archivo"
        );
    }

    /// Un MP3 más grande que el buffer de escaneo (1 MB) tiene que cerrar su cadena igual: la
    /// segunda pasada la sigue leyendo del disco. Sin eso, CUALQUIER canción de tamaño normal
    /// quedaba "posiblemente dañada" — o sea, el arreglo de los falsos positivos se habría llevado
    /// puesta la música de verdad.
    #[test]
    fn test_un_mp3_mas_grande_que_el_buffer_cierra_su_cadena_leyendo_del_disco() {
        // ~1.25 MB: cruza el buffer de 1 MB del escaneo.
        let mp3 = repeat(&mpeg_frame(), 3000);
        assert!(mp3.len() > BUFFER_SIZE, "el test debe cruzar el buffer");

        let mut data = vec![0u8; 8192];
        let offset = data.len();
        data.extend_from_slice(&mp3);
        data.extend_from_slice(&[0x00; 8192]);

        let found = scan_bytes(&data, &audio_sigs());
        let f = found
            .iter()
            .find(|f| f.offset == offset as u64)
            .expect("no se detectó el MP3 grande");
        assert_eq!(f.size, mp3.len() as u64, "tamaño exacto cruzando el buffer");
        assert_eq!(f.integrity(), Integrity::Intact);
    }

    /// Un MP3 con etiqueta ID3v2 adelante: la etiqueta no es audio, así que el tamaño es
    /// etiqueta + cadena de frames. (El tamaño de la etiqueta viene "synchsafe": 7 bits por byte.)
    #[test]
    fn test_un_mp3_con_etiqueta_id3_incluye_la_etiqueta_en_su_tamano() {
        let tag_body = 500usize;
        let mut mp3 = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00];
        // 500 = 0b111110100 -> synchsafe en 4 bytes de 7 bits
        mp3.extend_from_slice(&[
            0,
            0,
            ((tag_body >> 7) & 0x7F) as u8,
            (tag_body & 0x7F) as u8,
        ]);
        mp3.resize(mp3.len() + tag_body, 0u8);
        let audio = repeat(&mpeg_frame(), 30);
        mp3.extend_from_slice(&audio);

        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&mp3);
        data.extend_from_slice(&[0x00; 4096]);

        let found = scan_bytes(&data, &audio_sigs());
        let f = found
            .iter()
            .find(|f| f.offset == offset as u64)
            .expect("no se detectó el MP3 con ID3");
        assert_eq!(
            f.size,
            mp3.len() as u64,
            "el tamaño debe cubrir la etiqueta ID3 más todos los frames"
        );
    }

    // ── Regresiones que encontró la revisión adversarial (sesión 15) ──

    /// Un frame MPEG-2 / MPEG-2.5 (22050, 24000, 16000, 12000, 11025, 8000 Hz) tiene otra tabla de
    /// bitrates, otros sample rates y otra fórmula de largo que MPEG-1. El código solo conocía
    /// MPEG-1 y ni siquiera leía los bits de versión, así que a la MITAD del universo de MP3 —justo
    /// la de podcasts, audiolibros y notas de voz— le calculaba mal el largo, la cadena rompía en el
    /// primer frame y el archivo terminaba marcado "posiblemente dañado", o sea sin guardarse.
    #[test]
    fn test_se_reconocen_los_mp3_de_mpeg2_y_mpeg25() {
        // MPEG-2 Layer III, 22050 Hz: h[1]=0xF3 (versión 10, capa 01). h[2]=0x80 -> bitrate_idx 8
        // = 64 kbps en la tabla de MPEG-2, sr_idx 0 = 22050, sin padding.
        // Largo = 72 * 64000 / 22050 = 208 bytes.
        let mut v2 = vec![0xFF, 0xF3, 0x80, 0x00];
        v2.resize(208, 0x5A);
        // MPEG-2.5 Layer III, 11025 Hz: h[1]=0xE3. Largo = 72 * 64000 / 11025 = 417 bytes.
        let mut v25 = vec![0xFF, 0xE3, 0x80, 0x00];
        v25.resize(417, 0x5A);

        for (nombre, frame) in [("MPEG-2 22050 Hz", v2), ("MPEG-2.5 11025 Hz", v25)] {
            // Con etiqueta ID3 delante, que es como los escribe cualquier programa real. NOTA: sin
            // etiqueta no se detectan, porque la firma "MP3 (Sync)" es `FF FB` = MPEG-1 Layer III
            // solamente, y estos empiezan con `FF F3` / `FF E3`. Es un hueco PREEXISTENTE de
            // detección (no de esta sesión), anotado en los próximos pasos.
            let mut audio = vec![b'I', b'D', b'3', 0x03, 0x00, 0x00, 0, 0, 0, 0];
            audio.extend_from_slice(&frame.repeat(30));
            let mut data = vec![0u8; 4096];
            let offset = data.len();
            data.extend_from_slice(&audio);
            data.extend_from_slice(&[0x00; 4096]);

            let found = scan_bytes(&data, &audio_sigs());
            let f = found
                .iter()
                .find(|f| f.offset == offset as u64)
                .unwrap_or_else(|| panic!("no se detectó el audio {nombre}"));
            assert_eq!(f.size, audio.len() as u64, "tamaño real de {nombre}");
            assert_eq!(f.integrity(), Integrity::Intact, "{nombre}");
        }
    }

    /// En una tarjeta de cámara los archivos van uno pegado al otro. El recorrido de cajas paraba en
    /// el primer byte que no pareciera caja, no al terminar el archivo — y como el `ftyp` del
    /// siguiente SÍ es una caja válida, seguía de largo: tres videos se fusionaban en uno solo, y
    /// tres fotos HEIC de iPhone se convertían en una.
    #[test]
    fn test_dos_videos_pegados_no_se_fusionan_en_uno() {
        let a = make_mp4(2000);
        let b = make_mp4(3000);
        let mut data = vec![0u8; 4096];
        let (off_a, off_b) = (data.len(), data.len() + a.len());
        data.extend_from_slice(&a);
        data.extend_from_slice(&b); // pegado, sin relleno entre medio
        data.extend_from_slice(&[0x00; 4096]);

        let sigs = signatures_for_categories(&[FileCategory::Video]);
        let found = scan_bytes(&data, &sigs);

        let fa = found
            .iter()
            .find(|f| f.offset == off_a as u64)
            .expect("falta el 1ro");
        let fb = found
            .iter()
            .find(|f| f.offset == off_b as u64)
            .expect("falta el 2do");
        assert_eq!(fa.size, a.len() as u64, "el 1ro no debe englobar al 2do");
        assert_eq!(fb.size, b.len() as u64);
    }

    /// `size == 0` en una caja significa "esta caja llega hasta el fin del archivo" y es válido por
    /// norma (lo escriben cámaras y muxers que graban a streaming). Tratarlo como "acá no hay caja"
    /// cortaba el video justo antes de sus datos y lo declaraba ÍNTEGRO: el usuario guardaba un
    /// archivo vacío convencido de que estaba bien.
    #[test]
    fn test_una_caja_hasta_el_fin_del_archivo_no_se_declara_integra_a_medias() {
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&16u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0u8; 4]);
        mp4.extend_from_slice(&24u32.to_be_bytes());
        mp4.extend_from_slice(b"moov");
        mp4.extend_from_slice(&[0x11; 16]);
        mp4.extend_from_slice(&0u32.to_be_bytes()); // size 0 = hasta el fin del archivo
        mp4.extend_from_slice(b"mdat");
        mp4.resize(16 + 24 + 8 + 4000, 0x5A);

        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&mp4);
        data.extend_from_slice(&[0x00; 4096]);

        let sigs = signatures_for_categories(&[FileCategory::Video]);
        let found = scan_bytes(&data, &sigs);
        let f = found
            .iter()
            .find(|f| f.offset == offset as u64)
            .expect("no se detectó");

        assert_ne!(
            f.integrity(),
            Integrity::Intact,
            "no se puede afirmar que esté íntegro si no se sabe dónde termina"
        );
        assert!(
            f.size >= mp4.len() as u64,
            "y no puede cortarse antes de los datos: {} < {}",
            f.size,
            mp4.len()
        );
    }

    /// Un `ftyp` fabricado que declara un tamaño enorme se volvía "contenedor confiable" y
    /// `suppress_contained` borraba las fotos REALES que quedaban adentro: en la prueba de la
    /// revisión desaparecían 6 JPEG sin ningún aviso. Un `ftyp` real mide decenas de bytes.
    #[test]
    fn test_un_ftyp_gigante_no_se_traga_las_fotos_de_adentro() {
        let jpeg = {
            let mut j = vec![0xFF, 0xD8, 0xFF];
            j.resize(2048, 0x7C);
            j.extend_from_slice(&[0xFF, 0xD9]);
            j
        };
        let total = 200_000usize;
        let mut falso = Vec::new();
        falso.extend_from_slice(&(total as u32).to_be_bytes()); // ftyp que declara 200 KB
        falso.extend_from_slice(b"ftypisom");
        falso.extend_from_slice(&[0u8; 4]);
        falso.resize(total, 0x00);
        // una foto real adentro del rango que el ftyp falso dice ocupar
        let pos_foto = 50_000;
        falso[pos_foto..pos_foto + jpeg.len()].copy_from_slice(&jpeg);
        // y una caja plausible justo al final, para que el encadenado "cierre"
        falso.extend_from_slice(&24u32.to_be_bytes());
        falso.extend_from_slice(b"moov");
        falso.extend_from_slice(&[0x11; 16]);

        let mut data = vec![0u8; 4096];
        let off_foto = data.len() + pos_foto;
        data.extend_from_slice(&falso);
        data.extend_from_slice(&[0x00; 4096]);

        let sigs = signatures_for_categories(&[FileCategory::Photo, FileCategory::Video]);
        let found = scan_bytes(&data, &sigs);

        assert!(
            found.iter().any(|f| f.offset == off_foto as u64),
            "la foto real no puede desaparecer por un ftyp inventado"
        );
    }

    /// A un audio al que le falta un pedazo del final (imagen truncada, último sector pisado) hay
    /// que recuperarlo igual: es un archivo REAL. Pero tampoco puede salir como "íntegro", ni
    /// desarmarse en un archivo por frame — los tres resultados posibles estuvieron mal en algún
    /// momento de esta sesión.
    #[test]
    fn test_un_audio_cortado_al_final_se_recupera_entero_y_sin_mentir() {
        let mp3 = repeat(&mpeg_frame(), 40);
        let cortado = &mp3[..mp3.len() - 200]; // le falta parte del último frame
        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(cortado);

        let found = scan_bytes(&data, &audio_sigs());
        assert_eq!(found.len(), 1, "un archivo, no uno por frame");
        assert_eq!(found[0].offset, offset as u64);
        assert_eq!(
            found[0].integrity(),
            Integrity::Unverifiable,
            "no está íntegro, pero tampoco es basura: se guarda igual"
        );
        assert!(found[0].size >= (cortado.len() - 2000) as u64);
    }

    /// La ALINEACIÓN en sí, que es la propiedad que hace funcionar el escaneo de un disco físico en
    /// Windows y que no tenía ni un test: una revisión adversarial pudo desactivarla entera
    /// (`let aligned = offset;`) con los 97 tests en verde. Y encontró además un bug fino: con el
    /// buffer justo, el recorte producía una lectura de 528 bytes —no múltiplo de 512— justo en los
    /// offsets cuyo resto cae al final del sector.
    #[test]
    fn test_las_lecturas_siempre_quedan_alineadas_a_sector() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let datos: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
        file.write_all(&datos).unwrap();
        file.flush().unwrap();
        let mut f = File::open(file.path()).unwrap();

        // Se prueban tamaños de buffer y offsets variados, con foco en los restos cercanos al fin
        // del sector (497..511), que es donde estaba el bug.
        // Se incluye a propósito un buffer que NO es múltiplo de sector (528 = 16 + 512): ahí vivía
        // el bug, porque al recortar la lectura al tamaño del buffer quedaba en 528 bytes.
        for buf_len in [
            ISOBMFF_HEADER_READ + SECTOR as usize,
            2 * SECTOR as usize,
            4 * SECTOR as usize,
            1024 + SECTOR as usize,
        ] {
            let mut buf = vec![0u8; buf_len];
            for offset in (0u64..600).chain([1023, 1024, 1025, 4095, 4096]) {
                for want in [16usize, 100, 512, 1000] {
                    if want + SECTOR as usize > buf_len {
                        continue;
                    }
                    let Some((skew, n)) = read_aligned(&mut f, offset, want, &mut buf) else {
                        continue;
                    };
                    // 1) La lectura FÍSICA arranca en un múltiplo de sector y su largo también lo es.
                    let inicio_fisico = offset - skew as u64;
                    assert_eq!(
                        inicio_fisico % SECTOR,
                        0,
                        "inicio no alineado (offset {offset})"
                    );
                    let pos = f.stream_position().unwrap();
                    let largo_fisico = pos - inicio_fisico;
                    assert!(
                        largo_fisico.is_multiple_of(SECTOR) || pos == datos.len() as u64,
                        "largo físico {largo_fisico} no alineado (offset {offset}, want {want})"
                    );
                    // 2) Y los datos devueltos son EXACTAMENTE los del offset pedido.
                    let esperado = &datos[offset as usize..(offset as usize + n).min(datos.len())];
                    assert_eq!(
                        &buf[skew..skew + n],
                        esperado,
                        "datos mal en offset {offset}"
                    );
                }
            }
        }
    }

    /// "No pude saberlo" tiene que GUARDARSE (un archivo real en un disco que falla cae ahí), y esa
    /// es la diferencia entre recuperar y no recuperar. Una revisión adversarial borró la asignación
    /// de `end_unknown` —o sea, mandó todos esos archivos a "posiblemente dañado", que no se
    /// guarda— y los 97 tests siguieron en verde.
    #[test]
    fn test_lo_que_no_se_pudo_determinar_se_guarda_igual() {
        let sig = audio_sigs()
            .into_iter()
            .find(|s| s.name == "MP3 (Sync)")
            .unwrap();

        let mut sin_saber = found_with(sig.clone(), false);
        sin_saber.end_unknown = true;
        assert_eq!(
            sin_saber.integrity(),
            Integrity::Unverifiable,
            "no se pudo determinar el final: se guarda, sin afirmar nada"
        );

        let confirmado_incompleto = {
            let mut f = found_with(sig.clone(), true);
            f.end_unknown = true;
            f
        };
        assert_eq!(
            confirmado_incompleto.integrity(),
            Integrity::Unverifiable,
            "saber hasta dónde llega no es saber que está completo"
        );

        let mut rechazado = found_with(sig, false);
        rechazado.end_unknown = false;
        assert_eq!(
            rechazado.integrity(),
            Integrity::Suspect,
            "se comprobó que no cierra: eso sí es dudoso"
        );
    }

    /// Sin el índice (`moov`/`moof`/`meta`) el archivo no abre en ningún reproductor, así que no se
    /// puede afirmar que esté íntegro. La regla no tenía test: se podía borrar entera sin que nada
    /// fallara.
    #[test]
    fn test_un_isobmff_sin_indice_no_se_da_por_bueno() {
        // `ftyp` + `mdat`, sin `moov`: encadena bien pero no sirve como archivo.
        let mut sin_indice = Vec::new();
        sin_indice.extend_from_slice(&16u32.to_be_bytes());
        sin_indice.extend_from_slice(b"ftypisom");
        sin_indice.extend_from_slice(&[0u8; 4]);
        sin_indice.extend_from_slice(&2008u32.to_be_bytes());
        sin_indice.extend_from_slice(b"mdat");
        sin_indice.resize(16 + 2008, 0x5A);

        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&sin_indice);
        data.extend_from_slice(&[0x00; 4096]);

        let sigs = signatures_for_categories(&[FileCategory::Video]);
        let found = scan_bytes(&data, &sigs);
        if let Some(f) = found.iter().find(|f| f.offset == offset as u64) {
            assert_ne!(
                f.integrity(),
                Integrity::Intact,
                "sin `moov` no se puede declarar íntegro"
            );
        }

        // Y el mismo archivo CON `moov` sí se da por bueno (si no, el test pasaría por casualidad).
        let con_indice = make_mp4(2000);
        let mut data = vec![0u8; 4096];
        let offset = data.len();
        data.extend_from_slice(&con_indice);
        data.extend_from_slice(&[0x00; 4096]);
        let found = scan_bytes(&data, &sigs);
        let f = found
            .iter()
            .find(|f| f.offset == offset as u64)
            .expect("no se detectó");
        assert_eq!(f.integrity(), Integrity::Intact);
        assert_eq!(f.size, con_indice.len() as u64);
    }

    /// La primera caja tiene que ser `ftyp`: es lo que hace que el recorrido describa un ARCHIVO y
    /// no un pedazo cualquiera de datos. Tampoco tenía test.
    #[test]
    fn test_un_isobmff_que_no_empieza_en_ftyp_no_se_da_por_bueno() {
        // Empieza en `moov`, como si el candidato hubiera caído a mitad de un archivo.
        let mut a_mitad = Vec::new();
        a_mitad.extend_from_slice(&24u32.to_be_bytes());
        a_mitad.extend_from_slice(b"moov");
        a_mitad.extend_from_slice(&[0x11; 16]);
        a_mitad.extend_from_slice(&2008u32.to_be_bytes());
        a_mitad.extend_from_slice(b"mdat");
        a_mitad.resize(24 + 2008, 0x5A);

        assert!(
            crate::signatures::isobmff_end(&a_mitad, true).is_none(),
            "un recorrido que no arranca en `ftyp` no describe un archivo"
        );
    }

    /// El pase de refinamiento chequea la cancelación en DOS lugares: una vez por candidato (la que
    /// da respuesta inmediata cuando hay miles) y otra por chunk leído. Los tests anteriores pasaban
    /// con cualquiera de los dos, así que se fija el de por candidato por separado.
    #[test]
    fn test_la_cancelacion_se_nota_ya_en_el_primer_candidato() {
        let mp3 = repeat(&mpeg_frame(), 3000);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&mp3).unwrap();
        file.flush().unwrap();

        let sig = audio_sigs()
            .into_iter()
            .find(|s| s.name == "MP3 (Sync)")
            .unwrap();
        // Varios candidatos: con la guarda por candidato, NINGUNO se refina.
        let mut files: Vec<FoundFile> = (0..5).map(|_| found_with(sig.clone(), false)).collect();

        refine_audio_streams(file.path(), &mut files, &AtomicBool::new(true));

        assert!(
            files.iter().all(|f| !f.footer_found),
            "cancelado: no debe refinarse ninguno"
        );
        // Y ni siquiera se intentó: con la guarda por candidato no se llega a tocar el archivo, así
        // que tampoco quedan marcados como "no se pudo determinar".
        assert!(
            files.iter().all(|f| !f.end_unknown),
            "con la guarda por candidato no se llega a leer nada"
        );
    }

    /// Un error al LEER el origen no puede hacer perder el archivo: el candidato queda marcado
    /// "no se pudo determinar" y se guarda igual. Es el disco que está fallando, o sea el escenario
    /// central de la herramienta. La asignación no tenía test: se podía borrar y todo seguía verde.
    #[test]
    fn test_si_no_se_puede_leer_el_origen_el_archivo_se_conserva() {
        let sig = audio_sigs()
            .into_iter()
            .find(|s| s.name == "MP3 (Sync)")
            .unwrap();
        let mut files = vec![found_with(sig, false)];
        let inexistente = std::path::Path::new("/no/existe/este/origen.img");

        refine_audio_streams(inexistente, &mut files, &AtomicBool::new(false));

        assert!(files[0].end_unknown, "no se pudo leer: hay que conservarlo");
        assert_eq!(
            files[0].integrity(),
            Integrity::Unverifiable,
            "se guarda por defecto, sin afirmar nada de su final"
        );
    }

    /// Un audio MÁS LARGO que el máximo de su firma (un audiolibro, un set de DJ) tiene que dar UN
    /// archivo cortado al máximo, no miles. Cuando "se agotó el presupuesto" se confundió con "no
    /// pude leer", 57 MB de MP3 produjeron 12 380 archivos de 50 MB: 619 GB de salida.
    #[test]
    fn test_un_audio_mas_largo_que_el_maximo_da_un_archivo_y_no_miles() {
        let sig = audio_sigs()
            .into_iter()
            .find(|s| s.name == "MP3 (Sync)")
            .unwrap();
        // Máximo artificialmente chico para no escribir 50 MB en el test: se llama al walker
        // directamente con el mismo camino que usa el pase.
        let mp3 = repeat(&mpeg_frame(), 3000);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&mp3).unwrap();
        file.flush().unwrap();

        let tope = 100_000u64;
        let r = walk_audio_stream_on_disk(
            file.path(),
            0,
            tope,
            sig.audio_stream().unwrap(),
            1024 * 1024,
            &AtomicBool::new(false),
        );
        match r {
            EndResult::SizeGuess(size) => {
                assert!(
                    size > 0 && size <= tope,
                    "tamaño supuesto fuera de rango: {size}"
                )
            }
            otro => panic!(
                "un audio más largo que el máximo debe dar un tamaño supuesto, no {:?}",
                match otro {
                    EndResult::Size(_) => "Size",
                    EndResult::SizeUnverified(_) => "SizeUnverified",
                    EndResult::Rejected => "Rejected",
                    EndResult::Unreadable => "Unreadable",
                    EndResult::SizeGuess(_) => unreachable!(),
                }
            ),
        }
    }

    /// EL PEOR BUG DE LA SESIÓN, fijado para que no vuelva: un archivo cuyo tamaño es SUPUESTO —no
    /// medido— no puede actuar como contenedor. Un MP4 cortado cerca del principio del disco tomaba
    /// como tamaño "hasta el fin del origen", se volvía contenedor confiable, y `suppress_contained`
    /// borraba TODO lo que viniera después: en la prueba de la revisión, 20 fotos y una canción
    /// desaparecían sin ningún aviso y el usuario veía "1 archivo recuperado".
    #[test]
    fn test_un_video_cortado_no_borra_los_archivos_que_vienen_despues() {
        // MP4 con `ftyp` + `moov` + un `mdat` que declara MUCHO más de lo que hay.
        let mut mp4 = Vec::new();
        mp4.extend_from_slice(&16u32.to_be_bytes());
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0u8; 4]);
        mp4.extend_from_slice(&24u32.to_be_bytes());
        mp4.extend_from_slice(b"moov");
        mp4.extend_from_slice(&[0x11; 16]);
        mp4.extend_from_slice(&(1u32 << 30).to_be_bytes()); // declara 1 GB
        mp4.extend_from_slice(b"mdat");
        mp4.resize(16 + 24 + 8 + 2000, 0x5A);

        let mut data = vec![0u8; 4096];
        data.extend_from_slice(&mp4);

        // Y DESPUÉS, cinco fotos reales.
        let mut offsets_fotos = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(&[0x00; 4096]);
            offsets_fotos.push(data.len() as u64);
            let mut jpeg = vec![0xFF, 0xD8, 0xFF];
            jpeg.resize(3000, 0x7C);
            jpeg.extend_from_slice(&[0xFF, 0xD9]);
            data.extend_from_slice(&jpeg);
        }
        data.extend_from_slice(&[0x00; 4096]);

        let sigs = signatures_for_categories(&[FileCategory::Photo, FileCategory::Video]);
        let found = scan_bytes(&data, &sigs);

        for off in &offsets_fotos {
            assert!(
                found.iter().any(|f| f.offset == *off),
                "la foto en {off} no puede desaparecer por un video cortado que vino antes"
            );
        }
    }

    /// Las dos pasadas de refinamiento nuevas releen el disco, así que tienen que respetar el
    /// "Detener" del usuario: en un disco que está muriendo, seguir leyendo es exactamente lo que
    /// se pidió dejar de hacer. Mismo requisito que `refine_footers`.
    #[test]
    fn test_el_refinamiento_de_audio_respeta_la_cancelacion() {
        let mp3 = repeat(&mpeg_frame(), 3000);
        assert!(mp3.len() > BUFFER_SIZE, "debe necesitar la pasada en disco");
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&mp3).unwrap();
        file.flush().unwrap();

        let sig = audio_sigs()
            .into_iter()
            .find(|s| s.name == "MP3 (Sync)")
            .unwrap();
        let mut files = vec![found_with(sig, false)];
        let tamano_original = files[0].size;

        // Con la cancelación pedida: no toca nada.
        refine_audio_streams(file.path(), &mut files, &AtomicBool::new(true));
        assert!(!files[0].footer_found, "cancelado, no debe refinar");
        assert_eq!(files[0].size, tamano_original);

        // Sin cancelar: refina de verdad (o sea, el test anterior no pasa por casualidad).
        refine_audio_streams(file.path(), &mut files, &AtomicBool::new(false));
        assert!(files[0].footer_found);
        assert_eq!(files[0].size, mp3.len() as u64);
    }

    /// Arma un ISOBMFF válido: `ftyp` + `moov` + `mdat` con `datos` bytes de contenido.
    fn make_mp4(datos: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"ftypisom");
        b.extend_from_slice(&[0u8; 4]);
        b.extend_from_slice(&24u32.to_be_bytes());
        b.extend_from_slice(b"moov");
        b.extend_from_slice(&[0x11; 16]);
        b.extend_from_slice(&((datos + 8) as u32).to_be_bytes());
        b.extend_from_slice(b"mdat");
        b.resize(16 + 24 + 8 + datos, 0x5A);
        b
    }

    #[test]
    fn test_el_refinamiento_de_video_respeta_la_cancelacion() {
        // ISOBMFF mínimo REAL: `ftyp` + `moov` (el índice, sin el cual el archivo no abre en
        // ningún reproductor) + `mdat`. El fixture anterior no tenía `moov` y aun así se daba por
        // bueno: describía un archivo que en la vida real no sirve.
        let mp4 = make_mp4(1000);

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&mp4).unwrap();
        file.flush().unwrap();

        let sig = signatures_for_categories(&[FileCategory::Video])
            .into_iter()
            .find(|s| s.name == "MP4/M4V")
            .unwrap();
        let mut files = vec![found_with(sig, false)];
        let tamano_original = files[0].size;

        refine_isobmff_sizes(file.path(), &mut files, &AtomicBool::new(true));
        assert!(!files[0].footer_found, "cancelado, no debe refinar");
        assert_eq!(files[0].size, tamano_original);

        refine_isobmff_sizes(file.path(), &mut files, &AtomicBool::new(false));
        assert!(files[0].footer_found);
        assert_eq!(
            files[0].size,
            mp4.len() as u64,
            "el tamaño sale de recorrer las cajas"
        );
    }
}
