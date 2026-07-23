use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

use crate::scanner::FoundFile;
use crate::util::format_size;

// Mismo patrón que `scanner` y `clone`: `RECOVERY_IN_PROGRESS` le dice al handler de Ctrl+C que
// hay una recuperación en curso (para cancelarla en vez de matar el programa) y
// `RECOVERY_CANCEL_REQUESTED` es el flag que el handler setea y que el loop de extracción chequea.
// La cancelación es COOPERATIVA: no interrumpe un `read()` ya colgado en el kernel, solo evita
// seguir con el próximo bloque/archivo.
static RECOVERY_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static RECOVERY_CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Contadores de avance de la recuperación en curso. Se pasan POR PARÁMETRO a la implementación
/// (igual que el flag de cancelación) para poder testearlos sin globales y sin que dos tests en
/// paralelo se pisen; los puntos de entrada públicos pasan el global `RECOVERY_PROGRESS`, que es
/// el que lee la GUI para dibujar su propia barra (no tiene la de `indicatif`).
#[derive(Default)]
struct RecoveryProgress {
    files: AtomicU64,
    bytes: AtomicU64,
}

static RECOVERY_PROGRESS: RecoveryProgress = RecoveryProgress {
    files: AtomicU64::new(0),
    bytes: AtomicU64::new(0),
};

/// True si hay una recuperación corriendo ahora mismo. Lo usa el handler de Ctrl+C para decidir
/// entre cancelarla o dejar que el programa termine normalmente.
pub fn is_recovery_in_progress() -> bool {
    RECOVERY_IN_PROGRESS.load(Ordering::SeqCst)
}

/// Pide cancelar la recuperación en curso. Se detiene en el próximo bloque y devuelve el
/// resultado PARCIAL: los archivos ya extraídos son válidos y quedan en disco.
pub fn request_cancel() {
    RECOVERY_CANCEL_REQUESTED.store(true, Ordering::SeqCst);
}

/// Si ya se pidió cancelar la recuperación en curso.
///
/// La GUI la usa para pasar el botón a "Deteniendo…": la cancelación es cooperativa y el archivo
/// en curso se termina de procesar, así que sin esta señal el botón parece no haber hecho nada.
pub fn cancel_requested() -> bool {
    RECOVERY_CANCEL_REQUESTED.load(Ordering::SeqCst)
}

/// Archivos ya procesados (recuperados + truncados + fallidos) en la recuperación en curso.
/// El total contra el que se compara es `files.len()`, que la GUI ya tiene en la mano.
pub fn recovery_progress_files() -> u64 {
    RECOVERY_PROGRESS.files.load(Ordering::Relaxed)
}

/// Bytes ya escritos al destino en la recuperación en curso.
pub fn recovery_progress_bytes() -> u64 {
    RECOVERY_PROGRESS.bytes.load(Ordering::Relaxed)
}

/// Recupera los archivos encontrados extrayéndolos del origen (modo CLI: barra de progreso y
/// mensajes por terminal). Envoltorio delgado sobre `recover_files_impl` — la lógica es la misma
/// que usa la GUI, lo único que cambia es la salida por consola.
pub fn recover_files(
    source_path: &Path,
    files: &[FoundFile],
    output_dir: &Path,
) -> Result<RecoveryResult> {
    recover_files_entry(source_path, files, output_dir, false)
}

/// Igual que `recover_files`, pero SIN salida por terminal (ni `println!` ni barra `indicatif`).
/// Pensada para la GUI: un binario de subsistema gráfico en Windows no tiene consola, así que un
/// `println!` paniquearía. El avance se sigue con `recovery_progress_files()` /
/// `recovery_progress_bytes()` y la cancelación con `request_cancel()`.
pub fn recover_files_quiet(
    source_path: &Path,
    files: &[FoundFile],
    output_dir: &Path,
) -> Result<RecoveryResult> {
    recover_files_entry(source_path, files, output_dir, true)
}

