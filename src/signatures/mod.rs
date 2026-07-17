use std::fmt;

/// Categoría de archivo multimedia
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Photo,
    Video,
    Audio,
    Document,
}

impl fmt::Display for FileCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileCategory::Photo => write!(f, "📷 Foto"),
            FileCategory::Video => write!(f, "🎬 Video"),
            FileCategory::Audio => write!(f, "🎵 Audio"),
            FileCategory::Document => write!(f, "📄 Documento"),
        }
    }
}

/// Firma de archivo: magic bytes de cabecera y pie opcional
#[derive(Debug, Clone)]
pub struct FileSignature {
    pub name: &'static str,
    pub extension: &'static str,
    pub category: FileCategory,
    pub header: &'static [u8],
    pub header_offset: usize,
    /// Verificación adicional: (bytes_esperados, offset_desde_inicio_del_archivo)
    /// Usado para desambiguar formatos que comparten la misma cabecera (ej. RIFF, OggS)
    pub extra_check: Option<(&'static [u8], usize)>,
    pub footer: Option<&'static [u8]>,
    pub max_size: usize,
    /// Validación bit-level adicional para firmas de header corto (2 bytes) que de otra forma
    /// generan falsos positivos masivos en datos de alta entropía (ej. MP3 Sync, AAC ADTS).
    /// `(validador, bytes_necesarios_desde_el_inicio_del_header)`. El validador recibe el slice
    /// del buffer que empieza en el inicio del header y debe tener al menos esa cantidad de bytes.
    pub validator: Option<(fn(&[u8]) -> bool, usize)>,
    /// Para formatos que codifican su propio tamaño en el header (ej. BMP: BITMAPFILEHEADER
    /// trae el tamaño total en offset 2, 4 bytes little-endian) en vez de usar un footer o
    /// `max_size` fijo. `(offset_desde_inicio_del_header, cantidad_de_bytes_LE)`.
    pub size_from_header: Option<(usize, usize)>,
}

impl fmt::Display for FileSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (.{})", self.name, self.extension)
    }
}

/// Tabla de bitrates (kbps) para MPEG1 Layer III, indexada por el campo bitrate_index (4
/// bits) del 3er byte del header. Índices 0 (free) y 15 (inválido) se marcan con 0 y se
/// rechazan antes de usar la tabla.
const MP3_BITRATES_KBPS: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];

/// Tabla de sample rates (Hz) para MPEG1, indexada por sample_rate_index (2 bits). Índice 3
/// (reservado) se marca con 0 y se rechaza antes de usar la tabla.
const MP3_SAMPLE_RATES_HZ: [u32; 4] = [44100, 48000, 32000, 0];

/// Valida un frame header MPEG Audio (usado tras el sync FF FB de "MP3 (Sync)") para
/// descartar los falsos positivos masivos que produce ese header de 2 bytes en datos
/// de alta entropía. Dos niveles de chequeo:
/// 1. Bits reservados en el 3er byte del header (offset 2): bitrate index (bits 7-4) no debe
///    ser 0000 (free) ni 1111 (inválido); sampling rate index (bits 3-2) no debe ser 11
///    (reservado/inválido).
/// 2. Frame chaining (C2 fix v2): estos chequeos de bits solos solo rechazaban ~35-40% de los
///    falsos positivos en datos aleatorios de alta entropía (quedaban ~60-65% pasando, y como
///    esta firma no tiene footer cada uno de esos se carvea entero a max_size). Para
///    fortalecerlo, se calcula el largo del frame MPEG1 Layer III con la fórmula estándar
///    (144000 * bitrate_kbps / sample_rate_hz + padding) a partir de bitrate/sample_rate/
///    padding del propio header, y se verifica que en ese offset exista OTRO syncword válido
///    (11 bits FF Ex). Esto requiere que aparezcan 2 syncwords consecutivos a la distancia
///    matemática exacta, no solo 1 header plausible, lo que reduce drásticamente los falsos
///    positivos. Si no hay suficiente buffer para verificar el segundo syncword (candidato
///    cerca del final del buffer disponible), se acepta el candidato sin ese chequeo extra en
///    vez de rechazarlo solo por falta de datos.
fn validate_mp3_sync(bytes: &[u8]) -> bool {
    if bytes.len() < 3 {
        return false;
    }
    let b2 = bytes[2];
    let bitrate_index = (b2 >> 4) & 0x0F;
    let sample_rate_index = (b2 >> 2) & 0x03;
    if bitrate_index == 0x00 || bitrate_index == 0x0F || sample_rate_index == 0x03 {
        return false;
    }

    let bitrate_kbps = MP3_BITRATES_KBPS[bitrate_index as usize];
    let sample_rate_hz = MP3_SAMPLE_RATES_HZ[sample_rate_index as usize];
    let padding = ((b2 >> 1) & 0x01) as u32;

    // Ya se descartaron bitrate_index/sample_rate_index inválidos arriba, así que ambos son
    // > 0 acá; la división es segura.
    let frame_len = ((144_000 * bitrate_kbps) / sample_rate_hz + padding) as usize;

    if bytes.len() < frame_len + 2 {
        // No hay suficiente buffer para ver el siguiente syncword: aceptar sin chequeo extra.
        return true;
    }

    bytes[frame_len] == 0xFF && (bytes[frame_len + 1] & 0xE0) == 0xE0
}

