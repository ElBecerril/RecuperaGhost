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
    let documents_dir = output_dir.join("documentos");

    fs::create_dir_all(&photos_dir)
        .with_context(|| format!("No se pudo crear: {}", photos_dir.display()))?;
    fs::create_dir_all(&videos_dir)
        .with_context(|| format!("No se pudo crear: {}", videos_dir.display()))?;
    fs::create_dir_all(&audios_dir)
        .with_context(|| format!("No se pudo crear: {}", audios_dir.display()))?;
    fs::create_dir_all(&documents_dir)
        .with_context(|| format!("No se pudo crear: {}", documents_dir.display()))?;

    let mut source = File::open(source_path)
        .with_context(|| format!("No se pudo abrir: {}", source_path.display()))?;

    println!("  📂 Guardando en: {}", output_dir.display());
    println!();

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "  👻 Recuperando [{bar:40.green/white}] {pos}/{len} archivos",
        )
        .unwrap()
        .progress_chars("█▓▒░  "),
    );

    let mut recovered = 0u64;
    let mut truncated = 0u64;
    let mut failed = 0u64;
    let mut total_bytes = 0u64;
    let mut errors: Vec<String> = Vec::new();
    const MAX_ERRORS_GUARDADOS: usize = 3;

    for found in files {
        let dest_dir = match found.signature.category {
            crate::signatures::FileCategory::Photo => &photos_dir,
            crate::signatures::FileCategory::Video => &videos_dir,
            crate::signatures::FileCategory::Audio => &audios_dir,
            crate::signatures::FileCategory::Document => &documents_dir,
        };

        let filename = found.recovered_filename();
        let dest_path = dest_dir.join(&filename);

        match extract_file(&mut source, found, &dest_path) {
            Ok(bytes_written) => {
                total_bytes += bytes_written;
                if bytes_written != found.size {
                    truncated += 1;
                    if errors.len() < MAX_ERRORS_GUARDADOS {
                        errors.push(format!(
                            "{}: truncado, se escribieron {} de {} bytes esperados",
                            filename, bytes_written, found.size
                        ));
                    }
                } else {
                    recovered += 1;
                }
            }
            Err(e) => {
                failed += 1;
                if errors.len() < MAX_ERRORS_GUARDADOS {
                    errors.push(format!("{}: {:#}", filename, e));
                }
            }
        }

        pb.inc(1);
    }

    pb.finish_with_message("✅ Recuperación completada");
    println!();

    Ok(RecoveryResult {
        recovered,
        truncated,
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
    let leading_padding = (found.offset - aligned_offset) as usize;

    source
        .seek(SeekFrom::Start(aligned_offset))
        .with_context(|| format!("No se pudo posicionar en offset {}", aligned_offset))?;

    let mut dest_file =
        File::create(dest).with_context(|| format!("No se pudo crear: {}", dest.display()))?;

    // A partir de acá el archivo de destino ya existe en disco. Si `copy_to_dest` falla
    // (lectura del origen o escritura al destino a mitad de camino), el archivo parcial
    // queda huérfano a menos que lo borremos explícitamente antes de propagar el error.
    match copy_to_dest(source, &mut dest_file, found, leading_padding, dest) {
        Ok(total_written) => Ok(total_written),
        Err(e) => {
            drop(dest_file);
            let _ = std::fs::remove_file(dest);
            Err(e)
        }
    }
}

/// Copia los bytes del archivo encontrado desde `source` hacia `dest_file`, saltando el
/// padding inicial de alineación a sector. Separado de `extract_file` para poder limpiar
/// el archivo parcial en un único lugar cuando esto devuelve `Err`.
fn copy_to_dest(
    source: &mut File,
    dest_file: &mut File,
    found: &FoundFile,
    mut leading_padding: usize,
    dest: &Path,
) -> Result<u64> {
    const SECTOR: u64 = 512;

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
    /// Archivos que llegaron a EOF del origen antes de completar `found.size` bytes: se
    /// escribieron al destino pero quedaron incompletos/truncados. No cuentan como
    /// `recovered` ni como `failed` — son su propia categoría para que el resumen no los
    /// reporte como éxito pleno silenciosamente.
    pub truncated: u64,
    pub failed: u64,
    pub total_bytes: u64,
    pub output_dir: PathBuf,
    /// Mensajes de los primeros errores de extracción y truncamientos (hasta
    /// MAX_ERRORS_GUARDADOS), cada uno prefijado con el nombre de archivo de destino, para
    /// que el usuario sepa la causa real y a qué archivo corresponde.
    pub errors: Vec<String>,
}

impl RecoveryResult {
    pub fn summary(&self) -> String {
        // `errors` mezcla mensajes de fallos reales y de truncamientos (cada uno ya
        // autodescriptivo, ej. "recovered_0001.jpg: truncado, se escribieron..." vs
        // "recovered_0002.jpg: <error de I/O>"), así que el conteo de la línea de detalle
        // usa `total_problems` (failed + truncated), no `self.failed` solo — mostrar
        // "Fallidos: 0" mientras se listan causas que en realidad son truncamientos
        // confundía al usuario.
        let total_problems = self.failed + self.truncated;
        let failed_line = format!("  ❌ Fallidos: {}", self.failed);

        let truncated_line = if self.truncated > 0 {
            format!("\n  ⚠️  Truncados (incompletos): {}", self.truncated)
        } else {
            String::new()
        };

        let detail_line = if total_problems > 0 && !self.errors.is_empty() {
            let mut causas = self.errors.join(" | ");
            if total_problems as usize > self.errors.len() {
                causas.push_str(&format!(
                    " (+{} más)",
                    total_problems as usize - self.errors.len()
                ));
            }
            format!("\n     Detalle: {}", causas)
        } else {
            String::new()
        };

        format!(
            "  ✅ Archivos recuperados: {}\n{}{}{}\n  💾 Datos recuperados: {}\n  📂 Ubicación: {}",
            self.recovered,
            failed_line,
            truncated_line,
            detail_line,
            format_size(self.total_bytes),
            self.output_dir.display(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signatures::{FileCategory, FileSignature};

    /// Firma mínima de prueba: sin footer, header de 1 byte, para no depender de las firmas
    /// reales del módulo `signatures`.
    const TEST_SIGNATURE: FileSignature = FileSignature {
        name: "TEST",
        extension: "test",
        category: FileCategory::Document,
        header: &[0xAA],
        header_offset: 0,
        extra_check: None,
        footer: None,
        max_size: 4096,
        validator: None,
        size_from_header: None,
    };

    fn make_found(offset: u64, size: u64, index: usize) -> FoundFile {
        FoundFile {
            signature: TEST_SIGNATURE.clone(),
            offset,
            size,
            index,
            footer_found: false,
        }
    }

    /// Bug 1: si el tamaño estimado (`found.size`) excede lo que realmente hay disponible en
    /// el origen (ej. firma sin footer que se quedó en `max_size`), `recover_files` no debe
    /// contarlo como un éxito pleno idéntico a uno completo: debe quedar en `truncated`, no en
    /// `recovered`, y su archivo debe aparecer en `errors`.
    #[test]
    fn test_truncated_file_not_counted_as_recovered() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        // Solo 100 bytes disponibles en el origen a partir del offset 0.
        source_file.write_all(&[0x42u8; 100]).unwrap();
        source_file.flush().unwrap();

        // Se "encontró" un archivo que supuestamente mide 500 bytes, pero el origen
        // solo tiene 100 disponibles: debe quedar truncado.
        let found = make_found(0, 500, 1);
        let output_dir = tempfile::tempdir().unwrap();

        let result = recover_files(source_file.path(), &[found], output_dir.path()).unwrap();

        assert_eq!(result.recovered, 0, "no debe contar como recuperado pleno");
        assert_eq!(result.truncated, 1, "debe quedar registrado como truncado");
        assert_eq!(result.failed, 0);
        assert_eq!(
            result.total_bytes, 100,
            "debe reportar los bytes realmente escritos"
        );
        assert_eq!(result.errors.len(), 1);
        assert!(
            result.errors[0].contains("recovered_0001.test"),
            "el error debe indicar a que archivo corresponde: {}",
            result.errors[0]
        );
        assert!(
            result.errors[0].contains("truncado"),
            "el error debe indicar que fue un truncamiento: {}",
            result.errors[0]
        );

        // El archivo parcial sí debe existir en disco (100 bytes recuperados son mejor que
        // nada), solo que reportado como incompleto en vez de éxito pleno.
        let dest = output_dir
            .path()
            .join("documentos")
            .join("recovered_0001.test");
        assert!(dest.exists());
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), 100);
    }

    /// Bug 1 (caso feliz): cuando el origen tiene suficientes bytes, el archivo debe contarse
    /// como recuperado pleno y no como truncado.
    #[test]
    fn test_complete_file_counted_as_recovered() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        source_file.write_all(&vec![0x42u8; 500]).unwrap();
        source_file.flush().unwrap();

        let found = make_found(0, 200, 1);
        let output_dir = tempfile::tempdir().unwrap();

        let result = recover_files(source_file.path(), &[found], output_dir.path()).unwrap();

        assert_eq!(result.recovered, 1);
        assert_eq!(result.truncated, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.total_bytes, 200);
        assert!(result.errors.is_empty());
    }
}
