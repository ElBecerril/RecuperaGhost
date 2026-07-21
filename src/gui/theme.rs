//! Sistema visual de la GUI: paleta, escala tipográfica y tamaños.
//!
//! Valores tomados del mockup de la fase 2 (rediseño de la interfaz). La idea es que el resto de
//! la GUI no invente colores ni tamaños sueltos: todo sale de acá, así el look es uno solo.
//!
//! Dos decisiones de fondo, ambas por el público objetivo (no técnico, vista cansada):
//!
//! 1. **Tema claro fijo**, no el oscuro que egui trae por defecto. Una herramienta de rescate se
//!    usa una vez, con nervios, muchas veces junto a un tutorial: el contraste alto sobre fondo
//!    claro es el terreno conocido.
//! 2. **Todo más grande.** Los defaults de egui (cuerpo 12.5 pt, alto de control 18 pt) son de
//!    una interfaz de programador. Acá el cuerpo va a 16 y el botón principal a 48 pt de alto.

use eframe::egui;
use egui::{Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};

// ── Paleta ────────────────────────────────────────────────────────────────────
// Fija y clara: no sigue al tema del sistema operativo a propósito.

/// Fondo de la ventana.
pub const GROUND: Color32 = Color32::from_rgb(0xF2, 0xF5, 0xF7);
/// Fondo de los controles y las tarjetas (lo que "flota" sobre el fondo).
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
/// Bordes y separadores.
pub const BORDER: Color32 = Color32::from_rgb(0xD5, 0xDC, 0xE2);
/// Texto principal.
pub const TEXT: Color32 = Color32::from_rgb(0x1A, 0x25, 0x30);
/// Texto secundario (aclaraciones, letra chica).
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x51, 0x60, 0x6C);
/// Color de marca: acciones principales y selección.
pub const BRAND: Color32 = Color32::from_rgb(0x0E, 0x74, 0x90);
/// Marca, versión oscura (hover / presionado).
pub const BRAND_DARK: Color32 = Color32::from_rgb(0x15, 0x5E, 0x75);
/// Fondo tenue de marca, para lo que está seleccionado.
pub const BRAND_TINT: Color32 = Color32::from_rgb(0xE3, 0xF3, 0xF7);

/// Bien / archivo íntegro.
pub const OK: Color32 = Color32::from_rgb(0x17, 0x70, 0x3A);
/// Atención / archivo posiblemente dañado.
pub const WARN: Color32 = Color32::from_rgb(0x8A, 0x5A, 0x00);
/// Fondo del panel de atención.
pub const WARN_BG: Color32 = Color32::from_rgb(0xFF, 0xF4, 0xE0);
/// Peligro / error.
pub const DANGER: Color32 = Color32::from_rgb(0xB0, 0x27, 0x1D);
/// Neutro apagado: lo que no se puede verificar (ni se afirma ni se niega).
pub const NEUTRAL: Color32 = Color32::from_rgb(0x66, 0x70, 0x85);

// ── Tamaños ───────────────────────────────────────────────────────────────────

/// Alto mínimo del botón de la acción principal de cada pantalla.
pub const PRIMARY_BUTTON_HEIGHT: f32 = 48.0;

/// Nombre de la familia en negrita, para títulos. egui no deriva negritas solas: `ui.strong()`
/// solo cambia el color, así que la única forma de tener trazo grueso es registrar la variante
/// Bold como una familia aparte y pedirla explícitamente.
const BOLD: &str = "bold";

/// Embebe Atkinson Hyperlegible como fuente de la interfaz.
///
/// La que trae egui por defecto es Ubuntu-Light: chica **y** de trazo fino, la peor combinación
/// para el público de esta herramienta. Atkinson Hyperlegible está diseñada por el Braille
/// Institute justamente para baja visión: diferencia las formas que se confunden entre sí
/// (I/l/1, O/0, b/d), que es donde se pierde alguien leyendo una ruta o un nombre de archivo.
///
/// Licencia SIL Open Font License 1.1 (`assets/fonts/OFL.txt`), compatible con distribuir el
/// binario. Son ~110 KB sobre un `.exe` de 18 MB.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "atkinson".to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/AtkinsonHyperlegible-Regular.ttf"
        )),
    );
    fonts.font_data.insert(
        "atkinson_bold".to_owned(),
        egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/AtkinsonHyperlegible-Bold.ttf"
        )),
    );

    // OJO: se INSERTA al principio, no se reemplaza la lista. Detrás de Atkinson quedan las
    // fuentes de respaldo de egui, que son las que dibujan los emoji (📷 🎬 ⚠ 👻) y símbolos
    // sueltos. Pisar la lista entera los rompe todos.
    let proportional = fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .clone();
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "atkinson".to_owned());

    // La familia en negrita: la Bold adelante y los MISMOS respaldos detrás, para que un título
    // con un emoji no quede con un hueco.
    let mut bold = vec!["atkinson_bold".to_owned()];
    bold.extend(proportional);
    fonts.families.insert(FontFamily::Name(BOLD.into()), bold);

    ctx.set_fonts(fonts);
}

