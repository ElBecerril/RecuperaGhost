use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use crate::signatures::{FileCategory, FileSignature};
use crate::util::format_size;

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
        if self.had_errors {
            s.push_str(
                "\n   ⚠️  El escaneo tuvo errores de I/O leyendo el origen (sectores dañados u\n       otro fallo de lectura) — el resultado es parcial: puede faltar contenido de\n       las zonas que no se pudieron leer.",
            );
        }
        s
    }
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

/// Escanea un segmento del archivo buscando firmas multimedia.
/// Cada hilo abre su propio File handle y escanea secuencialmente dentro del segmento.
/// Solo retiene resultados con offset en [claim_start, claim_end).
///
/// Limitación conocida (M7, no resuelta aquí): `file.read()` más abajo no tiene timeout ni
/// forma de cancelarse. Si el dispositivo de origen deja de responder (ej. un USB que se cae
/// a media lectura), este read puede bloquear indefinidamente y el hilo nunca retorna.
/// Implementar cancelación está fuera de alcance de este cambio.
///
/// (B1) A propósito esta función NUNCA devuelve `Err`: un solo sector dañado (I/O error) en
/// cualquier punto del origen es el escenario CENTRAL de uso de esta herramienta (discos
/// fallando), y antes un solo error acá se propagaba con `?` hacia el caller, descartando en
/// el camino de 1 hilo TODO lo encontrado hasta ese punto, y en el camino multi-hilo el
/// resultado de los OTROS hilos que sí terminaron bien. Ahora los errores de lectura del
/// origen se tratan como "saltar y seguir" en vez de "abortar todo", y se reportan vía
/// `SegmentResult::had_errors` en vez de con `Result::Err`.
fn scan_segment(
    source_path: &Path,
    segment: &Segment,
    signatures: &[FileSignature],
    source_size: u64,
    max_header_len: usize,
    progress_bytes: &AtomicU64,
    inline_pb: Option<&ProgressBar>,
) -> SegmentResult {
    let mut file = match File::open(source_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "  ⚠️  No se pudo abrir {} para escanear [0x{:X}, 0x{:X}): {} — este segmento se omite",
                source_path.display(),
                segment.start,
                segment.end,
                e
            );
            return SegmentResult {
                found_files: Vec::new(),
                had_errors: true,
            };
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(segment.start)) {
        eprintln!(
            "  ⚠️  No se pudo posicionar en 0x{:X}: {} — este segmento se omite",
            segment.start, e
        );
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

        let max_to_read = std::cmp::min(
            BUFFER_SIZE as u64,
            segment.end - position,
        ) as usize;

        let bytes_read = match file.read(&mut buffer[..max_to_read]) {
            Ok(n) => n,
            Err(e) => {
                // (B1) No propagar: un sector dañado no debe tirar lo ya encontrado. Se
                // intenta saltar este bloque (avanzar `position` y reposicionar el file
                // handle después de él) y seguir escaneando el resto del segmento. El
                // `overlap` de antes del error ya no es válido (hay un hueco sin leer), así
                // que se descarta para no combinar bytes no contiguos.
                eprintln!(
                    "  ⚠️  Error de I/O leyendo en offset 0x{:X}: {} — saltando este bloque y continuando",
                    position, e
                );
                had_errors = true;
                overlap.clear();
                let next_position = position + max_to_read as u64;
                progress_bytes.fetch_add(max_to_read as u64, Ordering::Relaxed);
                if let Some(pb) = inline_pb {
                    pb.set_position(progress_bytes.load(Ordering::Relaxed));
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
                        eprintln!(
                            "  ⚠️  No se pudo reposicionar tras error de I/O: {} — abandonando el resto de este segmento",
                            seek_err
                        );
                        break;
                    }
                }
            }
        };
        if bytes_read == 0 {
            break;
        }

        // Buscar firmas: con overlap del chunk anterior si existe, o solo el buffer actual
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

        // Guardar overlap para el siguiente chunk (siempre, incluso con reads parciales)
        if bytes_read >= max_header_len {
            overlap = buffer[bytes_read - max_header_len..bytes_read].to_vec();
        } else if bytes_read > 0 {
            overlap = buffer[..bytes_read].to_vec();
        }

        position += bytes_read as u64;
        progress_bytes.fetch_add(bytes_read as u64, Ordering::Relaxed);
        if let Some(pb) = inline_pb {
            pb.set_position(progress_bytes.load(Ordering::Relaxed));
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
pub fn scan_source(
    source_path: &Path,
    signatures: &[FileSignature],
) -> Result<ScanResult> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;
    let file_size = get_source_size(&mut file, source_path)
        .with_context(|| "No se pudo obtener el tamaño del origen")?;
    drop(file);

    let num_threads = select_thread_count(source_path, file_size);
    scan_source_impl(source_path, signatures, file_size, num_threads)
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

    scan_source_impl(source_path, signatures, file_size, forced_threads.max(1))
}

