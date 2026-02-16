use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};

use crate::scanner::FoundFile;
use crate::signatures::format_size;

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
            Err(_e) => {
                failed += 1;
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
    })
}

/// Extrae un archivo individual del origen
fn extract_file(source: &mut File, found: &FoundFile, dest: &Path) -> Result<u64> {
    source.seek(SeekFrom::Start(found.offset))?;

    let mut dest_file = File::create(dest)
        .with_context(|| format!("No se pudo crear: {}", dest.display()))?;

    let mut remaining = found.size;
    let mut buf = vec![0u8; 64 * 1024]; // 64 KB chunks
    let mut total_written = 0u64;

    while remaining > 0 {
        let to_read = std::cmp::min(remaining as usize, buf.len());
        let bytes_read = source.read(&mut buf[..to_read])?;
        if bytes_read == 0 {
            break;
        }

        dest_file.write_all(&buf[..bytes_read])?;
        remaining -= bytes_read as u64;
        total_written += bytes_read as u64;
    }

    Ok(total_written)
}

/// Resultado de la recuperación
pub struct RecoveryResult {
    pub recovered: u64,
    pub failed: u64,
    pub total_bytes: u64,
    pub output_dir: PathBuf,
}

impl RecoveryResult {
    pub fn summary(&self) -> String {
        format!(
            "  ✅ Archivos recuperados: {}\n  ❌ Fallidos: {}\n  💾 Datos recuperados: {}\n  📂 Ubicación: {}",
            self.recovered,
            self.failed,
            format_size(self.total_bytes),
            self.output_dir.display(),
        )
    }
}