/// Aplica el sistema visual completo. Se llama una vez, desde el `CreationContext`.
pub fn apply(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut style = (*ctx.style()).clone();

    // Escala tipográfica. El default de egui es 12.5 para casi todo, que a un metro de distancia
    // y con la vista cansada no se lee.
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(16.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(17.0, FontFamily::Proportional),
        ),
        (TextStyle::Heading, FontId::new(26.0, bold_family())),
        (
            TextStyle::Monospace,
            FontId::new(14.0, FontFamily::Monospace),
        ),
    ]
    .into();

    // Áreas para dedos, no para francotiradores: el default de alto de control es 18 pt.
    style.spacing.interact_size.y = 40.0;
    style.spacing.button_padding = egui::vec2(14.0, 10.0);
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.indent = 20.0;

    let mut v = egui::Visuals::light();
    v.panel_fill = GROUND;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.extreme_bg_color = CARD; // fondo de los campos de texto
    v.faint_bg_color = GROUND;
    v.hyperlink_color = BRAND;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.window_rounding = Rounding::same(12.0_f32);
    v.menu_rounding = Rounding::same(8.0_f32);

    // Lo seleccionado se pinta con el color de marca.
    v.selection.bg_fill = BRAND_TINT;
    v.selection.stroke = Stroke::new(1.0_f32, BRAND);

    let radius = Rounding::same(8.0_f32);

    // Texto y superficies que no son interactivas.
    v.widgets.noninteractive.bg_fill = GROUND;
    v.widgets.noninteractive.weak_bg_fill = GROUND;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.rounding = radius;

    // Controles en reposo: tarjeta blanca con borde, no gris sobre gris.
    v.widgets.inactive.bg_fill = CARD;
    v.widgets.inactive.weak_bg_fill = CARD;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.rounding = radius;

    // El hover se tiñe de marca para que se vea qué está por clickearse.
    v.widgets.hovered.bg_fill = BRAND_TINT;
    v.widgets.hovered.weak_bg_fill = BRAND_TINT;
    v.widgets.hovered.bg_stroke = Stroke::new(1.5_f32, BRAND);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.rounding = radius;

    // Presionado / activo: tinte de marca con borde marcado, pero el TEXTO SIGUE OSCURO.
    //
    // Ojo acá, no es una decisión estética: `Visuals::strong_text_color()` devuelve literalmente
    // `widgets.active.fg_stroke.color`, así que este color es también el de TODOS los `ui.strong()`
    // de la aplicación — los títulos de cada paso, entre otros. Poner blanco (que era lo "obvio"
    // para un botón presionado en color de marca) dejaba los títulos en blanco sobre fondo claro,
    // o sea invisibles. El botón principal no necesita que esto sea blanco: `primary_button` fija
    // su propio relleno y su propio color de texto.
    v.widgets.active.bg_fill = BRAND_TINT;
    v.widgets.active.weak_bg_fill = BRAND_TINT;
    v.widgets.active.bg_stroke = Stroke::new(2.0_f32, BRAND_DARK);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.active.rounding = radius;

    // Desplegables abiertos.
    v.widgets.open.bg_fill = CARD;
    v.widgets.open.weak_bg_fill = CARD;
    v.widgets.open.bg_stroke = Stroke::new(1.0_f32, BRAND);
    v.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.open.rounding = radius;

    style.visuals = v;
    ctx.set_style(style);
}

/// La familia en negrita, para pedirla en un `FontId`.
pub fn bold_family() -> FontFamily {
    FontFamily::Name(BOLD.into())
}

/// Título de una sección o de una pantalla: negrita de verdad, no solo un color más oscuro.
///
/// Reemplaza a `ui.strong()` en los títulos. `strong()` en egui únicamente cambia el color del
/// texto, así que un título quedaba del mismo grosor que el párrafo de al lado.
pub fn section_title(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(
        egui::RichText::new(text.into())
            .font(FontId::new(17.0, bold_family()))
            .color(TEXT),
    );
}

/// Panel de aviso: fondo tenue, borde del mismo color y el texto adentro.
///
/// Un aviso que importa no puede verse como una línea de texto más entre otras; el bloque es lo
/// que hace que se lea. `fg` se usa para el borde y el texto, `bg` para el fondo.
pub fn notice(ui: &mut egui::Ui, fg: Color32, bg: Color32, text: &str) {
    egui::Frame::none()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, fg))
        .rounding(Rounding::same(8.0_f32))
        .inner_margin(egui::Margin::symmetric(14.0_f32, 12.0_f32))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(fg));
        });
}

/// Botón de la acción principal de una pantalla: color de marca, texto blanco y alto generoso.
///
/// Es la única acción "de peso" de cada pantalla; el resto van como botones normales. Se arma acá
/// y no en cada llamada para que todas las pantallas den exactamente el mismo botón.
pub fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .color(Color32::WHITE)
            .size(17.0),
    )
    .fill(BRAND)
    .min_size(egui::vec2(0.0, PRIMARY_BUTTON_HEIGHT))
}
