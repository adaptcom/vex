//! Print the command registry and default bindings as Markdown.
use vex_editor::{Keymap, commands::COMMANDS};

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
                format!("{:?}: `{keys}`", binding.mode)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let bindings = if command.name == "insert_text" {
            "Insert: unbound printable characters, Enter, Tab; direct text/paste events".to_owned()
        } else {
            bindings
        };
        println!(
            "| `{}` | {} | {} |",
            command.name,
            bindings,
            command.description().replace('|', "\\|").replace('\n', " ")
        );
    }
}
