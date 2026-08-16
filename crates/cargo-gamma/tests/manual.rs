use cargo_gamma_lib::internals::commands::Cli;
use clap::Parser as _;

#[test]
fn every_manual_command_parses() {
    let mut checked = 0;

    for line in include_str!("../src/main.rs").lines() {
        let Some(command) = line.strip_prefix("//! cargo gamma ") else {
            continue;
        };

        let command = command
            .split_once(" #")
            .map_or(command, |(command, _comment)| command)
            .trim_end_matches('\\')
            .trim();
        let mut arguments = vec!["cargo gamma"];

        let words = shell_words(command);

        arguments.extend(words.iter().map(String::as_str));

        let _cli =
            Cli::try_parse_from(arguments).unwrap_or_else(|error| panic!("manual command `cargo gamma {command}` does not parse: {error}"));
        checked += 1;
    }

    assert!(checked >= 20, "only {checked} manual commands were checked");
}

fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;

    for character in command.chars() {
        match (quote, character) {
            (Some(open), close) if open == close => quote = None,
            (None, '\'' | '"') => quote = Some(character),
            (None, whitespace) if whitespace.is_whitespace() => {
                if !word.is_empty() {
                    words.push(core::mem::take(&mut word));
                }
            }
            _ => word.push(character),
        }
    }

    assert!(quote.is_none(), "unclosed quote in manual command `{command}`");

    if !word.is_empty() {
        words.push(word);
    }

    for word in &mut words {
        if word.starts_with("$((") {
            "0".clone_into(word);
        }
    }

    words
}
