use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use crate::signatures::{format_size, FileCategory, FileSignature};

/// Archivo encontrado durante el escaneo
#[derive(Debug, Clone)]
pub struct FoundFile {
    pub signature: FileSignature,
    pub offset: u64,
    pub size: u64,
    pub index: usize,
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
}

impl ScanResult {
    pub fn summary(&self) -> String {
        format!(
            "📊 Resumen: {} archivos encontrados\n   📷 Fotos: {}  |  🎬 Videos: {}  |  🎵 Audios: {}\n   💾 Bytes escaneados: {}",
            self.found_files.len(),
            self.photos_count,
            self.videos_count,
            self.audios_count,
            format_size(self.bytes_scanned),
        )
    }
}

/// Obtiene el tamaño de la fuente (archivo o disco físico).
/// En discos físicos de Windows, `seek(End)` no funciona; se usa `IOCTL_DISK_GET_LENGTH_INFO`.
fn get_source_size(file: &mut File, source_path: &Path) -> Result<u64> {
    // Intentar seek(End) primero (funciona en archivos normales y en Linux/macOS)
    match file.seek(SeekFrom::End(0)) {
        Ok(size) if size > 0 => return Ok(size),
        _ => {}
    }

    // Fallback para discos físicos en Windows
    #[cfg(target_os = "windows")]
    {
        let src = source_path.to_string_lossy();
        if src.starts_with("\\\\.\\") {
            return get_disk_size_windows(file);
        }
    }

    // Último intento: leer hasta EOF contando bytes (lento pero funcional)
    let _ = source_path; // evitar warning en no-windows
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
    let src = source_path.to_string_lossy();
    if src.starts_with("\\\\.\\") || src.starts_with("/dev/") {
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
            std::cmp::max(header_end, extra_end)
        })
        .max()
        .unwrap_or(16)
}

/// Escanea un segmento del archivo buscando firmas multimedia.
/// Cada hilo abre su propio File handle y escanea secuencialmente dentro del segmento.
/// Solo retiene resultados con offset en [claim_start, claim_end).
fn scan_segment(
    source_path: &Path,
    segment: &Segment,
    signatures: &[FileSignature],
    source_size: u64,
    max_header_len: usize,
    progress_bytes: &AtomicU64,
    inline_pb: Option<&ProgressBar>,
) -> Result<Vec<FoundFile>> {
    let mut file = File::open(source_path)?;
    file.seek(SeekFrom::Start(segment.start))?;

    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut overlap: Vec<u8> = Vec::new();
    let mut found_files: Vec<FoundFile> = Vec::new();
    let mut position = segment.start;
    let mut file_index: usize = 0;

    loop {
        if position >= segment.end {
            break;
        }

        let max_to_read = std::cmp::min(
            BUFFER_SIZE as u64,
            segment.end - position,
        ) as usize;

        let bytes_read = file.read(&mut buffer[..max_to_read])?;
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

    Ok(found_files)
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
    let is_device = {
        let src = source_path.to_string_lossy();
        src.starts_with("\\\\.\\") || src.starts_with("/dev/")
    };
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

    let found_files = if num_threads <= 1 {
        // ── Fast path: 1 hilo, sin overhead de threads ──
        let segment = Segment {
            start: 0,
            end: file_size,
            claim_start: 0,
            claim_end: file_size,
        };
        let progress = AtomicU64::new(0);
        scan_segment(source_path, &segment, signatures, file_size, max_header_len, &progress, Some(&pb))?
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

        // Recolectar resultados, manejar errores
        let mut all_files: Vec<FoundFile> = Vec::new();
        let mut worker_error: Option<anyhow::Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(files)) => all_files.extend(files),
                Ok(Err(e)) => {
                    if worker_error.is_none() {
                        worker_error = Some(e);
                    }
                }
                Err(_) => {
                    if worker_error.is_none() {
                        worker_error =
                            Some(anyhow::anyhow!("Un hilo de escaneo falló inesperadamente"));
                    }
                }
            }
        }

        // Siempre señalar al monitor que termine
        progress.store(file_size, Ordering::Relaxed);
        let _ = monitor_handle.join();

        if let Some(e) = worker_error {
            return Err(e);
        }

        // Sort por offset, dedup defensivo, re-indexar
        all_files.sort_by_key(|f| f.offset);
        all_files.dedup_by(|a, b| {
            a.offset == b.offset && a.signature.extension == b.signature.extension
        });
        for (i, f) in all_files.iter_mut().enumerate() {
            f.index = i + 1;
        }

        all_files
    };

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

    Ok(ScanResult {
        found_files,
        bytes_scanned: file_size,
        photos_count,
        videos_count,
        audios_count,
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

                let absolute_offset = base_offset + i as u64;

                // Verificar que no está ya registrado (evitar duplicados del overlap)
                if found_files
                    .iter()
                    .any(|f| f.offset == absolute_offset && f.signature.extension == sig.extension)
                {
                    continue;
                }

                // Determinar tamaño buscando footer en el buffer (sin I/O extra)
                let max_possible = std::cmp::min(
                    sig.max_size as u64,
                    source_size.saturating_sub(absolute_offset),
                );
                let size = if let Some(footer) = sig.footer {
                    let remaining = &buf[i..];
                    if let Some(pos) = find_last_subsequence(remaining, footer) {
                        let found_size = pos as u64 + footer.len() as u64;
                        if found_size <= max_possible {
                            found_size
                        } else {
                            max_possible
                        }
                    } else {
                        // Footer no está en este buffer → usar max_size
                        max_possible
                    }
                } else {
                    max_possible
                };

                if size > 512 {
                    // Ignorar archivos menores a 512 bytes (probablemente falsos positivos)
                    *file_index += 1;
                    found_files.push(FoundFile {
                        signature: sig.clone(),
                        offset: absolute_offset,
                        size,
                        index: *file_index,
                    });
                }
            }
        }
    }
}

/// Busca la última ocurrencia de una subsecuencia en un buffer
fn find_last_subsequence(buf: &[u8], pattern: &[u8]) -> Option<usize> {
    if pattern.is_empty() || buf.len() < pattern.len() {
        return None;
    }

    for i in (0..=buf.len() - pattern.len()).rev() {
        if &buf[i..i + pattern.len()] == pattern {
            return Some(i);
        }
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
}
