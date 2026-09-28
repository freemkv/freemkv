//! Container source files: the one extension-to-scheme table (mpg-output-design v5 §6, G5)
//! the CLI, the picker, the drop filter and the engine all read.

/// Container sources by extension, and the `scheme://` that reads each (design §6, G5): the
/// one table the picker, the drop filter and the engine's source routing derive from.
pub const CONTAINER_SOURCES: &[(&str, &str)] = &[
    ("mkv", "mkv"),
    ("m2ts", "m2ts"),
    ("mts", "m2ts"),
    ("mp4", "mp4"),
    ("mpg", "mpg"),
    ("mpeg", "mpg"),
    ("vob", "mpg"),
];

/// The scheme that reads `path` as a container, from its extension (any case).
pub fn container_scheme(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    CONTAINER_SOURCES
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|(_, s)| *s)
}
