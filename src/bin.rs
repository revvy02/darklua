mod cli;

use std::process;

use anstyle::{AnsiColor, Color, Style};
use clap::Parser;
use cli::Darklua;
use env_logger::Builder;
use log::Level;

fn main() {
    // A panic on the file-watcher thread (notify's debouncer callback) would
    // otherwise leave the main thread parked on its channel forever: a live
    // process with a dead watcher that silently stops processing changes. Make
    // every panic fatal so a supervisor sees a non-zero exit and can restart.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        process::exit(101);
    }));

    let darklua = Darklua::parse();

    let filter = darklua.get_log_level_filter();

    formatted_logger().filter_module("darklua", filter).init();

    match darklua.run() {
        Ok(()) => {}
        Err(err) => {
            process::exit(err.exit_code());
        }
    }
}

fn formatted_logger() -> Builder {
    let mut builder = Builder::new();
    builder.format(|f, record| {
        use std::io::Write;

        let level = record.level();
        let (style, text) = colored_level(level);

        writeln!(f, " {style}{text}{style:#} > {}", record.args())
    });
    builder
}

fn colored_level(level: Level) -> (Style, &'static str) {
    let (color, text) = match level {
        Level::Trace => (AnsiColor::Magenta, "TRACE"),
        Level::Debug => (AnsiColor::Blue, "DEBUG"),
        Level::Info => (AnsiColor::Green, "INFO"),
        Level::Warn => (AnsiColor::Yellow, "WARN"),
        Level::Error => (AnsiColor::Red, "ERROR"),
    };
    (Style::new().fg_color(Some(Color::Ansi(color))), text)
}
