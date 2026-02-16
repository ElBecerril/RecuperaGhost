use std::fmt;

/// Categoría de archivo multimedia
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCategory {
    Photo,
    Video,
    Audio,
}

impl fmt::Display for FileCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileCategory::Photo => write!(f, "📷 Foto"),
            FileCategory::Video => write!(f, "🎬 Video"),
            FileCategory::Audio => write!(f, "🎵 Audio"),
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
}

impl fmt::Display for FileSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (.{})", self.name, self.extension)
    }
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
        },
        FileSignature {
            name: "MOV",
            extension: "mov",
            category: FileCategory::Video,
            // "moov" atom
            header: &[0x6D, 0x6F, 0x6F, 0x76],
            header_offset: 4,
            extra_check: None,
            footer: None,
            max_size: 2 * 1024 * 1024 * 1024,
        },
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

/// Formatea un tamaño en bytes de forma legible
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}