/// Valida un header ADTS (AAC) tras el syncword FF F1 de 12 bits. El campo layer (2 bits)
/// ya queda fijado por el byte F1 del header. Dos niveles de chequeo:
/// 1. Bits reservados en el 3er byte del header ADTS (offset 2): profile (2 bits, no debe ser
///    11 = reservado) y sampling_frequency_index (4 bits, valores 13-15 son reservados/
///    inválidos para ADTS).
/// 2. Frame chaining (C2 fix v2, mismo motivo que `validate_mp3_sync`): `frame_length` es un
///    campo explícito de 13 bits en los bytes 3-6 del header ADTS (2 bits en el byte 3, 8 bits
///    en el byte 4, 3 bits en el byte 5). Se verifica que en el offset
///    (header_start + frame_length) exista OTRO syncword ADTS válido (12 bits FF Fx). Si no
///    hay suficiente buffer para leerlo, se acepta el candidato sin ese chequeo extra.
fn validate_aac_adts(bytes: &[u8]) -> bool {
    if bytes.len() < 3 {
        return false;
    }
    let b2 = bytes[2];
    let profile = (b2 >> 6) & 0x03;
    let sampling_freq_index = (b2 >> 2) & 0x0F;
    if profile == 0x03 || sampling_freq_index > 12 {
        return false;
    }

    if bytes.len() < 6 {
        // No hay suficiente buffer para leer frame_length (bytes 3-5): aceptar sin chequeo
        // extra.
        return true;
    }
    let b3 = bytes[3];
    let b4 = bytes[4];
    let b5 = bytes[5];
    let frame_length =
        (((b3 & 0x03) as usize) << 11) | ((b4 as usize) << 3) | ((b5 >> 5) as usize);

    // frame_length incluye el propio header ADTS (mínimo 7 bytes sin CRC); un valor menor es
    // estructuralmente inválido.
    if frame_length < 7 {
        return false;
    }

    if bytes.len() < frame_length + 2 {
        // No hay suficiente buffer para ver el siguiente syncword: aceptar sin chequeo extra.
        return true;
    }

    bytes[frame_length] == 0xFF && (bytes[frame_length + 1] & 0xF0) == 0xF0
}