/// Preparación común de los dos puntos de entrada públicos: arma el estado global (flags y
/// contadores) y garantiza limpiarlo con un guard `Drop`, así un `?` a mitad de camino no deja
/// `RECOVERY_IN_PROGRESS` colgado en true (lo que haría que el próximo Ctrl+C no cerrara nada).
fn recover_files_entry(
    source_path: &Path,
    files: &[FoundFile],
    output_dir: &Path,
    quiet: bool,
) -> Result<RecoveryResult> {
    struct InProgressGuard;
    impl Drop for InProgressGuard {
        fn drop(&mut self) {
            RECOVERY_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
    }
    // `RECOVERY_IN_PROGRESS` se levanta AL FINAL, con el resto del estado ya limpio: la GUI usa
    // ese flag para saber cuándo mostrar el progreso y ofrecer "Detener". Si se levantara antes,
    // un clic en Detener caído en esa ventana lo pisaría el reset de acá y la cancelación se
    // perdería en silencio.
    RECOVERY_CANCEL_REQUESTED.store(false, Ordering::SeqCst);
    RECOVERY_PROGRESS.files.store(0, Ordering::Relaxed);
    RECOVERY_PROGRESS.bytes.store(0, Ordering::Relaxed);
    RECOVERY_IN_PROGRESS.store(true, Ordering::SeqCst);
    let _guard = InProgressGuard;

    recover_files_impl(
        source_path,
        files,
        output_dir,
        &RECOVERY_CANCEL_REQUESTED,
        &RECOVERY_PROGRESS,
        quiet,
    )
}

/// Núcleo de la recuperación. `cancel` y `progress` llegan por parámetro (no se leen los globales)
/// para poder testear sin interferencia entre tests en paralelo. `quiet` apaga toda la salida por
/// terminal.
fn recover_files_impl(
    source_path: &Path,
    files: &[FoundFile],
    output_dir: &Path,
    cancel: &AtomicBool,
    progress: &RecoveryProgress,
    quiet: bool,
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

    if !quiet {
        println!("  📂 Guardando en: {}", output_dir.display());
        println!(
            "{}",
            "  ☕ Guardar todo puede tardar. Puedes usar la compu normalmente, yo sigo aquí \
             trabajando. 👻"
                .bright_yellow()
        );
        println!();
    }

    // En modo GUI (`quiet`) no hay barra: el avance se sigue por los contadores atómicos.
    let pb = if quiet {
        None
    } else {
        let pb = ProgressBar::new(files.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "  👻 Recuperando [{bar:40.green/white}] {pos}/{len} archivos",
            )
            .unwrap()
            .progress_chars("█▓▒░  "),
        );
        Some(pb)
    };

    let mut cancelled = false;
    let mut recovered = 0u64;
    let mut truncated = 0u64;
    let mut failed = 0u64;
    let mut total_bytes = 0u64;
    let mut errors: Vec<String> = Vec::new();
    const MAX_ERRORS_GUARDADOS: usize = 3;

    for found in files {
        // Chequeo entre archivos: cortar acá deja el resultado parcial limpio (todo lo ya
        // extraído es válido). El chequeo de grano fino, a mitad de un archivo grande, lo hace
        // `copy_to_dest`.
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }

        let dest_dir = match found.signature.category {
            crate::signatures::FileCategory::Photo => &photos_dir,
            crate::signatures::FileCategory::Video => &videos_dir,
            crate::signatures::FileCategory::Audio => &audios_dir,
            crate::signatures::FileCategory::Document => &documents_dir,
        };

        let filename = found.recovered_filename();
        let dest_path = dest_dir.join(&filename);

        match extract_file(&mut source, found, &dest_path, cancel) {
            Ok(bytes_written) => {
                total_bytes += bytes_written;
                progress.bytes.fetch_add(bytes_written, Ordering::Relaxed);
                if bytes_written == 0 {
                    // Si la cancelación cayó justo entre crear el archivo y escribir el primer
                    // bloque, queda un archivo de 0 bytes: basura no abrible en la carpeta que el
                    // usuario va a mirar, y encima contada como "truncado" con un detalle que no
                    // le dice nada. No hay nada que conservar, así que se borra. Best-effort: si
                    // el borrado falla, se sigue igual (es cosmético, no pérdida de datos).
                    let _ = fs::remove_file(&dest_path);
                } else if bytes_written != found.size {
                    // Un archivo cortado a mitad por la cancelación cae acá: queda en disco pero
                    // incompleto, así que se cuenta como `truncated`, NUNCA como `recovered`.
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

        progress.files.fetch_add(1, Ordering::Relaxed);
        if let Some(pb) = &pb {
            pb.inc(1);
        }
    }

    // Si `copy_to_dest` cortó a mitad de un archivo, la cancelación se ve recién acá. Pero solo
    // cuenta como "interrumpida" si de verdad quedó algo sin procesar: apretar Ctrl+C mientras se
    // escribía el ÚLTIMO archivo, que igual se completó, no dejó nada afuera. Titular "RECUPERACIÓN
    // INTERRUMPIDA" ahí le hace creer a alguien no técnico que perdió archivos que sí tiene.
    let procesados = recovered + truncated + failed;
    if cancel.load(Ordering::SeqCst) && procesados < files.len() as u64 {
        cancelled = true;
    }

    if let Some(pb) = &pb {
        if cancelled {
            pb.abandon_with_message("⏹️  Recuperación cancelada");
        } else {
            pb.finish_with_message("✅ Recuperación completada");
        }
        println!();
    }

    Ok(RecoveryResult {
        recovered,
        truncated,
        failed,
        total_bytes,
        cancelled,
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
fn extract_file(
    source: &mut File,
    found: &FoundFile,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<u64> {
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
    match copy_to_dest(source, &mut dest_file, found, leading_padding, dest, cancel) {
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
    cancel: &AtomicBool,
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
        // Cancelación de grano fino: un solo archivo puede pesar GB, así que chequear solo entre
        // archivos dejaría la cancelación sin respuesta por minutos. Se corta devolviendo `Ok`
        // con lo escrito hasta acá (NO `Err`): el parcial es dato real y no hay que borrarlo,
        // pero como `total_written < found.size` el llamador lo cuenta como `truncated`.
        if cancel.load(Ordering::SeqCst) {
            break;
        }

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
    /// Archivos que quedaron incompletos: o llegaron a EOF del origen antes de completar
    /// `found.size` bytes, o el usuario canceló mientras se escribía ese archivo. Se
    /// escribieron al destino pero quedaron incompletos/truncados. No cuentan como
    /// `recovered` ni como `failed` — son su propia categoría para que el resumen no los
    /// reporte como éxito pleno silenciosamente.
    pub truncated: u64,
    pub failed: u64,
    pub total_bytes: u64,
    /// El usuario canceló antes de terminar. Lo ya extraído sigue siendo válido y se reporta;
    /// nunca se descarta. Mismo criterio que `ScanResult::cancelled`.
    pub cancelled: bool,
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

        // Aviso de cancelación primero: si no, el usuario lee "recuperados: N" y cree que terminó.
        let cancelled_line = if self.cancelled {
            "  ⏹️  Cancelaste la recuperación. Los archivos que alcanzó a guardar están abajo y se pueden abrir;\n     los que faltaban quedaron sin recuperar.\n"
        } else {
            ""
        };

        format!(
            "{}  ✅ Archivos recuperados: {}\n{}{}{}\n  💾 Datos recuperados: {}\n  📂 Ubicación: {}",
            cancelled_line,
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
        assert!(!result.cancelled);
    }

    /// Helper: corre la implementación con flag y contadores LOCALES, sin tocar los globales, para
    /// que estos tests no interfieran con otros corriendo en paralelo.
    fn run_local(
        source: &Path,
        files: &[FoundFile],
        out: &Path,
        cancel: &AtomicBool,
        progress: &RecoveryProgress,
    ) -> RecoveryResult {
        recover_files_impl(source, files, out, cancel, progress, true).unwrap()
    }

    /// Cancelar debe cortar temprano CONSERVANDO lo ya extraído (nunca descartarlo) y reportarlo
    /// con honestidad: los archivos que no se llegaron a procesar simplemente no se cuentan.
    #[test]
    fn test_cancellation_stops_early_and_keeps_partial() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        source_file.write_all(&vec![0x42u8; 4096]).unwrap();
        source_file.flush().unwrap();

        // 3 archivos de 1000 bytes cada uno; se cancela con el flag ya en true, así que corta
        // en el primer chequeo, antes de extraer nada.
        let files: Vec<FoundFile> = (0..3)
            .map(|i| make_found(i as u64 * 1000, 1000, i + 1))
            .collect();
        let output_dir = tempfile::tempdir().unwrap();

        let cancel = AtomicBool::new(true);
        let progress = RecoveryProgress::default();
        let result = run_local(
            source_file.path(),
            &files,
            output_dir.path(),
            &cancel,
            &progress,
        );

        assert!(result.cancelled, "debe reportarse como cancelada");
        assert_eq!(result.recovered, 0);
        assert_eq!(result.truncated, 0);
        assert_eq!(result.failed, 0);
        assert!(result.summary().contains("Cancelaste"));
    }

    /// Cancelar a mitad de un archivo grande: el parcial queda en disco (es dato real) pero se
    /// cuenta como `truncated`, nunca como `recovered`.
    #[test]
    fn test_cancellation_midfile_counts_as_truncated_not_recovered() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        // Más grande que el buffer interno (64 KB) para que haya más de una vuelta del loop.
        source_file.write_all(&vec![0x42u8; 200 * 1024]).unwrap();
        source_file.flush().unwrap();

        let found = make_found(0, 200 * 1024, 1);
        let output_dir = tempfile::tempdir().unwrap();

        // `copy_to_dest` chequea el flag al inicio de cada vuelta del loop de copia, así que con
        // el flag ya en true corta sin completar el archivo. Se ejercita `extract_file` directo
        // porque el chequeo entre archivos de `recover_files_impl` cortaría antes de llegar acá.
        let mut src = File::open(source_file.path()).unwrap();
        let dest = output_dir.path().join("parcial.test");
        let cancel_mid = AtomicBool::new(true);
        let written = extract_file(&mut src, &found, &dest, &cancel_mid).unwrap();

        assert!(
            written < found.size,
            "un archivo cortado por la cancelación no puede darse por completo"
        );
        // Clave: el parcial NO se borra (borrar es solo para errores de I/O). Como
        // `written != found.size`, el llamador lo cuenta como `truncated`, nunca como `recovered`.
        assert!(
            dest.exists(),
            "el parcial debe quedar en disco: es dato real"
        );
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), written);
    }

    /// Los contadores de progreso deben reflejar lo real: archivos procesados y bytes escritos.
    #[test]
    fn test_progress_counters_reflect_real_work() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        source_file.write_all(&vec![0x42u8; 4096]).unwrap();
        source_file.flush().unwrap();

        let files: Vec<FoundFile> = (0..3)
            .map(|i| make_found(i as u64 * 1000, 1000, i + 1))
            .collect();
        let output_dir = tempfile::tempdir().unwrap();

        let cancel = AtomicBool::new(false);
        let progress = RecoveryProgress::default();
        let result = run_local(
            source_file.path(),
            &files,
            output_dir.path(),
            &cancel,
            &progress,
        );

        assert_eq!(result.recovered, 3);
        assert_eq!(
            progress.files.load(Ordering::Relaxed),
            3,
            "un incremento por archivo procesado"
        );
        assert_eq!(
            progress.bytes.load(Ordering::Relaxed),
            result.total_bytes,
            "los bytes del contador deben coincidir con los realmente escritos"
        );
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 3000);
    }

    /// La variante quiet no debe cambiar el resultado respecto del camino del CLI.
    #[test]
    fn test_quiet_variant_matches_cli_result() {
        let mut source_file = tempfile::NamedTempFile::new().unwrap();
        source_file.write_all(&vec![0x42u8; 1024]).unwrap();
        source_file.flush().unwrap();

        let found = make_found(0, 300, 1);
        let out_quiet = tempfile::tempdir().unwrap();
        let result = recover_files_quiet(source_file.path(), &[found], out_quiet.path()).unwrap();

        assert_eq!(result.recovered, 1);
        assert_eq!(result.total_bytes, 300);
        assert!(!result.cancelled);
        // NO se asserta aquí `!is_recovery_in_progress()`: ese flag es un GLOBAL que las entradas
        // públicas de otros tests (recover_files/_quiet) levantan y bajan, así que con el harness
        // en paralelo un vecino a mitad de su recuperación lo dejaba en true y este test fallaba en
        // falso — la misma familia de bug que el falso verde de scan_progress. La limpieza del flag
        // por el Drop guard queda garantizada por el propio guard (se ejecuta aunque haya un `?`).
    }
}
