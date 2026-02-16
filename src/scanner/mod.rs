use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
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

/// Tamaño del buffer de lectura (1 MB)
const BUFFER_SIZE: usize = 1024 * 1024;

/// Escanea un archivo/dispositivo buscando firmas de archivos multimedia
pub fn scan_source(
    source_path: &Path,
    signatures: &[FileSignature],
) -> Result<ScanResult> {
    let mut file = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;

    let file_size = file
        .seek(SeekFrom::End(0))
        .with_context(|| "No se pudo obtener el tamaño del archivo")?;
    file.seek(SeekFrom::Start(0))?;

    println!(
        "  🔎 Escaneando: {}",
        source_path.display()
    );
    println!(
        "  📏 Tamaño: {}",
        format_size(file_size)
    );
    println!();

    let pb = ProgressBar::new(file_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  👻 [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({percent}%)"
        )
        .unwrap()
        .progress_chars("█▓▒░  "),
    );

    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut overlap = vec![0u8; 0];
    let mut found_files: Vec<FoundFile> = Vec::new();
    let mut total_read: u64 = 0;
    let mut file_index: usize = 0;

    // Precalcular el máximo alcance de verificación para saber cuánto overlap guardar
    let max_header_len: usize = signatures
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
        .unwrap_or(16);

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        // Combinar overlap del bloque anterior + bloque actual para no perder firmas en fronteras
        let search_buf = if overlap.is_empty() {
            &buffer[..bytes_read]
        } else {
            // Solo para la primera parte, verificar overlap
            let mut combined = overlap.clone();
            combined.extend_from_slice(&buffer[..bytes_read]);
            // Procesamos combined por separado
            check_signatures_in_buffer(
                &combined,
                total_read - overlap.len() as u64,
                signatures,
                &mut found_files,
                &mut file_index,
                file_size,
                &mut file,
            )?;
            overlap.clear();

            // Guardamos overlap para el siguiente bloque
            if bytes_read >= max_header_len {
                overlap = buffer[bytes_read - max_header_len..bytes_read].to_vec();
            }

            total_read += bytes_read as u64;
            pb.set_position(total_read);
            continue;
        };

        check_signatures_in_buffer(
            search_buf,
            total_read,
            signatures,
            &mut found_files,
            &mut file_index,
            file_size,
            &mut file,
        )?;

        // Guardar overlap para el siguiente bloque
        if bytes_read >= max_header_len {
            overlap = buffer[bytes_read - max_header_len..bytes_read].to_vec();
        }

        total_read += bytes_read as u64;
        pb.set_position(total_read);
    }

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
        bytes_scanned: total_read,
        photos_count,
        videos_count,
        audios_count,
    })
}

/// Busca firmas dentro de un buffer
fn check_signatures_in_buffer(
    buf: &[u8],
    base_offset: u64,
    signatures: &[FileSignature],
    found_files: &mut Vec<FoundFile>,
    file_index: &mut usize,
    source_size: u64,
    source_file: &mut File,
) -> Result<()> {
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

                // Determinar tamaño del archivo encontrado
                let size = determine_file_size(
                    sig,
                    absolute_offset,
                    source_size,
                    source_file,
                )?;

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
    Ok(())
}

/// Determina el tamaño de un archivo encontrado buscando su footer o usando max_size.
/// Lee en chunks de 4 MB para soportar archivos grandes sin límite artificial.
fn determine_file_size(
    sig: &FileSignature,
    offset: u64,
    source_size: u64,
    source_file: &mut File,
) -> Result<u64> {
    let max_possible = std::cmp::min(sig.max_size as u64, source_size - offset);

    if let Some(footer) = sig.footer {
        let saved_pos = source_file.stream_position()?;
        source_file.seek(SeekFrom::Start(offset))?;

        const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MB por chunk
        let footer_len = footer.len();
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut file_pos: u64 = 0;
        let mut last_footer_pos: Option<u64> = None;

        while file_pos < max_possible {
            let remaining = (max_possible - file_pos) as usize;
            let to_read = std::cmp::min(CHUNK_SIZE, remaining);
            let read = source_file.read(&mut buf[..to_read])?;
            if read == 0 {
                break;
            }

            if let Some(pos) = find_last_subsequence(&buf[..read], footer) {
                last_footer_pos = Some(file_pos + pos as u64);
            }

            file_pos += read as u64;

            // Retroceder para detectar footers que crucen el límite entre chunks
            if file_pos < max_possible && read >= footer_len {
                let overlap = (footer_len - 1) as i64;
                source_file.seek(SeekFrom::Current(-overlap))?;
                file_pos -= overlap as u64;
            }
        }

        source_file.seek(SeekFrom::Start(saved_pos))?;

        if let Some(footer_pos) = last_footer_pos {
            return Ok(footer_pos + footer_len as u64);
        }

        // Sin footer encontrado, usar tamaño conservador
        Ok(std::cmp::min(file_pos, max_possible))
    } else {
        // Sin footer definido, usar max_size como límite
        Ok(max_possible)
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
}
