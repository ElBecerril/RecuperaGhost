use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use crate::scanner::FoundFile;
use crate::util::format_size;

/// Recupera los archivos encontrados extrayéndolos del origen
pub fn recover_files(
    source_path: &Path,
    files: &[FoundFile],
    output_dir: &Path,
) -> Result<RecoveryResult> {
    // Crear directorio de salida con subcarpetas por tipo
    let photos_dir = output_dir.join("fotos");
    let videos_dir = output_dir.join("videos");
    let audios_dir = output_dir.join("audios");

    fs::create_dir_all(&photos_dir)
        .with_context(|| format!("No se pudo crear: {}", photos_dir.display()))?;
    fs::create_dir_all(&videos_dir)
        .with_context(|| format!("No se pudo crear: {}", videos_dir.display()))?;
    fs::create_dir_all(&audios_dir)
        .with_context(|| format!("No se pudo crear: {}", audios_dir.display()))?;

    let mut source = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;

    println!("  📂 Guardando en: {}", output_dir.display());
    println!();

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "  👻 Recuperando [{bar:40.green/white}] {pos}/{len} archivos"
        )
        .unwrap()
        .progress_chars("█▓▒░  "),
    );

    let mut recovered = 0u64;
    let mut failed = 0u64;
    let mut total_bytes = 0u64;
    let mut errors: Vec<String> = Vec::new();
    const MAX_ERRORS_GUARDADOS: usize = 3;

    for found in files {
        let dest_dir = match found.signature.category {
            crate::signatures::FileCategory::Photo => &photos_dir,
            crate::signatures::FileCategory::Video => &videos_dir,
            crate::signatures::FileCategory::Audio => &audios_dir,
        };

        let filename = format!(
            "recovered_{:04}.{}",
            found.index, found.signature.extension
        );
        let dest_path = dest_dir.join(&filename);

        match extract_file(&mut source, found, &dest_path) {
            Ok(bytes_written) => {
                recovered += 1;
                total_bytes += bytes_written;
            }
            Err(e) => {
                failed += 1;
                if errors.len() < MAX_ERRORS_GUARDADOS {
                    errors.push(format!("{:#}", e));
                }
            }
        }

        pb.inc(1);
    }

    pb.finish_with_message("✅ Recuperación completada");
    println!();

    Ok(RecoveryResult {
        recovered,
        failed,
        total_bytes,
        output_dir: output_dir.to_path_buf(),
        errors,
    })
}

/// Extrae un archivo individual del origen.
///
/// Los handles de disco físico en Windows (rutas tipo `\\.\PhysicalDriveN`) exigen que el
/// offset y el tamaño de cada lectura estén alineados a 512 bytes (tamaño de sector), igual
/// que en el escáner (ver `calculate_segments` en src/scanner/mod.rs). Aquí alineamos el seek
/// hacia abajo al múltiplo de 512 más cercano, leemos en bloques también múltiplos de 512, y
/// descartamos el padding sobrante (al inicio y al final) antes de escribir al archivo destino.
fn extract_file(source: &mut File, found: &FoundFile, dest: &Path) -> Result<u64> {
    const SECTOR: u64 = 512;

    let aligned_offset = (found.offset / SECTOR) * SECTOR;
    let mut leading_padding = (found.offset - aligned_offset) as usize;

    source
        .seek(SeekFrom::Start(aligned_offset))
        .with_context(|| format!("No se pudo posicionar en offset {}", aligned_offset))?;

    let mut dest_file = File::create(dest)
        .with_context(|| format!("No se pudo crear: {}", dest.display()))?;

    // Total de bytes a leer desde el offset alineado (incluye el padding inicial),
    // redondeado hacia arriba al siguiente múltiplo de 512.
    let total_needed = leading_padding as u64 + found.size;
    let mut remaining_read = total_needed.div_ceil(SECTOR) * SECTOR;

    let mut remaining_output = found.size;
    let mut buf = vec![0u8; 64 * 1024]; // 64 KB, múltiplo de 512
    let mut total_written = 0u64;

    while remaining_read > 0 && remaining_output > 0 {
        let to_read = std::cmp::min(remaining_read, buf.len() as u64) as usize;

        // Sub-loop tolerante a short reads: sigue leyendo hasta completar `to_read`
        // bytes (para no desalinear el cursor a 512) o hasta EOF real (read() == 0).
        let mut filled = 0usize;
        while filled < to_read {
            let n = source
                .read(&mut buf[filled..to_read])
                .with_context(|| format!("Error leyendo en offset {}", found.offset))?;
            if n == 0 {
                break; // EOF real, no es un error
            }
            filled += n;
        }
        let bytes_read = filled;
        if bytes_read == 0 {
            break;
        }
        remaining_read -= bytes_read as u64;

        let mut chunk = &buf[..bytes_read];
        if leading_padding > 0 {
            let skip_now = std::cmp::min(leading_padding, chunk.len());
            chunk = &chunk[skip_now..];
            leading_padding -= skip_now;
        }
        if chunk.is_empty() {
            continue;
        }

        let write_len = std::cmp::min(chunk.len() as u64, remaining_output) as usize;
        dest_file
            .write_all(&chunk[..write_len])
            .with_context(|| format!("No se pudo escribir en: {}", dest.display()))?;
        total_written += write_len as u64;
        remaining_output -= write_len as u64;
    }

    Ok(total_written)
}

/// Resultado de la recuperación
pub struct RecoveryResult {
    pub recovered: u64,
    pub failed: u64,
    pub total_bytes: u64,
    pub output_dir: PathBuf,
    /// Mensajes de los primeros errores de extracción (hasta MAX_ERRORS_GUARDADOS), para
    /// que el usuario sepa la causa real de los "Fallidos" en vez de solo un conteo.
    pub errors: Vec<String>,
}

impl RecoveryResult {
    pub fn summary(&self) -> String {
        let failed_line = if self.failed > 0 && !self.errors.is_empty() {
            let mut causas = self.errors.join(" | ");
            if self.failed as usize > self.errors.len() {
                causas.push_str(&format!(" (+{} más)", self.failed as usize - self.errors.len()));
            }
            format!(
                "  ❌ Fallidos: {} (errores: {})",
                self.failed, causas
            )
        } else {
            format!("  ❌ Fallidos: {}", self.failed)
        };

        format!(
            "  ✅ Archivos recuperados: {}\n{}\n  💾 Datos recuperados: {}\n  📂 Ubicación: {}",
            self.recovered,
            failed_line,
            format_size(self.total_bytes),
            self.output_dir.display(),
        )
    }
}
