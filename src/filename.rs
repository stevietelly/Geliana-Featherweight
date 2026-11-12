//! Output-filename generation from user-provided templates.
//!
//! Turns a template string (with date/time/monitor placeholders) into a
//! filesystem-safe filename, applying variable substitution, character
//! sanitization, Windows reserved-name protection, and a length cap.
//!
//! Contents:
//! - [`DEFAULT_TEMPLATE`]: fallback used when the user's template is empty.
//! - [`VARIABLES`]: the placeholder, chrono format mapping, and UI label.
//! - [`render`]: substitutes placeholders, sanitizes the result, and returns a
//!   safe filename component.
//! - `tests`: coverage for variable expansion, path-escape prevention,
//!   reserved-name handling, Unicode preservation, and length bounding.

use chrono::NaiveDateTime;

/// Fallback template used when the user leaves the filename template empty.
///
/// Expands to e.g. `Recording_2026-10-17_09-08-07_123` for a capture started on
/// 2026-10-17 at 09:08:07.123.
pub const DEFAULT_TEMPLATE: &str = "Recording_{d}_{t}_{ms}";

/// Placeholder tags supported in filename templates.
///
/// Each entry is `(tag, chrono_format, label)`:
/// - `tag` — the literal text in the template, e.g. `"{d}"`.
/// - `chrono_format` — the [`chrono`] format string the tag is replaced with.
/// - `label` — human-readable name shown in the UI so the user knows what each
///   placeholder means.
///
/// Tags are chosen to be unambiguous under sequential replacement: `{m}` is
/// month and `{min}` is minute, and neither tag is a substring of the other, so
/// the substitution order does not cause confusion.
pub const VARIABLES: [(&str, &str, &str); 9] = [
    ("{d}", "%Y-%m-%d", "Date"),
    ("{t}", "%H-%M-%S", "Time"),
    ("{y}", "%Y", "Year"),
    ("{m}", "%m", "Month"),
    ("{day}", "%d", "Day"),
    ("{h}", "%H", "Hour"),
    ("{min}", "%M", "Minute"),
    ("{s}", "%S", "Second"),
    ("{ms}", "%3f", "Milliseconds"),
];

/// Renders `template` into a filesystem-safe filename component.
///
/// Steps:
/// 1. Fall back to [`DEFAULT_TEMPLATE`] when `template` is blank.
/// 2. Substitute every [`VARIABLES`] tag and `{monitor}` with values derived
///    from `now` and `monitor`.
/// 3. Replace control characters and Windows-forbidden characters
///    (`<>:"/\|?*`) with `_` so the result cannot escape the output directory.
/// 4. Trim leading/trailing spaces and dots, which Windows strips anyway and
///    which could otherwise collapse or empty the name.
/// 5. Strip a trailing `.mp4` extension if present — the recorder adds its own.
/// 6. Truncate to at most 120 UTF-16 units, leaving room for the extension and
///    a collision suffix, counting UTF-16 (not `char`s) so non-BMP characters
///    like emoji are bounded correctly.
/// 7. If the result is empty, return `"Recording"`.
/// 8. If the leading dot-component is a Windows reserved device name (`CON`,
///    `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, including superscript
///    digits), prefix the name with `_`.
///
/// `now` provides the timestamp used for all date/time substitutions; the
/// caller typically passes the capture start time.
pub fn render(template: &str, now: NaiveDateTime, monitor: usize) -> String {
    let template = if template.trim().is_empty() {
        DEFAULT_TEMPLATE
    } else {
        template
    };
    let mut name = template.to_owned();
    // Substitute each timestamp variable tag with its formatted value.
    for (tag, format, _) in VARIABLES {
        name = name.replace(tag, &now.format(format).to_string());
    }
    // Substitute the monitor index (1-based, as shown in the UI).
    name = name.replace("{monitor}", &monitor.to_string());
    // Sanitize: replace control chars and Windows-forbidden filename chars
    // (< > : " / \ | ? *) so the name cannot escape the output directory.
    name = name
        .chars()
        .map(|ch| {
            if ch.is_control() || "<>:\"/\\|?*".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .collect();
    // Windows strips trailing spaces and dots from filename components, which
    // could turn "foo." into "foo" or leave an empty name; trim them.
    name = name.trim_matches([' ', '.']).to_owned();
    // Strip a user-supplied .mp4 extension; the recorder appends its own.
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(".mp4"))
    {
        name.truncate(name.len() - 4);
    }
    // Leave room for the extension and collision suffix; count Windows UTF-16
    // units so non-BMP characters cannot exceed the component length limit.
    let mut units = 0;
    name = name
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= 120
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    // If nothing usable remains, fall back to a constant name.
    if name.is_empty() {
        return "Recording".to_owned();
    }
    // Detect Windows reserved device names. Windows matches the reserved name
    // against the part before the first dot, ignoring trailing spaces, so we
    // isolate that component, uppercase it, and compare.
    let device = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_uppercase();
    // COM/LPT are reserved only with a numeric suffix (1-9).
    let numbered = device
        .strip_prefix("COM")
        .or_else(|| device.strip_prefix("LPT"));
    // CON/PRN/AUX/NUL are always reserved; COM1-9 and LPT1-9 are reserved too,
    // including the superscript digit variants ¹ ² ³.
    if matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || numbered.is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    {
        // Prefix with _ so the name is no longer a reserved device name.
        name.insert(0, '_');
    }
    name
}

/// Tests for variable expansion, path-escape prevention, reserved-name
/// handling, Unicode preservation, and length bounding.
#[cfg(test)]
mod tests {
    use super::*;
    // Fixed timestamp 2026-10-17 09:08:07.123 used by every test.
    fn now() -> NaiveDateTime {
        NaiveDateTime::parse_from_str("2026-10-17 09:08:07.123", "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }
    #[test]
    fn expands_all_variables_without_confusing_month_and_minute() {
        assert_eq!(
            render(
                "{d}_{t}_{y}_{m}_{day}_{h}_{min}_{s}_{ms}_{monitor}",
                now(),
                2
            ),
            "2026-10-17_09-08-07_2026_10_17_09_08_07_123_2"
        );
    }
    #[test]
    fn filenames_cannot_escape_the_output_directory() {
        let name = render("../../folder\\clip:foo?*.MP4", now(), 1);
        assert_eq!(name, "_.._folder_clip_foo__");
        assert!(!name.contains(['/', '\\', ':']));
    }
    #[test]
    fn empty_templates_and_reserved_windows_names_are_usable() {
        assert_eq!(render(" ", now(), 1), "Recording_2026-10-17_09-08-07_123");
        assert_eq!(render(".mp4", now(), 1), "mp4");
        assert_eq!(render("...", now(), 1), "Recording");
        for name in ["CON", "nul.txt", "COM1", "lpt9", "COM¹"] {
            assert!(render(name, now(), 1).starts_with('_'));
        }
    }
    #[test]
    fn unicode_and_unknown_tags_are_preserved_and_long_names_are_bounded() {
        assert_eq!(render("Запись_{unknown}", now(), 1), "Запись_{unknown}");
        assert!(render(&"🎥".repeat(200), now(), 1).encode_utf16().count() <= 120);
    }
}
