use tracing_subscriber::filter::LevelFilter;

/// Map `-v` count to a tracing level (0 → WARN, 1 → INFO, 2 → DEBUG,
/// 3+ → TRACE) and send all diagnostics to stderr so stdout stays clean
/// for human or JSON output.
pub fn init(verbosity: u8, no_color: bool) {
    let level = match verbosity {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(!no_color)
        .with_max_level(level)
        .init();
}
