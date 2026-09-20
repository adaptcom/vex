//! Print the command registry and default bindings as Markdown.
use vex_editor::{CommandInput, Keymap, commands::COMMANDS};

fn main() {
    let keymap = Keymap::default();
    println!("# Vex command reference\n");
    println!("Generated from command Rustdoc and the default keymap.\n");
    println!("| Command | Bindings by mode | Description |");
    println!("|---|---|---|");
    for command in COMMANDS {
        let bindings = keymap
            .bindings()
            .filter(|b| b.command.name == command.name)
            .map(|binding| {
                let keys = binding
                    .keys
                    .iter()
                    .map(ToString::to_string)
                    .collect::<String>();
                let argument = match command.input {
                    CommandInput::RegisterSelect | CommandInput::RegisterInsert => "<register>",
                    CommandInput::Character => "<char>",
                    _ => "",
                };
                format!("{:?}: `{keys}{argument}`", binding.mode)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let bindings = if command.name == "insert_character" {
            "Insert: unbound printable characters".to_owned()
        } else if command.name == "insert_text" {
            "Insert: direct literal text events".to_owned()
        } else if command.name == "insert_paste" {
            "Insert: bracketed paste; direct paste events".to_owned()
        } else {
            bindings
        };
        println!(
            "| `{}` | {} | {} |",
            command.name,
            bindings.replace('|', "\\|"),
            command.description().replace('|', "\\|").replace('\n', " ")
        );
    }
}
