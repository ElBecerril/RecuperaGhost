use colored::Colorize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn show_banner() {
    let ghost = r#"
             ▄▄██████▄▄
          ▄▄████████████▄▄
       ██████████████████████
       █████  ████████  █████
       █████  ████████  █████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████████████████████
       ██████  ██████  ██████
        ▀▀▀▀    ▀▀▀▀    ▀▀▀▀
    "#;

    println!("{}", ghost.bright_cyan());
    println!(
        "{}",
        "  ╔══════════════════════════════════════════════╗"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "     👻 R E C U P E G H O S T 👻              ".bright_white().bold(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        "   El Detective de Archivos Perdidos            ".bright_yellow(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╠══════════════════════════════════════════════╣"
            .bright_cyan()
    );
    println!(
        "{}{}{}",
        "  ║".bright_cyan(),
        format!("   by El_Becerril - v{}                         ", VERSION)
            .bright_green(),
        "║".bright_cyan()
    );
    println!(
        "{}",
        "  ╚══════════════════════════════════════════════╝"
            .bright_cyan()
    );
    println!();
    println!(
        "{}",
        "  🔍 Recupera fotos, videos, audios y más...".bright_white()
    );
    println!();
}