/// HEIC/HEIF y MP4 comparten la misma caja contenedora `ftyp` (ISOBMFF): la única forma de
/// distinguirlos es leer el `major_brand` de 4 bytes que sigue inmediatamente a "ftyp".
const HEIC_BRANDS: [&[u8; 4]; 10] = [
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// `bytes` empieza en "ftyp" (offset del header MP4/HEIC), no en el inicio de la caja.
fn is_heic_brand(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let brand: &[u8; 4] = bytes[4..8].try_into().unwrap();
    HEIC_BRANDS.contains(&brand)
}

/// Valida que la caja `ftyp` sea HEIC/HEIF (major_brand conocido), no MP4 genérico.
fn validate_heic_ftyp(bytes: &[u8]) -> bool {
    is_heic_brand(bytes)
}

/// 3GP y M4A también son cajas `ftyp` (ISOBMFF) válidas y comparten el mismo header/offset
/// que "MP4/M4V" — sin esta exclusión, cualquier .3gp o .m4a real se carvea DOS veces (una
/// bajo su firma propia y otra, redundante, bajo "MP4/M4V"). `bytes` empieza en "ftyp", igual
/// que `is_heic_brand`.
/// El major_brand de 3GP siempre empieza con los 3 bytes "3gp" (el 4to byte es un dígito de
/// versión variable, ej. "3gp4", "3gp5", "3gp6"). El de M4A real es "M4A " (con espacio
/// final), pero acá se compara solo el prefijo de 3 bytes "M4A" — a propósito, para que esta
/// exclusión sea simétrica con lo que la propia firma "M4A" matchea (su `header` también
/// compara solo esos mismos 3 bytes del brand, sin el 4to byte/espacio; ver esa firma más
/// abajo). Si se comparara el brand completo de 4 bytes acá, un candidato con basura en el 4to
/// byte (ej. datos corruptos, no un M4A real) matchearía la firma "M4A" pero no esta
/// exclusión, y volvería a duplicarse bajo "MP4/M4V".
fn is_3gp_or_m4a_brand(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let brand = &bytes[4..8];
    &brand[0..3] == b"3gp" || &brand[0..3] == b"M4A"
}

/// Valida que la caja `ftyp` NO sea HEIC/HEIF ni 3GP/M4A, para que "MP4/M4V" no duplique la
/// detección de fotos HEIC ni de videos/audios 3GP/M4A (mismo header `ftyp`, mismo offset).
fn validate_mp4_generic_ftyp(bytes: &[u8]) -> bool {
    !is_heic_brand(bytes) && !is_3gp_or_m4a_brand(bytes)
}

/// Valida un header BMP (`BITMAPFILEHEADER`) tras el magic "BM" de 2 bytes, para descartar los
/// falsos positivos masivos que produce ese header corto en datos de alta entropía (mismo
/// problema que `validate_mp3_sync`/`validate_aac_adts`, agravado acá porque `size_from_header`
/// lee 4 bytes de basura como tamaño total del archivo si no se valida nada). `bytes` empieza
/// en "BM" (offset 0 del header). Estructura real (little-endian):
/// - offset 0-1: "BM"
/// - offset 2-5 (u32): tamaño total del archivo (`bfSize`)
/// - offset 6-9: reservado1 + reservado2 (no se usan acá)
/// - offset 10-13 (u32): offset a los datos de píxeles (`bfOffBits`)
/// Se verifica que `bfSize` sea mayor al tamaño mínimo de header (14 bytes) y no exceda
/// `max_bmp_size` (el `max_size` de la firma), y que `bfOffBits` sea mayor a 14 y no mayor que
/// `bfSize`. Ambos campos son estructuralmente coherentes en cualquier BMP real, así que este
/// chequeo filtra la enorme mayoría de coincidencias casuales de "BM" en datos aleatorios sin
/// arriesgar rechazar BMPs válidos.
fn validate_bmp_header(bytes: &[u8], max_bmp_size: usize) -> bool {
    if bytes.len() < 14 {
        return false;
    }
    let file_size = u32::from_le_bytes(bytes[2..6].try_into().unwrap()) as usize;
    let pixel_offset = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;

    if file_size <= 14 || file_size > max_bmp_size {
        return false;
    }
    if pixel_offset <= 14 || pixel_offset > file_size {
        return false;
    }
    true
}

/// Adaptador para `FileSignature::validator` (firma `fn(&[u8]) -> bool`): fija `max_bmp_size`
/// al `max_size` de la firma "BMP" (50 MB), ver `validate_bmp_header`.
fn validate_bmp_default_max(bytes: &[u8]) -> bool {
    validate_bmp_header(bytes, 50 * 1024 * 1024)
}

/// Canon CR2 es TIFF little-endian ("II*\0") con un marcador propio "CR\x02\x00" en offset 8
/// (justo despues del puntero a IFD0 del header TIFF estandar).
/// `bytes` empieza en el header TIFF ("II*\0"), no en un offset separado.
fn is_cr2_marker(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    &bytes[8..12] == b"CR\x02\x00"
}

/// Valida que un header "II*\0" sea TIFF genuino y no un CR2 (ver "CR2 (Canon RAW)" abajo),
/// para no duplicar la deteccion del mismo header bajo dos firmas distintas.
fn validate_tiff_le_not_cr2(bytes: &[u8]) -> bool {
    !is_cr2_marker(bytes)
}

/// Retorna todas las firmas conocidas
pub fn all_signatures() -> Vec<FileSignature> {
    vec![
        // ═══════════════════════ FOTOS ═══════════════════════
        FileSignature {
            name: "JPEG",
            extension: "jpg",
            category: FileCategory::Photo,
            header: &[0xFF, 0xD8, 0xFF],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0xFF, 0xD9]),
            max_size: 25 * 1024 * 1024, // 25 MB
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "PNG",
            extension: "png",
            category: FileCategory::Photo,
            header: &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]),
            max_size: 30 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "GIF",
            extension: "gif",
            category: FileCategory::Photo,
            header: &[0x47, 0x49, 0x46, 0x38],
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x00, 0x3B]),
            max_size: 20 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "BMP",
            extension: "bmp",
            category: FileCategory::Photo,
            header: &[0x42, 0x4D],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes ("BM") sin esto genera falsos positivos masivos en datos
            // de alta entropía, agravado por size_from_header abajo (ver validate_bmp_header).
            validator: Some((validate_bmp_default_max, 14)),
            // BITMAPFILEHEADER: tamaño total del archivo en offset 2, 4 bytes little-endian.
            size_from_header: Some((2, 4)),
        },
        FileSignature {
            name: "WebP",
            extension: "webp",
            category: FileCategory::Photo,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x57, 0x45, 0x42, 0x50], 8)), // "WEBP" en offset 8
            footer: None,
            max_size: 25 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "CR2 (Canon RAW)",
            extension: "cr2",
            category: FileCategory::Photo,
            // Mismo header TIFF little-endian que "TIFF" abajo; se desambigua por el
            // marcador "CR\x02\x00" en offset 8.
            header: &[0x49, 0x49, 0x2A, 0x00],
            header_offset: 0,
            extra_check: Some((b"CR\x02\x00", 8)),
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "TIFF",
            extension: "tiff",
            category: FileCategory::Photo,
            header: &[0x49, 0x49, 0x2A, 0x00],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            // NEF (Nikon RAW) tambien es TIFF-based sin un marcador propio a offset fijo
            // (requeriria parsear tags IFD, ej. Make = "NIKON CORPORATION", fuera del alcance
            // de file carving por magic bytes); se recupera bajo esta firma generica como
            // .tiff, contenedor valido que preserva los datos aunque no distinga el fabricante.
            validator: Some((validate_tiff_le_not_cr2, 12)),
            size_from_header: None,
        },
        FileSignature {
            name: "TIFF (big-endian)",
            extension: "tiff",
            category: FileCategory::Photo,
            // Motorola byte order: "MM" + 0x002A (big-endian, vs "II" + 0x2A00 little-endian).
            header: &[0x4D, 0x4D, 0x00, 0x2A],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "HEIC/HEIF",
            extension: "heic",
            category: FileCategory::Photo,
            // Misma caja ftyp (ISOBMFF) que MP4; se desambigua por major_brand via validator.
            header: &[0x66, 0x74, 0x79, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: Some((validate_heic_ftyp, 8)),
            size_from_header: None,
        },

        // ═══════════════════════ VIDEOS ═══════════════════════
        FileSignature {
            name: "MP4/M4V",
            extension: "mp4",
            category: FileCategory::Video,
            // ftyp box
            header: &[0x66, 0x74, 0x79, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024, // 2 GB
            // Excluye brands HEIC/HEIF (ver "HEIC/HEIF" arriba) y 3GP/M4A (ver esas firmas
            // abajo) para no duplicar la deteccion de la misma caja ftyp bajo dos firmas
            // distintas.
            validator: Some((validate_mp4_generic_ftyp, 8)),
            size_from_header: None,
        },
        FileSignature {
            name: "AVI",
            extension: "avi",
            category: FileCategory::Video,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x41, 0x56, 0x49, 0x20], 8)), // "AVI " en offset 8
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "MKV",
            extension: "mkv",
            category: FileCategory::Video,
            header: &[0x1A, 0x45, 0xDF, 0xA3],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "FLV",
            extension: "flv",
            category: FileCategory::Video,
            header: &[0x46, 0x4C, 0x56, 0x01],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 1 * 1024 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        // Nota (M4): la firma "MOV" standalone (atomo "moov" en offset 4 desde cualquier
        // posición) fue eliminada — "moov" puede aparecer en cualquier parte del archivo
        // (normalmente al final), no solo al inicio, así que producía carvings inservibles
        // que empezaban 4 bytes antes de un átomo "moov" aleatorio. Los MOV reales (que
        // empiezan con "ftyp qt  ") ya se detectan vía la firma MP4/M4V de arriba.
        FileSignature {
            name: "3GP",
            extension: "3gp",
            category: FileCategory::Video,
            // ftyp3gp
            header: &[0x66, 0x74, 0x79, 0x70, 0x33, 0x67, 0x70],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 500 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },

        // ═══════════════════════ AUDIO ═══════════════════════
        FileSignature {
            name: "MP3 (ID3)",
            extension: "mp3",
            category: FileCategory::Audio,
            header: &[0x49, 0x44, 0x33],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "MP3 (Sync)",
            extension: "mp3",
            category: FileCategory::Audio,
            header: &[0xFF, 0xFB],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes: sin esto, datos de alta entropía generan cientos de
            // miles de falsos positivos carveados a max_size (ver C2).
            validator: Some((validate_mp3_sync, 3)),
            size_from_header: None,
        },
        FileSignature {
            name: "WAV",
            extension: "wav",
            category: FileCategory::Audio,
            header: &[0x52, 0x49, 0x46, 0x46], // RIFF
            header_offset: 0,
            extra_check: Some((&[0x57, 0x41, 0x56, 0x45], 8)), // "WAVE" en offset 8
            footer: None,
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "FLAC",
            extension: "flac",
            category: FileCategory::Audio,
            header: &[0x66, 0x4C, 0x61, 0x43],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "OGG Vorbis",
            extension: "ogg",
            category: FileCategory::Audio,
            header: &[0x4F, 0x67, 0x67, 0x53], // OggS
            header_offset: 0,
            // "\x01vorbis" en offset 28 (tras cabecera de página OGG con 1 segmento)
            extra_check: Some((&[0x01, 0x76, 0x6F, 0x72, 0x62, 0x69, 0x73], 28)),
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "AAC",
            extension: "aac",
            category: FileCategory::Audio,
            header: &[0xFF, 0xF1],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 50 * 1024 * 1024,
            // Header de solo 2 bytes (syncword ADTS real es de 12 bits): sin esto, datos de
            // alta entropía generan cientos de miles de falsos positivos (ver C2).
            validator: Some((validate_aac_adts, 3)),
            size_from_header: None,
        },
        FileSignature {
            name: "M4A",
            extension: "m4a",
            category: FileCategory::Audio,
            header: &[0x66, 0x74, 0x79, 0x70, 0x4D, 0x34, 0x41],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "WMA",
            extension: "wma",
            category: FileCategory::Audio,
            header: &[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 100 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "AMR (Nota de voz)",
            extension: "amr",
            category: FileCategory::Audio,
            header: &[0x23, 0x21, 0x41, 0x4D, 0x52],
            header_offset: 0,
            extra_check: None,
            footer: None,
            max_size: 20 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
        FileSignature {
            name: "OPUS",
            extension: "opus",
            category: FileCategory::Audio,
            header: &[0x4F, 0x67, 0x67, 0x53], // OggS
            header_offset: 0,
            // "OpusHead" en offset 28 (tras cabecera de página OGG con 1 segmento)
            extra_check: Some((&[0x4F, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64], 28)),
            footer: None,
            max_size: 50 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },

        // ═══════════════════════ DOCUMENTOS ═══════════════════════
        FileSignature {
            name: "PDF",
            extension: "pdf",
            category: FileCategory::Document,
            header: &[0x25, 0x50, 0x44, 0x46, 0x2D], // "%PDF-"
            header_offset: 0,
            extra_check: None,
            footer: Some(&[0x25, 0x25, 0x45, 0x4F, 0x46]), // "%%EOF"
            max_size: 200 * 1024 * 1024,
            validator: None,
            size_from_header: None,
        },
    ]
}

/// Filtra firmas por categoría
pub fn signatures_for_categories(categories: &[FileCategory]) -> Vec<FileSignature> {
    all_signatures()
        .into_iter()
        .filter(|sig| categories.contains(&sig.category))
        .collect()
}