/// Implementación central del escaneo: orquesta single-thread o multi-thread.
fn scan_source_impl(
    source_path: &Path,
    signatures: &[FileSignature],
    file_size: u64,
    num_threads: usize,
) -> Result<ScanResult> {
    println!(
        "  🔎 Escaneando: {}",
        source_path.display()
    );
    println!(
        "  📏 Tamaño: {}",
        format_size(file_size)
    );

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
        println!(
            "  ⏱️  Tiempo estimado: ~{} min {} seg",
            mins, secs
        );
        println!();
        println!(
            "{}",
            "  ☕ Estos escaneos son bastante tardados, así que te"
                .bright_yellow()
        );
        println!(
            "{}",
            "     recomendamos ir por un café o echarte un sueñito"
                .bright_yellow()
        );
        println!(
            "{}",
            "     en lo que nosotros chambeamos. 👻💤"
                .bright_yellow()
        );
    } else if estimated_secs > 5 {
        let mins = estimated_secs / 60;
        let secs = estimated_secs % 60;
        if mins > 0 {
            println!(
                "  ⏱️  Tiempo estimado: ~{} min {} seg",
                mins, secs
            );
        } else {
            println!(
                "  ⏱️  Tiempo estimado: ~{} seg",
                secs
            );
        }
    }

    if num_threads > 1 {
        println!("  🧵 Usando {} hilos de escaneo", num_threads);
    }
    println!();

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  👻 [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%)"
        )
        .unwrap()
        .progress_chars("█▓▒░  "),
    );

    let max_header_len = max_signature_reach(signatures);

    let (mut found_files, bytes_scanned_actual, had_errors) = if num_threads <= 1 {
        // ── Fast path: 1 hilo, sin overhead de threads ──
        let segment = Segment {
            start: 0,
            end: file_size,
            claim_start: 0,
            claim_end: file_size,
        };
        let progress = AtomicU64::new(0);
        // (B1) scan_segment ya no propaga errores de I/O con `?` — un sector dañado en
        // cualquier punto del origen ya no descarta todo lo encontrado antes de llegar a él.
        let result = scan_segment(source_path, &segment, signatures, file_size, max_header_len, &progress, Some(&pb));
        if result.had_errors {
            eprintln!("  ⚠️  El escaneo tuvo errores de I/O leyendo el origen; el resultado es parcial.");
        }
        // B3: reportar lo realmente leído, no file_size fijo — un EOF prematuro (bytes_read
        // == 0 antes de llegar a segment.end) corta el escaneo antes de tiempo.
        let scanned = progress.load(Ordering::Relaxed);
        (result.found_files, scanned, result.had_errors)
    } else {
        // ── Multi-hilo ──
        let segments = calculate_segments(file_size, num_threads, max_header_len as u64);
        let progress = Arc::new(AtomicU64::new(0));

        // Hilo dedicado de progreso: lee el atomic cada 100ms y actualiza ProgressBar
        let progress_monitor = progress.clone();
        let pb_monitor = pb.clone();
        let monitor_handle = std::thread::spawn(move || {
            loop {
                let pos = progress_monitor.load(Ordering::Relaxed);
                pb_monitor.set_position(std::cmp::min(pos, file_size));
                if pos >= file_size {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });

        // Spawn N hilos workers
        let source_buf = source_path.to_path_buf();
        let sigs_arc: Arc<Vec<FileSignature>> = Arc::new(signatures.to_vec());

        let handles: Vec<_> = segments
            .into_iter()
            .map(|segment| {
                let path = source_buf.clone();
                let sigs = sigs_arc.clone();
                let prog = progress.clone();
                std::thread::spawn(move || {
                    scan_segment(&path, &segment, &sigs, file_size, max_header_len, &prog, None)
                })
            })
            .collect();

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
                    eprintln!("  ⚠️  Un hilo de escaneo falló inesperadamente (panic); se conservan los resultados de los demás hilos.");
                    multi_had_errors = true;
                }
            }
        }
        if multi_had_errors {
            eprintln!("  ⚠️  El escaneo tuvo errores en uno o más hilos; el resultado es parcial.");
        }

        // B3: capturar lo realmente acumulado ANTES de forzar el atomic a file_size (eso
        // último solo es para que el hilo de progreso se detenga, no refleja lo leído).
        let scanned = progress.load(Ordering::Relaxed);

        // Siempre señalar al monitor que termine
        progress.store(file_size, Ordering::Relaxed);
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

    // A2: segundo pase de footer, en un solo hilo, para archivos cuyo footer no apareció
    // dentro del buffer/chunk original — ver `refine_footers`.
    refine_footers(source_path, &mut found_files);

    pb.finish_with_message("✅ Escaneo completado");
    println!();

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
    })
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
                    if extra_end > buf.len()
                        || &buf[extra_pos..extra_end] != *extra_bytes
                    {
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

                // Determinar tamaño: campo de tamaño en el header (BMP), footer, o max_size.
                let (size, footer_found) = if let Some((sf_offset, sf_len)) = sig.size_from_header
                {
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
        if !header.is_empty() && i + header.len() <= buf.len() && &buf[i..i + header.len()] == header
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
fn refine_footers(source_path: &Path, found_files: &mut [FoundFile]) {
    const REFINE_CHUNK: usize = 4 * 1024 * 1024;

    for f in found_files.iter_mut() {
        if f.footer_found {
            continue;
        }
        let Some(footer) = f.signature.footer else {
            continue;
        };

        let header_end = f.offset
            + f.signature.header_offset as u64
            + f.signature.header.len() as u64;
        let search_end = f.offset + f.signature.max_size as u64;

        if let Some(new_size) = find_footer_sequential(
            source_path,
            f.offset,
            header_end,
            search_end,
            f.signature.header,
            footer,
            REFINE_CHUNK,
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
fn find_footer_sequential(
    source_path: &Path,
    header_offset: u64,
    search_start: u64,
    search_end: u64,
    header: &[u8],
    footer: &[u8],
    chunk_size: usize,
) -> Option<u64> {
    if search_start >= search_end {
        return None;
    }

    let mut file = File::open(source_path).ok()?;
    file.seek(SeekFrom::Start(search_start)).ok()?;

    let mut pos = search_start;
    let mut overlap: Vec<u8> = Vec::new();
    let mut depth: i32 = 1;

    while pos < search_end {
        let to_read = std::cmp::min(chunk_size as u64, search_end - pos) as usize;
        let mut buf = vec![0u8; to_read];
        let bytes_read = file.read(&mut buf).ok()?;
        if bytes_read == 0 {
            break;
        }
        buf.truncate(bytes_read);

        let combined_start = pos - overlap.len() as u64;
        let skip_before = overlap.len(); // bytes ya contados en la iteración anterior
        let mut combined = overlap.clone();
        combined.extend_from_slice(&buf);

        let combined_len = combined.len();
        let (new_depth, footer_pos) =
            scan_nesting(&combined, header, footer, depth, skip_before, 0, combined_len);
        depth = new_depth;
        if let Some(rel_pos) = footer_pos {
            let abs_pos = combined_start + rel_pos as u64;
            return Some((abs_pos + footer.len() as u64).saturating_sub(header_offset));
        }

        let keep = std::cmp::max(header.len(), footer.len()).saturating_sub(1);
        overlap = if buf.len() >= keep {
            buf[buf.len() - keep..].to_vec()
        } else {
            buf.clone()
        };
        pos += bytes_read as u64;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::{signatures_for_categories, FileCategory};
    use std::io::Write;

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
        data[pos + 28..pos + 36]
            .copy_from_slice(&[0x4F, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64]);
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
        let all_categories = vec![FileCategory::Photo, FileCategory::Video, FileCategory::Audio];
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
            assert!(
                found,
                "No se encontro {} en offset 0x{:X}",
                ext, offset
            );
        }

        println!("\nTodas las {} firmas detectadas correctamente.", expected.len());
    }

    #[test]
    fn test_riff_disambiguation() {
        let (file, _) = create_test_image();
        let all_categories = vec![FileCategory::Photo, FileCategory::Video, FileCategory::Audio];
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
        assert_eq!(jpeg.size, 2050, "Tamano JPEG deberia ser 2050, es {}", jpeg.size);
        println!("\nFooter JPEG detectado correctamente: {} bytes.", jpeg.size);
    }

    #[test]
    fn test_recovery() {
        let (file, _) = create_test_image();
        let all_categories = vec![FileCategory::Photo, FileCategory::Video, FileCategory::Audio];
        let sigs = signatures_for_categories(&all_categories);

        let result = scan_source(file.path(), &sigs).unwrap();

        let output_dir = tempfile::tempdir().unwrap();
        let recovery = crate::recovery::recover_files(
            file.path(),
            &result.found_files,
            output_dir.path(),
        )
        .unwrap();

        assert_eq!(recovery.failed, 0, "Hubo {} fallos de recuperacion", recovery.failed);
        assert!(
            recovery.recovered > 0,
            "No se recupero ningun archivo"
        );
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
            (100 * 1024 * 1024, &[2, 3, 4, 5, 7, 8]),           // 100 MB exacto
            (100 * 1024 * 1024 + 1, &[2, 3, 5, 7]),             // 100 MB + 1 byte
            (17 * 1024 * 1024 + 12345, &[2, 3]),                 // ~17 MB no alineado
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
                    segments[num_threads - 1].claim_end, *file_size,
                    "file_size={}, threads={}: ultimo segmento no llega a file_size",
                    file_size, num_threads
                );
                for i in 1..num_threads {
                    assert_eq!(
                        segments[i].claim_start,
                        segments[i - 1].claim_end,
                        "file_size={}, threads={}: gap entre segmento {} y {}",
                        file_size, num_threads, i - 1, i
                    );
                }

                // Las zonas de lectura incluyen overlap
                for (i, seg) in segments.iter().enumerate() {
                    if i > 0 {
                        assert!(
                            seg.start <= seg.claim_start,
                            "file_size={}, threads={}: segmento {} start {} > claim_start {}",
                            file_size, num_threads, i, seg.start, seg.claim_start
                        );
                    }
                    assert!(
                        seg.end >= seg.claim_end,
                        "file_size={}, threads={}: segmento {} end {} < claim_end {}",
                        file_size, num_threads, i, seg.end, seg.claim_end
                    );
                }

                // No hay zonas claim vacías
                for i in 0..num_threads {
                    assert!(
                        segments[i].claim_start < segments[i].claim_end,
                        "file_size={}, threads={}: zona claim vacia en segmento {}",
                        file_size, num_threads, i
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
            assert!(count > 1, "Esperaba >1 hilo para 1GB en maquina multi-core, obtuve {}", count);
        }
        assert!(count <= 8, "Esperaba <=8 hilos, obtuve {}", count);
        assert!(
            count <= cpu_cores,
            "No debe exceder cores disponibles: {} > {}",
            count, cpu_cores
        );
        assert!(count >= 1, "Siempre al menos 1 hilo");

        // Archivo de exactamente 16 MB → 1 hilo (by_size = 16MB/16MB = 1)
        let count_16 = select_thread_count(&PathBuf::from("medium.img"), 16 * 1024 * 1024);
        assert_eq!(count_16, 1, "16MB exacto deberia dar 1 hilo (by_size=1)");

        // Archivo de 32 MB → max 2 hilos (by_size = 32/16 = 2)
        let count_32 = select_thread_count(&PathBuf::from("medium.img"), 32 * 1024 * 1024);
        assert!(count_32 <= 2, "32MB no deberia dar mas de 2 hilos, obtuve {}", count_32);
    }

    #[test]
    fn test_multithreaded_scan_consistency() {
        // Usar la imagen de test con TODAS las categorías (incluye RIFF/OggS disambiguation)
        let (file, expected) = create_test_image();
        let all_categories = vec![FileCategory::Photo, FileCategory::Video, FileCategory::Audio];
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
        let found_before = result
            .found_files
            .iter()
            .any(|f| f.offset == before as u64);

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
        // Resto de datos de la foto real, más largo, con el EOI real mucho más lejos
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
        println!("mp3 pass rate = {:.4}%  aac pass rate = {:.4}%", mp3_pct, aac_pct);

        // Antes del frame chaining (solo bits reservados) pasaba ~60-65% de datos aleatorios;
        // con frame chaining debe caer a un porcentaje marginal (umbral generoso para no ser
        // frágil ante variaciones del PRNG determinístico usado arriba).
        assert!(mp3_pct < 5.0, "MP3 sync validator deja pasar demasiados falsos positivos: {:.4}%", mp3_pct);
        assert!(aac_pct < 5.0, "AAC ADTS validator deja pasar demasiados falsos positivos: {:.4}%", aac_pct);
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

        println!("\nTIFF big-endian (MM*) detectado correctamente en offset 0x{:X}.", pos);
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

        // Caso valido: bfSize = 100, reservado1/2 = 0, bfOffBits = 54 (valor tipico BMP: 14
        // bytes de BITMAPFILEHEADER + 40 bytes de BITMAPINFOHEADER estandar).
        let mut valid = vec![0u8; 100];
        valid[0] = 0x42;
        valid[1] = 0x4D;
        valid[2..6].copy_from_slice(&100u32.to_le_bytes());
        valid[6..10].copy_from_slice(&[0, 0, 0, 0]);
        valid[10..14].copy_from_slice(&54u32.to_le_bytes());
        assert!(validator_fn(&valid), "BMP valido deberia pasar el validador");

        // Caso invalido: bfOffBits mayor que bfSize (estructuralmente imposible en un BMP real).
        let mut bad_offset = vec![0u8; 100];
        bad_offset[0] = 0x42;
        bad_offset[1] = 0x4D;
        bad_offset[2..6].copy_from_slice(&100u32.to_le_bytes());
        bad_offset[10..14].copy_from_slice(&500u32.to_le_bytes());
        assert!(!validator_fn(&bad_offset), "bfOffBits > bfSize deberia rechazarse");

        // Caso invalido: bfSize absurdamente grande (mayor al max_size de la firma).
        let mut bad_size = vec![0u8; 100];
        bad_size[0] = 0x42;
        bad_size[1] = 0x4D;
        bad_size[2..6].copy_from_slice(&u32::MAX.to_le_bytes());
        bad_size[10..14].copy_from_slice(&54u32.to_le_bytes());
        assert!(!validator_fn(&bad_size), "bfSize absurdo deberia rechazarse");

        // Fin a fin: un BMP valido embebido en un buffer se detecta via scan_source, y datos
        // aleatorios con "BM" al inicio pero campos incoherentes no. bfSize se declara en
        // 1000 (no 200): el scanner descarta cualquier archivo detectado de menos de 512
        // bytes por heuristica anti-falsos-positivos (preexistente, ver "size > 512" en
        // check_signatures_in_buffer) — un bfSize menor a ese umbral haria que el test fallara
        // por esa heuristica no relacionada, no por el validador BMP en si.
        let mut data = vec![0u8; 4096];
        let bmp_pos = 512usize;
        data[bmp_pos] = 0x42;
        data[bmp_pos + 1] = 0x4D;
        data[bmp_pos + 2..bmp_pos + 6].copy_from_slice(&1000u32.to_le_bytes());
        data[bmp_pos + 10..bmp_pos + 14].copy_from_slice(&54u32.to_le_bytes());

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
        assert_eq!(at_valid_bmp, vec!["bmp"], "Offset BMP valido tiene: {:?}", at_valid_bmp);

        let at_random_bmp = result
            .found_files
            .iter()
            .any(|f| f.offset == random_bmp_pos as u64 && f.signature.extension == "bmp");
        assert!(
            !at_random_bmp,
            "'BM' con campos aleatorios/incoherentes no deberia detectarse como BMP"
        );

        println!("\nValidador BMP acepta headers coherentes y rechaza campos incoherentes.");
    }
}
